# herdr

Terminal based agent runtime for coding agents.

This is a **personal fork**. It is not upstream `herdrdev/herdr` and nothing here
is headed there. Upstream's maintainer workflow, contributor guardrails, release
process, and docs governance have been removed on purpose — they do not apply.

`CLAUDE.md` is a symlink to this file.

## Working in this fork

One checkout (`~/github/herdr`), one branch (`main`), no worktrees. `main` is
`upstream/master` plus a short queue of local commits, led by the squashed
`feat: federate herdr servers over peer connections`. `git log
upstream/master..main` is the whole patch queue. Keep it short; that is what
makes the repeated rebase cheap.

Resync:

```bash
git fetch upstream
git rebase upstream/master
ZIG=/opt/zig0.15/zig just check
```

Rebase onto `upstream/master`, never `origin/master` — the fork's mirror is stale
and rebasing onto it moves you backward.

`.local/` is gitignored and is the right home for local notes, PRDs and scratch
specs.

### Engineering record

[`ENGINEERING_PLAN.md`](ENGINEERING_PLAN.md) is the completed execution record
for the federation correctness and performance work. Its phase 8 and phase 9
closure tables preserve the two former open lists and their evidence-backed
dispositions; they are historical indexes, not pending work. The Status section
and phase 10 final decisions record the deliberately accepted gaps.

### After a rebase

**Rerun `just windows-lint` specifically.** It is the one break `cargo nextest`
cannot catch on Linux, and `just check` runs it last so a Linux-green tree can
still be broken. It has bitten once already: upstream split `src/remote/` into a
cross-platform `attach.rs` plus a unix-only `host_unix.rs`, which stranded
`BridgeSocket::Api`, `local_path` and `describe` as dead code on the Windows
target. Fixed with `#[cfg_attr(windows, allow(dead_code))]` — deliberately not
`#[cfg(unix)]` on the variant, because Windows CI compiles tests and two unit
tests in `attach.rs` construct `Api`.

Conflicts concentrate where upstream touches the surfaces this fork rewrites:
`src/remote/`, `src/server/headless.rs`, `src/terminal/runtime.rs`,
`src/app/state.rs`, `src/protocol/wire.rs`, and `AGENTS.md` itself.

The peer-boundary work added more, and they are the awkward kind — small edits
spread across files upstream also touches: `src/terminal/remote.rs`,
`src/pane.rs`, `src/pane/terminal.rs`, `src/app/actions.rs`,
`src/app/input/clipboard.rs`, `src/events.rs`, `src/server/clients.rs`, and the
API schema trio (`src/api/schema.rs`, `src/api/schema/panes.rs`,
`src/api/schema/response.rs`). Most are one accessor or one match arm, so a
conflict there is usually resolved by keeping both sides rather than choosing.

Do not automate conflicts away — no merge drivers, no `-X ours`. A conflict on a
file this fork deliberately diverged is how upstream's change gets reviewed
before it is discarded. `rerere` is on, which is the right amount of automation:
it replays a conflict already resolved once and still stops on genuinely new
upstream content.

### How to work here

- Don't ask permission for routine work. Make the ordinary judgment call and say
  what you did.
- Don't propose a commit message for approval first. Write a good one and commit.
- No PR flow, no issue references, no bot-review gating. There is no upstream to
  satisfy.
- Aggressive cleanup is welcome. State a one-way cost once, with evidence, then
  do what was asked.
- Verify claims before repeating them. Check the file, run the command, read the
  output.

## Principles

