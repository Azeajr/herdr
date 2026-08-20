import contextlib
import io
import unittest
from unittest import mock

import scripts.low_impact as low_impact


@contextlib.contextmanager
def silenced():
    """Swallow the banner `main` prints, so `just check` output stays readable."""
    with contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()):
        yield


class ResolvePropertiesTests(unittest.TestCase):
    def test_defaults_drop_the_opt_in_ceilings(self):
        resolved = low_impact.resolve_properties({})

        self.assertEqual(resolved["CPUWeight"], "20")
        self.assertEqual(resolved["MemoryHigh"], "8G")
        # A ceiling that kills, and a ceiling that slows an idle machine, are both
        # deliberate choices rather than defaults.
        self.assertNotIn("MemoryMax", resolved)
        self.assertNotIn("CPUQuota", resolved)

    def test_an_override_replaces_one_property_and_leaves_the_rest(self):
        resolved = low_impact.resolve_properties({"HERDR_E2E_CPU_WEIGHT": "50"})

        self.assertEqual(resolved["CPUWeight"], "50")
        self.assertEqual(resolved["MemoryHigh"], "8G")

    def test_an_opt_in_ceiling_appears_only_when_asked_for(self):
        resolved = low_impact.resolve_properties({"HERDR_E2E_CPU_QUOTA": "30%"})

        self.assertEqual(resolved["CPUQuota"], "30%")

    def test_an_empty_override_removes_a_default(self):
        resolved = low_impact.resolve_properties({"HERDR_E2E_MEMORY_HIGH": ""})

        self.assertNotIn("MemoryHigh", resolved)


class BuildCommandTests(unittest.TestCase):
    def test_the_command_is_wrapped_after_a_separator(self):
        built = low_impact.build_command(["pytest", "-m", "e2e"], {}, "/repo")

        self.assertEqual(built[0], "systemd-run")
        self.assertIn("--user", built)
        self.assertIn("--working-directory=/repo", built)
        # Everything after `--` is the command, untouched.
        self.assertEqual(built[built.index("--") + 1 :], ["pytest", "-m", "e2e"])

    def test_the_unit_is_collected_so_a_failure_does_not_linger(self):
        built = low_impact.build_command(["true"], {}, "/repo")

        self.assertIn("--collect", built)

    def test_nice_is_passed_and_overridable(self):
        self.assertIn("--property=Nice=10", low_impact.build_command(["true"], {}, "/repo"))
        self.assertIn(
            "--property=Nice=3",
            low_impact.build_command(["true"], {"HERDR_E2E_NICE": "3"}, "/repo"),
        )

    def test_the_callers_environment_is_forwarded(self):
        """Without this the unit has no `uv` on PATH and an empty ZIG, and the suite
        dies for reasons that have nothing to do with what it tests."""
        built = low_impact.build_command(
            ["true"], {"ZIG": "/opt/zig0.15/zig", "PATH": "/usr/bin"}, "/repo"
        )

        self.assertIn("--setenv=ZIG=/opt/zig0.15/zig", built)
        self.assertIn("--setenv=PATH=/usr/bin", built)

    def test_systemd_owned_variables_are_not_forwarded(self):
        built = low_impact.build_command(
            ["true"], {"INVOCATION_ID": "stale", "NOTIFY_SOCKET": "/run/x"}, "/repo"
        )

        self.assertNotIn("--setenv=INVOCATION_ID=stale", built)
        self.assertNotIn("--setenv=NOTIFY_SOCKET=/run/x", built)

    def test_forwarded_values_stay_whole_arguments(self):
        """One argv element per variable, so a value with spaces cannot split."""
        built = low_impact.build_command(["true"], {"ARGS": "-k a or b"}, "/repo")

        self.assertIn("--setenv=ARGS=-k a or b", built)

    def test_a_command_argument_is_never_read_as_a_property(self):
        # A pytest flag that looks like one of ours must reach pytest, not systemd-run.
        built = low_impact.build_command(["pytest", "--property=x"], {}, "/repo")

        self.assertEqual(built[built.index("--") + 1 :], ["pytest", "--property=x"])


