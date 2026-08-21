# Herdr engineering plan

Execution record for the federation correctness and performance audit.
Started 2026-08-16.

## Status

Every finding in the initial audit has been addressed, and phase 6's stress
harness has been built and run. It found one new confirmed defect of its own,
recorded below as B6: typed input into a pane whose child has stopped reading
froze the server for 42 seconds. Fixed in three passes — the client no longer
sends a message per character, the server no longer writes one pty item per key,
and phase 7 cut what each key costs to route — which takes that burst to 3.2
seconds in a debug build and 0.2 seconds in a release one.

Phase 8 then found that every number in phases 1 through 7 came from a debug
build, because the harness has no release path. Rerunning in release closed B6
outright and reordered what is left; the phase 8 section has both.

Phase 9 is a separate line: correctness at the peer boundary rather than cost.
It came from a user report, not from the initial audit, and found four bugs where a
remote pane answered the local default to a question only the peer could answer.

**The plan is complete as of phase 10.** Both formerly-live lists now have an
evidence-backed disposition. Peer focus, copy mode, search and agent progress
have parity; the API polling floor and overload-close race are fixed; the
50-pane `full_render.render_virtual` path is below 16.7 ms after removing a
pane-scaled terminal lock query; and the remaining performance hypotheses were
measured and closed or explicitly accepted under the completion contract.

| Finding | State |
|---|---|
| B1 stale reconnect resurrects a view | Fixed. Reproduced first; upgraded from High confidence to Confirmed. |
| B2 silent event loss | Fixed. |
| B3 leaked remote PTYs | Fixed. |
| B4 unbounded writer queues | Fixed, all three parts. The Windows part is compiled, not run. |
| B5 API thread/memory exhaustion | Fixed, minus a backstop deliberately skipped. |
| P1 five-second remote-write freeze | Fixed. |
| P2 one-byte API reads | Fixed. |
| P2 frames do not wake the loop | Fixed. |
| P2 idle subscription cost | Measured, then fixed. ~5× on the loop. |
| P2 history capture stall | Measured, then fixed. ~500× on the loop. |
| B6 typed input freezes the server | Found by the phase 6 harness, not the initial audit. Fixed in three passes; 42.5 s to 3.2 s debug, 0.2 s release. |
| P3 100 ms API concurrency floor | Fixed. Release p99 at concurrency 32: 102.4 ms to 3.3 ms. |
| P4 50-pane render scaling | Fixed at the responsible layer. `render_virtual`: 92.5 ms to 14.6 ms in the focused run. |
| P5 terminal-core contention | Measured and disproven as the scaling mechanism; average wait 3–4 µs at 50 panes. |
| P6 detection cost | Measured separately; not material enough to optimize. |
| P7 frame-lock contention | Measured and disproven; 15-pane store p99 wait 4 µs, render wait effectively zero. |
| P8 scrollback allocation | Already lazy; populated ~10 MB-per-pane budget accepted. |
| P9 peer-boundary parity | Fixed and tested for focus, copy mode, search and agent progress. |

The two former live lists remain as historical indexes, with their closure
tables in place:

