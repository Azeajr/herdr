#!/usr/bin/env -S uv run --script
# /// script
# dependencies = [
#   "click>=8.3.0",
#   "rich>=14.2.0",
# ]
# requires-python = ">=3.12"
# ///

"""
lab — an LLM-operable herdr test lab: N isolated servers on this box, peered, with real
TUI clients you can drive and read back.

Not part of the release pipeline. Successor to peer_lab.py: same servers, same SSH lab,
but every command answers with JSON, clients survive between invocations, and a failure
can be frozen into an evidence bundle another agent can read without rebuilding anything.

Quick start:
    lab.py up --lab p1 --instances a,b --peer a->b   # build, boot, wire
    lab.py state a                                    # snapshot + peers, merged
    lab.py ui open a --client A                       # a real TUI that stays alive
    lab.py ui screen A                                # what it shows now
    lab.py ui hitbox A                                # where its controls are, exactly
    lab.py effect -- ui click A --text '+'            # act, then diff every instance
    lab.py logs all --grep peer                       # one timeline across servers
    lab.py evidence tab-button --note "…"             # freeze it
    lab.py destroy                                    # servers, clients, dirs, orphans

Design notes that are load-bearing:

  * The lab root is short on purpose (/tmp/hl-<lab>). A herdr server binds
    <config>/<app_dir>/herdr.sock, and a deep root overflows sun_path:
    "local socket name length exceeds capacity of sun_path of sockaddr_un".
  * One XDG_CONFIG_HOME/XDG_STATE_HOME pair per instance, so peer config is per-instance
    and the topology mirrors separate machines. Teardown is `rm -rf`.
  * Clients are tmux sessions on a per-lab tmux server, which is what makes multi-step UI
    flows (click -> dialog -> type -> confirm) expressible at all, and lets a human watch
    with `tmux -L hl-<lab> attach -t <client>`.
  * Input goes in with `tmux send-keys -l`. `-H` (hex) splits escape sequences: herdr logs
    "flushing lone escape after input timeout" and the click never arrives.
  * Output is JSON whenever stdout is not a tty. Exit codes are part of the contract:
    0 ok, 2 usage/precondition, 3 wait timed out, 4 assertion failed.

The ssh lab (throwaway sshd + key install/replace checks) is carried over unchanged in
behaviour; see `lab.py ssh-up --help`. It exists because `peer add --socket` never builds
a bridge, so nothing else exercises key install, the ssh stdio bridges, or the
/tmp/herdr-ssh-* and /tmp/herdr-peer-* directories they clean up on drop.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import shutil
import signal
import socket as socketlib
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import click
from rich.console import Console

from _common import (
    EVIDENCE_DIR,
    REPO_ROOT,
    LabError,
    cargo_build,
    git_ref,
    resolve_zig,
    scrubbed_env,
    zig_version_warning,
)

console = Console(stderr=True)

DEFAULT_LAB = "dev"
LAB_ROOT_PREFIX = Path("/tmp")
SSH_PORT = 22222
SSH_HOST = "127.0.0.1"
SSH_PEER_NAME = "lab-ssh"

EXIT_OK = 0
EXIT_PRECONDITION = 2
EXIT_TIMEOUT = 3
EXIT_ASSERT = 4


# ---------------------------------------------------------------------------
# output
# ---------------------------------------------------------------------------


@dataclass
class Out:
    """How this invocation answers. JSON unless a human is watching."""

    json_mode: bool = True

    def emit(self, payload: dict, *, human: str | None = None) -> None:
        if self.json_mode:
            print(json.dumps(payload, default=str))
        elif human is not None:
            print(human)
        else:
            print(json.dumps(payload, indent=2, default=str))


out = Out()


def fail(message: str, *, code: int = EXIT_PRECONDITION, **details) -> None:
    payload = {"ok": False, "error": message, **details}
    if out.json_mode:
        print(json.dumps(payload, default=str))
    else:
        console.print(f"[red]error:[/] {message}")
        for key, value in details.items():
            console.print(f"  {key}: {value}")
    sys.exit(code)


# ---------------------------------------------------------------------------
# lab manifest
# ---------------------------------------------------------------------------


@dataclass
class Lab:
    """A lab is a directory that describes itself.

    Everything a later invocation — or a later agent, or `gc` after a crash — needs to
    find the topology lives in lab.json, so no state is carried in this process.
    """

    name: str
    root: Path
    data: dict = field(default_factory=dict)

    @staticmethod
    def root_for(name: str) -> Path:
        return LAB_ROOT_PREFIX / f"hl-{name}"

    @classmethod
    def create(cls, name: str, binary: Path) -> Lab:
        root = cls.root_for(name)
        root.mkdir(parents=True, exist_ok=True)
        branch, commit = git_ref()
        lab = cls(
            name=name,
            root=root,
            data={
                "lab": name,
                "root": str(root),
                "bin": str(binary),
                "app_dir": None,
                "created": now_iso(),
                "branch": branch,
                "commit": commit,
                "instances": {},
                "clients": {},
                "peers": [],
            },
        )
        lab.save()
        return lab

    @classmethod
    def load(cls, name: str) -> Lab:
        root = cls.root_for(name)
        manifest = root / "lab.json"
        if not manifest.is_file():
            raise LabError(
                f"no lab '{name}' at {root} — run `lab.py up --lab {name}` first",
                lab=name,
            )
        return cls(name=name, root=root, data=json.loads(manifest.read_text()))

    @classmethod
    def load_or_create(cls, name: str, binary: Path) -> Lab:
        try:
            lab = cls.load(name)
        except LabError:
            return cls.create(name, binary)
        lab.data["bin"] = str(binary)
        return lab

    def save(self) -> None:
        (self.root / "lab.json").write_text(json.dumps(self.data, indent=2) + "\n")

    # --- instances ---------------------------------------------------------

    @property
    def binary(self) -> Path:
        return Path(self.data["bin"])

    @property
    def app_dir(self) -> str:
        # Derived from the binary, never guessed: debug builds namespace themselves
        # under `herdr-dev`, release builds under `herdr`.
        return self.data.get("app_dir") or "herdr-dev"

    def instance(self, name: str) -> dict:
        try:
            return self.data["instances"][name]
        except KeyError:
            raise LabError(
                f"no instance '{name}' in lab '{self.name}'",
                instances=sorted(self.data["instances"]),
            ) from None

    def add_instance(self, name: str, extra_env: dict[str, str] | None = None) -> dict:
        config = self.root / name / "config"
        state = self.root / name / "state"
        config.mkdir(parents=True, exist_ok=True)
        state.mkdir(parents=True, exist_ok=True)
        herdr_dir = config / self.app_dir
        # The dump is per *instance*, not per client: this server renders the
        # TUI, so it is the process that knows where the controls are. Always on
        # in the lab — it is what makes `ui click --control` exact, and it is
        # env-gated in the shipping binary, so this is still the same binary.
        extra_env = {
            "HERDR_HITBOX_DUMP": str(self.root / name / "hitbox.json"),
            **(extra_env or {}),
        }
        entry = {
            "name": name,
            "config": str(config),
            "state": str(state),
            "herdr_dir": str(herdr_dir),
            "sock": str(herdr_dir / "herdr.sock"),
            "client_sock": str(herdr_dir / "herdr-client.sock"),
            "log": str(herdr_dir / "herdr-server.log"),
            "client_log": str(herdr_dir / "herdr-client.log"),
            "extra_env": extra_env or {},
        }
        self.data["instances"][name] = entry
        self.save()
        return entry

    def env_for(self, name: str) -> dict[str, str]:
        entry = self.instance(name)
        return scrubbed_env(
            Path(entry["config"]), Path(entry["state"]), **entry.get("extra_env", {})
        )

    def instance_id(self, name: str) -> str | None:
        path = Path(self.instance(name)["herdr_dir"]) / "instance-id"
        return path.read_text().strip() if path.is_file() else None

    # --- clients -----------------------------------------------------------

    @property
    def tmux_socket(self) -> str:
        return f"hl-{self.name}"

    def client(self, name: str) -> dict:
        try:
            return self.data["clients"][name]
        except KeyError:
            raise LabError(
                f"no client '{name}' in lab '{self.name}' — `lab.py ui open <instance>` first",
                clients=sorted(self.data["clients"]),
            ) from None

    # --- history -----------------------------------------------------------

    def record(self, argv: list[str], code: int) -> None:
        # `destroy` deletes the root out from under its own history write.
        if not self.root.is_dir():
            return
        line = json.dumps({"at": now_iso(), "argv": argv, "exit": code})
        with (self.root / "history.jsonl").open("a") as handle:
            handle.write(line + "\n")


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds")


# ---------------------------------------------------------------------------
# herdr processes
# ---------------------------------------------------------------------------


def run_herdr(lab: Lab, instance: str, args: list[str], *, timeout: float = 30.0) -> tuple[int, str, str]:
    entry = lab.instance(instance)
    result = subprocess.run(
        [str(lab.binary), *args],
        env=lab.env_for(instance),
        capture_output=True,
        text=True,
        timeout=timeout,
        cwd=str(Path(entry["config"])),
    )
    return result.returncode, result.stdout, result.stderr


def run_herdr_json(lab: Lab, instance: str, args: list[str]) -> dict:
    """Run a herdr CLI command and parse its JSON answer.

    Non-JSON output is not an error by itself — several commands are human-formatted —
    so it comes back under "raw" for the caller to look at.
    """
    code, stdout, stderr = run_herdr(lab, instance, args)
    try:
        payload = json.loads(stdout)
    except (json.JSONDecodeError, ValueError):
        return {"exit": code, "raw": stdout.strip(), "stderr": stderr.strip()}
    payload["exit"] = code
    if stderr.strip():
        payload["stderr"] = stderr.strip()
    return payload


def detect_app_dir(lab: Lab, instance: str) -> str:
    """Read the config directory herdr itself reports, instead of assuming the build kind."""
    _, stdout, _ = run_herdr(lab, instance, ["--help"])
    for line in stdout.splitlines():
        if line.startswith("Config:"):
            config_path = Path(line.split(":", 1)[1].strip())
            return config_path.parent.name
    return "herdr-dev"


def server_pid(entry: dict) -> int | None:
    """The pid of the server owning this instance's config dir, or None.

    Instances differ only by environment, which never reaches the command line, so a
    `ps` match on the binary cannot tell two instances apart. Servers are started with
    their config dir as cwd precisely so /proc can. Non-Linux hosts get None, which
    every caller treats as "unknown", not "down" — liveness comes from the socket.
    """
    proc = Path("/proc")
    if not proc.is_dir():
        return None
    config = entry["config"]
    for entry_dir in proc.iterdir():
        if not entry_dir.name.isdigit():
            continue
        try:
            if os.readlink(entry_dir / "cwd") == config:
                cmdline = (entry_dir / "cmdline").read_bytes().split(b"\0")
                if len(cmdline) > 1 and cmdline[1] == b"server":
                    return int(entry_dir.name)
        except OSError:
            continue
    return None


def wait_for_socket(path: Path, *, timeout: float = 15.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.is_socket():
            return True
        time.sleep(0.2)
    return False


def start_server(lab: Lab, instance: str) -> dict:
    entry = lab.instance(instance)
    sock = Path(entry["sock"])
    if sock.is_socket() and server_pid(entry):
        return {"instance": instance, "started": False, "pid": server_pid(entry), **entry}
    # start_new_session so the server outlives this script and the shell that ran it.
    subprocess.Popen(
        [str(lab.binary), "server"],
        env=lab.env_for(instance),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
        cwd=str(Path(entry["config"])),
    )
    if not wait_for_socket(sock):
        raise LabError(
            f"instance '{instance}' never bound its api socket",
            sock=str(sock),
            log=entry["log"],
        )
    return {"instance": instance, "started": True, "pid": server_pid(entry), **entry}


def stop_server(lab: Lab, instance: str) -> dict:
    entry = lab.instance(instance)
    if not Path(entry["config"]).is_dir():
        return {"instance": instance, "stopped": False, "reason": "no config dir"}
    code, _, _ = run_herdr(lab, instance, ["server", "stop"])
    return {"instance": instance, "stopped": code == 0}


def api_call(lab: Lab, instance: str, method: str, params: dict | None) -> dict:
    """Raw JSON-RPC against an instance's api socket.

    The CLI covers most of the surface, but not all of it — `peer.workspace.open` and
    friends are reachable only this way.
    """
    entry = lab.instance(instance)
    request = json.dumps({"id": "lab", "method": method, "params": params or {}})
    conn = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
    conn.settimeout(15)
    try:
        conn.connect(entry["sock"])
        conn.sendall(request.encode() + b"\n")
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = conn.recv(65536)
            if not chunk:
                break
            buf += chunk
    except OSError as err:
        raise LabError(f"api call failed: {err}", instance=instance, method=method) from None
    finally:
        conn.close()
    text = buf.decode(errors="replace").strip()
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return {"raw": text}


# ---------------------------------------------------------------------------
# state
# ---------------------------------------------------------------------------


def instance_state(lab: Lab, instance: str) -> dict:
    """Everything one server currently believes, in one object.

    `api snapshot` does not carry peers and `peer list` does not carry panes, so an agent
    asking "what is true right now" would otherwise need three calls and a join.
    """
    snapshot = run_herdr_json(lab, instance, ["api", "snapshot"])
    peers = run_herdr_json(lab, instance, ["peer", "list", "--json"])
    panes = run_herdr_json(lab, instance, ["pane", "list"])
    result = snapshot.get("result", {}).get("snapshot", {})
    return {
        "instance": instance,
        "instance_id": lab.instance_id(instance),
        "running": Path(lab.instance(instance)["sock"]).is_socket(),
        "workspaces": result.get("workspaces", []),
        "tabs": result.get("tabs", []),
        "panes": panes.get("result", {}).get("panes", result.get("panes", [])),
        "agents": result.get("agents", []),
        "layouts": result.get("layouts", []),
        "focused": {
            "workspace": result.get("focused_workspace_id"),
            "tab": result.get("focused_tab_id"),
            "pane": result.get("focused_pane_id"),
        },
        "peers": peers.get("result", {}).get("peers", []),
    }


def identity_sets(state: dict) -> dict[str, list[str]]:
    return {
        "workspaces": [w.get("workspace_id") for w in state.get("workspaces", [])],
        "tabs": [t.get("tab_id") for t in state.get("tabs", [])],
        "panes": [p.get("pane_id") for p in state.get("panes", [])],
    }


def pane_backing(state: dict) -> dict[str, str]:
    return {
        p.get("pane_id"): p.get("peer") or "<local pty>"
        for p in state.get("panes", [])
    }


# ---------------------------------------------------------------------------
# tmux-hosted clients
# ---------------------------------------------------------------------------


def tmux(lab: Lab, args: list[str], *, check: bool = True) -> subprocess.CompletedProcess:
    result = subprocess.run(
        ["tmux", "-L", lab.tmux_socket, *args], capture_output=True, text=True
    )
    if check and result.returncode != 0:
        raise LabError(
            f"tmux {' '.join(args[:2])} failed: {result.stderr.strip() or result.stdout.strip()}",
            tmux_socket=lab.tmux_socket,
        )
    return result


def require_tmux() -> None:
    if shutil.which("tmux") is None:
        raise LabError("tmux is not installed; the lab hosts its TUI clients in tmux")


def client_alive(lab: Lab, name: str) -> bool:
    result = tmux(lab, ["has-session", "-t", name], check=False)
    return result.returncode == 0


def hitbox_path(lab: Lab, instance: str) -> Path:
    env = lab.instance(instance).get("extra_env", {})
    return Path(env.get("HERDR_HITBOX_DUMP") or (lab.root / instance / "hitbox.json"))


def open_client(lab: Lab, instance: str, name: str, cols: int, rows: int) -> dict:
    require_tmux()
    entry = lab.instance(instance)
    if client_alive(lab, name):
        return {"client": name, "instance": instance, "opened": False, "cols": cols, "rows": rows}

    env = lab.env_for(instance)
    env["TERM"] = "xterm-256color"
    # Only the variables that matter travel into tmux; the tmux server itself inherited
    # this process's environment, so the ones herdr must not see are unset explicitly.
    prefix = ["env"]
    for name_to_drop in (
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_STARTUP_CWD",
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
    ):
        prefix += ["-u", name_to_drop]
    for key in ("XDG_CONFIG_HOME", "XDG_STATE_HOME", "TERM", *entry.get("extra_env", {})):
        prefix.append(f"{key}={env[key]}")
    command = shlex.join([*prefix, str(lab.binary)])

    tmux(
        lab,
        ["new-session", "-d", "-s", name, "-x", str(cols), "-y", str(rows), command],
    )
    # Keep the pane after the client exits so a crash still leaves a readable screen.
    tmux(lab, ["set-option", "-t", name, "-p", "remain-on-exit", "on"], check=False)
    lab.data["clients"][name] = {
        "client": name,
        "instance": instance,
        "cols": cols,
        "rows": rows,
        "hitbox": str(hitbox_path(lab, instance)),
        "opened": now_iso(),
    }
    lab.save()
    return {"client": name, "instance": instance, "opened": True, "cols": cols, "rows": rows}


def capture(lab: Lab, name: str, *, ansi: bool = False) -> list[str]:
    lab.client(name)
    if not client_alive(lab, name):
        raise LabError(f"client '{name}' is not running", client=name)
    args = ["capture-pane", "-p", "-t", name]
    if ansi:
        args.insert(2, "-e")
    result = tmux(lab, args)
    return result.stdout.split("\n")[:-1] if result.stdout.endswith("\n") else result.stdout.split("\n")


# A lab instance has a state home nobody has ever used, so its first client lands on the
# onboarding welcome. That mode returns from `handle_mouse` before any chrome hit test
# (src/app/input/mouse.rs), so every click on the sidebar or the tab bar vanishes with no
# state change and no log line saying why. Detecting it is what stops the next agent from
# reporting dead buttons.
ONBOARDING_MARKER = "this is a mouse-first terminal"
MODAL_MARKER = "esc close"


def screen_gate(lines: list[str]) -> str | None:
    """Name the input-swallowing overlay on screen, if there is one."""
    if any(ONBOARDING_MARKER in line for line in lines):
        return "onboarding"
    if any(MODAL_MARKER in line for line in lines):
        return "modal"
    return None


def live_clients(lab: Lab) -> list[str]:
    return [name for name in lab.data.get("clients", {}) if client_alive(lab, name)]


def screen_diff(before: list[str], after: list[str], *, limit: int = 12) -> dict:
    """Row-wise diff of two captures, capped so an agent gets a signal and not a screen."""
    height = max(len(before), len(after))
    rows = [
        {
            "row": row,
            "before": before[row] if row < len(before) else "",
            "after": after[row] if row < len(after) else "",
        }
        for row in range(height)
        if (before[row] if row < len(before) else "") != (after[row] if row < len(after) else "")
    ]
    return {
        "changed": bool(rows),
        "changed_row_count": len(rows),
        "changed_rows": rows[:limit],
        "truncated": len(rows) > limit,
        "gate_before": screen_gate(before),
        "gate_after": screen_gate(after),
    }


def send_literal(lab: Lab, name: str, payload: str) -> None:
    """Write bytes to the client's stdin exactly as typed.

    `-l` and not `-H`: hex mode delivers escape sequences in pieces, and herdr's input
    reader flushes the lone ESC before the rest arrives ("flushing lone escape after
    input timeout"), so mouse reports never reach the app.
    """
    tmux(lab, ["send-keys", "-t", name, "-l", "--", payload])


def parse_keys(spec: str) -> str:
    """Turn a key spec like `C-a c Enter` into the characters a terminal would send."""
    named = {
        "enter": "\r",
        "cr": "\r",
        "esc": "\x1b",
        "escape": "\x1b",
        "tab": "\t",
        "space": " ",
        "bspace": "\x7f",
        "backspace": "\x7f",
        "up": "\x1b[A",
        "down": "\x1b[B",
        "right": "\x1b[C",
        "left": "\x1b[D",
    }
    outp = ""
    for token in spec.split():
        lowered = token.lower()
        if lowered in named:
            outp += named[lowered]
        elif len(token) > 2 and lowered.startswith("c-"):
            outp += chr(ord(token[2].lower()) - ord("a") + 1)
        else:
            outp += token
    return outp


BUTTONS = {"left": 0, "middle": 1, "right": 2}


def click_at(lab: Lab, name: str, col: int, row: int, button: str) -> dict:
    """Left/middle/right click at 0-indexed (col, row), SGR-encoded.

    Press and release go in as separate writes with a gap: sent as one blob they read as
    a chord the app does not act on.
    """
    code = BUTTONS[button]
    x, y = col + 1, row + 1
    send_literal(lab, name, f"\x1b[<{code};{x};{y}M")
    time.sleep(0.12)
    send_literal(lab, name, f"\x1b[<{code};{x};{y}m")
    return {"client": name, "col": col, "row": row, "button": button}


def find_on_screen(lab: Lab, name: str, needle: str, *, max_row: int | None = None) -> list[dict]:
    matches = []
    for row, line in enumerate(capture(lab, name)):
        if max_row is not None and row > max_row:
            break
        start = 0
        while (col := line.find(needle, start)) != -1:
            matches.append({"col": col, "row": row, "line": line})
            start = col + 1
    return matches


def inspect_target_cell(lab: Lab, name: str, col: int, row: int) -> tuple[str | None, str | None]:
    """The character a coordinate click will land on, and why it looks wrong.

    Returns `(cell, hint)`. `hint` is None when the target holds something
    visible. Blank targets are the trap: clicking empty space closes an open
    menu or overlay and dispatches nothing, which is indistinguishable from a
    control that does not work.
    """
    lines = capture(lab, name)
    if row < 0 or row >= len(lines):
        return None, f"row {row} is outside the {len(lines)}-row screen"
    line = lines[row]
    if col < 0 or col >= len(line):
        return None, f"col {col} is past the end of row {row} (width {len(line)})"
    cell = line[col]
    if cell.strip():
        return cell, None
    overlay = next(
        (idx for idx, text in enumerate(lines) if "┌" in text or "╭" in text),
        None,
    )
    hint = f"col {col},row {row} is blank"
    if overlay is not None:
        hint += (
            f"; an overlay is open (its top border is on row {overlay}) and a click on"
            " blank space dismisses it without activating anything. Prefer --text"
        )
    else:
        hint += "; prefer --text so the coordinate comes from the screen"
    return cell, hint


def read_hitbox(lab: Lab, name: str) -> dict:
    """Where the controls on this client's screen are, per the server that drew it.

    The server renders the TUI and hit-tests the mouse, so it is the process
    that knows; it writes this because the instance was started with
    `HERDR_HITBOX_DUMP`. These coordinates are the ones the mouse handler
    resolves against — not a column counted off a screen capture, which is the
    guess that has already produced one false bug report.
    """
    entry = lab.client(name)
    path = Path(entry.get("hitbox") or hitbox_path(lab, entry["instance"]))
    if not path.is_file():
        raise LabError(
            f"instance '{entry['instance']}' has written no hitbox dump at {path}"
            " — a lab created before the dump existed has to be recreated,"
            " since HERDR_HITBOX_DUMP is set when the server starts",
            client=name,
            code=EXIT_ASSERT,
        )
    try:
        return json.loads(path.read_text())
    except json.JSONDecodeError as err:
        raise LabError(f"hitbox dump at {path} is not JSON: {err}", client=name) from None


def resolve_control(lab: Lab, name: str, control: str, *, timeout: float = 5.0) -> dict:
    """Find CONTROL in the client's dump, waiting for a frame that has it.

    The wait matters: the dump is rewritten only when the chrome moves, so a
    control that an action is about to create is simply not there yet.
    """
    deadline = time.time() + timeout
    seen: list[str] = []
    while True:
        dump = read_hitbox(lab, name)
        seen = [entry["name"] for entry in dump["controls"]]
        match = next((entry for entry in dump["controls"] if entry["name"] == control), None)
        if match is None:
            # A menu row is addressable by its label too, since an index shifts
            # with whatever else the menu is offering.
            match = next(
                (entry for entry in dump["controls"] if entry.get("label") == control), None
            )
        if match is not None:
            return match
        if time.time() >= deadline:
            raise LabError(
                f"no control '{control}' on client {name} after {timeout}s",
                client=name,
                controls=seen,
                mode=dump.get("mode"),
                code=EXIT_ASSERT,
            )
        time.sleep(0.2)


def pane_rect(lab: Lab, instance: str, pane_id: str) -> dict | None:
    for layout in instance_state(lab, instance).get("layouts", []):
        for pane in layout.get("panes", []):
            if pane.get("pane_id") == pane_id:
                return pane.get("rect")
    return None


# ---------------------------------------------------------------------------
# logs
# ---------------------------------------------------------------------------

TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T[\d:.]+Z?)")


def read_log(path: Path, instance: str, stream: str) -> list[dict]:
    if not path.is_file():
        return []
    entries: list[dict] = []
    for line in path.read_text(errors="replace").splitlines():
        match = TS_RE.match(line)
        if match:
            entries.append(
                {"ts": match.group(1), "instance": instance, "stream": stream, "line": line}
            )
        elif entries:
            # Continuation of a multi-line record; keep it attached to its timestamp.
            entries[-1]["line"] += "\n" + line
    return entries


def collect_logs(lab: Lab, instances: list[str]) -> list[dict]:
    entries: list[dict] = []
    for name in instances:
        entry = lab.instance(name)
        entries += read_log(Path(entry["log"]), name, "server")
        entries += read_log(Path(entry["client_log"]), name, "client")
    entries.sort(key=lambda item: item["ts"])
    return entries


# ---------------------------------------------------------------------------
# cleanup
# ---------------------------------------------------------------------------


def stop_lab_target_servers(lab: Lab) -> list[dict]:
    """Kill any herdr running out of this lab's ssh target HOME.

    The far-side herdr is spawned by sshd, not by us, so no lab pid file knows about it.
    It used to outlive both `ssh-down` and `destroy`: still running from a binary that had
    just been deleted (so its ~195 MB stayed resident on tmpfs), and recreating
    `$HOME/.config` inside a root that had already been removed.

    Matched on the absolute path prefix and killed by pid. Never by pattern: `pkill -f`
    would also match the process doing the matching.
    """
    prefix = f"{lab.root}/sshlab/target/"
    result = subprocess.run(["ps", "-eo", "pid=,args="], capture_output=True, text=True)
    killed = []
    for line in result.stdout.splitlines():
        pid_text, _, args = line.strip().partition(" ")
        if not pid_text.isdigit() or not args.startswith(prefix):
            continue
        pid = int(pid_text)
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            continue
        killed.append({"pid": pid, "cmd": args})
    for _ in range(20):
        if not any(_pid_alive(entry["pid"]) for entry in killed):
            break
        time.sleep(0.1)
    for entry in killed:
        if _pid_alive(entry["pid"]):
            try:
                os.kill(entry["pid"], signal.SIGKILL)
                entry["killed"] = True
            except OSError:
                pass
    return killed


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def tail_text(path: Path, lines: int) -> list[str]:
    """The last N lines of a log, for payloads that have to explain themselves."""
    try:
        return path.read_text(errors="replace").splitlines()[-lines:]
    except OSError:
        return []


#: What `private_runtime_dir` in `src/remote/unix.rs` makes: the prefix, the creating pid,
#: and an attempt counter, and nothing else. Anchored on purpose — see below.
BRIDGE_DIR_RE = re.compile(r"^herdr-(?:peer|ssh)-(\d+)-\d+$")


def bridge_socket_dirs() -> list[dict]:
    """Every ssh/peer bridge dir in /tmp, paired with whether its creator still exists.

    The name carries the pid of the process that made it, and the directory is removed
    when that process drops the bridge. One whose pid is gone was left behind by a server
    that died without unwinding.

    Only a directory whose *whole* name is that pattern counts. `/tmp` is shared, this
    function reaps, and the same glob catches things herdr's own Rust tests leave behind:
    a peer test that fails panics before its cleanup line and strands
    `herdr-peer-<test>-<pid>-<nanos>-client.sock`, where the second-to-last dash field is
    a nanosecond timestamp rather than a pid. Reading it as one reached `os.kill` with a
    number too large for a C int, and `OverflowError` is not an `OSError` — so it escaped,
    and one loose file in `/tmp` failed `destroy`, `status` and `reap` for every lab in
    the suite at once. The pid guard stays broad for the same reason: nothing about
    another process's filename may be the thing that fails a teardown.
    """
    found = sorted(Path("/tmp").glob("herdr-peer-*")) + sorted(Path("/tmp").glob("herdr-ssh-*"))
    dirs = []
    for path in found:
        match = BRIDGE_DIR_RE.match(path.name)
        if not match or not path.is_dir():
            continue
        try:
            os.kill(int(match.group(1)), 0)
            alive = True
        except (OSError, OverflowError, ValueError):
            alive = False
        dirs.append({"path": str(path), "alive": alive})
    return dirs


def teardown(lab: Lab, *, remove: bool, reap: bool) -> dict:
    stopped = [stop_server(lab, name) for name in list(lab.data["instances"])]
    target_servers = []
    if lab.data.get("ssh"):
        stop_lab_sshd(lab)
        target_servers = stop_lab_target_servers(lab)
    tmux_killed = False
    if shutil.which("tmux"):
        tmux_killed = tmux(lab, ["kill-server"], check=False).returncode == 0
        # `kill-server` leaves the socket file behind. It is harmless — `tmux -L hl-<lab>
        # ls` reports "no server running" — but one accumulates per lab, and a stale
        # socket file reads like evidence of a live lab when it is not.
        if remove:
            socket_dir = Path(os.environ.get("TMUX_TMPDIR", f"/tmp/tmux-{os.getuid()}"))
            (socket_dir / lab.tmux_socket).unlink(missing_ok=True)

    orphans = []
    for entry in bridge_socket_dirs():
        if entry["alive"]:
            continue
        if reap:
            shutil.rmtree(entry["path"], ignore_errors=True)
            entry["reaped"] = True
        orphans.append(entry)

    removed = False
    if remove and lab.root.is_dir():
        shutil.rmtree(lab.root, ignore_errors=True)
        if lab.root.is_dir():
            # The ssh lab's target herdr is spawned by sshd, not by us, so it can outlive
            # the sshd by a moment and recreate $HOME/.config under the lab root while
            # the first pass is still walking it. One retry is enough, and reporting the
            # real answer matters more than the retry: `root_removed` used to be `True`
            # whether or not the directory was still there.
            time.sleep(0.5)
            shutil.rmtree(lab.root, ignore_errors=True)
        removed = not lab.root.is_dir()
    return {
        "lab": lab.name,
        "stopped": stopped,
        "ssh_target_servers": target_servers,
        "tmux_killed": tmux_killed,
        "bridge_dirs": orphans,
        "root_removed": removed,
    }


# ---------------------------------------------------------------------------
# ssh lab
#
# Carried over from peer_lab.py with its paths moved under the lab root. The trick is
# unchanged: an unprivileged sshd whose AuthorizedKeysFile points into the lab, a
# ForceCommand that exports HOME=<lab target> so the file herdr writes is the file sshd
# authenticates against, and an `ssh` shim that offers the bootstrap identity only when
# BatchMode is absent — standing in for a human typing a password, so herdr's batch
# preflight fails exactly as it would against a password-only target and the install path
# actually runs.
# ---------------------------------------------------------------------------


def ssh_paths(lab: Lab) -> dict[str, Path]:
    root = lab.root / "sshlab"
    client_home = root / "client"
    target_home = root / "target"
    return {
        "root": root,
        "target_home": target_home,
        "authorized_keys": target_home / ".ssh" / "authorized_keys",
        "client_home": client_home,
        "bootstrap_key": client_home / ".ssh" / "bootstrap",
        "bin": root / "bin",
        "host_key": root / "host_ed25519",
        "config": root / "sshd_config",
        "pid_file": root / "sshd.pid",
        "log": root / "sshd.log",
    }


def write_ssh_lab_files(lab: Lab) -> None:
    paths = ssh_paths(lab)
    for directory in (
        paths["target_home"] / ".ssh",
        paths["client_home"] / ".ssh",
        paths["bin"],
    ):
        directory.mkdir(parents=True, exist_ok=True)

    for key, comment in ((paths["host_key"], "herdr-lab-host"), (paths["bootstrap_key"], "herdr-lab-bootstrap")):
        if not key.is_file():
            subprocess.run(
                ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key), "-C", comment],
                check=True,
            )

    bootstrap_pub = paths["bootstrap_key"].with_suffix(".pub").read_text().strip()
    authorized = paths["authorized_keys"]
    existing = authorized.read_text().splitlines() if authorized.is_file() else []
    if bootstrap_pub not in existing:
        authorized.write_text("\n".join([*existing, bootstrap_pub]).strip() + "\n")
    authorized.chmod(0o600)

    paths["config"].write_text(
        f"""Port {SSH_PORT}
ListenAddress {SSH_HOST}
HostKey {paths["host_key"]}
PidFile {paths["pid_file"]}
AuthorizedKeysFile {paths["authorized_keys"]}
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
UsePAM no
StrictModes no
ForceCommand HOME={paths["target_home"]} exec /bin/sh -c "$SSH_ORIGINAL_COMMAND"
"""
    )

    shim = paths["bin"] / "ssh"
    shim.write_text(
        f"""#!/bin/sh
# Stands in for a human typing their password.
#
# herdr probes a peer with BatchMode=yes because its server has no terminal to answer a
# prompt from, and only installs a key when that probe fails. Offering the bootstrap
# identity on interactive connections only reproduces exactly that: the probe fails, the
# install connection gets in.
CLIENT_HOME="{paths["client_home"]}"
batch=no
for arg in "$@"; do
  case "$arg" in BatchMode=yes) batch=yes ;; esac