class MainTests(unittest.TestCase):
    def test_a_missing_command_is_a_usage_error(self):
        with mock.patch.dict(low_impact.os.environ, {}, clear=True), silenced():
            self.assertEqual(low_impact.main(["low_impact.py"]), low_impact.EXIT_USAGE)

    def test_a_leading_separator_from_just_is_stripped(self):
        with mock.patch.dict(
            low_impact.os.environ, {low_impact.DRY_RUN_ENV: "1"}, clear=True
        ):
            with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                self.assertEqual(low_impact.main(["low_impact.py", "--", "true"]), 0)
        run.assert_not_called()

    def test_a_dry_run_never_executes(self):
        with mock.patch.dict(
            low_impact.os.environ, {low_impact.DRY_RUN_ENV: "1"}, clear=True
        ):
            with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                self.assertEqual(low_impact.main(["low_impact.py", "true"]), 0)
        run.assert_not_called()

    def test_a_platform_without_systemd_runs_uncapped_rather_than_refusing(self):
        """macOS is a `[unix]` platform for `just`, so refusing there would simply
        break the recipe on a machine that can never cap."""
        with mock.patch.dict(low_impact.os.environ, {}, clear=True):
            with mock.patch.object(low_impact.shutil, "which", return_value=None):
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    run.return_value = mock.Mock(returncode=0)
                    exit_code = low_impact.main(["low_impact.py", "pytest"])

        self.assertEqual(exit_code, 0)
        run.assert_called_once_with(["pytest"])

    def test_that_platform_still_refuses_when_the_cap_is_required(self):
        with mock.patch.dict(
            low_impact.os.environ, {low_impact.REQUIRE_ENV: "1"}, clear=True
        ):
            with mock.patch.object(low_impact.shutil, "which", return_value=None):
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    exit_code = low_impact.main(["low_impact.py", "pytest"])

        self.assertEqual(exit_code, low_impact.EXIT_USAGE)
        run.assert_not_called()

    def test_systemd_present_but_unusable_refuses(self):
        """An ssh session without lingering: the binary is there and the bus is not.
        A presence check passed here, which is how this went unnoticed — the machine
        can cap, so quietly dropping the cap is the harm."""
        with mock.patch.dict(low_impact.os.environ, {}, clear=True):
            with mock.patch.object(
                low_impact.shutil, "which", return_value="/usr/bin/systemd-run"
            ):
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    run.return_value = mock.Mock(
                        returncode=1, stderr="Failed to connect to user scope bus"
                    )
                    exit_code = low_impact.main(["low_impact.py", "pytest"])

        self.assertEqual(exit_code, low_impact.EXIT_USAGE)
        # Only the probe ran; the suite never started.
        self.assertEqual(run.call_count, 1)


class ProbeTests(unittest.TestCase):
    def test_an_absent_binary_is_not_installed(self):
        with mock.patch.object(low_impact.shutil, "which", return_value=None):
            capability = low_impact.probe({})

        self.assertFalse(capability.usable)
        self.assertFalse(capability.installed)

    def test_a_working_systemd_run_is_usable(self):
        with mock.patch.object(
            low_impact.shutil, "which", return_value="/usr/bin/systemd-run"
        ):
            with mock.patch.object(low_impact.subprocess, "run") as run:
                run.return_value = mock.Mock(returncode=0, stderr="")
                capability = low_impact.probe({})

        self.assertTrue(capability.usable)

    def test_the_probe_reports_the_reason_it_failed(self):
        with mock.patch.object(
            low_impact.shutil, "which", return_value="/usr/bin/systemd-run"
        ):
            with mock.patch.object(low_impact.subprocess, "run") as run:
                run.return_value = mock.Mock(
                    returncode=1, stderr="Failed to connect to user scope bus\nmore\n"
                )
                capability = low_impact.probe({})

        self.assertFalse(capability.usable)
        self.assertTrue(capability.installed)
        self.assertEqual(capability.detail, "Failed to connect to user scope bus")


class SplitFlagsTests(unittest.TestCase):
    def test_a_bare_command_is_untouched(self):
        self.assertEqual(low_impact.split_flags(["just", "lint"]), (False, ["just", "lint"]))

    def test_the_separator_alone_is_stripped(self):
        self.assertEqual(low_impact.split_flags(["--", "just", "lint"]), (False, ["just", "lint"]))

    def test_the_flag_is_consumed_before_the_separator(self):
        self.assertEqual(
            low_impact.split_flags(["--never-refuse", "--", "just", "lint"]),
            (True, ["just", "lint"]),
        )

    def test_a_flag_after_the_separator_belongs_to_the_command(self):
        """Everything past `--` is the command's, including a flag spelled like ours."""
        never_refuse, command = low_impact.split_flags(["--", "pytest", "--never-refuse"])

        self.assertFalse(never_refuse)
        self.assertEqual(command, ["pytest", "--never-refuse"])

    def test_an_unknown_leading_flag_is_left_for_the_command(self):
        self.assertEqual(low_impact.split_flags(["--odd", "cmd"]), (False, ["--odd", "cmd"]))


