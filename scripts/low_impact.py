"""Run a command in a resource-limited cgroup, so a long suite yields the machine.

This workstation is shared. The scenario suites (`just test-e2e`, `just test-boxes`)
run for minutes, build the binary, start real servers and containers, and are the
heaviest thing on it. Left uncapped they both starve other work and get starved by it:
a 1-minute load average of 71 turned a 53s suite into 13m36s and failed two
normally-green tests on a 120s ssh timeout, which read exactly like a regression in the
code under test rather than like contention.

What the limits are, and are not:

- `CPUWeight` is a *share*, not a ceiling. Under contention this suite takes less; on an
  idle machine it still runs at full speed. A `CPUQuota` ceiling would slow every run
  even when nothing else wants the CPU, so it is opt-in rather than default.
- `MemoryHigh` throttles and reclaims; `MemoryMax` kills. The suite builds the binary
  inside itself (`herdr_bin` calls cargo), and a linker peak is exactly the kind of
  legitimate spike that must not be OOM-killed, so only `MemoryHigh` is set by default.
- **I/O is not capped, and on this machine it cannot be.** `IOWeight` needs the `io`
  controller delegated to the user slice (`cpu memory pids` is what is delegated here),
  and `ionice` needs a scheduler that honours priorities (`none` is what the NVMe queue
  uses). It is still passed so it takes effect wherever delegation exists, but the
  contention that actually bites is disk writeback, and this does not fix it. Not
  looping the suites remains the real mitigation.

Nothing silently falls back, but "cannot cap" and "failed to cap" are not the same
answer, because an uncapped run that believes it is capped is how the machine gets
hammered by something that reported success:

- A machine that *cannot* cap at all — no systemd, so macOS and containers included —
  warns and runs anyway. `just`'s `[unix]` recipes cover macOS, and refusing there would
  break them rather than protect anything.
- A machine that has `systemd-run` and still fails to use it is misconfigured in a
  fixable way, so it refuses: quietly dropping the cap on the shared box this exists to
  protect is the harm.
- `--never-refuse` drops that second clause, for a call site where refusing costs more
  than the cap is worth (`just lint` runs from the pre-commit hook, where a broken
  systemd would block committing rather than slow a test run).
- `HERDR_E2E_REQUIRE_CAP=1` restores it, and makes the first case refuse too.

Capability is probed rather than assumed from `systemd-run` being on `PATH`: it is
present and still unusable in an ssh session with no user manager.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys

#: Properties applied unless overridden. Empty value means "do not pass this property".
DEFAULT_PROPERTIES = {
    # A share, not a ceiling: 100 is the default weight, so this asks for roughly a
    # fifth of a contended machine and all of an idle one.
    "CPUWeight": "20",
    # Inert without `io` delegation; harmless, and correct where it is delegated.
    "IOWeight": "20",
    # Back-pressure, not a kill. See the module docstring on the in-suite cargo build.
    "MemoryHigh": "8G",
    # Opt-in ceilings, off by default.
    "MemoryMax": "",
    "CPUQuota": "",
}

#: Environment variable per property, so an override is deliberate and greppable.
ENV_OVERRIDES = {
    "CPUWeight": "HERDR_E2E_CPU_WEIGHT",
    "IOWeight": "HERDR_E2E_IO_WEIGHT",
    "MemoryHigh": "HERDR_E2E_MEMORY_HIGH",
    "MemoryMax": "HERDR_E2E_MEMORY_MAX",
    "CPUQuota": "HERDR_E2E_CPU_QUOTA",
}

#: Variables systemd owns for a unit. Forwarding the caller's copies would hand the new
#: unit stale values describing a different one.
SYSTEMD_OWNED_ENV = frozenset(
    {
        "INVOCATION_ID",
        "JOURNAL_STREAM",
        "LISTEN_FDNAMES",
        "LISTEN_FDS",
        "LISTEN_PID",
        "MAINPID",
        "MANAGERPID",
        "MEMORY_PRESSURE_WATCH",
        "MEMORY_PRESSURE_WRITE",
        "NOTIFY_SOCKET",
        "SYSTEMD_EXEC_PID",
        "WATCHDOG_PID",
        "WATCHDOG_USEC",
    }
)

NICE_ENV = "HERDR_E2E_NICE"
DEFAULT_NICE = "10"

UNCAPPED_ENV = "HERDR_E2E_UNCAPPED"
DRY_RUN_ENV = "HERDR_E2E_DRY_RUN"
#: Turn "this machine cannot cap" into an error instead of a warning.
REQUIRE_ENV = "HERDR_E2E_REQUIRE_CAP"
#: Set inside the unit, so a nested call does not wrap again.
#:
#: `systemd-run --user` makes a *sibling* unit under `app.slice`, never a child, so an
#: inner call would carve its work out of the outer cgroup rather than nest inside it —
#: `just check` depends on `just lint`, and both are capped. Sequential, so the practical
#: effect is small, but it splits one budget into two and prints the banner twice.
INSIDE_ENV = "HERDR_E2E_CAPPED"

EXIT_USAGE = 2


class Capability:
    """Whether this machine can cap, and why not when it cannot.

    `systemd-run` being on `PATH` is not the question — *usable* is. It exists and still
    fails on a host with no user manager reachable, which is an ordinary ssh session
    without lingering enabled:

        Failed to connect to user scope bus via local transport:
        $DBUS_SESSION_BUS_ADDRESS and $XDG_RUNTIME_DIR not defined

    A presence check passes there and the run dies with that message instead of anything
    actionable, so the probe runs the real thing once against `true`.
    """

    def __init__(self, usable: bool, *, installed: bool, detail: str = "") -> None:
        self.usable = usable
        self.installed = installed
        self.detail = detail


#: Values of `CI` that mean yes. Anything else — including the empty string, `false` and
#: `0` — means no. A bare truthiness test read all three as yes, because a non-empty
#: string is truthy, which turned a deliberate `CI=false` opt-out into a skipped cap.
CI_TRUE = frozenset({"1", "true", "yes", "on"})


def is_ci(env: dict[str, str]) -> bool:
    return env.get("CI", "").strip().lower() in CI_TRUE


def probe(env: dict[str, str]) -> Capability:
    """Ask systemd-run to do the smallest possible real job."""
    if shutil.which("systemd-run") is None:
        # No systemd at all: macOS, a container, a non-systemd distro. `just`'s `[unix]`
        # recipes cover macOS, so this is an ordinary platform rather than a fault.
        return Capability(False, installed=False, detail="systemd-run is not installed")
    probed = subprocess.run(
        ["systemd-run", "--user", "--quiet", "--wait", "--pipe", "--collect", "--", "true"],
        capture_output=True,
        text=True,
        env=env,
    )
    if probed.returncode == 0:
        return Capability(True, installed=True)
    detail = probed.stderr.strip().splitlines()
    return Capability(
        False,
        installed=True,
        detail=detail[0] if detail else f"systemd-run exited {probed.returncode}",
    )


def resolve_properties(env: dict[str, str]) -> dict[str, str]:
    """The properties to pass, after environment overrides, dropping empty ones."""
    resolved = {}
    for name, default in DEFAULT_PROPERTIES.items():
        value = env.get(ENV_OVERRIDES[name], default).strip()
        if value:
            resolved[name] = value
    return resolved


def forwarded_env(env: dict[str, str]) -> list[str]:
    """`--setenv` arguments carrying the caller's environment into the unit.

    A transient user unit does not inherit the invoking shell's environment; it gets the
    user manager's. Without this the suite dies immediately and for confusing reasons:
    `uv` is not on the unit's `PATH` at all, and `ZIG` — which this repo's build requires
    because the system zig is too new — arrives empty. Forwarding everything keeps the
    wrapper transparent, so the only difference from running the command directly is the
    cgroup it lands in.
    """
    return [
        f"--setenv={name}={value}"
        for name, value in sorted(env.items())
        if name not in SYSTEMD_OWNED_ENV
    ]


def build_command(command: list[str], env: dict[str, str], cwd: str) -> list[str]:
    """The full `systemd-run` invocation wrapping `command`."""
    properties = resolve_properties(env)
    nice = env.get(NICE_ENV, DEFAULT_NICE).strip() or DEFAULT_NICE
    wrapper = [
        "systemd-run",
        "--user",
        "--wait",
        "--pipe",
        # Without --collect a failed unit lingers in `systemctl --user list-units`.
        "--collect",
        f"--working-directory={cwd}",
        f"--property=Nice={nice}",
    ]
    wrapper += [f"--property={name}={value}" for name, value in properties.items()]
    wrapper += forwarded_env(env)
    wrapper.append(f"--setenv={INSIDE_ENV}=1")
    return [*wrapper, "--", *command]


def describe(command: list[str], env: dict[str, str]) -> str:
    properties = resolve_properties(env)
    nice = env.get(NICE_ENV, DEFAULT_NICE).strip() or DEFAULT_NICE
    shown = ", ".join(f"{name} {value}" for name, value in properties.items())
    return f"low-impact: nice {nice}; {shown}\n  {' '.join(command)}"


def printable(built: list[str]) -> str:
    """The wrapped command with the forwarded environment collapsed to a count.

    Every variable is one `--setenv=` argument, so printing them in full buries the
    properties under a screen of noise and copies whatever secrets the caller happens to
    hold — tokens, agent sockets — into the terminal and any log capturing it. Forwarding
    them is not new exposure, since a direct run inherits them anyway; printing them is.
    """
    # The nesting marker is ours rather than the caller's, and saying so is useful, so it
    # stays visible and out of the count.
    marker = f"--setenv={INSIDE_ENV}=1"
    hidden = [
        argument
        for argument in built
        if argument.startswith("--setenv=") and argument != marker
    ]
    forwarded = len(hidden)
    kept = [argument for argument in built if argument not in hidden]
    if not forwarded:
        return " ".join(kept)
    separator = kept.index("--")
    return " ".join(
        [*kept[:separator], f"<{forwarded} environment variables forwarded>", *kept[separator:]]
    )


#: Call-site flag: never turn "cannot cap" into an error.
NEVER_REFUSE_FLAG = "--never-refuse"


def split_flags(argv: list[str]) -> tuple[bool, list[str]]:
    """Consume our own flags, then the `--`, and return the command.

    Only what precedes the first `--` is ours, so a command free to contain anything —
    including a flag spelled like one of ours — is never misread.
    """
    rest = list(argv)
    never_refuse = False
    while rest and rest[0] != "--" and rest[0].startswith("-"):
        if rest[0] == NEVER_REFUSE_FLAG:
            never_refuse = True
            rest = rest[1:]
            continue
        break
    # `just` passes the recipe body through a shell, so a stray separator can arrive.
    if rest and rest[0] == "--":
        rest = rest[1:]
    return never_refuse, rest


def main(argv: list[str]) -> int:
    never_refuse, command = split_flags(argv[1:])
    if not command:
        print(
            f"usage: {argv[0]} [{NEVER_REFUSE_FLAG}] [--] <command> [args...]",
            file=sys.stderr,
        )
        return EXIT_USAGE

    env = dict(os.environ)
    skip = None
    if env.get(INSIDE_ENV) == "1":
        skip = "already inside a capped unit"
    elif env.get(UNCAPPED_ENV) == "1":
        skip = f"disabled by {UNCAPPED_ENV}"
    elif is_ci(env):
        # A CI runner is a whole machine for one job, so there is nothing to yield to and
        # no user session to yield through: `systemd-run --user` has no bus on a GitHub
        # runner and would refuse below. `just check` runs in preview.yml upstream, so
        # capping there would fail the release workflow rather than protect anything.
        skip = "CI runner"

    if skip is not None:
        print(f"low-impact: not capping ({skip})", file=sys.stderr)
        # Checked before running, never after: a dry run must not execute.
        if env.get(DRY_RUN_ENV) == "1":
            print(" ".join(command))
            return 0
        return subprocess.run(command).returncode

    print(describe(command, env), file=sys.stderr)
    if env.get(DRY_RUN_ENV) == "1":
        print(printable(build_command(command, env, os.getcwd())))
        return 0

    capability = probe(env)
    if not capability.usable:
        # The split that matters. A machine that *cannot* cap — no systemd, so macOS and
        # containers included — must still be able to run the suite, or the recipe is
        # simply broken there. A machine that has systemd and fails anyway is
        # misconfigured in a fixable way, and quietly dropping the cap on the shared box
        # this exists to protect is the harm. Loud either way; only the exit differs.
        # `--never-refuse` drops the "installed, so this is fixable" clause. It is for a
        # call site where refusing costs more than the cap is worth — `just lint` runs
        # from the pre-commit hook, and a broken systemd there would block committing
        # rather than slow a test run. An explicit REQUIRE_CAP still wins.
        strict = env.get(REQUIRE_ENV) == "1" or (capability.installed and not never_refuse)
        note = "refusing" if strict else "running without limits"
        print(
            f"low-impact: cannot enforce limits ({capability.detail}); {note}.",
            file=sys.stderr,
        )
        if strict:
            print(
                f"low-impact: set {UNCAPPED_ENV}=1 to run uncapped deliberately.",
                file=sys.stderr,
            )
            return EXIT_USAGE
        return subprocess.run(command).returncode

    return subprocess.run(build_command(command, env, os.getcwd())).returncode


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
