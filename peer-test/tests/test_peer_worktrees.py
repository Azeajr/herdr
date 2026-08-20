"""Worktree actions inside a peer view, which must run where the checkout is.

A peer view's cwd is a path on the other machine. Run here, `git worktree` usually
fails — and when the same path happens to exist locally too, it quietly succeeds
against the wrong host's repo. So every assertion below checks *both* sides: the
answer `a` gives, and the checkout `b` actually has on disk.

`HOME` is redirected into the test's own tmp dir because a peer picks its own
worktree directory (`~/.herdr/worktrees` by default) — that is the behaviour under
test, and it must not write into the real home.
"""

from __future__ import annotations

import os
import signal
import subprocess
import time

import pytest


def _git(repo, *args: str) -> None:
    subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True)


def _make_repo(path):
    """A repo with a committed file and one extra worktree already checked out."""
    path.mkdir(parents=True)
    _git(path, "init", "--quiet")
    _git(path, "config", "user.email", "peer-test@example.invalid")
    _git(path, "config", "user.name", "Peer Test")
    (path / "README.md").write_text("peer worktree test\n")
    _git(path, "add", "README.md")
    _git(path, "commit", "--quiet", "-m", "initial")
    _git(path, "worktree", "add", "--quiet", "-b", "spare", str(path.parent / "spare"), "HEAD")
    return path


@pytest.fixture
def peer_repo(lab_factory, tmp_path):
    """Two servers, a real repo checked out on `b`, and a view onto it on `a`."""
    home = tmp_path / "home"
    home.mkdir()
    repo = _make_repo(tmp_path / "repo")

    lab = lab_factory(instances="a,b", peers=("a->b",), HOME=str(home))
    lab.cli("b", "workspace", "create", "--cwd", str(repo), "--label", "repo")
    lab.wait_peer_enumerated("a", "b")
    opened = lab.cli("a", "peer", "open", f"{lab.instance_id('b')}:w1", "--peer", "b", "--focus")

    lab.repo = repo
    lab.home = home
    lab.view = opened["result"]["workspace"]["workspace_id"]
    return lab


def worktrees(result) -> dict[str, dict]:
    return {entry["branch"]: entry for entry in result["result"]["worktrees"]}


def labels(lab, instance: str) -> list[str]:
    return [ws["label"] for ws in lab.state(instance)["workspaces"]]