done
set -- -p {SSH_PORT} -o StrictHostKeyChecking=no \\
       -o UserKnownHostsFile="$CLIENT_HOME/.ssh/known_hosts" "$@"
if [ "$batch" = no ]; then
  set -- -i "$CLIENT_HOME/.ssh/bootstrap" "$@"
fi
exec /usr/bin/ssh "$@"
"""
    )
    shim.chmod(0o755)


def lab_sshd_pid(lab: Lab) -> int | None:
    pid_file = ssh_paths(lab)["pid_file"]
    if not pid_file.is_file():
        return None
    try:
        pid = int(pid_file.read_text().strip())
        os.kill(pid, 0)
    except (ValueError, OSError):
        return None
    return pid


def start_lab_sshd(lab: Lab) -> dict:
    if pid := lab_sshd_pid(lab):
        return {"sshd": "already running", "pid": pid}
    paths = ssh_paths(lab)
    sshd = shutil.which("sshd") or "/usr/bin/sshd"
    if not Path(sshd).is_file():
        raise LabError("no sshd binary found; install openssh")
    result = subprocess.run([sshd, "-f", str(paths["config"]), "-E", str(paths["log"])])
    if result.returncode != 0:
        raise LabError("lab sshd failed to start", log=str(paths["log"]))
    for _ in range(50):
        if pid := lab_sshd_pid(lab):
            return {"sshd": "up", "pid": pid, "host": SSH_HOST, "port": SSH_PORT}
        time.sleep(0.1)
    raise LabError("lab sshd never wrote its pid file", pid_file=str(paths["pid_file"]))


def stop_lab_sshd(lab: Lab) -> dict:
    pid = lab_sshd_pid(lab)
    if pid is None:
        return {"sshd": "not running"}
    os.kill(pid, signal.SIGTERM)
    for _ in range(50):
        if lab_sshd_pid(lab) is None:
            return {"sshd": "stopped", "pid": pid}
        time.sleep(0.1)
    return {"sshd": "did not exit", "pid": pid}


def ssh_instance_env(lab: Lab) -> dict[str, str]:
    """Instance S's extra environment: lab `$HOME`, shimmed `ssh`, local far side.

    HOME matters only to ssh here — herdr derives its own directories from XDG — so
    pointing it at the lab keeps known_hosts and any ssh config out of the real home.
    """
    paths = ssh_paths(lab)
    return {
        "HOME": str(paths["client_home"]),
        "PATH": f"{paths['bin']}{os.pathsep}{os.environ.get('PATH', '')}",
        # Same machine, so the far side runs the very binary under test.
        "HERDR_REMOTE_BINARY": str(lab.binary),
    }


def run_herdr_on_tty(lab: Lab, args: list[str], *, timeout: float = 60.0) -> int:
    """Run a herdr CLI command against instance S with a terminal attached.

    `peer setup-ssh` installs a key only when stdin is a terminal, on the grounds that ssh
    would otherwise have nowhere to prompt. `--yes` covers the key-install confirmation
    and nothing else: setup then asks whether to install a matching herdr on the far side,
    and that prompt reads from the terminal too, so the answers are fed in up front.
    Output goes to the log rather than a pipe, so nothing waits on the ssh master that
    ControlPersist leaves running with the same stdout.
    """
    import pty

    log_path = ssh_paths(lab)["log"]
    primary, secondary = pty.openpty()
    try:
        with log_path.open("a") as log:
            process = subprocess.Popen(
                [str(lab.binary), *args],
                env=lab.env_for("s"),
                stdin=secondary,
                stdout=log,
                stderr=subprocess.STDOUT,
            )
        os.close(secondary)
        secondary = -1
        os.write(primary, b"y\ny\ny\n")
        try:
            return process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            return -1
    finally:
        os.close(primary)
        if secondary != -1:
            os.close(secondary)


def read_authorized_keys(lab: Lab) -> list[dict]:
    path = ssh_paths(lab)["authorized_keys"]
    if not path.is_file():
        return []
    entries = []
    for line in path.read_text().splitlines():
        fields = line.split()
        if len(fields) < 2:
            continue
        entries.append({"blob": fields[1], "comment": " ".join(fields[2:])})
    return entries


def peer_key_path(lab: Lab) -> Path:
    return Path(lab.instance("s")["herdr_dir"]) / "peer_id_ed25519"


def herdr_key_comment(lab: Lab) -> str:
    """The comment herdr stamped on the key it generated, read back from disk."""
    public = peer_key_path(lab).with_suffix(".pub")
    return " ".join(public.read_text().split()[2:]) if public.is_file() else ""


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


class LabGroup(click.Group):
    """Turns LabError into the JSON error shape and the documented exit codes."""

    def invoke(self, ctx: click.Context):
        try:
            return super().invoke(ctx)
        except LabError as err:
            fail(str(err), code=err.code, **err.details)


@click.group(cls=LabGroup, context_settings={"help_option_names": ["-h", "--help"]})
@click.option("--lab", "lab_name", default=DEFAULT_LAB, help="Lab name (root: /tmp/hl-<name>).")
@click.option("--json/--no-json", "json_mode", default=None, help="Force JSON output (default: on unless stdout is a tty).")
@click.pass_context
def cli(
    ctx: click.Context,
    lab_name: str,
    json_mode: bool | None,
) -> None:
    """N isolated herdr servers, real TUI clients, one timeline, JSON answers."""
    out.json_mode = (not sys.stdout.isatty()) if json_mode is None else json_mode
    ctx.obj = {"lab": lab_name, "argv": sys.argv[1:]}


def open_lab(ctx: click.Context) -> Lab:
    lab = Lab.load(ctx.obj["lab"])
    ctx.call_on_close(lambda: lab.record(ctx.obj["argv"], 0))
    return lab


@cli.command()
@click.option("--instances", default="a,b", help="Comma-separated instance names.")
@click.option("--peer", "peers", multiple=True, help="Peer wiring, e.g. `a->b`. Repeatable.")
@click.option("--build/--no-build", default=True, help="Build the branch first.")
@click.option("--bin", "binary", default=None, help="Use this herdr binary instead of building.")
@click.option(
    "--env",
    "extra_env",
    multiple=True,
    help="KEY=VALUE for every instance's server and client, e.g. --env HERDR_LOG=herdr=debug. Repeatable.",
)
@click.pass_context
def up(
    ctx: click.Context,
    instances: str,
    peers: tuple[str, ...],
    build: bool,
    binary: str | None,
    extra_env: tuple[str, ...],
) -> None:
    """Create the lab, boot instances, wire peers."""
    require_tmux()
    warnings = []
    if binary:
        path = Path(binary).resolve()
        if not path.is_file():
            raise LabError(f"no herdr binary at {path}")
    elif build:
        zig = resolve_zig()
        if warning := zig_version_warning(zig):
            warnings.append(warning)
        path = cargo_build(zig=zig, quiet=out.json_mode)
    else:
        path = REPO_ROOT / "target" / "debug" / "herdr"
        if not path.is_file():
            raise LabError(f"no debug build at {path}; drop --no-build or pass --bin")

    lab = Lab.load_or_create(ctx.obj["lab"], path)
    names = [name.strip() for name in instances.split(",") if name.strip()]

    env_overrides = {}
    for pair in extra_env:
        key, sep, value = pair.partition("=")
        if not sep:
            raise LabError(f"bad --env '{pair}'; use KEY=VALUE")
        env_overrides[key] = value

    started = []
    for name in names:
        if name not in lab.data["instances"]:
            lab.add_instance(name, env_overrides)
        if lab.data.get("app_dir") is None:
            lab.data["app_dir"] = detect_app_dir(lab, name)
            # Re-derive the paths now that the app dir is known.
            lab.data["instances"].pop(name)
            lab.add_instance(name, env_overrides)
        if env_overrides:
            lab.data["instances"][name]["extra_env"].update(env_overrides)
        started.append(start_server(lab, name))
        lab.data["instances"][name]["instance_id"] = lab.instance_id(name)
    lab.save()

    wired = []
    for spec in peers:
        source, _, target = spec.partition("->")
        source, target = source.strip(), target.strip()
        if not source or not target:
            raise LabError(f"bad --peer spec '{spec}'; use `a->b`")
        wired.append(connect_peer(lab, source, target))

    lab.record(ctx.obj["argv"], 0)
    out.emit(
        {
            "ok": True,
            "lab": lab.name,
            "root": str(lab.root),
            "bin": str(lab.binary),
            "app_dir": lab.app_dir,
            "instances": {entry["instance"]: entry for entry in started},
            "peers": wired,
            "warnings": warnings,
        },
        human=f"lab {lab.name} up at {lab.root} — instances {', '.join(names)}",
    )


def connect_peer(lab: Lab, source: str, target: str, *, name: str | None = None) -> dict:
    peer_name = name or target
    target_sock = lab.instance(target)["sock"]
    listing = run_herdr_json(lab, source, ["peer", "list", "--json"])
    existing = {peer.get("name") for peer in listing.get("result", {}).get("peers", [])}
    if peer_name not in existing:
        code, stdout, stderr = run_herdr(
            lab, source, ["peer", "add", peer_name, "--socket", target_sock, "--yes"], timeout=60
        )
        if code != 0:
            raise LabError(
                f"peer add {source}->{target} failed",
                stdout=stdout.strip(),
                stderr=stderr.strip(),
            )
    record = {
        "from": source,
        "to": target,
        "name": peer_name,
        "transport": "socket",
        "target_sock": target_sock,
    }
    lab.data["peers"] = [
        peer for peer in lab.data["peers"] if not (peer["from"] == source and peer["name"] == peer_name)
    ] + [record]
    lab.save()
    time.sleep(1.0)
    listing = run_herdr_json(lab, source, ["peer", "list", "--json"])
    for peer in listing.get("result", {}).get("peers", []):
        if peer.get("name") == peer_name:
            record["connection"] = peer.get("connection")
            record["target_instance_id"] = peer.get("instance_id")
    return record


@cli.command()
@click.pass_context
def status(ctx: click.Context) -> None:
    """Instances, clients, peers, and whether each is actually up."""
    lab = open_lab(ctx)
    instances = {}
    for name, entry in lab.data["instances"].items():
        pid = server_pid(entry)
        instances[name] = {
            "running": Path(entry["sock"]).is_socket() and pid is not None,
            "pid": pid,
            "sock": entry["sock"],
            "log": entry["log"],
            "instance_id": lab.instance_id(name),
        }
    clients = {
        name: {**entry, "running": client_alive(lab, name)}
        for name, entry in lab.data["clients"].items()
    }
    out.emit(
        {
            "ok": True,
            "lab": lab.name,
            "host": socketlib.gethostname(),
            "root": str(lab.root),
            "instances": instances,
            "clients": clients,
            "peers": lab.data["peers"],
            "sshd": lab_sshd_pid(lab),
            "bridge_dirs": bridge_socket_dirs(),
        },
        human="\n".join(
            [f"lab {lab.name} at {lab.root}"]
            + [f"  {name}: {'up' if info['running'] else 'down'} pid={info['pid']}" for name, info in instances.items()]
            + [f"  client {name}: {'up' if info['running'] else 'down'} -> {info['instance']}" for name, info in clients.items()]
        ),
    )


@cli.command(context_settings={"ignore_unknown_options": True})
@click.argument("instance")
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
@click.pass_context
def cli_cmd(ctx: click.Context, instance: str, args: tuple[str, ...]) -> None:
    """Run a herdr CLI command against INSTANCE: `lab.py cli a -- pane list`."""
    lab = open_lab(ctx)
    payload = run_herdr_json(lab, instance, list(args))
    out.emit({"ok": payload.get("exit", 0) == 0, "instance": instance, **payload})
    if payload.get("exit", 0) != 0:
        sys.exit(payload["exit"])


cli.add_command(cli_cmd, name="cli")


@cli.command("api")
@click.argument("instance")
@click.argument("method")
@click.option("--params", default=None, help="JSON object of params.")
@click.pass_context
def api_cmd(ctx: click.Context, instance: str, method: str, params: str | None) -> None:
    """Raw JSON-RPC against an instance's api socket, for methods the CLI lacks."""
    lab = open_lab(ctx)
    parsed = json.loads(params) if params else None
    out.emit({"ok": True, "instance": instance, "response": api_call(lab, instance, method, parsed)})


