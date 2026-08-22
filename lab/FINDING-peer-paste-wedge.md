# Finding: client input wedge after large bracketed paste to a focused peer pane

Status: reproduced and narrowed 2026-08-21; root cause not yet fixed.
Evidence bundle: peer-test/evidence (lab m3a, `peer-paste-wedge`).

## Reproduction

1. Lab: two servers, a peered to b (`a->b`, socket transport). Workspace
   `remote-ws` on b, opened on a via the sidebar picker so a peer-backed pane
   (`w2:p1`) is focused in a real TUI client on a.
2. Send a ~64 KiB bracketed paste to the client's stdin in 1024-byte chunks
   (tmux send-keys -l pacing).
3. The paste content arrives correctly on b's pane (byte stream intact).
4. Afterwards the client is wedged: all subsequent keyboard input (plain text,
   keys) produces no events anywhere — not on b, not locally, nothing in the
   server log. API/screen paths still work.

Small pastes (single-chunk) to a focused peer pane work fine and leave the
client healthy. Local panes with the same chunked large paste are fine.

## Observed client state while wedged

- Process alive, sleeping, 5 threads.
- Main thread: epoll wait (normal event loop).
- One worker thread: blocked in `unix_stream_read_generic` — a synchronous
  blocking read on the herdr-client.sock connection (socketpair fd set
  3028361 <-> 3029163, server side pid = a's server). It never returns.
- No new raw_input warnings after the wedge; server log shows no input events
  for anything typed afterwards.

## Reading

The input path appears to make a synchronous request/response call over the
client socket during (or immediately after) delivering a large peer-bound
paste, and the reply never arrives — deadlocking that thread and, with it,
all subsequent keyboard input. Chunking matters: single-write pastes do not
trigger it, which points at a partial-write / re-entrancy window mid-paste
(e.g. flow-control or pane-write request issued while the paste stream is
still being consumed).

## Next steps

1. Find the synchronous request site in the client input path for remote
   panes (`src/terminal/remote.rs`, `src/client/input.rs`) and identify which
   method blocks.
2. Decide the fix: make the request async/event-driven, or guarantee the
   reply is dispatched while a paste stream is in flight.
3. Pin as regression: paste-matrix `[[pins]]` cell
   `bracketed-paste/peer:b:w1:p1/large` plus an "input still alive after
   paste" assertion (type + expect arrival) added to the runner.