def _wait_stopped(lab, instance: str, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not lab.status()["instances"][instance]["running"]:
            return
        time.sleep(0.2)
    raise AssertionError(f"instance {instance} is still running")


def test_worktree_list_in_a_peer_view_reports_the_peers_repo(peer_repo):
    listed = peer_repo.cli("a", "worktree", "list", "--workspace", peer_repo.view)

    source = listed["result"]["source"]
    assert source["repo_root"] == str(peer_repo.repo)
    entries = worktrees(listed)
    # The parent checkout plus the one the fixture added; the parent's branch name is
    # whatever `git init` chose here, so it is found by not being linked.
    assert len(entries) == 2, entries
    assert "spare" in entries
    # Every workspace id that comes back has to be usable *here*: the local view for a
    # checkout this server holds, and nothing at all for one it does not.
    checked_out = next(entry for entry in entries.values() if not entry["is_linked_worktree"])
    assert checked_out["open_workspace_id"] == peer_repo.view
    # Absent, not null: an id this server cannot act on is better left unsaid than
    # reported as the peer's own, which would resolve to an unrelated workspace here.
    assert "open_workspace_id" not in entries["spare"]
    assert entries["spare"]["path"] == str(peer_repo.repo.parent / "spare")


def test_create_lands_on_the_peer_and_comes_back_as_a_view(peer_repo):
    created = peer_repo.cli(
        "a", "worktree", "create", "--workspace", peer_repo.view, "--branch", "worktree/routed"
    )

    result = created["result"]
    assert result["type"] == "worktree_created"
    # The checkout is the peer's, in the peer's own worktree directory — this server
    # never chose the path.
    checkout = result["worktree"]["path"]
    assert checkout.startswith(str(peer_repo.home)), checkout
    assert (peer_repo.home / ".herdr" / "worktrees").is_dir()
    # And what came back is a local view onto it, not a local workspace with a local pty.
    view = result["workspace"]["workspace_id"]
    assert view != peer_repo.view
    assert result["root_pane"]["peer"] == "b"
    assert result["worktree"]["open_workspace_id"] == view
    assert "worktree-routed" in labels(peer_repo, "b")


def test_remove_deletes_on_the_peer_and_closes_the_view(peer_repo):
    created = peer_repo.cli(
        "a", "worktree", "create", "--workspace", peer_repo.view, "--branch", "worktree/gone"
    )
    view = created["result"]["workspace"]["workspace_id"]
    checkout = created["result"]["worktree"]["path"]

    removed = peer_repo.cli("a", "worktree", "remove", "--workspace", view)

    assert removed["result"]["workspace_id"] == view
    assert removed["result"]["path"] == checkout
    # Gone on the machine that had it, and the view that showed it is gone here.
    assert "worktree-gone" not in labels(peer_repo, "b")
    assert view not in [ws["workspace_id"] for ws in peer_repo.state("a")["workspaces"]]


def test_the_peers_refusal_is_reported_as_the_peers(peer_repo):
    # The parent checkout is not a linked worktree, and only the peer can know that.
    refused = peer_repo.cli(
        "a", "worktree", "remove", "--workspace", peer_repo.view, expect=1
    )
    assert "not_linked_worktree" in refused.payload["stderr"]


def test_open_reuses_the_view_this_server_already_holds(peer_repo):
    first = peer_repo.cli(
        "a", "worktree", "open", "--workspace", peer_repo.view, "--branch", "spare"
    )
    assert first["result"]["already_open"] is False
    view = first["result"]["workspace"]["workspace_id"]

    second = peer_repo.cli(
        "a", "worktree", "open", "--workspace", peer_repo.view, "--branch", "spare"
    )

    # A second view onto one peer terminal would have the two reclaiming each other's
    # attach on the peer forever, so opening again has to find the first.
    assert second["result"]["already_open"] is True
    assert second["result"]["workspace"]["workspace_id"] == view
    assert len([ws for ws in peer_repo.state("a")["workspaces"] if ws["workspace_id"] == view]) == 1


def test_a_worktree_action_is_refused_rather_than_run_here_when_the_peer_is_down(peer_repo):
    # Only `b` goes away: `lab.py down` stops every instance, and `a` has to stay up to
    # be the one refusing. Killed by pid rather than by pattern, which would match this
    # process too.
    os.kill(peer_repo.status()["instances"]["b"]["pid"], signal.SIGTERM)
    _wait_stopped(peer_repo, "b")

    refused = peer_repo.cli("a", "worktree", "list", "--workspace", peer_repo.view, expect=1)

    # The failure that matters is not the wording but the absence of a local answer: a
    # list produced here would be this machine's repo wearing the peer's name.
    assert "unavailable" in refused.payload["stderr"]
    assert "worktree_list" not in refused.payload["stderr"]


def test_the_sidebar_git_menu_reaches_the_peers_repo(peer_repo):
    peer_repo.ui_open("a", "A")
    peer_repo.wait_for("A", "b:w1:p1")

    peer_repo.click("A", text="b:w1:p1", button="right")
    # Withheld before this change, because every action behind it ran local git.
    assert peer_repo.sees("A", "New worktree"), peer_repo.screen("A")

    peer_repo.click("A", text="New worktree")
    peer_repo.wait_for("A", "new worktree on b")
    # No checkout path is previewed: the peer picks it, and one derived from this
    # server's worktree directory would name a path on the wrong machine.
    assert peer_repo.sees("A", "repo on b"), peer_repo.screen("A")

    peer_repo.type_text("A", "worktree/from-ui")
    peer_repo.keys("A", "Enter")

    # The dialog closes on the peer's answer, and the view it opened is the pane the
    # peer created — which is what `b` gaining the workspace proves.
    peer_repo.wait_gone("A", "new worktree on b", timeout=30.0)
    peer_repo.wait_for("A", "b:w2:p1", timeout=30.0)
    assert "worktree-from-ui" in labels(peer_repo, "b")
