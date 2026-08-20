<p align="center">
  <img src="assets/logo.png" alt="herdr" width="100" />
</p>

# herdr — personal fork

A personal fork of [herdrdev/herdr](https://github.com/herdrdev/herdr), the
terminal runtime for coding agents. Upstream is the real project — go there for
docs, releases, and support: [herdr.dev](https://herdr.dev).

This fork exists to carry one feature that isn't upstream, and to stay current
with upstream development while doing it. It is not a distribution, accepts no
contributions, and sends nothing back upstream.

## what this adds

**Cross-machine peer federation.** One herdr server can register another as a
peer and open that peer's workspaces locally, over a Unix socket or over ssh.

- A peer's workspaces are enumerated and browsable from the sidebar, and opening
  one renders it as an ordinary local workspace whose content comes from the peer.
- The remote server owns the VT state and streams cells; the local side blits
  them. Input, resize authority, scrollback, search and agent detection all cross
  the boundary.
- Peer-backed panes reconnect on their own with backoff and say so in the pane
  while stale.
- Splitting a peer-backed pane spawns the new pane on the peer. A pane spawned
  that way is closed on the peer when its local view closes; a view onto a pane
  that already existed there is not.
- ssh peers stand up a local socket pair bridged over ssh stdio, so nothing
  downstream needs an ssh-specific path. The remote host needs a compatible
  `herdr`; interactive SSH setup can bootstrap one when it is missing, while
  physical hosts running this fork should install the same checkout explicitly.

Aggregation lives in the **server**, not the TUI — herdr's client is a thin
blitter holding no domain state, so the local server acts as a protocol client of
the remote one.

## install and update this fork

Build and install the current checkout as `~/.local/bin/herdr`:

```bash
git clone https://github.com/Azeajr/herdr.git
cd herdr
ZIG=/opt/zig0.15/zig just install
```

Set `HERDR_INSTALL_DIR` to use another binary directory. The installed binary is
marked as a source build, so `herdr update` cannot replace it with an upstream
release; pull or rebase this checkout and rerun `just install` to update it.

For SSH federation between physical hosts, install the same checkout on both
hosts. `herdr peer add <name> --ssh <destination> --yes` then registers the
remote server.

## test this fork

| Command | Coverage |
|---|---|
| `ZIG=/opt/zig0.15/zig just check` | Formatting, Rust tests, Windows lint, and maintenance tests |
| `ZIG=/opt/zig0.15/zig just test-e2e` | Local isolated servers and real TUI clients through tmux |
| `ZIG=/opt/zig0.15/zig just test-boxes` | SSH federation across disposable Docker machines, including degraded links and missing-`herdr` discovery |

The local lab and Docker topology are documented in
[`peer-test/README.md`](peer-test/README.md). The lab is local-only; Docker owns
cross-machine scenarios, and neither is an installation path.

## build

Needs Zig **0.15.2 exactly** for the vendored libghostty-vt. A newer Zig fails
with a `readFileAlloc` arity error that reads like a code bug.

```bash
ZIG=/opt/zig0.15/zig cargo build --release
ZIG=/opt/zig0.15/zig just check     # fmt, clippy, tests, Windows lint, script tests
```

## tracking upstream

`main` is `upstream/master` plus a short queue of local commits.
`git log upstream/master..main` is the whole diff against upstream.

```bash
git fetch upstream
git rebase upstream/master
ZIG=/opt/zig0.15/zig just check
```

Rebase onto `upstream/master`, never `origin/master` — this fork's mirror of
master is stale, and rebasing onto it moves you backward.

Conflicts against upstream are kept rather than automated away: a conflict on a
file this fork deliberately diverged is how upstream's change gets reviewed
before it is discarded. `rerere` is on, so a conflict already resolved once
replays itself, while genuinely new upstream content still stops and shows up.

## how this differs from upstream, beyond the feature

- `AGENTS.md` (and `CLAUDE.md`, its symlink) carries this fork's instructions.
  Upstream's maintainer workflow, contributor guardrails, release process and
  docs governance are removed — none of it applies here.
- Upstream's issue templates, release-audit and triage agent skills, pi
  extensions and dependabot config are removed. The inert remainder
  (`CONTRIBUTING`, `SPONSORS`, `MAINTAINERS`, the workflows) is left alone
  deliberately: deleting it would buy tidiness and cost a permanent rebase
  conflict carrying nothing worth reading.

## branches

- `main` — this work.
- `browser-pane` — browser pane support, parked. The only copy; not upstream, not
  merged upstream, not in `main`. Based on an older master, so it needs a rebase
  before it builds.

## license

Apache-2.0, same as upstream. See [LICENSE](LICENSE).