- [Phase 8 closure](#still-open) — performance, phases 0–8.
- [Phase 9 closure](#still-open-at-the-peer-boundary) — correctness of what a
  remote pane can answer.

Phase 10 below records the final measurements and recommendations.

### Completion contract (approved 2026-08-19)

"Finish the engineering plan" means every item in both live lists gets an
evidence-backed disposition. A confirmed user-visible defect is fixed and
tested. A hypothesis is measured once and either promoted to a finding or
closed. Expected behaviour and unavailable-platform checks may close as
explicitly accepted gaps; completion does not require manufacturing a patch for
something the evidence does not call a defect.

The approved product and engineering decisions are:

- Peer-backed panes get parity with local panes for user-visible focus, copy
  mode, search and agent progress. Off-screen text, soft wraps and word motion
  are answered by asynchronous peer-authoritative queries, not reconstructed
  from streamed frame cells. Stale replies are rejected by query generation.
- Pane focus is terminal-scoped. It must not masquerade as the peer TUI's outer
  focus or change notification suppression on that machine.
- An incompatible peer-protocol change is allowed when correctness needs it.
  Variants remain append-only, the protocol version moves only when the current
  value has been published, and all federated machines are upgraded together.
- Performance work uses release binaries and fixed 1/15/50-pane workloads. The
  unexplained API polling floor is removed. Fifty-pane rendering targets a
  16.7 ms frame where a localized evidence-backed change can reach it; terminal
  parsing and detection are instrumented before they are optimized.
- Scrollback allocation is measured before changing it. Make it lazy only if
  eager ownership is confirmed and the fix remains localized; otherwise record
  the configured ~10 MB per-pane budget as accepted behaviour.
- Frame-lock contention and ANSI-versus-semantic transport each get one focused
  measurement. If neither is material, close them as disproven hypotheses.
- Windows PTY pressure remains compile-verified by `just windows-lint`; real
  runtime pressure testing is an accepted environmental gap for this cycle.
- B5's skipped bounded request-channel backstop remains skipped while an
  architecture test proves the connection cap keeps it unreachable.

The execution baseline was resynced before new work: the eight-commit fork queue
was squashed and rebased onto upstream `7d35ebe7`; protocol 22 remains free
against upstream's published protocol 20. Post-rebase `just windows-lint` and
`just check` passed (3,808 Rust tests and 138 maintenance tests).

Accepted gaps, all deliberate:

- The Windows PTY queue is verified by `just windows-lint` only. Its
  drop-under-pressure behaviour is reasoned, never observed.
- B5's bounded request channel was skipped, with reasoning in the phase 2d
  section.
- Remote receive-to-render latency is represented by separate frame-store and
  render-lock timings rather than one end-to-end timestamp. Two earlier gaps
  remain partial — subscription cost is covered only by `api.pane_info` rather than
  per-subscription counters, and persistence reports capture and resolve but not
  serialization and file write separately. Resource cardinality is sampled by the
  stress harness from outside the process rather than by the profiler.
- Windows PTY pressure remains compile-only for this cycle; no Windows hardware
  was available for runtime pressure testing.

## Measuring anything here: use the lab

`peer-test/scripts/lab.py` gives isolated instances under `/tmp/hl-<name>` with
their own `XDG_CONFIG_HOME`, plus `lab cli`, `lab api`, `--env`, a real
tmux-hosted TUI client through `lab ui open`, and `lab destroy`.

```sh
lab() { uv run peer-test/scripts/lab.py --lab p1 "$@"; }
lab up --instances a --no-build --env HERDR_RENDER_PROF=1
lab ui open a --client A && lab ui onboard A
lab ui text A 'seq 1 200000'; lab ui keys A Enter
lab destroy
```

This is not a style preference. Most of this work was done without it, by
hand-rolling `HERDR_SESSION` servers in the shared `herdr-dev` namespace with a
Python socket script for API calls and manual teardown — which also meant
writing a `config.toml` into the real dev config directory and having to
remember to delete it.

It produced one wrong answer that mattered. The history-capture measurement was
taken with no client attached, reported 116 µs, and concluded the finding was
not reproducible. Rerun through the lab with a real client it is 5–10 ms: off by
fifty times, in the direction of dismissing a real defect.

**A measurement of a terminal, taken with no client attached, is not a
measurement of that terminal.**

## Why this order

This is dependency order, not the initial audit's own "Top next actions"
order. That list puts instrumentation at position 5, which is wrong for
execution: three of the fixes ranked above it state numeric acceptance criteria
that cannot be measured until the instrumentation exists.

1. **Shared primitives before consumers.** P1, B4 and B5 all need the same
   admission accounting. B1 and B3 both need peer-instance identity carried into
   asynchronous completions.
2. **Measurement before claims.** Fixes whose validation is numeric need the
   counters first. Findings whose magnitude is unmeasured need a benchmark
   before a fix is chosen.
3. **Conflict surface.** This fork rebases onto `upstream/master`, and most of
   these fixes land on the files that already conflict. Work that touches one
   file is batched into one diff.

| # | Step | Status |
|---|---|---|
| **0** | Baseline, reference verification, protocol and schema check | Done |
| **1** | Instrumentation floor: loop stall histogram, queue depth gauges | Done |
| **2a** | Admission accounting primitive (`src/queue_budget.rs`) | Done |
| **2b** | B4: client control queue bound with disconnect on overflow | Done |
| **2c** | P1 remote writer actor, plus the P2 remote-frame wake | Done |
| **2c′** | B4 remainder: client write deadline, Windows PTY staging queue | Done |
| **2d** | B5: connection cap, drain batch, and the P2 chunked request read | Done |
| **3a** | B1: validate a reconnect result before installing it | Done |
| **3b** | B3: retained peer-pane cleanups, retried on same-instance reconnect | Done |
| **4** | B2: explicit `EventHub` gap and batched delivery | Done |
| **5** | Measure, then decide: idle subscription cost, history capture stall | Done |
| **6** | Stress harness, then the section 6 hypotheses | Done, plus the B6 fix it turned up |
| **7** | B6 remainder: measure the per-key routing split, then cut it | Done |
| **8** | Rerun the workloads against a release build | Done; it reordered the rest |

## Phase 0 result

Baseline at `7be24fa0`. `just check` green, 138 script tests, under a 1-minute
load average of 0.11. The load figure matters: `AGENTS.md` records that
contention makes the heavy suites look like code regressions, so a green run on
an idle machine is the trustworthy kind.

**References.** The initial audit contained 44 unique `file:line` references. 40
land exactly on the construct described. Four were corrected: `src/ipc.rs:166`
to `:185`, `src/api/mod.rs:82` to `:84`, `:90` to `:91`, and `src/pane.rs:288` to
`:289`. The underlying claims were all correct; only line numbers drifted.

Constants spot-checked and confirmed: `MAX_EVENTS = 512`,
`CLIENT_ACCEPT_POLL_INTERVAL = 250ms`, the 1,024-slot PTY channel, and the
detection cadence of 500/300/50 ms.

**Prior fork work does not overlap.** The queue already carried
`d803debb fix(peers): bound peer writes`, close enough to P1 to rule out. It is
not the same fix: `RemoteTerminalRuntime::send` still blocked on the calling
thread, and what that commit added was the five-second `STREAM_WRITE_TIMEOUT` —
the bound P1 objects to rather than a fix for it.

**Protocol and schema.** The fork is on `PROTOCOL_VERSION = 21`. Upstream was on
19 when this started and has since claimed 20, so 21 is still free but the margin
is now one version. Recheck after every rebase — this is the value most likely to
collide next. Only
phase 4 ended up needing the schema artifact regenerated:

```bash
ZIG=/opt/zig0.15/zig HERDR_UPDATE_API_SCHEMA=1 cargo nextest run --locked generated_protocol_schema_artifact_is_current
```

## Phase 1 result

`src/render_prof.rs` gained two measurement kinds. `duration` reports count,
average and maximum, which cannot answer any of the acceptance criteria in the
initial audit — those are percentiles. `histogram` adds p50/p95/p99 over fixed
buckets spanning 50 µs to 5 s, recording without allocating. `gauge` samples a
level and retains its peak, because the useful facts about a queue are how deep
it is now and how deep it ever got, and a counter gives neither.

The main loop records `loop.active`, `loop.park`, `loop.wake.*` and the API and
internal-event queue depths. Active time is measured *between parks* rather than
per iteration: several paths `continue` without reaching the park, so a stall is
a stretch that never parks, and per-iteration timing would split exactly the
stretches worth seeing. Depth is sampled at the park, where what remains is real
backlog rather than work about to be handled.

Idle versus an 80-request API burst:

| Metric | Idle | Burst |
|---|---:|---:|
| `loop.active` p50 | 250 µs | 5,000 µs |
| `loop.active` p99 | 500 µs | 36,533 µs |
| `loop.park` p50 | 251,833 µs | 100,000 µs |
| `queue.api.depth` max | 0 | 2 |
| wake reasons | timer 4 | api 6, timer 3 |

The burst's p99 is roughly six times its own average of 5,975 µs — the
distribution shape that motivated the histogram, since `duration` would have
reported the average and hidden the tail. The idle `loop.park` of 251 ms
independently confirms the 250 ms `CLIENT_ACCEPT_POLL_INTERVAL` that the P2
remote-frame finding depends on.

## Phase 2a and 2b result

**2a changed shape once the consumers were read.** The plan called for a bounded
queue. The four consumers share no transport — a `Mutex` and `Condvar` over
three lanes, a `std::sync::mpsc` behind a Tokio channel, a Tokio channel, and a
remote writer that did not exist yet. A generic bounded *channel* would have
fitted none of them. What they share is the admission decision, so
`src/queue_budget.rs` owns accounting only and callers keep their storage.

One policy call worth knowing: an item larger than the entire byte limit is
admitted when the queue is empty. Refusing it would be a permanent stall rather
than backpressure — nothing can drain to make room. `force_admit` exists for the
same reason at the other end: a message that must be delivered still has to be
*accounted*, because accounting that skips an enqueued item makes every later
release wrong.

**2b and 2c were swapped.** The primitive is dead code until something consumes
it, and `just lint` runs clippy with `-D warnings` (verified: it exits 1). The
client transport is much the smaller consumer, so it went first and exercised
the primitive before the remote writer depended on it — which is how
`force_admit` was found to be missing.

**B4 overstates its own finding slightly.** Only the `control` lane was
unbounded; `ordered` and `render` were already capped at one item, and their
real gap is bytes, which is section 5's point rather than B4's.

Sizing note for anyone changing `CLIENT_CONTROL_QUEUE_LIMITS`: a single
legitimate control message can carry `MAX_CLIPBOARD_IMAGE_PAYLOAD`, 16 MiB. The
byte limit must clear that with room to spare, or a normal clipboard write
arriving behind a queued bell would disconnect a healthy client.

## Phase 2c result

Both P1 and the P2 remote-frame notification.

Each remote view now owns a bounded writer queue drained by its own thread.
`send` serializes and enqueues; the socket write no longer happens on the
calling thread, which is the server loop. Framing stays on the caller so a
message that cannot be encoded is still reported to whoever produced it.
`send_bytes_after` enqueues too, so it no longer parks a Tokio worker either.

Resize is a superseded generation rather than a separate lane. The item keeps
its place and is dropped where it would have been written, so a drag collapses
to its final size without resize and input being reordered against each other —
section 5 asks for the coalescing, and this gets it without the ordering cost a
separate slot would have.

**A regression introduced and then fixed.** `stop` used to write `Detach` inline
before shutting the socket down. Merely queueing it let the shutdown win that
race, so the peer would hold its control lock until the socket broke.
`wait_until_drained` gives the detach the same 250 ms budget the inline write
had.

**A test whose premise disappeared.** `dropping_a_runtime_does_not_wait_on_a_busy_writer`
held the writer mutex, and there is no writer mutex any more. Rewritten to park
the writer thread for real by filling the socket buffer against a peer that
accepted the connection and then stopped reading.

No new harness was needed: `MutePeer` already completes a handshake and then
never reads, which is exactly the driver the initial audit asks for. That is the
phase 6 item that was pulled forward, and it turned out to already exist.

Validation: 600 sends to a wedged peer complete in under a second against a
five-second write timeout, and the queue stays inside both bounds.

### The P2 notification, and the global it needed

A frame now wakes the loop instead of waiting to be swept up, and only on the
idle-to-dirty transition, which keeps the coalescing sweeping gave for free.
Phase 1 had already measured what this removes: an idle loop parks 251 ms.

The handle is published in a `OnceLock` at server start rather than passed in,
which is the least tidy thing in this phase. A pane is handed its wake handle at
construction; a peer view cannot be — it is opened on a worker thread several
frames down a call chain carrying no application state, and the runtime only
reaches the loop afterwards. The alternatives were widening every function on
that chain, or setting the handle at each of the four install sites, where a
missed one costs that view a render deadline and nothing louder. `RemoteShared`
still takes the handle as a parameter, with the global supplying only the
default, so the transition semantics are tested directly rather than through
global state. `crate::instance_id::active()` is reached the same way from the
same function.

## Phase 2c′ result

**A claim in this plan was wrong.** An earlier revision said
`interprocess::local_socket::Stream` does not expose a send timeout, and that is
why this work was deferred out of 2b. It does, and
`crate::ipc::set_local_stream_send_timeout` already wraps it, handling the
Windows `Unsupported` case. The API server, the remote terminal and the session
socket all use it; the client writer did not.

The deadline matters for a reason distinct from the queue bound. The byte cap
disconnects the *client*; it does nothing for the writer thread parked inside
`write_all` on a socket whose buffer never drains, so that thread and its socket
were never reclaimed. Five seconds, matching the other write bounds — a liveness
backstop, not a latency target, since a client merely slow to read must not be
torn down.

**The Windows PTY staging queue.** Input arrived through a 1,024-slot channel
and was forwarded straight into an unbounded one, so the bound above it achieved
nothing. That stage is now `sync_channel` at the same size.

Its four producers need three policies, and one policy for all of them would
deadlock:

| producer | policy | why |
|---|---|---|
| input thread | blocking `send` | Where backpressure belongs: it fills the bounded channel above, which reports `Full` to callers, exactly as the unix path does. |
| reader thread | `try_send`, drop on full | This thread drains the pty. Blocking it stops reading and leaves the child blocked writing its own output. |
| `write_terminal_response` | `try_send`, drop on full | Reached through the handle from whatever thread processes terminal state, so blocking risks parking a caller — the same class of stall as P1. |
| control thread | `try_send`, drop on full | Also services shutdown, which must not wait on a writer that is not draining. |

Bounded by items only. Byte accounting would be better and was not added to code
this machine can compile but not run.

**Verified by compilation, not execution.** `just windows-lint` covers the
Windows half; nothing here ran it.

## Phase 2d result

**B5's three parts are not equally load-bearing, and one is not needed.** The
finding treats the unbounded request channel as an independent defect. It is
not: a connection thread blocks on its own response before sending again, so
in-flight requests cannot exceed live connection threads. The channel was
unbounded only because *connections* were, and capping those bounds it too.

So this caps concurrent connections at 128, refusing further ones with an
explicit `server_overloaded` response rather than dropping them silently, and
bounds the API drain to the same 64-request budget internal events already use.
The connection count is held by a guard, so a thread that panics still returns
its slot. The listener's unchecked `thread::spawn` became a named
`Builder::spawn` whose failure is logged rather than propagated — it could
previously take the listener down and lose the API for the rest of the session.

The bounded channel itself was **not** done: 105 mechanical edits across 27
files, almost all test setup, for a backstop the connection cap makes
unreachable. Worth doing if a code path ever starts dispatching requests without
waiting for their responses.

**The one-byte read** is now 8 KiB chunks. The subtlety was the size limit: the
old reader checked for the newline before checking length, so a line whose
content exactly fills the limit is accepted while one byte more is not. A
chunked read sees both in one buffer and has to put that boundary back
deliberately. Both directions are tested.

Reading past the newline is safe here, and worth stating because it would not be
in general: this is the only read the API server performs on a connection, so
bytes after the first line would have sat unread regardless.

**A gap this uncovered.** The tests for request-line parsing live in a
`#[cfg(all(test, windows))]` module, so none run on Linux. The reader was
rewritten with no local coverage, and the first version of these tests went into
that same Windows-only module and silently did not run. They now live in the
unix module. Anything touching `read_initial_request_line_with_limits` should
check which module its tests landed in.

## Phase 3 result

### B1 — the stale reconnect

**Upgraded to Confirmed.** The characterization test was written first, as the
refactor-risk rule in `AGENTS.md` requires for identity work, and the
adversarial case failed exactly as predicted: a view abandoned because its peer
was replaced was resurrected by a reconnect that finished against the old
server, coming back live and accepting input.

**The prescribed fix was larger than the defect needs.** The initial audit asks for a
monotonically increasing reconnect generation. `reconnect_due` already refuses
to start an attempt while `in_flight` is set, so attempts are serialized per
view and there is no second generation to tell apart. What was missing is not
the identity of the attempt but the validity of its answer, so the handler now
checks that the view was not retired mid-flight and that the runtime is not on a
different server from the one the registry currently reports.

The instance check is skipped when the registry has no instance for the peer.
That is not a hole: removing a peer removes its views, so the missing-entry path
has already shut the runtime down.

**A stall the obvious fix would have introduced.** Discarding a result leaves
`in_flight` set. Harmless for a retired view, which is never retried; for a live
view whose peer was replaced it would mean never reconnecting again. That path
reports a failed attempt instead, so it backs off and retries.

### B3 — the leaked peer panes

**The finding is slightly off about where the id was lost.** It says the pane id
required for retry is discarded. The *event* already carried it; the handler
threw it away, keeping only a count.

Failed cleanups are now retained as records — pane id, issuing instance, reason
— and retried when that same peer reconnects. Reconnect is both the first moment
a retry can succeed and a natural rate limit, since the peer's own backoff
governs how often it happens; hence no separate timer or attempt counter.

`expected_instance` is threaded from the runtime through the cleanup call and
the failure event, because a peer-local pane id names the intended pane only
while the issuing server is answering. A record whose instance no longer matches
is kept and reported but never retried: sending it to a replacement could close
an unrelated pane. Records with no instance at all are reportable and never
retryable, which is a real limitation rather than an oversight.

Records are taken out before retrying and put back by the failure event, so a
retry that fails again is retained once rather than duplicated. That plus
de-duplication by pane id keeps one stuck pane from reading as a growing pile.

**A semantic change to a number already on screen.** `failed_pane_cleanups` was
a monotonic count of failures ever; it is now the number of unresolved records,
so a successful retry decrements it and it can reach zero. Same field name and
type, so no schema change.

## Phase 4 result

B2 has two halves that fail differently. The correctness half: the replay buffer
evicted unread events and said nothing, so a subscriber could not tell a gap
from a quiet period and built an incomplete history believing it complete. The
throughput half: a subscription emitted at most one event per poll while its
connection slept between polls, a ceiling no buffer size can lift.

`EventHub` now answers with an explicit `Gap`, which subscriptions turn into a
`subscription.overflow` event carrying the oldest retained and resumed-from
sequences. Delivery is batched to 64 per poll.

The section 5 items are folded in. The buffer is a `VecDeque`, because eviction
happens at the front on every push once full and draining the front of a `Vec`
moves everything retained. `matching_after` scans under the lock and clones only
what it returns, replacing a path that cloned the entire tail so the caller
could take the first match and drop the rest — and it advances the cursor past
non-matching events, so a subscriber filtering a rare kind stops rescanning the
buffer every poll.

The boundary worth knowing: a subscriber whose next wanted event is exactly the
oldest retained has missed nothing, and reporting that as a gap would fire
constantly on a busy server. Tested directly.

This added API surface, so `docs/next/api/herdr-api.schema.json` was
regenerated.

## Phase 5 result: idle subscription cost

One pane, one idle `pane.scroll_changed` subscription. Debug build, so treat
absolutes as an upper bound and the proportions as the finding.

| | idle, no subscription | one idle subscription |
|---|---:|---:|
| `api.pane_info` calls/sec | 0 | 10 |
| average per call | — | ~690 µs |
| loop wakes/sec | 4 (timer only) | 10 (api) |

Ten calls per second is exactly the 100 ms poll, none of it triggered by
anything changing: roughly 6.9 ms/sec of `loop.active` time per idle
subscription, the same budget rendering and input compete for.

**The initial audit attributes the cost to the wrong things.** It lists "terminal
lock acquisitions, snapshot formatting, regex scans, allocation, and `/proc`
inspection". Broken down:

| component | average | share |
|---|---:|---:|
| `foreground_cwd` | ~580 µs | 83% |
| `cwd` | ~40 µs | 6% |
| all the rest of `PaneInfo` | ~90 µs | 11% |

It is one thing: the foreground process-tree walk, ten times a second, for a
pane where nothing happened.

**This bears on more than subscriptions.** Every `PaneInfo` pays it —
`pane.list`, the agent-status fallback probe, and the 1.5-second peer pane
refresh, which is hypothesis 4. That hypothesis is partly answered: the
expensive part of that refresh is this walk.

### The fix, and the three rejected

A narrow scroll probe (what the initial audit prescribes) adds public API surface for
an internal optimization and fixes only one of three subscription kinds.
Skipping the `process_argv` read that this path discards needs four platform
implementations, three unrunnable here, for maybe a fifth of the cost. Skipping
the walk when the shell is its own foreground group would remove the cost
outright but rests on reasoning rather than evidence, and the same machinery
backs agent detection.

So the cwd is cached on the runtime, gated on the terminal's content sequence
with a two-second TTL behind it. The gate is sound rather than lucky: a `cd`, or
a job starting or ending, redraws a prompt or prints something, so the sequence
moves and the answer is recomputed. The TTL bounds the one case the gate cannot
see — a directory change that prints nothing.

Verified through the lab with a real client, because the whole premise is that
`content_seq` holds still and that had only been tested without one:

| scenario | cache hits | `api.pane_info` average |
|---|---:|---:|
| before the fix | — | ~690 µs |
| idle, real client | 9–10 of 10 | ~160–206 µs |
| pane printing 5×/sec | 5 of 10 | ~290–330 µs |

It degrades gracefully rather than falling off a cliff: at half the poll rate
the hit rate is half, and every miss is a pane that genuinely changed, where
recomputing is correct.

The remaining ~140 µs is the rest of `PaneInfo` and was not pursued.

## Phase 5 result: history capture

Measured through the lab with `experimental.pane_history` enabled and a real
client attached.

| panes with content | on-loop `capture_history` |
|---|---:|
| 1 | 5,417 µs, then 9,829 µs |
| 1 of 6 (five empty) | 6,867 µs |

Five to ten milliseconds on the loop, before the save thread is spawned, against
a 16.7 ms frame — for a single pane holding roughly 117 KB. Upgraded from High
confidence to **Confirmed**.

Cost tracks retained content rather than pane count: five empty panes added
almost nothing, matching "panes × retained scrollback". Scaling across panes that
all hold content was not measured here; the phase 6 persist workload measures it.

### The fix

Capture is split. On the loop, beside the structural snapshot, the save decides
*which* panes to record and *where* each sits, taking a handle per pane — a
reference-count bump on the terminal. The save thread reads the bytes through
those handles.

That split preserves the property the initial audit warns about and an existing test
already asserts: structural and history snapshots pair positionally, so the
positions must come from one consistent moment on the loop. Only content is
deferred, and content read a moment later is not an inconsistency.

| | before | after |
|---|---:|---:|
| on-loop `capture_history` | 5,417 / 9,829 µs | 14 / 16 µs |
| save-thread `resolve_history` | — | 114 / 10,363 µs |

On-loop cost is now independent of retained content, while the real work happens
on the thread that should do it — loop work proportional to session structure,
total background CPU about the same.

`TerminalRuntime::snapshot_history` had no callers left outside tests and is
gone, so no eager variant remains for a future caller to reach for on the loop.
The pane methods behind it stay for the unix-only handoff path, with
`#[cfg_attr(windows, allow(dead_code))]` per the precedent in `AGENTS.md`.

## Phase 6 result: the stress harness

`peer-test/scripts/stress.py` drives the seven workloads in section 7 at their
stated cardinalities; `peer-test/scripts/_stress.py` holds the measurement
primitives. Lifecycle goes through `lab.py` as a subprocess, the way the pytest
suite already drives it; everything inside a measured phase talks to the server
directly, because a `uv run` per request measures uv.

Three design points are load-bearing, each because getting them wrong produced a
wrong answer first:

- **A fresh server per cardinality.** Gauge peaks are retained for the life of a
  process deliberately, so 50 panes measured after 15 in the same server reports
  whichever was worse and calls it 50.
- **Percentiles do not average.** The profiler logs a summary per window, not its
  buckets, so aggregating windows can only report the *worst* window's percentile.
  Named `p99_worst_us` so nobody reads it as a true p99.
- **A driver that silently does nothing reads as a clean result.** The first
  version of the input workload reported no queue pressure because its `tmux
  send-keys` calls were not arriving. Sends now raise on failure, and the workload
  probes that a keystroke reaches the pane before believing a quiet queue.

`just test-stress` runs the bounds — admission caps hold, resources come back, a
pasted burst leaves the server answering — as assertions rather than numbers, in a
`stress` marker disjoint from `e2e` and `boxes`. The numbers stay in
`peer-test/evidence/stress-<workload>-<stamp>/report.json`.

Two instruments were added for hypotheses that could not otherwise be answered:
`full_render.peer_bytes`/`client_bytes` with `full_render.serialize` (H1), and
`queue.pty_input.items` with `queue.pty_input.rejected` plus the pty read and
write syscall counters (H5). All are `render_prof` names, no new surface.

### What the workloads found

Debug build, one-minute load average under 1.4. Treat absolutes as upper bounds
and the ratios as the finding.

**Output — the PTY parse path dominates, and the tail is seconds.**

| panes | pty MB | loop.active avg | loop.active max | full render avg | RSS peak |
|---:|---:|---:|---:|---:|---:|
| 1 | 1.4 | 2.9 ms | 15 ms | 9.8 ms | 47 MB |
| 15 | 21.3 | 24.9 ms | 1,114 ms | 99.8 ms | 184 MB |
| 50 | 71.0 | 17.2 ms | 1,065 ms | 88.5 ms | 533 MB |

At 50 panes, `pty.ghostty_write` ran 22,803 times averaging 372 µs — about 8.5 s
of parse in a 10 s window. (Said "on the loop" when first written; it is not. The
probe at `src/pane/terminal.rs:1289` runs on the pty reader thread while it holds
the terminal core lock, which starves the loop indirectly. The distinction decides
what a fix looks like: shorten the hold, not move work off the loop.) Section 1's conclusion that fixed-geometry
rendering is not the bottleneck holds; the bottleneck is parsing plus full renders
that scale with pane count. Note also `api.pane_info` at 248 ms maximum under this
load, against 690 µs idle before the phase 5 fix: the process-tree walk degrades
badly under contention, and its cache cannot help a pane that is genuinely
changing.

**Fanout — cost is per client, and identical geometry does not help.**

| clients | geometry | full render avg | full render max | loop.active max |
|---:|---|---:|---:|---:|
| 1 | distinct | 13.3 ms | 19.9 ms | 69 ms |
| 5 | distinct | 45.9 ms | 65.0 ms | 283 ms |
| 15 | distinct | 105.8 ms | 156.2 ms | 430 ms |
| 15 | same | 118.7 ms | 160.5 ms | 360 ms |

One pane, no slow reader in either column. A control run with the slow reader
removed changed nothing, so the SIGSTOPped client is not the cause — client count
is. The client control queue never exceeded 3 items, which is correct: frames go to
the render lane, which replaces rather than queues.

**Peer — the far side pays, and a semantic frame is about 25 KB.**

| peer panes | near loop avg | far loop avg | far loop max | peer frame MB | frames sent |
|---:|---:|---:|---:|---:|---:|
| 1 | 2.1 ms | 1.6 ms | 15 ms | 0.15 | 6 |
| 15 | 1.8 ms | 23.0 ms | 718 ms | 1.15 | 47 |

Serving 15 peer views costs the far server what 15 local output panes cost it. The
near side is cheap. `full_render.skip_identical` fired 271 times against 47 sends,
so most prepared frames are already being discarded as unchanged.

**Persist — the phase 5 split holds at cardinality.** This closes the gap the plan
listed as open.

| panes | on-loop capture avg | max | save-thread resolve avg | max |
|---:|---:|---:|---:|---:|
| 1 | 10 µs | 15 µs | 6.6 ms | 10.0 ms |
| 15 | 40 µs | 42 µs | 50.6 ms | 51.7 ms |
| 50 | 109 µs | 115 µs | 136.7 ms | 144.1 ms |

Each pane holds 20,000 lines. On-loop cost is ~2 µs per pane and stays three orders
of magnitude under a frame at 50 panes; the content cost lands on the save thread.

**API — the cap holds, the refusal does not always arrive.**

| concurrency | sent | overloaded | failed | p50 | p99 | queue depth max | threads peak | threads after |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 3 | 0 | 0 | 0.3 ms | 4.5 ms | 0 | 13 | +0 |
| 32 | 96 | 0 | 0 | 102 ms | 108 ms | 0 | 41 | +0 |
| 256 | 768 | 93 | 178 | 104 ms | 125 ms | 63 | 141 | +0 |

Threads peak at 141 against a 128 cap plus 13 baseline, and return to baseline with
no descriptor leak. Two things are worth following up:

- **The refusal races the close.** 178 of 768 connections saw `Broken pipe` instead
  of the `server_overloaded` line. The listener writes the refusal and drops the
  stream; a client still writing its request gets EPIPE and never reads the answer.
  B5's "explicit response rather than dropping silently" only holds for clients that
  finished sending first.
- **A ~100 ms floor appears at concurrency ≥ 32** while a single request is 0.3 ms.
  Not investigated.

**Churn — no leak.** 5 and 20 rounds of stopping and restarting a peer with a view
open: every round recovered, p50 260 ms and max 362 ms (the 250 ms reconnect poll),
`failed_pane_cleanups` zero, threads and descriptors back to baseline, RSS +1.1 MB.
B1 and B3 hold under storm conditions.

### B6 — typed input into a stalled pane freezes the server

Found by the input workload. **New, confirmed, and the most serious thing in this
phase.** Everything in this subsection is the state *before* the fix below;
`stress.py run input --at 200 --opt bytes=4096` still drives it, but on current
code it reports 3.2 s rather than 42.

800 KB of client keystrokes sent to a pane whose child is not reading its PTY —
`sleep 600` — leaves the server unable to answer the API for **42 seconds**. The
same bytes through the other two paths cost nothing like it:

| submission path | input items | pty writes | bytes/write | pty reads | server unresponsive |
|---|---:|---:|---:|---:|---:|
| typed (`send-keys -l`) | 431,066 | 431,066 | 1 | 430,229 | 22–42 s |
| bracketed paste | 200 | 200 | 4,096 | 672 | 0.7 s |
| API `pane.send_text` | — | — | 124 | 13,121 | 0.0 s |

The mechanism, in the order it was established:

1. The main loop stops ticking entirely: 7 ticks in 43.4 s, where it parks for at
   most 250 ms. `pane list` times out; the process is alive and its main thread is
   in `futex_do_wait`.
2. The window covering the stall records `pty.bytes=819,200` against
   `pty.ghostty_write=818,401` — **one parse cycle per byte**, each 1 µs of parse
   inside about 52 µs of surrounding work.
3. The write side matches: 431,066 queued input items for 420 KB, one byte per pty
   write. So this is not the pty fragmenting a burst — the client submits input one
   byte at a time, and every byte becomes its own queue item, pty write, echo read,
   parse and render request.
4. The terminal core lock is taken and released hundreds of thousands of times in
   that stretch, which is what starves the loop: total lock *hold* time is only
   about 1.4 s, but the loop never wins the lock between reacquisitions.

The queue bound is not the protection here: `queue.pty_input.items` peaked at 43 of
1,024 and nothing was ever rejected. Backpressure never engages because each item is
one byte.

Realism, stated honestly: a human typing cannot produce 800 KB. What can is an
automated driver, a terminal that sends a large paste without bracketing, or any
client forwarding a stream of input. The cost is linear in bytes, so a fraction
of that burst is a proportional fraction of the freeze.

### B6 — the fix, and what it did not fix

**The fragmentation is in the client, one layer above where it looks.** The four
counts the workload reports separate the suspects, and only one of them moved:

| stage | before | measured how |
|---|---:|---|
| client stdin reads | 102 for 409,606 bytes | reads arrive as 4 KB chunks — the host is not dripping |
| client → server messages | 409,606 | **one message per byte** |
| server routing batches | 409,600 batches of 1 event | one batch per message |
| pty writes | 409,267 | one write per event |

`RawInputByteFramer` emits one chunk per parsed event, which for plain text is one
chunk per UTF-8 character, and `send_unix_input_chunks` sent each chunk as its own
`ClientLoopEvent`, which the client loop wrote as its own `ClientMessage::Input`.
So a 4 KB stdin read became 4,096 socket messages.

The fix joins adjacent *plain* chunks back together before they leave the reader
thread. Anything the loop already treats specially — palette replies, default
colour replies, pixel mouse reports — flushes what has accumulated first, so
ordering is unchanged, and the server re-parses the joined bytes with the same
framer into exactly the same events.

| 800 KB typed into a stalled pane | before | after |
|---|---:|---:|
| client → server messages | 819,200 | **201** |
| server routing batches | 819,200 | **201** |
| server unresponsive | 42.5 s | **8.3 s** |
| loop.active max | 42.8 s | 8.8 s |

The pasted burst is unchanged at 0.8 s, which is the control: it was already
arriving as whole chunks.

**Two attempts that did not work, both reverted.** Coalescing pty reads until
`WouldBlock` (the pty returns one byte per read in this state regardless), and a
vectored write across queued input items (the actor is woken per item and outruns
its producer, so a `writev` covered one entry). Both are recorded in comments at
the sites so they are not tried again.

**The server half, done next.** The client fix left the server turning each
character into its own pty queue item and write — 819,200 of them for that burst.
`App` now joins consecutive keys destined for the same pane and writes once at the
end of the routing batch.

The safety argument is the scope, not the flushing discipline: joining only
happens while `input_batch_active` is set, which is set at the top of
`route_client_events_from` and cleared before its final flush. Every other caller
— the API, a single forwarded key, a test — takes the immediate path exactly as
before, so holding input can never depend on someone remembering to flush.
Within the batch, anything that is not a key flushes first, because a paste, a
mouse report or a text commit sends its own bytes and must not overtake what was
typed before it.

| 800 KB typed into a stalled pane | original | client fix | plus server batching |
|---|---:|---:|---:|
| client → server messages | 819,200 | 201 | 201 |
| pty queue items | 819,200 | 819,200 | **201** |
| pty writes | 819,200 | 819,200 | **201**, 4,075 bytes each |
| pty reads (echo) | 463,343 | 463,343 | **4,515** |
| server unresponsive | 42.5 s | 8.3 s | **4.1 s** |

Paste is unchanged at 0.7 s throughout, which is the control.

That 4.1 s is not directly comparable with phase 7's numbers. Re-measuring this
same commit at a one-minute load average near 2.0 gives 5.0 s — the machine, not
the code. Phase 7 states a before and an after taken back to back at the same
load, which is the only comparison worth trusting here.

**A test hazard worth naming.** Three tests asserted the old contract of one pty
message per character. One failed; the other two *hung*, because they awaited a
message per character in a loop with no timeout and the messages they were
waiting for no longer exist. A hanging test does not fail — it pins a runner
thread, and `just check` printed one failure and then sat reporting "7 tests still
running" with no further output. `.config/nextest.toml` now sets
`slow-timeout = { period = "60s", terminate-after = 1 }`, so a stuck test fails
instead of waiting; the whole suite takes 33 seconds, so nothing legitimate is
near that bound. The three tests now drain with `try_recv` — routing forwards
synchronously, so everything sent is already queued when the call returns, and a
drain cannot hang.

**What remained.** Per-event routing: 819,200 events still passing through key
encoding, lease bookkeeping and the interception checks one at a time. Phase 7
took most of that without touching the event-per-character shape.

`client.stdin.reads`/`bytes` and `client.input.messages`/`bytes` are new
counters, and the client now flushes profiler windows at all — it linked the
profiler but had no flush call, so anything recorded in that process was
accumulated and never written.

## Phase 7 result: per-key input routing

The plan listed this as a design decision rather than an optimization, on the
premise that the only way to cut it was to stop producing an event per
character. That premise was wrong, and so was the number attached to it: the
recorded "about 21 µs each" is not consistent with its own 4.1 s total. Measured
directly, one key cost **5,027 ns**.

**Measure the split first.** Four temporary `duration` probes partitioned the
per-key path. That needed one profiler change: `avg_us` truncates to whole
microseconds, which reports `0` for anything on this path, so the duration line
now carries `total_us` as well. The split, over 819,200 keys:

| stage | ns/key | share |
|---|---:|---:|
| three direct-binding interception scans | 1,946 | 34% |
| residual — lease bookkeeping, dispatch, popup check, selection reset | 1,575 | 28% |
| focused-pane resolution | 1,381 | 24% |
| terminal-core access and key encoding | 766 | 14% |

**The prediction going in was wrong in a useful way.** The terminal core lock was
expected to dominate, because that is B6's own mechanism — the loop starved by
hundreds of thousands of reacquisitions. On the mean it is the *smallest* stage.
Removing one of its three lock round trips per key later saved 26 ns, which puts
an uncontended acquisition at about 20 ns and says the terminal stage is the key
encoder, not lock traffic. The lock still owns the tail: `input.key.terminal`
reached 1,978 µs against a sub-microsecond mean, and that tail is essentially all
of `input.key.total`'s.

### What was done

**The binding scans, 1,946 → 118 ns.** `Keybinds` carries a precomputed answer to
"could any direct binding fire on an unmodified printable key", and the three
scans are skipped when it is false and the key is one. Exact, not heuristic: a
non-control `Char` whose modifiers are within `SHIFT` can only match a combo of
the same shape, because every branch of `key_parts_match_combo` either compares
the modifier sets directly, requires the expected set to be empty, or requires it
to be exactly `SHIFT`. That is checked exhaustively over printable ASCII rather
than argued from samples.

Three things this turned up that are worth keeping:

- The combo-side predicate already existed as `is_unmodified_printable`, and
  `validate_binding` already **rejects** every direct binding that satisfies it.
  So the flag is false in practice today. It is reused rather than
  reimplemented, so if that validation rule is ever relaxed the fast path follows
  it automatically.
- The traversal destructures `Keybinds` without `..`. A keybind field added later
  is a compile error, not a silent hole in the fast path.
- `navigate` is excluded from it, and that is correctness rather than tuning:
  `navigate_pane_left` defaults to a bare `h`, so folding navigate-mode bindings
  in "as a harmless superset" reported true for the default config and the fast
  path never ran at all. Caught by writing the defaults test.

**Pane resolution, 1,381 → 1,020 ns.** Resolving the focused pane's terminal id
and then its runtime walked the workspace's tabs twice, because the runtime
lookup resolved the id again. One walk now returns both. The terminal id is
cloned where the prepared input is built rather than at resolution, so a key
intercepted before that point does not allocate.

**One redundant lock.** An eager `keyboard_protocol()` read existed only to fill a
`debug!` field on the Esc/ALT branch, while `encode_terminal_key` reads the
protocol itself. `debug!` evaluates fields lazily, so it moved inside the macro.

**The probes were then removed.** They cost about 270 ns per sample with the
profiler enabled — a quarter of what they were measuring — and `input.route.batch`
already gives per-key time divided by the batch's event count. Their finding is
recorded here instead.

### Result

Both runs on the same machine at a one-minute load average of 1.9–2.0, since
`AGENTS.md` records that contention alone changes these numbers.

| 800 KB typed into a stalled pane | before | after |
|---|---:|---:|
| per key | 5,027 ns | **2,704 ns** |
| routing batch average | 20,489 µs | 11,020 µs |
| routing batch maximum | 38,615 µs | 26,307 µs |
| server unresponsive | 5.0 s | **3.2 s** |

Not producing an event per character was not needed for any of it, and is no
longer worth its design cost: the remaining per-key work is the lease bookkeeping
and dispatch that a coalesced event would still have to do somewhere.

## Phase 8 result: everything above was measured in a debug build

`peer-test/scripts/_common.py::cargo_build` builds `cargo build`, full stop.
Every number in phases 1 through 7 came through it. That was never stated as a
risk, only as a per-section caveat to "treat absolutes as upper bounds", and it
turned out to change which findings are real.

One qualifier that shapes the results: `build.rs` builds libghostty-vt
`ReleaseFast` regardless of the Rust profile, so the terminal parser itself was
never the unoptimized part. What debug was hiding is the Rust around it.

Rerun with `stress.py --bin target/release/herdr`, load average 0.6–1.9.

**Input (B6) — effectively closed.**

| 800 KB typed into a stalled pane | debug | release |
|---|---:|---:|
| server unresponsive | 3.1 s | **0.2 s** |
| routing batch average | 11,020 µs | 1,158 µs |
| per key | 2,704 ns | 563 ns |

The remaining lease-bookkeeping cost phase 7 left behind is 0.2 s of stall in a
build anyone actually runs. It is no longer worth pursuing.

**Output — 3–7× faster, and every scaling problem survives intact.**

| panes | loop.active avg | loop.active max | full render avg | RSS peak |
|---:|---:|---:|---:|---:|
| 1 | 466 µs | 2.9 ms | 1.7 ms | 33 MB |
| 15 | 8,665 µs | 150 ms | 19.5 ms | 172 MB |
| 50 | 4,875 µs | 354 ms | 27.0 ms | 521 MB |

Against debug's 2.9/24.9/17.2 ms average and 9.8/99.8/88.5 ms full render. Two
things do not improve:

- **RSS is identical** — 47/184/533 MB debug against 33/172/521 MB release. Memory
  was never a build artifact. At 50 panes that is about 10 MB per pane, which is
  the configured scrollback budget being taken literally.
- **The shape is unchanged.** A full render still costs 16× more at 50 panes than
  at 1, and 27 ms average is past a 16.7 ms frame. `full_render.render_virtual` is
  ~80% of it (21.9 ms of 27.0 ms), so that is where to look.

**The parse cost per byte scales with pane count, and nothing explains it yet.**

| panes | bytes per call | avg per call | total in window | **ns per byte** |
|---:|---:|---:|---:|---:|
| 1 | 351 | 6 µs | 0.03 s | **18** |
| 15 | 918 | 85 µs | 1.99 s | **94** |
| 50 | 1,017 | 143 µs | 10.02 s | **141** |

Same 71 MB of output, the same ReleaseFast parser, 7.8× the cost per byte. Larger
reads at higher pane counts account for a factor of three at most, and the total
inside `pty.ghostty_write` barely moved between debug and release (8.5 s to
10.0 s) while everything around it got several times faster.

Stated carefully, because the obvious reading is the one this plan has already
got wrong once: `pty.ghostty_write` is recorded in `src/pane/terminal.rs:1289`
around a region that *already holds* the core lock, so it measures hold time, not
wait time. Whatever is scaling is inside the hold — a candidate is scrollback
trimming as each pane fills its 10 MB budget, another is cache behaviour at
521 MB resident. This is the strongest argument yet for the terminal-lock
contention instrument, which is the one measurement gap from the initial audit that
was never built.

**API — the 100 ms floor is not compute.**

| concurrency | p99 debug | p99 release | overloaded | failed |
|---:|---:|---:|---:|---:|
| 1 | 4.5 ms | 0.7 ms | 0 | 0 |
| 32 | 108 ms | **102.6 ms** | 0 | 0 |
| 256 | 125 ms | **124.9 ms** | 67 | 183 |

Single requests got six times faster; the floor did not move at all. A cost
identical in both builds is a sleep or a poll interval, not work — which makes
this cheaper to chase than it looked, and points at a poll rather than at
handler cost. Threads and descriptors still return to baseline, and the refusal
still races its own close (183 of 768).

### The hypotheses

| # | Hypothesis | Answer |
|---|---|---|
| 1 | Full semantic remote frames may dominate federation bandwidth | **Partly.** ~25 KB per frame, 1.15 MB for 15 peer panes over 8 s, serialized in ~1.3 ms each. The ANSI comparison the hypothesis asks for still needs a terminal-attach client; both peers and TUI clients negotiate semantic frames. |
| 2 | Remote frame mutex hold time may block reader-side replacement | **Not reached.** No contention instrument exists, and the peer workload found the far server's render loop, not the near side's frame lock, to be where the cost is. |
| 3 | Detection process probing may become material above ~50 panes | **Not separated, and phase 8 narrowed it.** In release at 50 panes `api.pane_info` averages 6.7 ms and peaks at 114 ms while `foreground_cwd` peaks at only 25 ms — so unlike the debug run, the walk is *not* most of the worst case, and what the rest is remains unmeasured. |
| 4 | The 1.5 s peer pane refresh may duplicate expensive `PaneInfo` work | **Yes**, as phase 5 already found: the walk is the cost, and it is now cached behind the content sequence. |
| 5 | Local TUI input can be silently dropped when the 1,024-entry PTY queue fills | **No — it is worse.** The queue never fills and nothing is dropped; the same scenario froze the server for 42 s instead. Now 3.2 s in debug and 0.2 s in release, which closes it. See B6, phase 7 and phase 8. |
| 6 | Multiple distinct client geometries may scale worse than the benchmark | **Yes, but not for the stated reason.** Cost scales with client count and identical geometry is no cheaper, so it is not the distinctness. |

<a id="still-open"></a>
### Phase 8 closure (formerly “Still open”)

Phase 10 closed this list. The original release-build findings are retained
below because they state the evidence that drove the final work; their
dispositions are:

| Item | Final disposition |
|---|---|
| Terminal-core hold time | Instrumented. At 50 panes average lock wait is 3–4 µs while hold is 353–415 µs. Scrollback-disabled hold falls to 117 µs, so contention is disproven and retained-history work is the cause. |
| `full_render.render_virtual` | Fixed. A pane-scaled alternate-screen query blocked on every hidden terminal; an atomic mode cache cut `compute_view` from 88.8 ms to 0.14 ms and the focused 50-pane virtual render from 92.5 ms to 14.6 ms. |
| API concurrency floor | Fixed. Initial request polling now starts at 1 ms and backs off; p99 at concurrency 32 fell from 102.4 ms to 3.3 ms. The overload path drains an in-flight request before close, with a deterministic large-request regression test. |
| Memory per pane | Accepted. Idle RSS is 21/26/37 MB at 1/15/50 panes; populated RSS is 33/174/517 MB. Allocation is already lazy, so the ~10 MB populated history budget is not an eager-allocation defect. |
| Detection cost | Measured separately. At 50 panes process probes average 190 µs, screen reads 270 µs, and classification below 1 µs; no optimization justified. |
| Frame lock / transport | Closed by focused measurements. Frame-lock waits are immaterial. ANSI cuts bytes sharply but increases full-render CPU, so semantic remains the default and ANSI remains opt-in. |

The findings as they stood before closure:

1. **Terminal-core hold time under pane count** — parsing costs 18 ns/byte at one
   pane and 141 ns/byte at fifty, for the same bytes through the same
   already-optimized parser, and the total inside `pty.ghostty_write` is the one
   figure that did not improve in release. Cause unknown, and the terminal-lock
   contention instrument — one of the initial audit's three unbuilt measurement gaps —
   is what would say whether contention is even the mechanism.
2. **`full_render.render_virtual` scaling** — 1.1 ms at one pane, 21.9 ms at fifty,
   which is ~80% of a full render and past a frame budget on its own. Fixed-geometry
   render scaling was ruled out in section 1; this is the part that was not.
3. **The ~100 ms API concurrency floor** — 0.7 ms at concurrency 1 against 102.6 ms
   at 32, *identical* in debug and release, so it is a poll interval rather than
   work. Cheaper to chase than the debug numbers suggested. The refusal racing its
   own close (183 of 768 connections got `Broken pipe` instead of
   `server_overloaded`) is the correctness half of the same area.
4. **Memory per pane** — 521 MB resident at 50 panes, unchanged by build profile,
   about 10 MB per pane. That is the configured scrollback budget being taken at
   face value rather than a leak, so the question is whether the budget should be
   allocated lazily, not whether something is wrong.
5. **Detection cost separated from `PaneInfo` (H3)** — `api.pane_info` averages
   6.7 ms at 50 panes and peaks at 114 ms, while `foreground_cwd` peaks at only
   25 ms of that. Most of the worst case is something else and still unidentified.
6. **Frame lock contention (H2)** and **ANSI-versus-semantic frames (H1)** — both
   close hypotheses rather than chase a known cost.

Closed by phase 8: B6's remaining stall. It is 0.2 s in a release build, and the
per-event routing redesign the plan once contemplated would be for nothing.

Not performance-ranked: B5's bounded request channel (skipped backstop, made
unreachable by the connection cap), the Windows PTY staging queue (verified by
`windows-lint` only, no hardware here to run it on), and squashing the six
commits above `upstream/master` before the next rebase — see the standing note,
upstream is now six commits ahead on these same files.

## Phase 9 result: the peer boundary drops facts, not bytes

A separate line of work from phases 0–8, and a different kind of defect. Those
were about cost; these are about a remote pane quietly answering the local
default to questions only the peer can answer. Started from one report — cut and
paste not working from a remote instance — which turned out to be four distinct
bugs sharing a cause.

`TerminalRuntime::pty()` returns `None` for `Self::Remote`, so every accessor
shaped `self.pty()…unwrap_or_default()` answers as though the pane were empty.
51 of them were written that way, 49 still are. Most are legitimately pty-only;
the ones a user reaches are not, and none of them fail loudly.

| Fixed | Was |
|---|---|
| OSC 52 clipboard from a peer pane | Routed to the foreground client only, which a federating connection never is. On a headless peer — the ordinary shape — dropped entirely. |
| Paste into a peer pane | Sent as raw `Input`, so it arrived as typing and a pasted newline ran as a command. Now a structured `ClientInputEvent::Paste` the peer re-brackets. |
| Mouse-select and copy | Nothing happened, silently. Two causes, and the visible one was not the real one — see below. |
| Clicking a URL | `visible_hyperlinks` empty, and the plain-text fallback died on the same `extract_selection` that copy did. |
| Scroll position | `scroll: null` over the API and no scrollbar, while the wheel scrolled the pane perfectly well. |

**The copy bug is the one worth remembering.** `extract_selection` answering
`None` was real, pinned, and *not why nothing happened*.
`forward_pane_mouse_button` took every click on a peer-backed pane — a view
cannot ask whether the peer's program wants the mouse, so it assumed yes — which
meant `Selection::anchor` was never reached and there was no selection to copy.
Fixing the visible cause alone would have shipped a correct read into a path
nothing reaches. It was found only because the fix did not work and an added log
never fired.

**Where each answer comes from now**, which is the design rule the rest of this
should follow:

- *On the frame, because it describes those cells* — scroll position, OSC 8
  links. Polling them separately lets the answer and the cells it describes come
  from different moments, which maps screen rows onto the wrong buffer rows
  without looking wrong.
- *Pushed on change* — bracketed paste, mouse reporting. Facts about the peer's
  program that decide what this side does with input.
- *Asked of the peer* — selection text, via the new `pane.read_range`.
  Deliberately not reconstructed from frame cells: matching the terminal means
  matching how it joins soft-wrapped lines and where it trims, which is a
  formatter in the vendored terminal. The wrapped case is precisely the one
  reconstruction gets wrong.

Validated by differential test rather than by inspection: identical content in a
local and a peer-backed pane, the same drag, byte-identical clipboards —
including a selection spanning a soft wrap, which comes back rejoined with no
newline on both sides.

<a id="still-open-at-the-peer-boundary"></a>
### Phase 9 closure (formerly “Still open at the peer boundary”)

All four user-visible gaps are fixed:

| Item | Final disposition |
|---|---|
| Focus | Forwarded as terminal-scoped `ClientInputEvent` focus messages. The peer TUI's outer-focus state and notification suppression are unchanged. |
| Copy mode | Opens on peer-backed panes; scroll navigation stays frame-local and selection text remains peer-authoritative. |
| Search and text motion | Added additive `pane.text_query` JSON API forwarding. Off-screen search, soft-wrap coordinates and word motions run beside the owning terminal buffer; stale asynchronous replies are generation-rejected. |
| Agent OSC progress | Added to peer metadata and additive `PaneInfo` fields, with the generated schema refreshed. |

Validated by focused unit tests, a real two-server scrollback search/motion E2E,
and the full Linux/Windows gate. The original items follow for historical
context.

1. **Focus events never reach a peer-backed pane.** `runtime.rs` documents the
   absence and names the fix — a terminal-scoped message on the control
   connection. Now cheap: `ClientInputEvent` already carries `FocusGained` and
   `FocusLost`, so it is the same shape as the paste fix and needs no protocol
   change. One thing to check first: whether those map to *outer-terminal* focus
   on the peer, which would disturb its notification suppression.
2. **Copy mode refuses to open on a peer-backed pane** (`copy_mode.rs`, an
   explicit guard, not a fallout of missing state). More tractable now that
   scroll metrics exist, but it also wants `search_text_matches` and
   `word_motion_target`, which are peer queries.
3. **Search finds nothing in a peer pane.** Needs off-screen content, so it is a
   peer query and not a frame read. The one item here that is genuinely a new
   subsystem: match positions come back in the peer's buffer coordinates and
   have to be mapped into a viewport that may scroll between request and reply,
   and the search UI is incremental and currently synchronous.
4. **`agent_osc_progress` is empty for a peer-backed agent.** Already pinned as
   "a real gap rather than a safe default". `RemotePaneMetadata` is the carrier
   and already brings agent and status across.

Not ranked: the remaining 49 pty-only accessors. Most are correct as they stand —
handoff, history, agent detection — and the characterization test in
`src/app/api/peers/mod.rs` is the inventory, sorted into safe defaults, answers
that have an interception above them, and real gaps. Read it before assuming any
particular `None` is a bug.

## Phase 10 result: both live lists are closed

All reported measurements in this phase used `target/release/herdr`, fixed
geometry, fresh servers per cardinality, and the low-impact wrapper. The stress
driver now disables the lab's env-gated hitbox dump because production does not
enable it and the performance workloads never click controls.

### API polling and overload correctness

| concurrency | baseline p99 | final p99 | baseline failed | final failed |
|---:|---:|---:|---:|---:|
| 1 | 0.8 ms | 0.7 ms | 0 | 0 |
| 32 | 102.4 ms | 3.3 ms | 0 | 0 |
| 256 | 117.1 ms | 34.7 ms | 112 | 0 |

The initial request reader used the general 100 ms connection poll interval.
It now starts at 1 ms and exponentially backs off to 100 ms, preserving cheap
idle connections without charging the accept/write race a full poll. Rejected
connections write their structured overload response and briefly drain the
request before close, so unread request bytes cannot reset the socket and erase
the response. Tests pin fast-to-slow backoff, an overload response against a
256 KiB request still being written, the 128-connection cap, and one dispatched
request per connection. The last two are the architecture proof that B5's
separate bounded request-channel backstop remains unnecessary.

### Render and terminal-core scaling

| panes | baseline full render | final full render | baseline `render_virtual` | final `render_virtual` |
|---:|---:|---:|---:|---:|
| 1 | 1.9 ms | 1.7 ms | 1.3 ms | 1.0 ms |
| 15 | 42.9 ms | 14.3 ms | 36.6 ms | 8.0 ms |
| 50 | 68.0 ms | 27.1 ms | 56.7 ms | **14.6 ms** |

Subphase instrumentation localized one focused 50-pane run's 92.5 ms virtual
render to 88.8 ms in `compute_view`; drawing was 3.5 ms. Background-tab sizing
asked every live terminal whether it was on the alternate screen, serially
locking 49 hidden terminals that were concurrently parsing output. An atomic
alternate-screen cache, updated beside PTY/history/handoff writes, cut
`compute_view` to 0.14 ms. A state-transition test pins enter and exit updates.

Terminal-core measurements reject lock contention as the parser-scaling cause.
At 50 panes the average wait is only 3–4 µs while the average hold is 353–415
µs. With scrollback disabled, hold falls to 117 µs and Ghostty write to 47 µs;
the cost is retained-history work and memory locality inside the owner, not
threads waiting to enter it. No parser rewrite follows from that evidence.

### Memory, detection, frame lock and transport

| panes | idle RSS | populated RSS |
|---:|---:|---:|
| 1 | 21 MB | 33 MB |
| 15 | 26 MB | 174 MB |
| 50 | 37 MB | 517 MB |

Scrollback allocation is already lazy. Disabling it holds 50-pane RSS to 43 MB,
which corroborates that the populated delta is the configured history budget,
not an eager reservation or leak.

| panes | process probe avg | screen read avg | classify avg | `PaneInfo` avg | foreground-CWD avg |
|---:|---:|---:|---:|---:|---:|
| 1 | 82 µs | 255 µs | 17 µs | 268 µs | 230 µs |
| 15 | 69 µs | 272 µs | 16 µs | 676 µs | 658 µs |
| 50 | 190 µs | 270 µs | <1 µs | 955 µs | 930 µs |

Detection is separate from `PaneInfo` and is not material at this scale. The
`PaneInfo` average is almost entirely foreground-CWD lookup; its prior 114 ms
peak did not reproduce in the isolated workload (50-pane p99 16.9 ms).

At 15 peer panes, frame-store p99 lock wait is 4 µs, render-side wait is
effectively zero, and store hold averages 113 µs. H2 is disproven. At 50 local
panes ANSI emitted about 21 KB against semantic frames' 811 KB, but full-render
CPU rose from 27.1 to 36.1 ms because ANSI diff encoding moved work onto the
already-busy server. H1 is a real bandwidth/CPU trade rather than a universal
win.

### Final decisions

| Decision | + | − | Recommendation |
|---|---|---|---|
| Atomic alternate-screen cache | Removes pane-scaled core-lock waits; virtual render reaches 14.6 ms at 50 panes. | Adds one cached scalar whose write paths must stay synchronized. | **Keep.** The transition test and narrow ownership make the maintenance cost proportionate. |
| Exponential initial-request polling | Removes the 100 ms floor while backing idle connections off. | A newly accepted silent connection wakes several times before reaching 100 ms. | **Keep.** The cap bounds that cost and measured latency improves by ~31× at concurrency 32. |
| Default semantic frames | Lower server CPU and required for peer cell semantics. | Roughly 39× more bytes than ANSI in the focused run. | **Keep semantic default; keep ANSI opt-in.** Do not redesign federation transport from this result. |
| Default scrollback budget | Preserves useful retained history and search. | About 10 MB resident per populated pane and measurable parser cost at saturation. | **Accept for this cycle.** Allocation is already lazy; expose/tune the existing config rather than changing semantics. |
| Detection implementation | Costs remain small and isolated from `PaneInfo`. | Process-walk tails still exist. | **No optimization.** Revisit only with a user-visible detection latency or larger-cardinality reproduction. |
| Windows PTY pressure | Windows compile/clippy protects portability. | Runtime pressure behavior remains unobserved. | **Accept compile-only gap.** A future run needs actual Windows hardware, not speculative code. |

### Final validation

`ZIG=/opt/zig0.15/zig just check` passed after the final changes: formatting and
clippy, 3,817 Rust tests, Windows-target clippy, all JavaScript/integration
suites, and 138 maintenance tests. `just test-e2e` passed all 30 selected tmux,
peer and SSH-lab scenarios (11 non-e2e cases deselected). The SSH cleanup test
was also made namespace-safe: it now follows only bridge directories created by
its disposable lab, so an unrelated live Herdr session in shared `/tmp` cannot
produce a false failure.

## Standing notes

**Fork cost.** These fixes concentrate on `src/server/headless.rs`,
`src/terminal/runtime.rs`, `src/terminal/remote.rs` and `src/pane.rs` — surfaces
`AGENTS.md` lists as recurring conflict sites. Phase 6 added two more:
`src/client/input.rs` and `src/app/input/terminal.rs`, both on the input path,
and phase 7 concentrated on `src/app/input/terminal.rs` again plus
`src/config/keybinds.rs`. Phase 9 spread further again — see the conflict-surface
list in `AGENTS.md`, which it extended. The completed fork delta is maintained
as one squashed commit above `upstream/master` so the next rebase has one
reviewable conflict set.

That drift was taken once already: the six upstream commits this note used to
list — `76211757` on `src/app/input/terminal.rs`, `src/app/input/mod.rs` and
`src/server/headless.rs`, `19022d03` on `src/pane.rs`, `94093acb` on
`src/client/input/windows_vti.rs` — are in `main` now, and
`git log main..upstream/master` is empty against the local ref.

Do not read that as current. The local `upstream/master` only moves on
`git fetch upstream`, so re-check after fetching rather than trusting this
paragraph; the drift is what a rebase costs, and it gets cheaper the sooner it
is taken.

**Tests are part of each fix.** Every finding carries a prescribed validation. A
fix landed without its test does not close the finding.

**`just check` reports through a pipeline.** When it is run as
`just check 2>&1 | tail`, the task notification shows the *tail's* exit code, not
the recipe's. Several failures in this work were announced as "exit code 0",
including a run that was not failing but *hanging*. Read the output, not the
summary — and if the output stops without a summary line, the run is stuck rather
than slow.

**Windows lint catches what Linux cannot.** Twice here: a `large_enum_variant`
from an inline `Mutex` field, and dead code left behind when a caller moved
off-loop. Run `just windows-lint` after anything that changes struct sizes or
removes callers.