@cli.command("state")
@click.argument("instance")
@click.pass_context
def state_cmd(ctx: click.Context, instance: str) -> None:
    """Everything one instance believes: workspaces, tabs, panes, layouts, peers."""
    lab = open_lab(ctx)
    out.emit({"ok": True, **instance_state(lab, instance)})


@cli.group()
def peer() -> None:
    """Peer wiring."""


@peer.command("connect")
@click.argument("source")
@click.argument("target")
@click.option("--name", default=None, help="Peer name on the source (default: target instance name).")
@click.pass_context
def peer_connect(ctx: click.Context, source: str, target: str, name: str | None) -> None:
    """Wire SOURCE -> TARGET over TARGET's api socket."""
    lab = open_lab(ctx)
    out.emit({"ok": True, "peer": connect_peer(lab, source, target, name=name)})


# --- ui --------------------------------------------------------------------


@cli.group()
def ui() -> None:
    """Drive and read a real TUI client. Clients live in tmux and outlive each command."""


@ui.command("open")
@click.argument("instance")
@click.option("--client", "name", default=None, help="Client name (default: the instance name, uppercased).")
@click.option("--cols", default=120)
@click.option("--rows", default=40)
@click.option("--settle", default=3.0, help="Seconds to wait for the first render.")
@click.pass_context
def ui_open(ctx: click.Context, instance: str, name: str | None, cols: int, rows: int, settle: float) -> None:
    """Attach a persistent TUI client to INSTANCE."""
    lab = open_lab(ctx)
    client_name = name or instance.upper()
    result = open_client(lab, instance, client_name, cols, rows)
    if result["opened"]:
        time.sleep(settle)
    gate = screen_gate(capture(lab, client_name))
    payload = {"ok": True, **result, "gate": gate, "attach": f"tmux -L {lab.tmux_socket} attach -t {client_name}"}
    if gate == "onboarding":
        payload["hint"] = (
            f"onboarding is up and swallows every chrome click; run `ui onboard {client_name}` first"
        )
    out.emit(payload)


