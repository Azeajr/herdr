"""The ssh path: key install, the stdio bridge, and what a real ssh peer leaves behind.

`peer add --socket` never builds a bridge, so nothing else in this suite exercises key
install, the bridge directories, or an `ssh://` peer history entry — and `ssh://` is the
only kind of history entry the add-peer dialog offers back as a recent.
"""

from __future__ import annotations

import shutil
import time

import pytest

#: Matches `SSH_HOST` in lab.py — the loopback address the throwaway sshd listens on.
SSH_HOST = "127.0.0.1"


@pytest.fixture
def ssh_lab(lab_factory):
    if shutil.which("sshd") is None:
        pytest.skip("no sshd binary; the ssh lab needs openssh")
    # `ssh-up` adds instance `s` itself, but it loads an existing lab, so one has to
    # exist first. `a` is that anchor and nothing else.
    lab = lab_factory(instances="a")
    lab.run("ssh-up", timeout=300)
    yield lab
    lab.run("ssh-down", expect=(0, 2), timeout=120)


@pytest.mark.timeout(600)
def test_authorized_keys_install_and_replace_rules_hold(ssh_lab):
    """Every assertion `ssh-check` makes, against a real sshd rather than a fixture."""
    result = ssh_lab.run("ssh-check", timeout=300)

    assert result["failed"] == [], result.payload
    # A silently shrinking check list would turn this into a test that asserts nothing.
    assert len(result["checks"]) == 10, result["checks"]
    assert all(check["pass"] for check in result["checks"])


@pytest.mark.timeout(900)
def test_an_ssh_peer_records_a_recent_that_survives_a_cold_start(ssh_lab):
    """The `[peer_history]` half of `477dca2d`, observed where a user would see it.

    The add-peer dialog only offers `ssh://` history entries as recents, so a socket peer
    cannot show this. Revert the fix and the dialog comes back empty after the restart.
    """
    peered = ssh_lab.run("ssh-peer", timeout=600)
    assert [peer["name"] for peer in peered["peers"]], peered.payload

    config = ssh_lab.config_text("s")
    assert "[peer_history]" in config, config
    assert "ssh://" in config, config

    # Down takes the sshd with it, so `ssh-up` is the restart, not `up`.
    ssh_lab.down()
    ssh_lab.run("ssh-up", timeout=300)

    ssh_lab.ui_open("s", "S")
    ssh_lab.click("S", text="menu")
    ssh_lab.click("S", text="add peer")
    ssh_lab.wait_for("S", "add peer")

    # A recent row is rendered as ` <name> — <target>` (`ui/dialogs.rs`). Match the whole
    # row: the peer's own sidebar header is labelled with the same destination, so
    # searching for the host alone passes even with `peer_history` empty.
    assert ssh_lab.sees("S", f"lab-ssh — ssh://{SSH_HOST}"), ssh_lab.screen("S")


@pytest.mark.timeout(600)
def test_bridge_directories_are_cleaned_up_when_the_peer_goes_away(ssh_lab):
    # Other real Herdr sessions may have their own live bridges in /tmp. Pin the
    # directories this lab creates instead of treating that shared namespace as empty.
    existing = {entry["path"] for entry in ssh_lab.run("ssh-status")["bridge_dirs"]}
    ssh_lab.run("ssh-peer", timeout=600)

    # A bridge dir exists only while the bridge is up, and `ssh-peer` returns before the
    # connection has necessarily settled — reading the list once turns this into a race.
    live = wait_for_live_bridge(ssh_lab, excluding=existing)
    assert live, "an ssh peer with no live bridge dir means the bridge never came up"

    torn_down = ssh_lab.run("ssh-down", timeout=120)

    lab_paths = {entry["path"] for entry in live}
    still_alive = {
        entry["path"] for entry in torn_down["bridge_dirs"] if entry["alive"]
    }
    assert lab_paths.isdisjoint(still_alive), torn_down.payload


def wait_for_live_bridge(
    lab, *, excluding: set[str] | None = None, timeout: float = 60.0
) -> list[dict]:
    excluded = excluding or set()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        live = [
            entry
            for entry in lab.run("ssh-status")["bridge_dirs"]
            if entry["alive"] and entry["path"] not in excluded
        ]
        if live:
            return live
        time.sleep(1.0)
    return []