- **State is separated from runtime.** `AppState` is pure data, testable without PTYs or async. `PaneState` is separate from `PaneRuntime`. Workspace logic doesn't need real terminals.
- **Render is pure.** `compute_view()` handles geometry and mutations. `render()` takes `&AppState` and only draws. Never mutate state during render.
- **No god objects.** If a module is doing too many things, split it. `app/` is already split into state, actions, and input. Keep it that way.
- **Platform code is isolated.** OS-specific behavior lives in the matching `src/platform/<os>.rs` file, with only shared traits, types, wrappers, and testable contracts in `src/platform/mod.rs`. Core modules don't have `#[cfg(target_os)]`.
- **Detection is decoupled.** The detector reads a screen snapshot, never touches the parser or viewport state.
- **Screen detection is evidence-based.** When changing `src/detect/manifests/`, first capture the relevant bottom-buffer state with `herdr agent read <pane> --source detection --format text` and, when styling or alternate screen behavior matters, `--format ansi`. Decide which visible controls are invariant, which are alternatives, and encode them as explicit AND/OR gates. Do not match whole-pane incidental text, and do not use the user-visible viewport for agent status because users can scroll it.
- **UI patterns should be reused.** Herdr is a mouse-first TUI. New dialogs, onboarding, settings, and post-update flows should follow the existing UI/UX language and interaction patterns instead of inventing one-off screens. Prefer reusing existing modal/screen structure, affordances, and close actions so the app feels consistent.

## Multiplicative performance paths

Treat work reachable from view computation, rendering, background-pane resizing,
PTY parsing, detection, and client frame fanout as multiplicative. Before adding
work, identify its frequency and cardinality: per byte, event, or render × panes,
tabs, or workspaces × attached clients.

Inside pane-scaled render and layout loops:

- Use narrow terminal-state accessors. Do not collect aggregate input state,
  format terminal snapshots, inspect process trees, perform filesystem I/O, or
  allocate when one scalar fact is enough.
- Keep terminal-core lock duration minimal.
- Preserve hidden-source and retained-render early exits. Hidden panes still
  parse output, but their output must not trigger presentation work merely to
  keep terminal or detection state current.
- When a change adds or widens work in one of these loops, profile fixed geometry
  with 1 and at least 15 populated panes and report the scaling delta. Use
  `just bench-render-scale` to exercise both background-workspace and active-pane
  cardinality when applicable.

Prefer deterministic operation or architecture tests to wall-clock limits.
Performance benchmarks are supporting evidence, not substitutes for behavioral
coverage.

**State the build profile with every measurement, and prefer release.**
`peer-test/scripts/stress.py` and `lab.py` default to `target/debug/herdr`; pass
`--bin target/release/herdr` for anything that will be reported as a finding.
This is not a rounding error. Rerunning the stress workloads in release closed
one finding outright — a 3.2 s input stall was 0.2 s — while leaving pane-count
render scaling and a 100 ms API floor completely unchanged, so the debug numbers
had mis-ranked which problems were real. Note that `build.rs` builds
libghostty-vt `ReleaseFast` in both profiles, so a debug build understates only
the Rust around the parser, not the parser.

A measurement that does not record its binary cannot be compared with a later
one. `stress.py` writes `binary` and `profile` into its report for that reason.

## Runtime/client boundary guardrail

Herdr is migrating toward a server-owned runtime protocol with the TUI as one client. New work should not deepen the current server/TUI coupling.

Before adding state, API fields, events, commands, or socket messages, classify the feature:

- Shared runtime/session fact: belongs in server state and should be exposed through the JSON API/event path when practical.
- TUI presentation state: belongs only in the TUI/client layer.

Do not add new shared behavior that only works through the private TUI client socket. Use neutral server/API names, not UI-surface names like sidebar, row, card, or widget.

Examples:

- Pane/agent metadata, process state, terminal state, events: server/runtime.
- Sidebar layout, token placement, colors, selection, modals, mouse/viewport state: TUI/client.
- Workspace/tab/pane remain shared session organization for now, but avoid making them mandatory identity for unrelated runtime features.

## Testing

Use `just` recipes by default instead of invoking cargo or scripts directly.

```bash
just test               # cargo nextest + maintenance script tests
just check              # formatting check + cargo nextest + Windows lint + script tests
just test-e2e           # peer/UI scenarios through tmux (needs tmux); not in `just check`
just test-boxes         # cross-machine scenarios on the Docker peer boxes (needs Docker)
```

The peer harness has three deliberately separate surfaces:

- `just test-e2e` drives the local-only `peer-test/scripts/lab.py` harness.
- `just test-boxes` builds one debug binary and bind-mounts it into disposable
  cross-machine containers. The containers do not mount the checkout or run the lab.
- `ZIG=/opt/zig0.15/zig just install` installs this fork on a physical host. Do not add a
  second installer under `peer-test/`.