@ui.command("onboard")
@click.argument("name")
@click.option("--settle", default=1.5)
@click.pass_context
def ui_onboard(ctx: click.Context, name: str, settle: float) -> None:
    """Clear the onboarding welcome so chrome clicks reach the app.

    Enter leaves the welcome, but it lands on the integrations settings screen where a
    second Enter installs agent integrations into the real `$HOME` — the lab isolates XDG,
    not that. So this presses Enter exactly once and then Escape, never Enter twice.
    """
    lab = open_lab(ctx)
    lab.client(name)
    steps = []
    if screen_gate(capture(lab, name)) == "onboarding":
        send_literal(lab, name, parse_keys("Enter"))
        time.sleep(settle)
        steps.append("Enter")
    if screen_gate(capture(lab, name)) == "modal":
        send_literal(lab, name, parse_keys("Escape"))
        time.sleep(settle)
        steps.append("Escape")
    gate = screen_gate(capture(lab, name))
    if gate is not None:
        raise LabError(
            f"client '{name}' still shows a {gate} overlay after {steps}",
            client=name,
            gate=gate,
            code=EXIT_ASSERT,
        )
    out.emit({"ok": True, "client": name, "keys": steps, "gate": None})


@ui.command("close")
@click.argument("name")
@click.pass_context
def ui_close(ctx: click.Context, name: str) -> None:
    """Kill a client (its server keeps running)."""
    lab = open_lab(ctx)
    lab.client(name)
    tmux(lab, ["kill-session", "-t", name], check=False)
    lab.data["clients"].pop(name, None)
    lab.save()
    out.emit({"ok": True, "client": name, "closed": True})


