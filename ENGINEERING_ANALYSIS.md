# Herdr engineering analysis

Captured 2026-08-15.

> **Status: this is a snapshot, not a live document.** Every finding below has
> since been addressed; [ENGINEERING_PLAN.md](ENGINEERING_PLAN.md) is the
> execution record and the place to look for current state. The text here is left
> as it was written so that what was predicted can be compared with what was
> found — several predictions were wrong in instructive ways, and the plan says
> which.
>
> Two things to know before using it:
>
> - **The `file:line` references have drifted.** They were verified against
>   `7be24fa0` (plan, phase 0) and eight phases plus an upstream rebase have moved
>   most of them. Spot-checked while writing this note: of seven sampled, one
>   still lands. The prose descriptions are still accurate; search by construct,
>   not by line.
> - **Every acceptance criterion below was validated against a debug build.** The
>   harness had no release path and its reports did not record the profile. Phase 8
>   reran the workloads in release and it changed which findings were real, so
>   treat the numeric targets stated here ("p99 below roughly 50 ms", "normally
>   below 30 ms") as debug-era figures.

The investigation found no P0 issue, but it did identify seven P1 defects and
three P2 performance problems. The highest-value fixes are remote-write
isolation, reconnect generation checks, and bounded API/client queues.

“Confirmed” below means the defect follows directly from an executable code
path or bounded-capacity construction. “High confidence” means the cross-module
failure timeline is strong but has not yet been reproduced.

## 1. Runtime/performance map

| Path | Execution flow and boundaries | Multiplicative work |
|---|---|---|
| Local terminal output | PTY actor thread → `process_pty_bytes` under Ghostty terminal lock → content/detection sequences → coalesced `RenderSignal` → server-loop notification → view computation/render → per-client encoding/write. See `src/pane.rs:1934`, `src/pane/terminal.rs:1194`, and `src/server/headless.rs:660`. | Per PTY read × panes; then per render target and attached client. |
| User input | Client reader thread → bounded server-event channel → main server loop → semantic/raw input handling → local bounded PTY actor queue or remote socket. See `src/server/headless.rs:444` and `src/terminal/runtime.rs:786`. | Per key/mouse/paste event. Remote writes currently block this path. |
| Remote pane | One blocking reader thread per view receives semantic `FrameData` → replaces retained frame under a mutex → dirty flag → main loop polls dirty views → full UI render. Input/resize/scroll use a shared blocking writer. See `src/terminal/remote.rs:136`, `src/terminal/remote.rs:950`, and `src/terminal/remote.rs:1138`. | Full frame bytes × remote panes × frame rate; rendering × distinct client geometry. |
| Peer control | One OS thread per peer: identify → subscribe before enumeration → event reads with 500 ms timeout → 1.5-second pane refresh when views exist → 15-second heartbeat → reconnect/backoff. See `src/server/peer/control.rs:281`. | Per peer; periodic pane enumeration scales with every pane on that peer. |
| Detection | One Tokio task per local pane; 500 ms unidentified, 300 ms identified, and 50 ms transient cadence. Process probing is normally every five seconds; idle screen scans are skipped when content sequence is unchanged. See `src/pane.rs:289`, `src/pane.rs:2157`, and `src/pane.rs:2451`. | Timer ticks × local panes; process walking and screen snapshots are the expensive boundaries. |
| Persistence | Dirty state → debounce → structural and optional history capture on the main loop → background JSON serialization/write. History walks every local pane and materializes all retained ANSI scrollback. See `src/app/session.rs:39` and `src/persist/snapshot.rs:464`. | Panes × retained scrollback; optional history is the dangerous case. |
| JSON API | Listener thread → one thread per connection → unbounded request channel → exhaustive main-loop drain → response channel. Subscriptions remain on their connection thread and poll every 100 ms. See `src/api/server.rs:82`, `src/api/mod.rs:84`, and `src/server/headless.rs:3761`. | Connections × requests/subscriptions; currently no admission or fairness bound. |

The ownership model is mostly sound: `AppState` owns pure session/application
facts, `TerminalRuntimeRegistry` owns runtime handles, local terminal cores are
lock-protected, and peer control threads communicate through `AppEvent`. The
most important violations are blocking I/O entering the sole server loop and
background completions that lack generation identity.

The existing render safeguards are good: local dirty signals coalesce,
AppEvents are bounded at 256 and drained in batches of 64, hidden sources avoid
presentation work, and semantic/ANSI client frames have skip/diff paths.

Actual baseline from
`ZIG=/opt/zig0.15/zig just bench-render-scale`:

| Scenario | 1 pane median | 15 panes | 50 panes |
|---|---:|---:|---:|
| Background workspaces | 512 µs | 568 µs / 1.11× | 556 µs / 1.09× |
| Active panes | 488 µs | 585 µs / 1.20× | 690 µs / 1.41× |

The benchmark passed in 0.31 seconds under a 1-minute load average of 1.10.
Fixed-geometry rendering therefore does not currently look like the primary
bottleneck.

> **Refined by phase 8.** That conclusion holds for what the benchmark measures
> and does not generalise the way this section reads. `bench-render-scale` renders
> synthetic geometry; a real server under output load spends its render time in
> `full_render.render_virtual`, which the benchmark does not exercise. Measured in
> release, a full render costs 1.7 ms at one pane and 27.0 ms at fifty — past a
> frame budget, and 16× rather than the 1.41× above. Render scaling *is* a
> bottleneck; this benchmark is simply not where it shows.

## 2. Measurement gaps

Herdr already has a useful opt-in `HERDR_RENDER_PROF` facility covering render
phases, PTY bytes, changed cells, and encoding bytes in `src/render_prof.rs`.
The missing measurements are mostly outside rendering.

| Gap | Smallest useful mechanism | Built? |
|---|---|---|
| Main-loop stalls and wake latency | Environment-gated loop-iteration histogram: active work duration, sleep duration, maximum stall, and wake reason. | **Yes**, phase 1: `loop.active`, `loop.park`, `loop.wake.*`. |
| Queue/backpressure health | Current/max item count and bytes for API, client control, PTY input, and proposed remote-writer queues; count full/rejected/disconnected outcomes. | **Yes**, phases 1 and 6: `queue.*.depth` gauges plus `queue.pty_input.rejected`. |
| Remote latency | Timestamp frame receipt and first render consumption; record remote bytes/frame, receive-to-render latency, write latency/timeouts, and reconnect attempts. | **Partly.** Bytes per frame and serialize time exist (phase 6); receive-to-render latency does not. |
| Terminal lock contention | Opt-in sampled wait/hold timings around Ghostty core access, split by parser, render snapshot, detection, API read, and persistence. | **No — and now the top of the plan's still-open list.** Phase 8 found parse cost rising from 18 to 141 ns/byte between 1 and 50 panes with no explanation, and no instrument can say whether that is contention. |
| Subscription cost | Polls, events delivered, sequence lag, overflow gaps, `pane_get`/`pane_read` calls, and poll duration by subscription kind. | **Partly.** Covered through `api.pane_info` rather than per-subscription counters. |
| Persistence stalls | Structural capture time, history capture time, pane/history bytes, serialization time, and file-write time separately. | **Partly.** `capture_history` and `resolve_history` are split (phase 5); serialization and file write are not separated. |
| Detection cost | Process-probe duration/count, screen snapshot duration/bytes, manifest evaluation time, and idle-skip counts. | **No.** H3 is unanswered because of it: `api.pane_info` peaks at 114 ms at 50 panes while `foreground_cwd` peaks at 25 ms, and nothing accounts for the rest. |
| Resource cardinality | Periodically sample threads, RSS, pane count, remote views, clients, peers, and pending queue bytes. | **Partly.** Sampled by the stress harness from outside the process, not by the profiler. |

These can use the same opt-in, fixed-name, windowed design as `render_prof`; no
metrics service or high-cardinality pane labels are needed.

## 3. Confirmed/high-confidence bugs

### B1 — P1: stale reconnect completion can resurrect a view on a replaced peer

- **Finding:** Reconnect results identify only the local terminal. They carry
  neither an attempt generation nor the expected peer identity.
- **Evidence:** The worker captures the old instance and sends only
  `terminal_id + result` in `src/app/api/peers/views.rs:63` and
  `src/events.rs:240`. A peer identity change marks the existing runtime dead in
  `src/app/api.rs:275`, but a later success unconditionally replaces it in
  `src/app/api/peers/views.rs:250`.
- **Failure timeline:** old reconnect succeeds → replacement event is processed
  and marks the slot dead → old success event arrives → handler installs the old
  connected runtime. No later identity transition is guaranteed to invalidate
  it.
- **Impact:** The pane can display and accept input for a terminal belonging to
  the obsolete server. The cleared spawned claim limits destructive cleanup,
  but input and displayed identity remain wrong.
- **Trigger:** Peer replacement while a reconnect worker is in flight, combined
  with cross-thread event reordering.
- **Confidence:** High.
- **Fix:** Add a monotonically increasing reconnect generation and expected peer
  instance to the result. Apply it only if the current runtime is still
  in-flight, non-dead, has the same peer/target, and the current peer registry
  still reports that instance. Shut down stale successful runtimes.
- **Validation:** Deterministic unit timeline: begin reconnect, process a
  replacement identity, deliver the old success, and assert that the dead view
  remains dead and the returned runtime is shut down.

### B2 — P1: event subscriptions silently lose burst events

- **Finding:** The event replay ring evicts old entries without exposing a
  sequence gap, while each subscription can emit only one matching event every
  100 ms.
- **Evidence:** `EventHub` retains 512 entries and silently drains overflow in
  `src/api/event_hub.rs:12`. `ActiveEventSubscription` returns after its first
  match in `src/api/subscriptions.rs:332`, and the connection then sleeps 100 ms
  in `src/api/server.rs:722`.
- **Impact:** Sustained matching throughput above 10 events/sec or a burst
  exceeding the replay window produces incomplete automation/lifecycle history
  with no resync signal.
- **Trigger:** Bursty pane/workspace/plugin events, slow socket writes, or a
  subscriber falling more than 512 global events behind.
- **Confidence:** Confirmed.
- **Fix:** Drain a bounded batch per wake and have the hub return an explicit gap
  result when `last_sequence + 1` predates the oldest retained event.
  Close/resync the subscription or emit a documented overflow event.
- **Validation:** Publish 600 matching events while delaying the subscriber.
  Assert either complete ordered delivery or one explicit overflow
  indication—never silent truncation.

### B3 — P1: failed peer-pane cleanup permanently leaks remote PTYs

- **Finding:** When closing a pane spawned on a peer fails, Herdr keeps only a
  counter; the pane ID required for retry is discarded.
- **Evidence:** Cleanup is fire-and-forget in
  `src/app/api/peers/lifecycle.rs:520`. Failures become
  `PeerPaneCleanupFailed`, but `src/app/peers.rs:308` stores only a count. The
  failed-open path explicitly states that no reaper exists in
  `src/app/api/peers/forward.rs:1194`.
- **Impact:** Remote shells, agents, memory, process trees, and scrollback can
  remain alive indefinitely after their local view disappears.
- **Trigger:** Peer disconnect/restart during close, or successful remote
  creation followed by local attach failure.
- **Confidence:** Confirmed.
- **Fix:** Retain pending cleanup records containing peer handle, expected peer
  instance ID, and peer pane ID. Retry with bounded backoff after reconnection
  only when the instance still matches. Expose unresolved records for explicit
  cleanup instead of guessing after server replacement.
- **Validation:** Make the first close fail, reconnect the same peer instance,
  and verify exactly one successful retry. Replace the peer instance and verify
  no close is sent for the stale ID.

### B4 — P1: blocking-writer queues can grow without bound

- **Finding:** Two writer paths defeat backpressure:
  - Client control messages use an unbounded `VecDeque`, while the socket writer
    has no send timeout.
  - Windows PTY input enters a bounded 1,024-item channel but is immediately
    forwarded into an unbounded `std::mpsc` queue behind the actual writer.
- **Evidence:** Client queue and enqueue are in
  `src/server/client_transport.rs:181` and
  `src/server/client_transport.rs:218`; blocking `write_all`/`flush` is in
  `src/server/client_transport.rs:729`. Windows staging is in
  `src/pty/actor.rs:146`.
- **Impact:** Unbounded memory growth. A client that remains connected but stops
  reading can accumulate clipboard, notification, bell, mode, and other
  reliable messages. A stalled ConPTY writer can accumulate input and terminal
  responses.
- **Trigger:** Slow/non-reading client or Windows child that stops consuming
  input.
- **Confidence:** Confirmed.
- **Fix:** Account queue bytes as well as items. Disconnect a client when its
  reliable backlog crosses the limit and give its writer a bounded send
  timeout. On Windows, replace the hidden unbounded stage with a bounded,
  priority-aware writer queue.
- **Validation:** Use a client and a fake Windows writer that stop reading. Flood
  messages and assert bounded bytes, deterministic disconnect/backpressure, and
  writer-thread exit.

### B5 — P1: API ingress permits thread/memory exhaustion and server-loop starvation

- **Finding:** Every accepted API connection spawns an unchecked OS thread,
  requests enter an unbounded channel, and the sole server loop drains that
  channel until it becomes empty.
- **Evidence:** Thread-per-connection is in `src/api/server.rs:105`, the
  unbounded sender type is in `src/api/mod.rs:91`, and the exhaustive drain is
  in `src/server/headless.rs:3761`. By contrast, AppEvents already use a bounded
  channel and fixed drain budget.
- **Impact:** Slowloris connections consume stacks/threads; large request bursts
  retain up to 1 MiB each; a continuously replenished queue can prevent
  rendering, input, scheduled work, and peer reconciliation. Thread-spawn
  failure can also kill the API listener thread.
- **Trigger:** Many partial connections, automation bursts, or a same-user
  process flooding the local socket.
- **Confidence:** Confirmed.
- **Fix:** Cap concurrent connections, use a bounded request channel with an
  explicit overload response, and drain a fixed request batch per loop
  iteration.
- **Validation:** Hold hundreds of partial connections and separately flood
  large valid requests. Assert bounded threads/RSS, explicit overload errors,
  and continued local render/input responsiveness.

## 4. Performance findings

### P1 — Remote writes can freeze the entire server for five seconds

- **Finding:** Remote input, resize, scroll, paste, and mouse messages perform
  blocking socket writes directly from the main server path.
- **Evidence:** The configured write timeout is five seconds in
  `src/terminal/remote.rs:90`; `send` locks the writer and calls framed write
  plus flush in `src/terminal/remote.rs:950`. The shared runtime API invokes it
  synchronously in `src/terminal/runtime.rs:786`. Delayed prompt submission also
  performs blocking I/O on a Tokio worker in `src/terminal/remote.rs:924`.
- **Impact:** Up to five seconds of lost responsiveness for all local panes,
  clients, API calls, timers, and peers.
- **Trigger:** A connected peer whose socket stops draining after buffers fill.
- **Confidence:** Confirmed.
- **Fix:** Give each remote view a bounded writer actor. Preserve input order,
  coalesce resize, and mark the connection failed when the queue or write
  deadline is exceeded.
- **Validation/workload:** Fake peer completes the handshake and then stops
  reading; fill the socket and send input while probing local API/render
  responsiveness.
- **Measurement:** Main-loop maximum stall, input enqueue latency, writer queue
  bytes, write timeout count.
- **Expected result:** Local p99 response stays below roughly 50 ms rather than
  approaching five seconds; remote backlog remains bounded.

### P2 — API requests are read one byte per read call

- **Finding:** Initial JSON request parsing uses a one-byte buffer and polls the
  stream for every byte.
- **Evidence:** `src/api/server.rs:539` declares `[0u8; 1]`, reads it, and pushes
  one byte. The repository already has a counted chunked helper in
  `src/ipc.rs:185`.
- **Impact:** Thousands of read calls for ordinary multi-kilobyte JSON and up to
  roughly one million for the maximum request.
- **Trigger:** Every nontrivial API request; amplified by bursts and large
  plugin/graphics metadata.
- **Confidence:** Confirmed.
- **Fix:** Use `poll_local_stream_read_count` with an 8–16 KiB buffer, append up
  to the newline/limit, and preserve the existing timeout behavior.
- **Validation/workload:** Parse identical 128 B, 4 KiB, and 1 MiB lines through
  the existing local-stream tests while counting reads.
- **Measurement:** Read calls/request, parse CPU, request latency.
- **Expected result:** Read calls drop from approximately `N` to
  `ceil(N / buffer_size)`—orders of magnitude for large requests.

### P2 — Remote frames do not wake the server loop

- **Finding:** Frame receipt only stores the frame and sets an atomic dirty flag.
  It sends no event or render notification.
- **Evidence:** `src/terminal/remote.rs:136` sets `dirty`;
  `src/terminal/remote.rs:1138` calls it from the reader. The server discovers
  the update only during its next sweep, and its idle deadline is capped at 250
  ms in `src/server/headless.rs:855`. Local PTY output, by contrast, calls
  `render_notify.notify_one()`.
- **Impact:** Idle remote-output-to-frame latency is 0–250 ms, averaging near
  125 ms before render scheduling.
- **Trigger:** A frame arrives just after the loop parks with no local activity.
- **Confidence:** Confirmed.
- **Fix:** Give remote shared state a render notifier and notify only on the
  dirty transition from false to true, preserving coalescing.
- **Validation/workload:** Park the loop, send one remote frame, and timestamp
  receipt and client frame emission.
- **Measurement:** Remote receive-to-render p50/p95/p99.
- **Expected result:** Visible updates wake within approximately one frame
  budget, normally below 30 ms.

### P2 — Idle subscriptions perform expensive snapshots and process/filesystem work

- **Finding:** Output matching performs a full `pane_read` every 100 ms; scroll
  and fallback agent subscriptions perform `pane_get` every 100 ms even when
  nothing changed.
- **Evidence:** `src/api/subscriptions.rs:344`,
  `src/api/subscriptions.rs:454`, and `src/api/subscriptions.rs:510`. A full
  `PaneInfo` obtains scroll metrics and Unix CWD/foreground CWD information in
  `src/app/creation.rs:463`.
- **Impact:** Terminal lock acquisitions, snapshot formatting, regex scans,
  allocation, and `/proc` inspection scale as
  `10 × subscriptions × seconds` while idle.
- **Trigger:** Long-lived output, agent-status, or scroll subscriptions; peer
  control contributes additional subscription and enumeration load.
- **Confidence:** Confirmed path; impact magnitude not yet benchmarked.
- **Fix:** Gate output reads on terminal content revision; use agent events
  except on explicit sequence-gap recovery; introduce a narrow scroll
  revision/metrics probe instead of constructing full `PaneInfo`.
- **Validation/workload:** Hold 1, 32, and 256 idle subscriptions for 30 seconds,
  then exercise one relevant change per pane.
- **Measurement:** Snapshot calls, `/proc` calls, terminal-lock time, CPU, event
  latency.
- **Expected result:** Zero recurring pane snapshots after initial state while
  idle, with unchanged detection latency after real changes.

### P2 — Optional pane-history capture blocks the server loop across all scrollback

- **Finding:** Although file serialization is backgrounded, history capture
  itself happens before spawning the save thread and materializes every pane’s
  full ANSI history.
- **Evidence:** Capture occurs in `src/app/session.rs:39`, walks all panes in
  `src/persist/snapshot.rs:464`, and calls
  `recent_unwrapped_ansi(usize::MAX)` in `src/pane.rs:2765`. The feature is
  opt-in, but the default scrollback budget is 10 MB per pane.
- **Impact:** Main-loop latency, terminal-core lock contention, and large
  transient allocations—potentially hundreds of megabytes across many full
  panes.
- **Trigger:** `experimental.pane_history` enabled plus large scrollback when a
  debounced save fires.
- **Confidence:** High.
- **Fix:** Capture the cheap structural snapshot on-loop, clone stable runtime
  handles/IDs, and capture history on the save worker; alternatively budget
  history capture across loop iterations. Preserve positional
  structural/history consistency.
- **Validation/workload:** 1, 15, and 50 panes at 0%, 50%, and 100% scrollback
  budgets while continuously typing/rendering.
- **Measurement:** Maximum loop stall, history bytes, lock wait/hold, transient
  RSS, total save duration.
- **Expected result:** On-loop work becomes proportional to session structure
  and should stay near a frame budget; total background save CPU may remain
  similar.

## 5. Efficiency opportunities

- Replace `EventHub`’s front-drained `Vec` with `VecDeque`; current overflow
  moves retained elements under the hub mutex. Add `next_matching_after` or
  bounded-batch APIs so every subscription does not clone the entire tail.
- Store remote frames as `Arc<FrameData>` and clone the `Arc` under the mutex
  before cell blitting. The current render holds the frame mutex across every
  cell copy in `src/terminal/remote.rs:967`.
- In the proposed remote writer, keep ordered input but make resize a
  replaceable slot. Rapid resize should not queue obsolete dimensions behind
  input.
- Add narrow revision and scroll accessors instead of using `PaneInfo` as a
  universal probe.
- Bound queues by bytes as well as message count; a one-message limit does not
  constrain a multi-megabyte frame.
- Replace unchecked `std::thread::spawn` in reconnect/API churn paths with named
  `Builder::spawn` plus explicit failure handling, or a small bounded blocking
  executor.

## 6. Hypotheses worth testing

1. **Full semantic remote frames may dominate federation bandwidth.** Measure
   serialized bytes and CPU for 1/15/50 remote panes under sparse and
   full-screen updates. Compare against the existing terminal-ANSI encoder
   before considering protocol deltas.

2. **Remote frame mutex hold time may block reader-side frame replacement.**
   Instrument wait/hold time with 300×100 panes and multiple render geometries.
   Test the `Arc<FrameData>` lock-narrowing change only if contention is visible.

3. **Detection process probing may become material above roughly 50 panes.**
   Profile 1/15/50/100 panes, distinguishing unidentified, identified-idle, and
   working states. Record process-tree and manifest costs separately.

4. **The 1.5-second peer pane refresh may duplicate expensive `PaneInfo` work.**
   Count remote `pane.list` calls, returned bytes, CWD probes, and terminal locks
   with 1/5 peers and 1/50 panes per peer.

5. **Local TUI input can be silently dropped when the 1,024-entry PTY queue
   fills.** The key path reports failure internally, but text commits discard
   it. Stall a child’s PTY reads, send more than 1,024 events, and determine
   whether a bounded retry/coalescing buffer or visible overload indication is
   appropriate.

6. **Multiple distinct client geometries may scale worse than the current
   benchmark.** Extend the render benchmark to 1/5/15 clients with identical
   versus distinct sizes and both semantic and ANSI encodings.

## 7. Recommended performance harness

The minimum useful harness is an opt-in extension of `render_prof`, plus
deterministic stress drivers already compatible with the tmux/fake-peer
infrastructure.

| Workload | Cardinalities | Primary metrics |
|---|---|---|
| Local idle/high output | 1, 15, 50 panes; empty and 10 MB scrollback | PTY parse CPU, terminal-lock time, frames/sec, loop stall, RSS |
| Input/resize | Rapid typing, paste, and resize into reading and stalled PTYs | Enqueue latency, queue bytes/full events, input-to-write latency |
| Client fanout | 1, 5, 15 clients; same/distinct geometry; one slow reader | Render/encode CPU, bytes/client, control/render queue depth |
| Peer views | 1, 15, 50 panes; latency/bandwidth injection | Peer bytes, receive-to-render latency, writer backlog, reconnect time |
| Reconnect churn | Disconnect/reconnect/replacement storms | Attempt generations, stale completions, thread count, recovery latency |
| API | 1, 32, 256 concurrent requests/subscriptions | Accept threads, queue lag, handler time, overloads, event gaps |
| Persistence | 1, 15, 50 full-history panes | Capture/encode/write phases, max loop stall, transient RSS |

Use `perf`/flamegraphs and `heaptrack` externally before adding
allocator-specific dependencies. The current render benchmark should remain the
regression gate for fixed-geometry layout/render scaling.

## 8. Top next actions

> **All ten are done.** The plan executed them in dependency order rather than
> this one — three of the fixes ranked above instrumentation stated numeric
> acceptance criteria that could not be measured until it existed, which is
> recorded in the plan's "Why this order". For what is actually next, see the
> plan's "Still open"; nothing on that list is from this section.

1. **Fix:** Move remote view writes behind a bounded writer actor; this removes
   the only demonstrated multi-second stall from the main loop.
2. **Fix:** Add reconnect generation and expected-instance validation; include
   the stale-success replacement test.
3. **Fix:** Bound client control bytes and API admission, add socket write
   deadlines, and batch API draining.
4. **Fix:** Add explicit EventHub gap detection and bounded batch delivery
   before relying further on subscriptions for federation.
5. **Instrument:** Extend the opt-in profiler with main-loop stalls, queue bytes,
   remote receive-to-render latency, and API sequence lag.
6. **Fix:** Retain instance-scoped pending peer-pane cleanup IDs and retry them
   after same-instance reconnection.
7. **Fix and benchmark:** Replace one-byte API request reads with the existing
   counted chunk helper.
8. **Benchmark:** Quantify idle subscription snapshot/process cost, then add
   content/scroll revision gates and narrow probes.
9. **Benchmark:** Measure optional history capture across 1/15/50
   full-scrollback panes; move capture off-loop if the maximum stall exceeds one
   frame budget.
10. **Stress test:** Exercise slow clients, stalled PTYs, peer replacement, API
    floods, and reconnect churn while tracking RSS, threads, queue bounds, and
    loop latency.