Run `just check` before committing. Don't bypass a failing check; fix it, or say
exactly why a narrower check is enough.

`just check` needs `ZIG=/opt/zig0.15/zig` on this machine. `/usr/bin/zig` is
0.16.0 and the vendored libghostty-vt requires 0.15.2 exactly; without the prefix
the build script panics with a `readFileAlloc` arity error that reads like a code
bug.

Git hooks are **not** active here — `core.hooksPath` is unset and `.git/hooks` is
empty, so `.githooks/pre-commit` never runs `just lint`. Run validation yourself.

### Interactive validation limits

`just lint`, `just check`, `just test-e2e` and `just test-boxes` are the heaviest recipes
in the repo: clippy over all targets, a Windows-target build, or minutes of real servers,
tmux clients and containers. All four run inside a resource-limited cgroup by default via
`scripts/low_impact.py`. Read that module's docstring before changing the limits; it
records what they cover and what they cannot.

- `just lint` is short but memory-hungry — `--all-targets` compiles tests and benches,
  measured at 2.3–3.0G peak after a source edit, several times the whole e2e suite's
  750M. That peak is the reason it is capped; its ~20s of CPU is not.
- Capability is **probed**, not assumed from `systemd-run` being on `PATH`: it is present
  and still unusable in an ssh session with no user manager. A machine that *cannot* cap
  at all warns and runs anyway. `HERDR_E2E_REQUIRE_CAP=1` makes that case refuse too.
- Defaults are a CPU *share* and a memory *soft* limit (`CPUWeight`, `MemoryHigh`), not
  ceilings: a run on an idle machine is not slowed down, and the suite's own cargo build
  cannot be OOM-killed. `HERDR_E2E_CPU_QUOTA` and `HERDR_E2E_MEMORY_MAX` add hard ceilings
  when deliberately wanted; `HERDR_E2E_CPU_WEIGHT`, `HERDR_E2E_MEMORY_HIGH`,
  `HERDR_E2E_IO_WEIGHT` and `HERDR_E2E_NICE` tune the rest.
- `HERDR_E2E_UNCAPPED=1` runs without limits deliberately. `HERDR_E2E_DRY_RUN=1` prints
  the wrapped command and exits; it reports the forwarded environment as a count rather
  than printing it, so a dry run does not copy tokens into a terminal or log.
- The caller's environment is forwarded explicitly. A transient user unit inherits the
  *user manager's* environment, not the shell's, so without this `uv` is absent from
  `PATH` and `ZIG` arrives empty and the run dies for unrelated-looking reasons.
- **Disk I/O is not capped** and on a machine without the `io` controller delegated to the
  user slice it cannot be. Contention there is real: at a 1-minute load average of 71 the
  boxes suite went from 53s to 13m36s and two green tests failed on a 120s ssh timeout,
  looking exactly like a regression in the code under test. Check `/proc/loadavg` before
  believing a failure in either suite, and do not loop them — each `just test-boxes`
  invocation recreates all three containers, so a loop competes with itself.

### Test conventions

Unit tests live next to the code (`#[cfg(test)] mod tests`). New `AppState` or `Workspace` behavior should be testable with `AppState::test_new()` and `Workspace::test_new()` without PTYs.

Bare `cargo test` is **not** equivalent to `just test`. nextest runs a process per test; `cargo test` shares one, so `workspace::tests::generated_workspace_ids_are_short_base32_handles` fails spuriously on the shared `NEXT_WORKSPACE_ID`. Never treat that as a regression.

For broad refactors, classify the risk before editing. Treat changes as refactor-risk when they touch two or more core surfaces, persisted state, protocol/API IDs, workspace/tab/pane identity, restore/handoff, agent detection authority, or UI/input state projection. Before moving code, identify the protected behavior and add or name characterization tests. Identity/state refactors should use the test-only invariants `AppState::assert_invariants_for_test()` or `Workspace::assert_invariants_for_test()` with adversarial state from `AppState::test_with_adversarial_identity_state()` or `Workspace::test_adversarial_identity_state()`.

When testing a new Herdr build from inside an existing Herdr session, use
`cargo run -- ...` and clear inherited Herdr socket overrides so the debug
binary talks to the debug `herdr-dev` server instead of the installed stable
server:

```bash
env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH cargo run -- <command>
```

## Agent Detection Updates

Agent detection changes should use the manifest hot-reload loop. Use the project-local `herdr-throwaway-repro` skill to create a disposable named session and drive the real agent UI through Herdr's CLI/API into the target state. Read the pane with `herdr agent read <pane> --source detection --format text` and inspect matching with `herdr agent explain <pane> --json`. Update the bundled manifest in `src/detect/manifests/<agent>.toml`, copy that manifest to the local override path at `~/.config/herdr/agent-detection/<agent>.toml`, then run `herdr server reload-agent-manifests` against the session under test. Check whether an override already exists before writing one. Once the rule is correct, remove the temporary override or restore the previous one exactly so the bundled manifest remains the source of truth.

Do not add large agent-specific full-screen fixture suites for routine manifest tuning. Keep Rust tests focused on manifest parsing, rule semantics, skip-state semantics, source precedence, cache reload behavior, and update flow. Use live pane reads for agent-specific screen evidence.

## Vendored libghostty-vt

`vendor/libghostty-vt.vendor.json` records the upstream source commit currently vendored.

Local patches on top of the vendored source must be tracked in `vendor/libghostty-vt.patches.md` and stored as patch files under `vendor/patches/libghostty-vt/`. Each entry should say why the patch exists, the upstream PR/discussion, vendored base commit, touched files, verification, and the exact removal condition.

When updating libghostty-vt, check every active patch in `vendor/libghostty-vt.patches.md`. If the new upstream commit contains the fix, remove the local patch and index entry, then rerun the listed verification. If not, reapply the patch on top of the new vendored source.

`just check` runs maintenance tests that verify local libghostty-vt patch files are listed in the index and reverse-apply cleanly against the vendored tree. Do not leave a patch file untracked or an indexed patch unapplied.

## Code Conventions

- Rust: no `unwrap()` in production code. Use `tracing` for logging. Use `#[allow]` only with a comment explaining why.
- Rust platform-specific code must be compile-gated. Put OS APIs and substantial OS behavior in `src/platform/`; when platform checks are needed elsewhere, use `#[cfg(windows)]`, `#[cfg(unix)]`, or target-specific `#[cfg(...)]` on imports, fields, functions, impls, and match arms so Windows-only code does not compile into Unix builds and Unix-only code does not compile into Windows builds. Use `cfg!(...)` only for pure cross-platform policy constants whose branches both compile on every target.
- Don't add dependencies without a reason. Check whether existing dependencies cover the need first.
- Any change to API schema types requires regenerating the committed artifact:
  ```bash
  ZIG=/opt/zig0.15/zig HERDR_UPDATE_API_SCHEMA=1 cargo nextest run --locked generated_protocol_schema_artifact_is_current
  ```
- `src/protocol/wire.rs::PROTOCOL_VERSION` gates client/server compatibility. After a rebase, compare against the newest published upstream tag before assuming the current number is still free:
  ```bash
  git show $(git describe --tags --abbrev=0 upstream/master)^{commit}:src/protocol/wire.rs | grep -m1 PROTOCOL_VERSION
  ```
  This fork is at 22 against upstream's published 20, and carries four
  incompatible changes: `ClientMessage::Hello.instance_id`, the `scroll` field on
  `FrameData`, and the `Clipboard` and `TerminalInputModes` messages sent to
  terminal-stream clients. Versions must match *exactly*, so **every federated
  machine has to be updated together** — a peer left on the old number is refused
  with a version message rather than misbehaving, which is the intended failure
  but still an outage until it is upgraded.
- Adding a `ServerMessage` or `ClientMessage` variant means **appending** it.
  `bincode` tags variants by position, so inserting one renumbers every variant
  after it and every message past that point decodes as the wrong thing.
- `FrameData` has hand-written mirrors in `tests/client_mode.rs`,
  `tests/cross_area.rs` and `tests/multi_client.rs`, deliberately not importing
  the real struct so they check the wire shape rather than agree with it by
  construction. A field added to it must be added to all three, and the two frame
  digests in `src/ui/tab_surface.rs` move with it.

Use lowercase conventional commits, no emojis, no AI co-author lines.