@ui.command("screen")
@click.argument("name")
@click.option("--format", "fmt", type=click.Choice(["text", "ansi"]), default="text")
@click.option("--rows", "row_limit", default=None, type=int, help="Only the first N rows.")
@click.pass_context
def ui_screen(ctx: click.Context, name: str, fmt: str, row_limit: int | None) -> None:
    """Print what the client shows right now. Row index == click row."""
    lab = open_lab(ctx)
    cols = lab.client(name).get("cols")
    lines = capture(lab, name, ansi=fmt == "ansi")
    gate = screen_gate(lines)
    if row_limit is not None:
        lines = lines[:row_limit]
    out.emit(
        {
            "ok": True,
            "client": name,
            "format": fmt,
            "cols": cols,
            "rows": len(lines),
            "gate": gate,
            "lines": lines,
        },
        human="\n".join(f"{row:3} |{line}" for row, line in enumerate(lines) if line.strip()),
    )


@ui.command("find")
@click.argument("name")
@click.argument("text")
@click.option("--max-row", default=None, type=int, help="Stop after this row.")
@click.pass_context
def ui_find(ctx: click.Context, name: str, text: str, max_row: int | None) -> None:
    """Locate TEXT on the client's screen; answers with click coordinates."""
    lab = open_lab(ctx)
    matches = find_on_screen(lab, name, text, max_row=max_row)
    out.emit({"ok": bool(matches), "client": name, "text": text, "matches": matches})