class NeverRefuseTests(unittest.TestCase):
    def test_it_runs_uncapped_where_the_default_would_refuse(self):
        """systemd present but unusable: normally a refusal, but `just lint` runs from
        the pre-commit hook and refusing there blocks committing."""
        with mock.patch.dict(low_impact.os.environ, {}, clear=True):
            with mock.patch.object(
                low_impact.shutil, "which", return_value="/usr/bin/systemd-run"
            ):
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    run.side_effect = [
                        mock.Mock(returncode=1, stderr="Failed to connect to user scope bus"),
                        mock.Mock(returncode=0),
                    ]
                    exit_code = low_impact.main(
                        ["low_impact.py", "--never-refuse", "--", "just", "lint"]
                    )

        self.assertEqual(exit_code, 0)
        # The probe, then the command itself — unwrapped.
        self.assertEqual(run.call_args_list[1].args[0], ["just", "lint"])

    def test_require_cap_still_overrides_it(self):
        with mock.patch.dict(
            low_impact.os.environ, {low_impact.REQUIRE_ENV: "1"}, clear=True
        ):
            with mock.patch.object(
                low_impact.shutil, "which", return_value="/usr/bin/systemd-run"
            ):
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    run.return_value = mock.Mock(returncode=1, stderr="no bus")
                    exit_code = low_impact.main(
                        ["low_impact.py", "--never-refuse", "--", "just", "lint"]
                    )

        self.assertEqual(exit_code, low_impact.EXIT_USAGE)

    def test_a_usable_machine_still_caps(self):
        """The flag changes the failure path only; a working machine is still capped."""
        with mock.patch.dict(low_impact.os.environ, {}, clear=True):
            with mock.patch.object(
                low_impact.shutil, "which", return_value="/usr/bin/systemd-run"
            ):
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    run.side_effect = [
                        mock.Mock(returncode=0, stderr=""),
                        mock.Mock(returncode=0),
                    ]
                    low_impact.main(["low_impact.py", "--never-refuse", "--", "just", "lint"])

        self.assertEqual(run.call_args_list[1].args[0][0], "systemd-run")


class NestingTests(unittest.TestCase):
    def test_the_marker_is_set_inside_the_unit(self):
        built = low_impact.build_command(["just", "lint"], {}, "/repo")

        self.assertIn(f"--setenv={low_impact.INSIDE_ENV}=1", built)

    def test_a_nested_call_does_not_wrap_again(self):
        """`just check` depends on `just lint` and both are capped. `systemd-run --user`
        makes a sibling, not a child, so wrapping twice splits one budget into two."""
        with mock.patch.dict(
            low_impact.os.environ, {low_impact.INSIDE_ENV: "1"}, clear=True
        ):
            with mock.patch.object(low_impact.shutil, "which") as which:
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    run.return_value = mock.Mock(returncode=0)
                    exit_code = low_impact.main(["low_impact.py", "cargo", "clippy"])

        self.assertEqual(exit_code, 0)
        run.assert_called_once_with(["cargo", "clippy"])
        which.assert_not_called()

    def test_a_nested_failure_still_propagates(self):
        with mock.patch.dict(
            low_impact.os.environ, {low_impact.INSIDE_ENV: "1"}, clear=True
        ):
            with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                run.return_value = mock.Mock(returncode=101)
                exit_code = low_impact.main(["low_impact.py", "cargo", "clippy"])

        self.assertEqual(exit_code, 101)


