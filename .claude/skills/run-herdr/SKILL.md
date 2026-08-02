---
name: run-herdr
description: Build and launch herdr (this repo) from source, and drive/verify the TUI via tmux. Use when asked to run, start, build, or smoke-test herdr locally, especially from inside an existing herdr session.
---

# Run herdr

## Build

Needs zig 0.15.2 exact (vendored libghostty-vt requirement). Check first:

```bash
zig version   # must be 0.15.2
```

If missing/wrong version on Arch, package `zig0.15` provides it at `/opt/zig0.15/zig`:

```bash
pacman -Qi zig0.15 2>&1 | head -1   # confirm installed
export ZIG=/opt/zig0.15/zig
```

Build **debug**, not release, for local dev/test runs:

```bash
cargo build --locked
```

## Why debug, not release

`src/config/io.rs` picks the config/socket dir by `debug_assertions`:
- debug build → `herdr-dev` (isolated)
- release build → `herdr` (same dir as your installed stable herdr)

A `--release` build talks to the same socket as your real installed server. If
that server is a different protocol version, launch refuses with a protocol
mismatch error instead of running standalone. Debug build sidesteps this
automatically.

## Nested-session guard

If you're running this from inside an existing herdr session, herdr detects
its own env vars and refuses to nest by default. Clear them before launching
the test instance:

```bash
env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
    -u HERDR_PANE_ID -u HERDR_TAB_ID -u HERDR_WORKSPACE_ID \
    ./target/debug/herdr
```

(Only `HERDR_ENV`/`HERDR_PANE_ID`/`HERDR_TAB_ID`/`HERDR_WORKSPACE_ID` gate the
nesting check; the socket vars matter if you also want the debug client
pointed at a specific debug server instead of autodetecting `herdr-dev`.)

## Driving it (TUI, no GUI window)

It's a terminal app — verify via tmux, not a screenshot tool:

```bash
tmux new-session -d -s herdr_check -x 120 -y 30 -c /home/spark343/github/herdr \
  "env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
       -u HERDR_PANE_ID -u HERDR_TAB_ID -u HERDR_WORKSPACE_ID \
   ./target/debug/herdr"
sleep 2
tmux capture-pane -t herdr_check -p     # check render came up

tmux send-keys -t herdr_check "echo hello" Enter
sleep 1
tmux capture-pane -t herdr_check -p     # check command ran, output visible

tmux kill-session -t herdr_check        # teardown
```

A blank/garbled capture means it didn't actually launch — don't declare success
on exit code alone.
