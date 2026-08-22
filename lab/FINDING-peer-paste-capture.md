# Finding: peer paste capture failures — corrected analysis

Status: CORRECTED 2026-08-21. The original "client input wedge" claim in
commit c042e762 was a **harness artifact**, not a herdr bug. This file
supersedes it.

## What the deeper investigation showed

1. **The client never wedges.** A clean lab (m4) proved that after a 64 KiB
   chunked bracketed paste to a focused pane (local *or* peer), typed input
   continues to flow and execute normally. The earlier "wedge" evidence was a
   verification mistake: `lab.py ui text` types literally without pressing
   Enter, so the check for *executed* command output looked like dead input
   when the shell simply had an unfinished giant line at the prompt.

2. **The runner's peer-cell failures are real but self-inflicted.** The
   `stty raw -echo; head -c N` capture strategy leaves the pane's interactive
   zsh with a raw tty. Once one cell's reader is interrupted or interleaves
   with the next cell's `pane run` command text, subsequent cells run against
   a mangled tty: commands echo with `^M`, `zsh: command not found: raw`,
   orphaned `head` processes, and pastes land nowhere. That is exactly what
   the failing peer-cell captures show (`stty ` as captured content, empty
   files).

3. **Peer pastes themselves work.** A clean peer pane with a single
   stty-raw/head capture receives the pasted bytes byte-exact through the
   full client → a → b → pane-PTY path.

## What remains genuinely open

A robust per-cell capture method that does not corrupt the pane between
cells. Candidates:

- Run the whole matrix with each cell in its own freshly split pane, tearing
  the pane down after the cell.
- Restore the tty inside the same `pane run` command *and* verify the prompt
  is live before sending the paste (a "prompt gate" assertion).
- Have the capture program itself set raw mode on its own stdin via a tiny
  script (`python3 -c 'import tty,...'`) instead of touching the shell's tty.

## Disposition — RESOLVED 2026-08-21

Adopted fix: per-cell capture panes. Each cell splits a fresh pane off the
target workspace's base pane with `pane split --focus`, runs the capture
inside it, and tears the pane down after reading. The pane's shell tty is
never reused across cells, so the corruption mode above cannot recur.

Result: all 8 implemented bracketed-paste cells (local and peer ×
tiny/large/multibyte/ansi) pass byte-exact through the local oracle in a
single 58s run (envelope `lab-20260821T214904-aff879dc`, commit 803bddd9).

No herdr code change was ever indicated by this line of investigation.