class CiTests(unittest.TestCase):
    def test_only_real_truthy_values_count_as_ci(self):
        """`CI=false` is how a tool says *not* CI. A bare truthiness check read the
        non-empty string as yes and skipped the cap on exactly the machines that had
        asked for it."""
        for value in ("1", "true", "TRUE", "yes", "on"):
            self.assertTrue(low_impact.is_ci({"CI": value}), value)
        for value in ("false", "0", "no", "off", "", "   "):
            self.assertFalse(low_impact.is_ci({"CI": value}), value)
        self.assertFalse(low_impact.is_ci({}))

    def test_a_dry_run_in_ci_prints_instead_of_executing(self):
        """The CI branch used to return before the dry-run check, so `--dry-run` ran
        the suite. A dry run must never execute."""
        with mock.patch.dict(
            low_impact.os.environ,
            {"CI": "true", low_impact.DRY_RUN_ENV: "1"},
            clear=True,
        ):
            with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                exit_code = low_impact.main(["low_impact.py", "just", "_check-inner"])

        self.assertEqual(exit_code, 0)
        run.assert_not_called()

    def test_ci_runs_uncapped_rather_than_refusing(self):
        """`just check` runs in preview.yml. A GitHub runner has no user bus, so the
        refusal path would fail the release workflow instead of protecting anything —
        and there is nothing to yield to on a machine dedicated to one job."""
        with mock.patch.dict(low_impact.os.environ, {"CI": "true"}, clear=True):
            with mock.patch.object(low_impact.shutil, "which") as which:
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    run.return_value = mock.Mock(returncode=0)
                    exit_code = low_impact.main(["low_impact.py", "just", "_check-inner"])

        self.assertEqual(exit_code, 0)
        run.assert_called_once_with(["just", "_check-inner"])
        # Not even probed: the decision does not depend on the runner having systemd.
        which.assert_not_called()

    def test_ci_still_propagates_a_failure(self):
        with mock.patch.dict(low_impact.os.environ, {"CI": "true"}, clear=True):
            with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                run.return_value = mock.Mock(returncode=101)
                exit_code = low_impact.main(["low_impact.py", "just", "_check-inner"])

        self.assertEqual(exit_code, 101)


class PrintableTests(unittest.TestCase):
    def test_forwarded_variables_are_counted_not_printed(self):
        """A dry run must not copy the caller's tokens into the terminal or a log."""
        built = low_impact.build_command(
            ["pytest"], {"SECRET_TOKEN": "hunter2", "PATH": "/usr/bin"}, "/repo"
        )

        shown = low_impact.printable(built)

        self.assertNotIn("hunter2", shown)
        self.assertIn("<2 environment variables forwarded>", shown)
        # The properties are the point of a dry run and stay visible.
        self.assertIn("--property=CPUWeight=20", shown)
        self.assertTrue(shown.endswith("-- pytest"))

    def test_a_command_with_no_forwarded_environment_is_unchanged(self):
        shown = low_impact.printable(low_impact.build_command(["pytest"], {}, "/repo"))

        self.assertIn("-- pytest", shown)


class WrappedRunTests(unittest.TestCase):
    def test_the_escape_hatch_runs_the_command_unwrapped(self):
        with mock.patch.dict(
            low_impact.os.environ, {low_impact.UNCAPPED_ENV: "1"}, clear=True
        ):
            with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                run.return_value = mock.Mock(returncode=7)
                exit_code = low_impact.main(["low_impact.py", "pytest"])

        self.assertEqual(exit_code, 7)
        run.assert_called_once_with(["pytest"])

    def test_the_wrapped_exit_code_is_the_suite_s_own(self):
        """A failing suite has to keep failing through the wrapper."""
        with mock.patch.dict(low_impact.os.environ, {}, clear=True):
            with mock.patch.object(
                low_impact.shutil, "which", return_value="/usr/bin/systemd-run"
            ):
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    # The probe answers first, then the suite itself fails.
                    run.side_effect = [
                        mock.Mock(returncode=0, stderr=""),
                        mock.Mock(returncode=1),
                    ]
                    exit_code = low_impact.main(["low_impact.py", "pytest"])

        self.assertEqual(exit_code, 1)

    def test_a_usable_machine_actually_wraps_the_command(self):
        with mock.patch.dict(low_impact.os.environ, {}, clear=True):
            with mock.patch.object(
                low_impact.shutil, "which", return_value="/usr/bin/systemd-run"
            ):
                with mock.patch.object(low_impact.subprocess, "run") as run, silenced():
                    run.side_effect = [
                        mock.Mock(returncode=0, stderr=""),
                        mock.Mock(returncode=0),
                    ]
                    low_impact.main(["low_impact.py", "pytest"])

        wrapped = run.call_args_list[1].args[0]
        self.assertEqual(wrapped[0], "systemd-run")
        self.assertEqual(wrapped[-1], "pytest")


if __name__ == "__main__":
    unittest.main()
