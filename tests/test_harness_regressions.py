"""Exercise the harness inputs that failed E2E run 39227338849."""

import configparser
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BASE = (ROOT / "tests/package-base.sh").read_text()
HARNESS = (ROOT / "tests/run-e2e.sh").read_text()


class HarnessRegressions(unittest.TestCase):
    def test_cloud_init_package_names_match_the_base(self):
        # Execute the real distro selection and real cloud-config heredoc,
        # without downloading or modifying a disk.
        selection = BASE[BASE.index('case "$PACKAGE_BASE" in') : BASE.index("\nesac") + 6]
        seed = BASE[
            BASE.index('cat > "$USER_DATA" <<EOF') : BASE.index(
                "\nEOF", BASE.index('cat > "$USER_DATA" <<EOF')
            )
            + 4
        ]
        for base, expected in [
            ("ubuntu-cloud", "network-manager"),
            ("fedora-cloud", "NetworkManager"),
            ("fedora-workstation", "NetworkManager"),
        ]:
            with self.subTest(base=base), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "cloud-config"
                env = {
                    **os.environ,
                    "PACKAGE_BASE": base,
                    "USER_DATA": str(output),
                    "SSH_PUBLIC_KEY": "ssh-ed25519 TEST",
                    "FEDORA_CLOUD_URL": "https://example/fedora",
                    "UBUNTU_CLOUD_URL": "https://example/ubuntu",
                    "FEDORA_CLOUD_SHA256": "0",
                    "UBUNTU_CLOUD_SHA256": "0",
                }
                subprocess.run(["bash", "-euc", selection + "\n" + seed], env=env, check=True)
                packages = output.read_text().split("packages:\n", 1)[1].split("runcmd:", 1)[0]
                self.assertEqual(
                    [line.removeprefix("  - ") for line in packages.splitlines()],
                    ["openssh-server", "podman", expected],
                )

    def test_package_manager_fixture_is_valid_ini_and_keeps_marker(self):
        # Evaluate the exact echo payload written immediately before dnf runs.
        line = next(
            line.strip()
            for line in HARNESS.splitlines()
            if "e2e-package-manager-edit" in line and ">> /etc/dnf/dnf.conf" in line
        )
        command = line.split(" >> ", 1)[0]
        marker = subprocess.check_output(["bash", "-ec", command], text=True)
        config = configparser.ConfigParser()
        config.read_string("[main]\ngpgcheck=1\n" + marker)
        self.assertEqual(dict(config["main"]), {"gpgcheck": "1"})
        self.assertIn("e2e-package-manager-edit", marker)
        self.assertIn("package-manager sidecar does not contain the source edit", HARNESS)

    def test_bootloader_trial_uses_writable_staging_through_undo(self):
        block = HARNESS.split('if [ "$E2E_MODE" = "bootloader-trial" ]; then', 1)[1]
        block = block.split('step "===', 1)[0]
        self.assertNotIn("/usr/local/bin", block)
        self.assertIn("umask 077; mktemp -d /var/tmp/bootc-trial.XXXXXXXX", block)
        for operation in ["--to systemd-boot", "--promote", "--undo"]:
            calls = [
                line for line in HARNESS.splitlines() if "migrate-bootloader " + operation in line
            ]
            self.assertEqual(len(calls), 1)
            self.assertIn("$TRIAL_BIN_DIR/bootc-rebase", calls[0])

    def test_discovery_diagnostics_preserve_stage_failure(self):
        start = HARNESS.index(
            "    if ! ssh $SSH_OPTS root@localhost",
            HARNESS.index('step "=== bootloader-trial: staging'),
        )
        end = HARNESS.index("    TRIAL_ID=", start)
        block = HARNESS[start:end]
        for stage_status, diagnostic_status in [(1, 0), (1, 23), (0, 0)]:
            with self.subTest(stage=stage_status, diagnostic=diagnostic_status):
                with tempfile.TemporaryDirectory() as directory:
                    script = block.replace(
                        "/tmp/bootloader-trial-stage.log", str(Path(directory) / "stage.log")
                    )
                    stubs = """
ssh() {
    case "$*" in
        *migrate-bootloader*) echo stage-output; return "$STAGE_STATUS" ;;
        *) cat >/dev/null; echo diagnostic-output; return "$DIAGNOSTIC_STATUS" ;;
    esac
}
"""
                    result = subprocess.run(
                        ["bash", "-euc", stubs + script],
                        env={
                            **os.environ,
                            "SSH_OPTS": "",
                            "TRIAL_BIN_DIR": "/fixture",
                            "VM_TARGET_IMAGE": "example.invalid/test",
                            "STAGE_STATUS": str(stage_status),
                            "DIAGNOSTIC_STATUS": str(diagnostic_status),
                        },
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    self.assertEqual(result.returncode, stage_status)
                    self.assertIn("[bootloader-stage] stage-output", result.stdout)
                    self.assertEqual("diagnostic-output" in result.stdout, stage_status != 0)
                    self.assertEqual(
                        "WARN: could not collect" in result.stdout, diagnostic_status != 0
                    )

    def test_remote_diagnostics_retain_early_failures_and_continue(self):
        body = HARNESS.split("<<'TRIAL_DISCOVERY_DIAGNOSTICS'\n", 1)[1].split(
            "\nTRIAL_DISCOVERY_DIAGNOSTICS", 1
        )[0]
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            (fixture / "cmdline").write_text("ostree=/fixture/deployment\n")
            bls = fixture / "boot/loader/entries"
            bls.mkdir(parents=True)
            (bls / "fixture.conf").write_text("title test fixture\n")
            body = body.replace("/proc/cmdline", str(fixture / "cmdline"))
            for path in ["/sysroot", "/usr/lib/modules", "/boot"]:
                body = body.replace(path, str(fixture / path.lstrip("/")))
            stubs = """
ostree() { echo ostree-attempt; return "$OSTREE_STATUS"; }
readlink() { echo readlink-attempt; return "$READLINK_STATUS"; }
find() { echo find-attempt; return "$FIND_STATUS"; }
uname() { echo fixture-kernel; }
"""
            for failures in [(0, 0, 0), (7, 0, 0), (0, 7, 0), (0, 0, 7)]:
                with self.subTest(failures=failures):
                    result = subprocess.run(
                        ["bash", "-uc", stubs + body],
                        env={
                            **os.environ,
                            **dict(
                                zip(
                                    ["OSTREE_STATUS", "READLINK_STATUS", "FIND_STATUS"],
                                    map(str, failures),
                                    strict=True,
                                )
                            ),
                        },
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    self.assertEqual(result.returncode, int(any(failures)))
                    self.assertIn("ostree-attempt", result.stdout)
                    self.assertIn("readlink-attempt", result.stdout)
                    self.assertIn("fixture-kernel", result.stdout)
                    self.assertIn("title test fixture", result.stdout)

    def test_remote_setup_propagates_failed_executable_staging(self):
        setup = HARNESS.split("<<'TRIAL_BIN_SETUP'\n", 1)[1].split("\nTRIAL_BIN_SETUP", 1)[0]
        with tempfile.TemporaryDirectory() as directory:
            for name in ["bootc-rebase", "bootc-migrate"]:
                (Path(directory) / name).touch()
            # Inject only stat's root ownership result; no host permission changes.
            stubs = "stat() { if [[ $2 == '%u:%a' ]]; then echo 0:700; else echo 0; fi; }; "
            result = subprocess.run(
                ["bash", "-c", stubs + "chmod() { return 17; }; " + setup, "bash", directory],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 17)
            # Replacing a staged binary with a symlink must fail before chmod.
            binary = Path(directory) / "bootc-rebase"
            binary.unlink()
            binary.symlink_to("bootc-migrate")
            result = subprocess.run(
                ["bash", "-c", stubs + "chmod() { echo REACHED; }; " + setup, "bash", directory],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn("REACHED", result.stdout)


GEOCLUE_TIMEOUT_LOG = """\
\x1b[1;34m[e2e 21:16:01] === post-migration: system health ===\x1b[0m
[health]   system state: degraded
[health]   boot completed degraded; failed units are checked below
[health] FAIL: unit failed: geoclue.service
[health]     \u00d7 geoclue.service - Location Lookup Service
[health]          Loaded: loaded (/usr/lib/systemd/system/geoclue.service; static)
[health]          Active: failed (Result: timeout) since Fri 2026-11-13 21:16:58 UTC; 3min ago
[health]         Process: 3726 ExecStart=/usr/libexec/geoclue (code=killed, signal=TERM)
[health]   ok: system D-Bus is active
FAIL: the migrated system is not healthy (see the [health] lines above)
"""


class KnownFlakeSignature(unittest.TestCase):
    """tests/e2e-known-flake.sh decides the one e2e retry (#368)."""

    def matches(self, log):
        with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as handle:
            handle.write(log)
        try:
            result = subprocess.run(
                [str(ROOT / "tests/e2e-known-flake.sh"), handle.name], check=False
            )
        finally:
            os.unlink(handle.name)
        return result.returncode == 0

    def test_geoclue_timeout_alone_matches(self):
        self.assertTrue(self.matches(GEOCLUE_TIMEOUT_LOG))
        # A baseline health report of the same unit does not change that.
        baseline = "[health] FAIL: unit failed: geoclue.service\n"
        baseline += "NOTE: baseline health findings above are reported, not asserted\n"
        self.assertTrue(self.matches(baseline + GEOCLUE_TIMEOUT_LOG))

    def test_every_other_failure_does_not_match(self):
        verdict = "FAIL: the migrated system is not healthy (see the [health] lines above)\n"
        cases = {
            "empty log": "",
            "passing run": "OK: migration complete\n",
            "a second failed unit": GEOCLUE_TIMEOUT_LOG.replace(
                verdict,
                "[health] FAIL: unit failed: power-profiles-daemon.service\n"
                "[health]          Active: failed (Result: timeout)\n" + verdict,
            ),
            "another unit times out": GEOCLUE_TIMEOUT_LOG.replace(
                "geoclue.service", "gdm.service"
            ),
            "geoclue crashed, not timed out": GEOCLUE_TIMEOUT_LOG.replace(
                "Result: timeout", "Result: core-dump"
            ),
            "another assertion failed": GEOCLUE_TIMEOUT_LOG
            + "FAIL: /home did not survive\n",
            "an earlier assertion failed": "FAIL: the ESP was not restored\n"
            + GEOCLUE_TIMEOUT_LOG,
            "an error elsewhere": "ERROR: bootc install failed\n" + GEOCLUE_TIMEOUT_LOG,
            "a panic elsewhere": "thread 'main' panicked at src/main.rs\n"
            + GEOCLUE_TIMEOUT_LOG,
            "no health verdict": GEOCLUE_TIMEOUT_LOG.replace(verdict, ""),
            "health failed for another check": GEOCLUE_TIMEOUT_LOG.replace(
                verdict, "[health] FAIL: logind does not answer\n" + verdict
            ),
        }
        for name, log in cases.items():
            with self.subTest(name):
                self.assertFalse(self.matches(log))

    def test_workflow_retries_once_and_only_on_the_signature(self):
        workflow = (ROOT / ".github/workflows/e2e-tests.yml").read_text()
        step = workflow[workflow.index("- name: Run E2E test suite") :]
        step = step[: step.index("\n      - name:", 1)]
        self.assertEqual(step.count("./tests/run-e2e.sh"), 2)
        self.assertIn("./tests/e2e-known-flake.sh e2e-run.log", step)
        self.assertIn("#368", step)


if __name__ == "__main__":
    unittest.main()