@ui.command("hitbox")
@click.argument("name")
@click.option("--control", default=None, help="Resolve one control instead of listing them all.")
@click.option("--timeout", default=5.0, help="How long to wait for --control to appear.")
@click.pass_context
def ui_hitbox(ctx: click.Context, name: str, control: str | None, timeout: float) -> None:
    """Where herdr says this client's controls are. Exact, unlike a counted column."""
    lab = open_lab(ctx)
    if control is not None:
        match = resolve_control(lab, name, control, timeout=timeout)
        out.emit({"ok": True, "client": name, "control": match})
        return
    dump = read_hitbox(lab, name)
    out.emit(
        {"ok": True, "client": name, **dump},
        human="\n".join(
            f"{entry['name']:<24} click {entry['click']['col']:>3},{entry['click']['row']:<3}"
            f" rect {entry['rect']['x']},{entry['rect']['y']}"
            f" {entry['rect']['width']}x{entry['rect']['height']}"
            + (f"  {entry['label']}" if entry.get("label") else "")
            for entry in dump["controls"]
        ),
    )


@ui.command("keys")
@click.argument("name")
@click.argument("spec")
@click.option("--settle", default=1.0)
@click.pass_context
def ui_keys(ctx: click.Context, name: str, spec: str, settle: float) -> None:
    """Press keys, e.g. `ui keys A 'C-b n'` or `ui keys A Enter`."""
    lab = open_lab(ctx)
    lab.client(name)
    send_literal(lab, name, parse_keys(spec))
    time.sleep(settle)
    out.emit({"ok": True, "client": name, "keys": spec})


@ui.command("text")
@click.argument("name")
@click.argument("text")
@click.option("--settle", default=0.5)
@click.pass_context
def ui_text(ctx: click.Context, name: str, text: str, settle: float) -> None:
    """Type literal text (no key-name parsing)."""
    lab = open_lab(ctx)
    lab.client(name)
    send_literal(lab, name, text)
    time.sleep(settle)
    out.emit({"ok": True, "client": name, "text": text})


@ui.command("click")
@click.argument("name")
@click.option("--col", type=int, default=None)
@click.option("--row", type=int, default=None)
@click.option("--text", "text", default=None, help="Click the first cell of this screen text.")
@click.option("--pane", "pane_id", default=None, help="Click the centre of this pane's rect, from the API.")
@click.option(
    "--control",
    default=None,
    help="Click a control from this client's hitbox dump, by name or menu label.",
)
@click.option("--index", default=0, help="Which --text match to use.")
@click.option("--button", type=click.Choice(list(BUTTONS)), default="left")
@click.option("--settle", default=1.5)
@click.option("--timeout", default=5.0, help="How long to wait for --control to appear.")
@click.option(
    "--require-hit",
    is_flag=True,
    help="Exit 4 instead of warning when --col/--row lands on a blank cell.",
)
@click.pass_context
def ui_click(
    ctx, name, col, row, text, pane_id, control, index, button, settle, timeout, require_hit
) -> None:
    """Click at coordinates, at screen text, at a pane's rect, or at a named control."""
    lab = open_lab(ctx)
    client = lab.client(name)
    source = "coords"
    resolved = None
    if control is not None:
        # The most exact of the four: these coordinates come from herdr's own hit
        # rectangles, so they need no blank-cell check — a menu row is mostly
        # blank and still entirely clickable.
        resolved = resolve_control(lab, name, control, timeout=timeout)
        col, row, source = resolved["click"]["col"], resolved["click"]["row"], "control"
    elif text is not None:
        matches = find_on_screen(lab, name, text)
        if len(matches) <= index:
            raise LabError(f"'{text}' not found on client {name}", matches=matches, code=EXIT_ASSERT)
        col, row, source = matches[index]["col"], matches[index]["row"], "text"
    elif pane_id is not None:
        rect = pane_rect(lab, client["instance"], pane_id)
        if rect is None:
            raise LabError(f"no rect for pane {pane_id}", code=EXIT_ASSERT)
        col = rect["x"] + rect["width"] // 2
        row = rect["y"] + rect["height"] // 2
        source = "pane"
    if col is None or row is None:
        raise LabError("give --col/--row, --text, --pane, or --control")
    warnings = []
    cell = None
    if source == "coords":
        # A hand-counted column is a guess, and a click on blank space silently
        # dismisses an open menu instead of activating anything — which reads
        # exactly like a dead button. Say so rather than let it look like a bug.
        cell, hint = inspect_target_cell(lab, name, col, row)
        if hint is not None:
            if require_hit:
                raise LabError(hint, col=col, row=row, cell=cell, code=EXIT_ASSERT)
            warnings.append(hint)
    result = click_at(lab, name, col, row, button)
    time.sleep(settle)
    payload = {"ok": True, "source": source, **result}
    if source == "coords":
        payload["cell"] = cell
    if resolved is not None:
        payload["control"] = resolved
    if warnings:
        payload["warnings"] = warnings
    out.emit(payload)


