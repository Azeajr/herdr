# peer-test

Local tooling for testing herdr's peer, UI and ssh behaviour. Not part of the release
pipeline.

`scripts/lab.py` is a `uv run` single-file lab: N isolated servers on this box,
peered, with real TUI clients an agent can drive and read back. Its small
`scripts/_common.py` module holds the build and environment helpers shared with the
pytest fixtures.

`tests/` is the scenario suite: the same commands, with assertions and teardown. See
[Scenarios](#scenarios) below.

`scripts/stress.py` is the load harness: the same labs, driven hard, answering with the
server's own profiler windows instead of assertions.

| Entry point | Purpose |
|---|---|
| `just test-e2e` | Local isolated servers plus real tmux-hosted TUI clients |
| `just test-boxes` | Cross-machine SSH federation across disposable Docker machines |
| `just test-stress` | The load *bounds*: admission caps, resource recovery, burst absorption |
| `lab.py` | Manual local reproduction and evidence capture |
| `stress.py` | Load *measurement*: seven workloads at several cardinalities each |

`lab.py` never dispatches itself to another host. Physical installations use the
repository-level `just install`; the SSH lab only bootstraps a temporary binary inside
its disposable lab root.

---

## lab.py in one minute

```sh
# A function, not a variable: zsh does not word-split unquoted parameters, so
# `L="uv run …"; $L up` fails there with "No such file or directory".
lab() { uv run peer-test/scripts/lab.py --lab p1 "$@"; }

lab up --instances a,b --peer 'a->b'      # build, boot, wire  (--no-build to reuse target/debug)
lab state b                               # what b believes right now
lab cli b -- workspace create --label remote-ws --focus
lab cli a -- peer open "$(lab state b | jq -r .instance_id):w1:p1" --peer b --focus

lab ui open a --client A                  # a real TUI that stays alive between commands
lab ui onboard A                          # clear the onboarding welcome; see below
lab ui screen A                           # what it shows; row index == click row
lab ui hitbox A                           # where herdr says its controls are
lab ui text A 'echo hi'; lab ui keys A Enter
lab ui wait A --contains hi --timeout 15  # exit 3 if it never shows up

lab ui click A --control 'workspace[0]' --button right   # exact, not counted
lab ui click A --control Rename                          # a menu row, by its label

lab effect -- ui click A --text '+'       # act, then diff every instance's state
lab logs all --grep '(?i)mouse|peer' --tail 20
lab evidence plus-click --note "why this bundle exists"
lab destroy                               # servers, clients, dirs, orphan bridge dirs
```

Output is JSON whenever stdout is not a tty (`--no-json` forces the human rendering).
Exit codes are part of the contract: **0** ok, **2** usage/precondition, **3** wait timed
out, **4** assertion failed.

`lab.py gc` finds labs left behind by a crashed session; `gc --force` destroys the dead ones.

## Scenarios

```sh
just test-e2e                       # real servers + TUI clients through tmux
just test-boxes                     # cross-machine, on the Docker peer boxes
just test-stress                    # the load bounds (needs tmux)
just test-e2e -k hide -x            # extra args go straight to pytest
```

None is in `just check`: they need tmux and Docker, and `test-e2e` takes minutes. The
three markers are disjoint, so a scenario is in exactly one suite, decided by its
directory.

The suite drives `lab.py` as a subprocess and never imports it — its JSON output and exit
codes (**0** ok, **2** usage/precondition, **3** wait timed out, **4** assertion) are
already the contract, so a scenario reads like the commands a human would run.

| File | What it holds down |
|---|---|
| `tests/test_harness.py` | The suite's own preconditions: a lab boots two peered servers, and a client gets past onboarding |
| `tests/test_cold_start.py` | `[peer_hidden]` and `[peer_history]` across a server restart, and the history file's cap and dedupe after twelve real adds |
| `tests/test_peer_sidebar.py` | The peer header under real mouse events: the workspace picker, collapse/expand, and the hide refusal *and its expiry* |
| `tests/test_peer_panes.py` | A context-menu "Split right" on a peer-backed pane landing on **both** servers |
| `tests/test_ssh_lab.py` | `ssh-check`'s ten authorized_keys assertions, bridge cleanup, and an `ssh://` recent surviving a cold start |
| `tests/test_hitbox.py` | Clicking by control name: a context menu by label, the launcher menu, a peer header by handle, and a control that is not drawn failing instead of hitting blank space |
| `tests/boxes/` | box1 → box2 peering container-to-container, and box3 refusing discovery with no herdr installed |
| `tests/boxes/test_netem.py` | The same peering over a 200ms/1% link, and a peer plus its already-open view recovering from a full partition |
| `tests/stress/` | The load bounds: the API admission cap refusing without leaking, and a pasted burst into a stalled pane leaving the server able to answer |

Conventions worth keeping:

- **Fixtures tear down on failure too**, and a failing test freezes a `lab.py evidence`
  bundle *before* its lab is destroyed. `lab_factory` fails the test if a lab root
  survives `destroy`, so a leak is reported rather than left in `/tmp`.
- **Wait for a condition, never sleep.** `wait_for` / `wait_gone` poll the client screen;
  `wait_peer_enumerated` polls server state. Asserting that something is *absent* is only
  meaningful after the thing that would have drawn it has arrived.
- **Address controls by name or text, not coordinates.** `click(..., control=...)` uses
  herdr's own hit rectangles and is the most exact; `click(..., text=...)` finds the cell
  on screen. A `col`/`row` click is sent with `--require-hit`, so landing on blank space
  fails the test instead of looking like a dead button.

## stress.py

```sh
uv run peer-test/scripts/stress.py list
uv run peer-test/scripts/stress.py run api --at 1,32,256
uv run peer-test/scripts/stress.py run output --at 1,15,50 --seconds 6
uv run peer-test/scripts/stress.py run input --at 200 --opt bytes=4096 --opt via=paste
```

Each run prints a markdown table and writes the full profiler aggregate to
`peer-test/evidence/stress-<workload>-<stamp>/report.json`. The server is started with
`HERDR_RENDER_PROF=1`, and the harness reads the `event="render.prof"` windows the server
logs, scoped to the phase by a byte offset in the log.

| Workload | Cardinality | What it answers |
|---|---|---|
| `output` | panes | PTY parse and render cost as live panes multiply |
| `input` | input events | What a burst into a pane whose child stopped reading costs (`--opt via=ui\|paste\|api`) |
| `fanout` | clients | Render cost per attached client (`--opt geom=same`, `--opt slow=0`) |
| `peer` | peer panes | What serving federated views costs the far server, and frame bytes |
| `churn` | reconnect rounds | Whether reconnect storms leak threads, descriptors or panes |
| `api` | concurrent connections | Admission cap behaviour and queue depth under a flood |
| `persist` | panes with scrollback | Session save split between the loop and the save thread |

Things that were wrong before they were right, and are now enforced:

- **A fresh server per cardinality.** Gauge peaks are retained for the life of a process
  on purpose, so 50 panes measured after 15 in the same server reports whichever was
  worse and labels it 50.
- **Percentiles do not average.** The log carries a summary per window, not its buckets,
  so `p99_worst_us` is the worst window — an upper bound, never an understatement.
- **A driver that silently does nothing reads as a clean result.** `tmux send-keys`
  failures raise, and the `input` workload probes that a keystroke reaches the pane
  before concluding anything from a quiet queue.
- **`SESSION_SAVE_DEBOUNCE` is five seconds and every change re-arms it.** A driver that
  churns continuously for six seconds produces zero saves and reports history capture as
  free. `persist` changes one thing, then waits.

## Why it is shaped this way

- **CLI testing alone proves nothing about the buttons.** A CLI/API request is gated on
  `request_targets_peer_workspace` before anything is handled locally; a keybind or click
  becomes a client input event handled in-process. The two have disagreed in production.
  `effect` exists to catch exactly that: it diffs every instance around an action and says
  when one gained a tab that should have been created on its peer. It diffs the client
  screens too, because plenty of correct UI actions change no server state at all — see
  the next section.
- **Clients live in tmux**, one tmux server per lab, so a client survives between commands
  and multi-step UI flows (click → dialog → type → confirm) are expressible. Watch the agent
  work with `tmux -L hl-<lab> attach -t <client>`; detach with `C-b d`.
- **Evidence is a directory, not a screenshot.** `evidence` writes states, screens (text and
  ANSI), full logs, a merged cross-instance `timeline.log`, and the lab's command history to
  `peer-test/evidence/<host>-<lab>-<name>-<stamp>/`, which another agent can read without
  rebuilding the experiment.

## Before you report a dead button

Both of these produced a false "left clicks on chrome do nothing" report once already.

- **A fresh lab starts the client in onboarding.** A lab instance has a state home nobody
  has used, so the first client shows the onboarding welcome. `Mode::Onboarding` returns
  from `handle_mouse` before any chrome hit test (`src/app/input/mouse.rs`), so every click
  on the sidebar or the tab bar vanishes with no state change and no log line saying why.
  `ui open` reports `"gate": "onboarding"`; clear it with `ui onboard <client>`.
  Do that rather than pressing Enter yourself: Enter leaves the welcome but lands on the
  integrations settings screen, where a **second Enter installs agent integrations into
  your real `$HOME`** — the lab isolates XDG, not that. `ui onboard` presses Enter once,
  then Escape.
- **Chrome buttons that create things may open a name prompt first.** The tab-bar `+` opens
  the new-tab name dialog (`ui.prompt_new_tab_name` defaults **on**), so the tab appears
  only after `ui keys <client> Enter`. The sidebar `new` button creates a workspace
  immediately (`ui.prompt_new_workspace_name` defaults **off**). An `effect` run whose
  `diff` is empty but whose `screens.<client>.changed` is true is this case, and its
  verdict says so.

- **A hand-counted `--col` is a guess, not a measurement.** A context menu is drawn at the
  point you right-clicked, not at a fixed place, and a click on blank space *closes* an
  open menu without dispatching anything (`mouse.rs:576-586`) — identical in every
  observable way to a control that does nothing. This produced a false bug report once
  already. Use `ui click --control '<name>'` or `ui click --text '<label>'`. With
  `--col/--row`, `ui click` reports the `cell` it is about to hit and warns when that cell
  is blank; `--require-hit` turns the warning into exit 4.

## Clicking a control by name

`ui hitbox <client>` answers with the rectangles herdr computed for this frame, and
`ui click --control <name>` clicks the point that resolves back to that control. Nothing
is counted, and a control that is not drawn fails with exit 4 and the list of the ones
that are — rather than clicking whatever happens to be underneath.

```sh
lab ui hitbox A                          # every control, with its click point
lab ui hitbox A --control 'menu[0]'      # one, waiting up to --timeout for it
```

| Name | What it is |
|---|---|
| `sidebar`, `sidebar.workspace_list`, `sidebar.agent_panel`, `sidebar.footer` | Sidebar regions |
| `sidebar.new`, `sidebar.launcher` | The footer's new-workspace button and the launcher |
| `workspace[<i>]`, `peer[<handle>]` | A workspace row; a peer group header, named by handle |
| `tab_bar`, `tab[<i>]`, `tab_new`, `tab_scroll_left`, `tab_scroll_right` | The tab bar |
| `terminal`, `toast`, `mobile_header`, `mobile_menu` | Pane area, toast, mobile chrome |
| `menu`, `menu[<i>]`, `global_menu`, `global_menu[<i>]` | An open context or launcher menu |

Menu rows are addressable by label too — `--control Rename` — which is what a scenario
should use, since an index moves when the menu offers something else. The dump also
carries `mode`, which is the client's own answer to "did that click dispatch anything".

Under the hood the server writes this to `HERDR_HITBOX_DUMP` after each render — the
server owns `AppState` and hit-tests the mouse, so it is the process that knows. `lab.py`
sets the variable per instance when it starts the server, so a lab created before this
existed has to be recreated. It is env-gated in the shipping binary rather than behind a
cargo feature: a `#[cfg(feature)]` build would mean testing a binary nobody runs.

`effect` reports `screens.<client>` with `changed`, `changed_row_count`, up to 12
`changed_rows`, and the overlay `gate` before and after. `--no-screens` turns it off.

Not everything odd is a bug: the sidebar header really is the literal `" spaces"`
(`src/ui/sidebar.rs`), not a clipped `"Workspaces"`.

## Constraints worth knowing before you debug the harness instead of herdr

- **Lab roots must be short.** A server binds `<config>/<app_dir>/herdr.sock`; a deep root
  fails with `local socket name length exceeds capacity of sun_path of sockaddr_un`. Labs
  live at `/tmp/hl-<lab>/` for that reason — do not point one at a scratch directory.
- **The ssh lab needs ~200 MB free in `/tmp`.** `ssh-peer` makes herdr install itself on
  the "remote", which copies the whole debug binary into the lab root — on a tmpfs, under
  whatever quota it has. Out of space, `ssh-peer` now exits 4 with the sshd log tail
  (`remote install exited with exit status: 1`) instead of reporting an empty peer list
  and exiting 0.
- **Input goes in with `tmux send-keys -l`, never `-H`.** Hex mode splits escape sequences;
  herdr logs `flushing lone escape after input timeout` and the mouse report never arrives.
- **`pane read --source recent` is empty on a headless server with no attached client.**
  Use `visible` or `detection`.
- **Raise log detail per lab** with `up --env HERDR_LOG=herdr=debug`. That is what turns
  "the click did nothing" into `event=Mouse(MouseEvent { kind: Down(Left), column: 35, … })`
  in `logs`.
- **`destroy` reaps orphan `/tmp/herdr-peer-*` and `/tmp/herdr-ssh-*` dirs** whose creating
  process is gone (`--keep-orphans` to leave them). Dirs whose pid is alive are never touched.
- **The ssh lab's far-side herdr is sshd's child, not the lab's.** No pid file knows about
  it, so it used to survive `ssh-down` *and* `destroy`, run on from a deleted binary at
  195 MB of tmpfs apiece, and recreate `$HOME/.config` under a root that had just been
  removed. `teardown` and `ssh-down` now match it on the lab-root path prefix and kill it
  by pid; `ssh_target_servers` in their output says what went.

## The ssh lab

`peer add --socket` never builds a bridge, so nothing else exercises key install, the ssh
stdio bridges, or the bridge directories they clean up on drop. The ssh lab adds a real
OpenSSH target on this box without touching your `~/.ssh`:

```sh
lab ssh-up       # throwaway sshd on 127.0.0.1:22222 + instance `s`
lab ssh-check    # assert herdr's authorized_keys install/replace rules
lab ssh-peer     # wire s -> the lab target over ssh, exercising the bridge
lab ssh-status
lab ssh-down     # stop s and the sshd; bridge dirs should vanish
```

The ssh lab's peer target is **not** another lab instance: `ssh-peer` wires `s` to a herdr
the sshd spawns under the lab target `HOME`, so diffing lab instances says nothing about
that peer. Read its state through `cli s -- peer list --json` instead.

It stays off your real ssh setup because the sshd's `AuthorizedKeysFile` points into the lab,
a `ForceCommand` exports `HOME=<lab target>` so the file herdr writes is the file sshd
authenticates against, and an `ssh` shim on instance `s`'s PATH offers the bootstrap identity
only when `BatchMode` is absent — standing in for a human typing a password, so herdr's batch
preflight fails exactly as it would against a password-only target and the install path runs.

## Cross-machine

For installed servers on physical hosts, check out the same commit and run
`ZIG=/path/to/zig just install` on each host, then peer the servers over ssh. The
automated cross-machine scenarios use the disposable Docker boxes below instead.

## Docker peer boxes

Three disposable machines, without owning three machines. Each container has its own
rootfs, hostname, `$HOME`, `PATH` and network namespace, which is what the same-box SSH
lab cannot give you: `resolve_peer_remote_herdr` has to find a binary on a filesystem this
host does not share. `box1` and `box2` form the peering pair; `box3` deliberately has no
Herdr binary.

`just test-boxes` is the normal entry point: it builds `target/debug/herdr`, starts the
boxes, runs the scenarios, and tears them down. For a manual session:

```sh
ZIG=/opt/zig0.15/zig cargo build --locked
./peer-test/docker/boxes.sh up       # start and wait for ssh on all three
./peer-test/docker/boxes.sh status   # hostname + which herdr each box resolves
./peer-test/docker/boxes.sh down
```

### Breaking the link between two boxes

```sh
./peer-test/docker/boxes.sh netem box2 --to box1 200ms 1%   # a realistic link
./peer-test/docker/boxes.sh netem box2 --to box1 0ms 100%   # a partition
./peer-test/docker/boxes.sh netem box2 show
./peer-test/docker/boxes.sh netem box2 clear
```

`netem` shapes a box's **egress**, and `--to` scopes it to packets addressed to another
box: the rule hangs off band 3 of a `prio` qdisc that nothing but the destination filter
can reach, so the ssh you are watching through keeps its normal path even at 100% loss.
You reach the box through its published port, not through its peer address. Without that
scoping a partition takes the observer down with the subject, and you cannot tell a peer
that failed to recover from an assertion that never ran.

One direction is enough for a partition: the other box's packets still arrive, nothing
answers them, and every connection between the two stalls.

In scenarios, use the `netem` fixture rather than calling the script — the containers are
session-scoped, so a rule left behind does not fail your test, it silently degrades every
scenario that runs after it.

| Box | herdr | For |
|---|---|---|
| `box1`, `box2` | host build, bind-mounted at `/usr/local/bin/herdr` | The peering pair |
| `box3` | **none** | The negative discovery case |

Nothing touches your `~/.ssh`: `boxes.sh` generates its own key and two ssh configs under
`peer-test/docker/.secrets/` (gitignored) -- one for the host to reach the boxes on
`127.0.0.1:2201-2203`, one mounted into the boxes so they reach each other by hostname.
Use `./peer-test/docker/boxes.sh config` to print the first for `ssh -F`.

Notes that cost time when they were wrong:

- **The base image is Arch on purpose.** The binary is built on the host and mounted in
  dynamically linked, so the image's glibc must be at least the host's. A Debian or Ubuntu
  base fails at exec with a `GLIBC_2.4x not found` error.
- **Mount the binary onto the default `PATH`.** A non-interactive ssh session does not
  inherit the image's `ENV PATH`, so a binary under `~/.local/bin` is invisible to
  `command -v herdr`.
- **`herdr server` -- no subcommand -- is the headless server.** `herdr server start` is
  not a thing, and the error it gives points at the TUI instead.
- Arch base has no `hostname` binary; use `uname -n`.
- **`boxes.sh up` does not update a box that is already running.** `docker compose up -d
  --build` leaves an unchanged container alone, and the herdr binary is bind-mounted, so a
  box that is already up keeps serving the build it started with — `cargo build` replaces
  the file with a new inode, which the running mount does not follow. The `boxes` fixture
  is session-scoped, so a suite started against live containers tests the *old* binary and
  a fix you just made appears not to work. `boxes.sh down` first, or `docker compose
  restart`. The same applies to a change to the image or the compose file.
- **`box3` must have no Herdr binary anywhere on its filesystem.** The discovery scenario
  asserts the stronger filesystem property, not only that `herdr` is absent from `PATH`.
