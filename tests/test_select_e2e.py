"""CI selection tests: pure policy tables plus real temporary Git histories."""

import contextlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/select_e2e.py"
SPEC = importlib.util.spec_from_file_location("select_e2e", SCRIPT)
selector = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(selector)
TUI_PATH = "crates/bootc-migrate/src/tui/mod.rs"
SCENARIOS = [
    {"name": "btrfs", "disk_size": "40G"},
    {"name": "exploratory", "allow_failure": True},
    {"name": "ext4"},
    {"name": "luks"},
    {"name": "TUI", "mode": "tui-migrate", "disk_size": "40G"},
]


class SelectionTests(unittest.TestCase):
    def test_exact_baseline_and_bounded_tui_union(self):
        gating = [cell for cell in SCENARIOS if cell.get("allow_failure") is not True]
        for event in ("push", "pull_request"):
            for size in (1, 2, 3, 4, 8):
                for run in range(1, 2 * len(gating) + 1):
                    expected = [gating[(run * size + n) % len(gating)]
                                for n in range(min(size, len(gating)))]
                    for paths in ([], ["docs/testing.md"], [TUI_PATH], [TUI_PATH] * 3):
                        with self.subTest(event=event, size=size, run=run, paths=paths):
                            before = json.dumps(SCENARIOS)
                            plan = selector.select_scenarios(SCENARIOS, event, run, size, paths)
                            picked = plan["scenarios"]
                            wanted = list(expected)
                            if TUI_PATH in paths and gating[-1] not in wanted:
                                wanted.append(gating[-1])
                            self.assertEqual(picked, wanted)
                            self.assertEqual(picked[:len(expected)], expected)
                            self.assertLessEqual(len(picked), len(expected) + 1)
                            self.assertEqual(json.dumps(SCENARIOS), before)
                            if TUI_PATH in paths:
                                self.assertEqual(picked.count(gating[-1]), 1)
                                self.assertIn("require tui-migrate", plan["reason"])

    def test_path_scope(self):
        cases = [(path, True) for path in sorted(selector.TUI_PATHS)] + [
            (TUI_PATH, True),
            ("crates/bootc-migrate/src/tui/nested/new.rs", True),
            ("crates/bootc-migrate/src/tui/new\n name.rs", True),
            ("crates/bootc-migrate/src/tui_other/mod.rs", False),
            ("crates/bootc-migrate-core/src/migration/boot.rs", False),
            ("docs/testing.md", False),
            ("Cargo.lock.backup", False),
            ("tests/tui-e2e-driver.py.bak", False),
        ]
        for path, expected in cases:
            with self.subTest(path=path):
                self.assertEqual(selector.affects_tui([path]), expected)
        self.assertFalse(selector.affects_tui([]))

    def test_full_matrix_unchanged(self):
        for event in selector.FULL_EVENTS:
            for paths in ([], [TUI_PATH]):
                with self.subTest(event=event, paths=paths):
                    self.assertEqual(
                        selector.select_scenarios(SCENARIOS, event, 1, 2, paths)["scenarios"],
                        SCENARIOS,
                    )

    def test_invalid_inputs_fail_closed(self):
        for field, value in [
            ("run_number", 0), ("run_number", -1), ("run_number", 1.5),
            ("run_number", True), ("size", 0), ("size", -1), ("size", 2.5),
            ("size", True), ("event", "workflow_call"), ("scenarios", []),
            ("scenarios", {}), ("scenarios", [{"mode": "tui-migrate"}]),
            ("scenarios", [{"name": "only", "allow_failure": True}]),
        ]:
            arguments = dict(scenarios=SCENARIOS, event="push", run_number=1, size=2, paths=[])
            arguments[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                selector.select_scenarios(**arguments)

    def test_tui_must_be_unique_and_gating(self):
        for scenarios in [
            SCENARIOS[:-1],
            [*SCENARIOS, {"name": "second TUI", "mode": "tui-migrate"}],
            [*SCENARIOS[:-1], {**SCENARIOS[-1], "allow_failure": True}],
            [*SCENARIOS, {"name": "optional TUI", "mode": "tui-migrate", "allow_failure": True}],
        ]:
            with self.subTest(scenarios=scenarios), self.assertRaisesRegex(
                ValueError, "exactly one gating tui-migrate",
            ):
                selector.select_scenarios(scenarios, "push", 2, 2, [TUI_PATH])

    def test_event_validation(self):
        sha = "a" * 40
        cases = [
            ("push", {}), ("push", []), ("push", {"before": sha}),
            ("push", {"before": "HEAD", "after": sha}),
            ("push", {"before": sha, "after": "--help"}),
            ("push", {"before": sha, "after": None}),
            ("push", {"before": sha, "after": selector.ZERO_SHA}),
            ("pull_request", {"pull_request": None}),
            ("pull_request", {"pull_request": {"base": {"sha": selector.ZERO_SHA},
                                                "head": {"sha": sha}}}),
            ("unsupported", {}),
        ]
        for event, payload in cases:
            with self.subTest(event=event, payload=payload), self.assertRaises(ValueError):
                selector.event_diff(event, payload)

    def test_git_timeout_stops_planning_without_output(self):
        payload = {"before": "a" * 40, "after": "b" * 40}
        with tempfile.TemporaryDirectory() as directory:
            event_file = Path(directory) / "event.json"
            event_file.write_text(json.dumps(payload))
            argv = [str(SCRIPT), "--event", "push", "--event-path", str(event_file),
                    "--run-number", "2", "--size", "2"]
            stdout, stderr = io.StringIO(), io.StringIO()
            timeout = subprocess.TimeoutExpired("git", selector.GIT_TIMEOUT_SECONDS)
            with mock.patch.object(selector.subprocess, "run", side_effect=timeout) as run:
                with self.assertRaises(subprocess.TimeoutExpired):
                    selector.changed_paths("push", payload)
                self.assertEqual(run.call_args.kwargs["timeout"], selector.GIT_TIMEOUT_SECONDS)
                with mock.patch.object(sys, "argv", argv), \
                        mock.patch.object(sys, "stdin", io.StringIO(json.dumps(SCENARIOS))), \
                        contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                    self.assertEqual(selector.main(), 1)
                self.assertEqual(run.call_count, 2)  # No retries or fallback to a smaller diff.
            self.assertEqual(stdout.getvalue(), "")
            self.assertEqual(
                stderr.getvalue(),
                f"E2E planning failed: git exceeded {selector.GIT_TIMEOUT_SECONDS} seconds\n",
            )

    def test_full_events_do_not_read_git(self):
        with mock.patch.object(selector.subprocess, "run") as run:
            for event in selector.FULL_EVENTS:
                self.assertEqual(selector.changed_paths(event, {}), [])
            run.assert_not_called()


class GitRangeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        self.git("init", "-q", "-b", "main")
        self.write("README.md", "base\n")
        self.write(TUI_PATH, "base\n")
        self.base = self.commit()

    def git(self, *arguments):
        return subprocess.run(
            ["git", "-c", "user.name=CI tests", "-c", "user.email=ci@example.invalid",
             "-c", "commit.gpgsign=false", *arguments],
            cwd=self.repo, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        ).stdout.decode().strip()

    def write(self, path, text):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-q", "-m", "fixture")
        return self.git("rev-parse", "HEAD")

    def pull_request(self, base, head):
        return {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}

    def test_full_pr_and_push_include_earlier_tui_commit(self):
        self.git("checkout", "-q", "-b", "feature")
        self.write(TUI_PATH, "TUI changed in earlier commit\n")
        self.commit()
        self.write("README.md", "docs-only latest commit\n")
        head = self.commit()
        self.git("checkout", "-q", "main")
        self.write("base-only.txt", "base advanced\n")
        advanced_base = self.commit()
        for event, payload in [
            ("pull_request", self.pull_request(advanced_base, head)),
            ("push", {"before": self.base, "after": head}),
        ]:
            with self.subTest(event=event):
                paths = selector.changed_paths(event, payload, self.repo)
                self.assertEqual(set(paths), {TUI_PATH, "README.md"})
                plan = selector.select_scenarios(SCENARIOS, event, 2, 2, paths)
                self.assertEqual(plan["scenarios"], [SCENARIOS[0], SCENARIOS[2], SCENARIOS[-1]])

    def test_pr_excludes_changes_only_on_base_branch(self):
        self.git("checkout", "-q", "-b", "feature")
        self.write("README.md", "docs only\n")
        head = self.commit()
        self.git("checkout", "-q", "main")
        self.write(TUI_PATH, "base-only TUI change\n")
        base = self.commit()
        paths = selector.changed_paths("pull_request", self.pull_request(base, head), self.repo)
        self.assertEqual(paths, ["README.md"])
        self.assertFalse(selector.affects_tui(paths))
        # A force-push between those same tips must include the reverted TUI change.
        pushed = selector.changed_paths("push", {"before": base, "after": head}, self.repo)
        self.assertEqual(set(pushed), {"README.md", TUI_PATH})
        self.assertTrue(selector.affects_tui(pushed))

    def test_renames_deletions_and_nul_filenames(self):
        moved = "elsewhere/name with\nnewline.rs"
        self.git("mv", TUI_PATH, "removed.rs")
        unusual_tui = "crates/bootc-migrate/src/tui/new\n file.rs"
        self.write(unusual_tui, "new\n")
        self.write(moved, "rename into TUI next\n")
        first = self.commit()
        paths = selector.changed_paths("push", {"before": self.base, "after": first}, self.repo)
        self.assertEqual(set(paths), {TUI_PATH, "removed.rs", unusual_tui, moved})
        self.assertTrue(selector.affects_tui(paths))
        self.git("mv", moved, TUI_PATH)
        self.git("rm", unusual_tui)
        second = self.commit()
        paths = selector.changed_paths("push", {"before": first, "after": second}, self.repo)
        self.assertEqual(set(paths), {moved, TUI_PATH, unusual_tui})
        self.assertTrue(selector.affects_tui(paths))

    def test_new_branch_reads_entire_tree(self):
        self.write("README.md", "last commit only touched docs\n")
        head = self.commit()
        paths = selector.changed_paths(
            "push", {"before": selector.ZERO_SHA, "after": head}, self.repo,
        )
        self.assertEqual(set(paths), {"README.md", TUI_PATH})
        self.assertTrue(selector.affects_tui(paths))

    def test_missing_history_and_unrelated_pr_fail_closed(self):
        with self.assertRaises(subprocess.CalledProcessError):
            selector.changed_paths("push", {"before": "f" * 40, "after": self.base}, self.repo)
        self.git("checkout", "-q", "--orphan", "unrelated")
        self.git("rm", "-rf", ".")
        self.write("other.txt", "unrelated root\n")
        head = self.commit()
        with self.assertRaises(subprocess.CalledProcessError):
            selector.changed_paths("pull_request", self.pull_request(self.base, head), self.repo)

    def cli(self, event, payload):
        event_file = self.repo / "event.json"
        event_file.write_text(json.dumps(payload))
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--event", event, "--event-path", str(event_file),
             "--run-number", "2", "--size", "2"], cwd=self.repo,
            input=json.dumps(SCENARIOS), text=True, capture_output=True, check=False,
        )

    def test_cli_success_and_error_outputs(self):
        for event, payload, expected in [
            ("push", {"before": selector.ZERO_SHA, "after": self.base},
             [SCENARIOS[0], SCENARIOS[2], SCENARIOS[-1]]),
            ("push", {"before": self.base, "after": self.base}, SCENARIOS[:1] + SCENARIOS[2:3]),
            ("schedule", {}, SCENARIOS),
            ("workflow_dispatch", {}, SCENARIOS),
        ]:
            with self.subTest(event=event, payload=payload):
                result = self.cli(event, payload)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(json.loads(result.stdout)["scenarios"], expected)
                self.assertEqual(result.stderr, "")
        for payload in [{}, {"before": "f" * 40, "after": self.base}]:
            with self.subTest(payload=payload):
                result = self.cli("push", payload)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertIn("E2E planning failed:", result.stderr)


if __name__ == "__main__":
    unittest.main()