@ui.command("wait")
@click.argument("name")
@click.option("--contains", "needle", required=True)
@click.option("--gone", is_flag=True, help="Wait for it to disappear instead.")
@click.option("--timeout", default=15.0)
@click.pass_context
def ui_wait(ctx: click.Context, name: str, needle: str, gone: bool, timeout: float) -> None:
    """Poll the client's screen until TEXT appears (or disappears). Exit 3 on timeout."""
    lab = open_lab(ctx)
    deadline = time.time() + timeout
    while time.time() < deadline:
        matches = find_on_screen(lab, name, needle)
        if bool(matches) != gone:
            out.emit(
                {
                    "ok": True,
                    "client": name,
                    "contains": needle,
                    "gone": gone,
                    "waited": round(timeout - (deadline - time.time()), 2),
                    "matches": matches,
                }
            )
            return
        time.sleep(0.25)
    fail(
        f"timed out after {timeout}s waiting for '{needle}' to {'disappear' if gone else 'appear'}",
        code=EXIT_TIMEOUT,
        client=name,
        screen=capture(lab, name),
    )


# --- logs ------------------------------------------------------------------


@cli.command("logs")
@click.argument("which", default="all")
@click.option("--grep", "pattern", default=None, help="Regex over the whole line.")
@click.option("--since", default=None, help="ISO timestamp lower bound.")
@click.option("--tail", default=50, help="Keep only the last N matching records (0 = all).")
@click.option("--stream", type=click.Choice(["server", "client", "both"]), default="both")
@click.pass_context
def logs_cmd(ctx: click.Context, which: str, pattern: str | None, since: str | None, tail: int, stream: str) -> None:
    """One time-ordered timeline across every instance's server and client logs."""
    lab = open_lab(ctx)
    names = list(lab.data["instances"]) if which == "all" else [which]
    entries = collect_logs(lab, names)
    if stream != "both":
        entries = [entry for entry in entries if entry["stream"] == stream]
    if since:
        entries = [entry for entry in entries if entry["ts"] >= since]
    if pattern:
        regex = re.compile(pattern)
        entries = [entry for entry in entries if regex.search(entry["line"])]
    if tail:
        entries = entries[-tail:]
    out.emit(
        {"ok": True, "count": len(entries), "entries": entries},
        human="\n".join(f"{entry['instance']}/{entry['stream'][0]} {entry['line']}" for entry in entries),
    )


# --- effect ----------------------------------------------------------------


@cli.command(context_settings={"ignore_unknown_options": True})
@click.option("--no-screens", is_flag=True, help="Diff server state only; skip client screens.")
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
@click.pass_context
def effect(ctx: click.Context, no_screens: bool, args: tuple[str, ...]) -> None:
    """Run a lab.py subcommand and diff every instance's state around it.

    `lab.py effect -- ui click A --text '+'` answers with what each server actually did,
    which is the whole point: a UI action that should route to a peer and instead lands
    locally looks identical on screen.

    Client screens are diffed too. Plenty of real UI actions change no server state at all
    because they open a modal first — the tab-bar `+` opens a name prompt, since
    `ui.prompt_new_tab_name` defaults on — and a state-only diff cannot tell that apart
    from a click that never landed.
    """
    lab = open_lab(ctx)
    if not args:
        raise LabError("nothing to run; use `lab.py effect -- <subcommand> …`")

    names = list(lab.data["instances"])
    clients = [] if no_screens else live_clients(lab)
    before = {name: instance_state(lab, name) for name in names}
    screens_before = {name: capture(lab, name) for name in clients}
    action = subprocess.run(
        [sys.argv[0], "--lab", lab.name, "--json", *args],
        capture_output=True,
        text=True,
    )
    try:
        action_result = json.loads(action.stdout)
    except json.JSONDecodeError:
        action_result = {"raw": action.stdout.strip(), "stderr": action.stderr.strip()}
    after = {name: instance_state(lab, name) for name in names}
    screens = {
        name: screen_diff(screens_before[name], capture(lab, name)) for name in clients
    }

    diff = {}
    for name in names:
        old, new = identity_sets(before[name]), identity_sets(after[name])
        diff[name] = {
            kind: {
                "added": [item for item in new[kind] if item not in old[kind]],
                "removed": [item for item in old[kind] if item not in new[kind]],
            }
            for kind in old
        }
        old_backing, new_backing = pane_backing(before[name]), pane_backing(after[name])
        changed = {
            pane: {"before": old_backing.get(pane), "after": new_backing[pane]}
            for pane in new_backing
            if pane in old_backing and old_backing[pane] != new_backing[pane]
        }
        if changed:
            diff[name]["pane_backing_changed"] = changed

    verdicts = []
    for name in names:
        gained = diff[name]["tabs"]["added"] or diff[name]["panes"]["added"]
        peer_backed = any(
            backing != "<local pty>" for backing in pane_backing(before[name]).values()
        )
        others_gained = any(
            diff[other]["tabs"]["added"] or diff[other]["panes"]["added"]
            for other in names
            if other != name
        )
        if gained and peer_backed and not others_gained:
            verdicts.append(
                f"{name} gained {gained} while every other instance gained nothing, and {name} "
                "has peer-backed panes — if this action targeted a peer-backed workspace, it stayed local"
            )

    state_changed = any(
        diff[name][kind][side]
        for name in names
        for kind in ("workspaces", "tabs", "panes")
        for side in ("added", "removed")
    )
    for name, delta in screens.items():
        if delta["gate_after"] == "onboarding":
            verdicts.append(
                f"client {name} is on the onboarding welcome, which returns before every chrome "
                f"hit test — clicks there do nothing by design; run `ui onboard {name}` first"
            )
        elif delta["changed"] and not state_changed:
            verdicts.append(
                f"client {name}'s screen changed but no instance changed state — the action most "
                f"likely opened a modal (a name prompt, a menu). Read `ui screen {name}` and "
                "confirm it before concluding the action did nothing"
            )
    gated = any(delta["gate_after"] == "onboarding" for delta in screens.values())
    if clients and not gated and not state_changed and not any(d["changed"] for d in screens.values()):
        verdicts.append(
            "no instance changed state and no client screen changed — this action really was a no-op"
        )

    out.emit(
        {
            "ok": action.returncode == 0,
            "action": list(args),
            "action_exit": action.returncode,
            "action_result": action_result,
            "diff": diff,
            "screens": screens,
            "verdicts": verdicts,
        }
    )
    if action.returncode != 0:
        sys.exit(action.returncode)


# --- evidence --------------------------------------------------------------


@cli.command("evidence")
@click.argument("name")
@click.option("--note", default="", help="Why this bundle exists.")
@click.pass_context
def evidence_cmd(ctx: click.Context, name: str, note: str) -> None:
    """Freeze the lab: states, screens, logs, merged timeline, command history."""
    lab = open_lab(ctx)
    stamp = datetime.now().strftime("%Y-%m-%dT%H-%M-%S")
    dest = EVIDENCE_DIR / f"{socketlib.gethostname()}-{lab.name}-{name}-{stamp}"
    dest.mkdir(parents=True, exist_ok=True)

    instances = list(lab.data["instances"])
    for instance in instances:
        entry = lab.instance(instance)
        (dest / f"state-{instance}.json").write_text(
            json.dumps(instance_state(lab, instance), indent=2, default=str) + "\n"
        )
        for label, path in (("server", entry["log"]), ("client", entry["client_log"])):
            source = Path(path)
            if source.is_file():
                shutil.copy2(source, dest / f"{instance}-herdr-{label}.log")

    screens = []
    for client in list(lab.data["clients"]):
        if not client_alive(lab, client):
            continue
        (dest / f"screen-{client}.txt").write_text("\n".join(capture(lab, client)) + "\n")
        (dest / f"screen-{client}.ansi").write_text("\n".join(capture(lab, client, ansi=True)) + "\n")
        screens.append(client)

    entries = collect_logs(lab, instances)
    (dest / "timeline.log").write_text(
        "\n".join(f"{entry['instance']}/{entry['stream']} {entry['line']}" for entry in entries) + "\n"
    )
    shutil.copy2(lab.root / "lab.json", dest / "lab.json")
    history = lab.root / "history.jsonl"
    if history.is_file():
        shutil.copy2(history, dest / "history.jsonl")

    branch, commit = git_ref()
    (dest / "README.md").write_text(
        "\n".join(
            [
                f"# {name}",
                "",
                f"- lab: `{lab.name}` at `{lab.root}`",
                f"- captured: {now_iso()} on {socketlib.gethostname()}",
                f"- build: {branch} @ {commit} (`{lab.binary}`)",
                f"- instances: {', '.join(instances)}",
                f"- clients captured: {', '.join(screens) or 'none'}",
                f"- peers: {json.dumps(lab.data['peers'])}",
                "",
                f"## Note\n\n{note or '(none)'}",
                "",
                "## Files",
                "",
                "- `state-<instance>.json` — workspaces/tabs/panes(+peer backing)/layout rects/peers",
                "- `screen-<client>.txt` / `.ansi` — what the user would have seen",
                "- `<instance>-herdr-server.log`, `<instance>-herdr-client.log` — full logs",
                "- `timeline.log` — every log above, merged and time-ordered",
                "- `history.jsonl` — every lab.py command run against this lab",
                "- `lab.json` — topology: instances, sockets, clients, peers",
                "",
            ]
        )
    )
    out.emit(
        {
            "ok": True,
            "bundle": str(dest),
            "host": socketlib.gethostname(),
            "instances": instances,
            "clients": screens,
            "records": len(entries),
        },
        human=f"evidence: {dest}",
    )


# --- teardown --------------------------------------------------------------


@cli.command()
@click.pass_context
def down(ctx: click.Context) -> None:
    """Stop servers and clients, keep the directory (logs survive)."""
    lab = open_lab(ctx)
    out.emit({"ok": True, **teardown(lab, remove=False, reap=False)})


@cli.command()
@click.option("--keep-orphans", is_flag=True, help="Do not reap orphan bridge dirs.")
@click.pass_context
def destroy(ctx: click.Context, keep_orphans: bool) -> None:
    """Stop everything and delete the lab root."""
    lab = open_lab(ctx)
    out.emit({"ok": True, **teardown(lab, remove=True, reap=not keep_orphans)})


@cli.command()
@click.option("--force", is_flag=True, help="Destroy every lab whose servers are gone.")
def gc(force: bool) -> None:
    """Find labs left behind by crashed sessions."""
    labs = []
    for manifest in sorted(LAB_ROOT_PREFIX.glob("hl-*/lab.json")):
        try:
            lab = Lab(name=manifest.parent.name.removeprefix("hl-"), root=manifest.parent, data=json.loads(manifest.read_text()))
        except (json.JSONDecodeError, OSError):
            labs.append({"root": str(manifest.parent), "readable": False})
            continue
        live = {name: server_pid(entry) for name, entry in lab.data.get("instances", {}).items()}
        stale = not any(live.values())
        record = {"lab": lab.name, "root": str(lab.root), "pids": live, "stale": stale}
        if stale and force:
            record["destroyed"] = teardown(lab, remove=True, reap=True)
        labs.append(record)
    out.emit({"ok": True, "labs": labs, "bridge_dirs": bridge_socket_dirs()})


# --- ssh lab ---------------------------------------------------------------


@cli.command("ssh-up")
@click.option("--build/--no-build", default=False)
@click.pass_context
def ssh_up(ctx: click.Context, build: bool) -> None:
    """Start the throwaway sshd and instance `s`, which dials it."""
    lab = open_lab(ctx)
    if build:
        cargo_build(quiet=out.json_mode)
    if "s" not in lab.data["instances"]:
        lab.add_instance("s")
    write_ssh_lab_files(lab)
    lab.data["instances"]["s"]["extra_env"] = ssh_instance_env(lab)
    lab.data["ssh"] = {key: str(value) for key, value in ssh_paths(lab).items()}
    lab.save()
    sshd = start_lab_sshd(lab)
    started = start_server(lab, "s")
    out.emit({"ok": True, "sshd": sshd, "instance": started, "ssh": lab.data["ssh"]})


@cli.command("ssh-status")
@click.pass_context
def ssh_status(ctx: click.Context) -> None:
    """sshd, instance `s`, authorized_keys, and live bridge dirs."""
    lab = open_lab(ctx)
    out.emit(
        {
            "ok": True,
            "sshd": {"pid": lab_sshd_pid(lab), "host": SSH_HOST, "port": SSH_PORT},
            "instance_s": {"running": Path(lab.instance("s")["sock"]).is_socket(), "pid": server_pid(lab.instance("s"))},
            "authorized_keys": read_authorized_keys(lab),
            "bridge_dirs": bridge_socket_dirs(),
        }
    )


@cli.command("ssh-check")
@click.pass_context
def ssh_check(ctx: click.Context) -> None:
    """Assert herdr's authorized_keys install and replace rules against a real sshd."""
    lab = open_lab(ctx)
    if lab_sshd_pid(lab) is None:
        raise LabError("lab sshd is not running; run `ssh-up` first")

    paths = ssh_paths(lab)
    checks: list[dict] = []

    def check(label: str, ok: bool, detail: str = "") -> None:
        checks.append({"check": label, "pass": bool(ok), "detail": detail})

    key = peer_key_path(lab)
    bootstrap_pub = paths["bootstrap_key"].with_suffix(".pub").read_text().split()[1]

    # 1. A target herdr has never touched: the probe fails, a key is made and installed,
    #    and the unrelated bootstrap entry is left alone.
    key.unlink(missing_ok=True)
    key.with_suffix(".pub").unlink(missing_ok=True)
    paths["authorized_keys"].write_text(
        paths["bootstrap_key"].with_suffix(".pub").read_text().strip() + "\n"
    )
    run_herdr_on_tty(lab, ["peer", "setup-ssh", SSH_HOST, "--yes"])

    comment = herdr_key_comment(lab)
    entries = read_authorized_keys(lab)
    check("a key was generated", bool(comment), "no peer_id_ed25519.pub")
    check("the bootstrap entry survived", any(e["blob"] == bootstrap_pub for e in entries))
    check("exactly one entry for this client", sum(e["comment"] == comment for e in entries) == 1)

    # 2. The reported bug: a key regenerated after the config dir was cleared used to
    #    append beside its predecessor instead of replacing it.
    stale = next((e["blob"] for e in entries if e["comment"] == comment), "")
    legacy, other = "AAAALEGACYHERDRKEYBLOB", "AAAAOTHERMACHINEKEYBLOB"
    with paths["authorized_keys"].open("a") as handle:
        handle.write(f"ssh-ed25519 {legacy} herdr-peer\n")
        handle.write(f"ssh-ed25519 {other} herdr-peer herdr/someone@elsewhere\n")
    key.unlink(missing_ok=True)
    key.with_suffix(".pub").unlink(missing_ok=True)
    run_herdr_on_tty(lab, ["peer", "setup-ssh", SSH_HOST, "--yes"])

    comment = herdr_key_comment(lab)
    entries = read_authorized_keys(lab)
    fresh = key.with_suffix(".pub").read_text().split()[1]
    check("still exactly one entry for this client", sum(e["comment"] == comment for e in entries) == 1)
    check("it is the regenerated key", any(e["blob"] == fresh for e in entries))
    check("the superseded key is gone", not any(e["blob"] == stale for e in entries))
    check("the legacy bare herdr-peer entry was claimed", not any(e["blob"] == legacy for e in entries))
    check("another machine's herdr key survived", any(e["blob"] == other for e in entries))
    check("the bootstrap entry survived", any(e["blob"] == bootstrap_pub for e in entries))

    # 3. The installed key has to actually authenticate, which is the one thing reading
    #    the file back cannot tell us.
    probe = subprocess.run(
        [str(paths["bin"] / "ssh"), "-T", "-o", "BatchMode=yes", "-i", str(key), SSH_HOST, "true"],
        env=lab.env_for("s"),
        capture_output=True,
        text=True,
    )
    check("sshd accepts it with BatchMode=yes", probe.returncode == 0, probe.stderr.strip())

    failed = [entry["check"] for entry in checks if not entry["pass"]]
    out.emit({"ok": not failed, "checks": checks, "failed": failed})
    if failed:
        sys.exit(EXIT_ASSERT)


@cli.command("ssh-peer")
@click.pass_context
def ssh_peer(ctx: click.Context) -> None:
    """Wire instance `s` to the lab target over ssh, exercising the bridge."""
    lab = open_lab(ctx)
    if lab_sshd_pid(lab) is None:
        raise LabError("lab sshd is not running; run `ssh-up` first")
    listing = run_herdr_json(lab, "s", ["peer", "list", "--json"])
    existing = {peer.get("name") for peer in listing.get("result", {}).get("peers", [])}
    add_exit = 0
    if SSH_PEER_NAME not in existing:
        # On a terminal, because approving the far-side binary install is a prompt and
        # `peer add` refuses to federate with a remote it cannot match.
        add_exit = run_herdr_on_tty(
            lab, ["peer", "add", SSH_PEER_NAME, "--ssh", SSH_HOST, "--yes"], timeout=180
        )
    time.sleep(2)
    peers = run_herdr_json(lab, "s", ["peer", "list", "--json"]).get("result", {}).get("peers", [])
    added = any(peer.get("name") == SSH_PEER_NAME for peer in peers)
    payload = {
        "ok": added,
        "add_exit": add_exit,
        "peers": peers,
        "bridge_dirs": bridge_socket_dirs(),
    }
    if not added:
        # The add runs on a pty and the far side is the sshd's child, so the reason —
        # usually a failed remote install — only ever lands in the sshd log. Saying
        # `peers: []` and exiting 0 made an environment problem look like a peer bug.
        payload["error"] = f"peer '{SSH_PEER_NAME}' was not added"
        payload["sshd_log"] = str(ssh_paths(lab)["log"])
        payload["sshd_log_tail"] = tail_text(ssh_paths(lab)["log"], 8)
    out.emit(payload)
    if not added:
        sys.exit(EXIT_ASSERT)


@cli.command("ssh-down")
@click.option("--reap/--no-reap", default=True, help="Delete bridge dirs whose creating process is gone.")
@click.pass_context
def ssh_down(ctx: click.Context, reap: bool) -> None:
    """Stop instance `s` and the lab sshd."""
    lab = open_lab(ctx)
    stopped = stop_server(lab, "s") if "s" in lab.data["instances"] else {"stopped": False}
    sshd = stop_lab_sshd(lab)
    target_servers = stop_lab_target_servers(lab)
    remaining = []
    for entry in bridge_socket_dirs():
        if not entry["alive"] and reap:
            shutil.rmtree(entry["path"], ignore_errors=True)
            entry["reaped"] = True
        remaining.append(entry)
    out.emit(
        {
            "ok": True,
            "instance_s": stopped,
            "sshd": sshd,
            "ssh_target_servers": target_servers,
            "bridge_dirs": remaining,
        }
    )


if __name__ == "__main__":
    cli()
