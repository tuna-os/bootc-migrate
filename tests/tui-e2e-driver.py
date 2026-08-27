#!/usr/bin/env python3
"""Expect-style pty driver for bootc-migrate's interactive TUIs.

Runs entirely on the system under test (the E2E VM, a Corral VM, or a
developer machine) with nothing beyond the python3 standard library: it
spawns the TUI binary on a pseudo-terminal, waits for screen-identifying
text, and types the keys a human would. `tests/run-e2e.sh`'s tui-migrate
mode uploads this file and uses it to drive a real migration through the
wizard instead of the plain CLI invocation, closing the "no TUI
assertions in CI" gap tracked on #15 — the Rust unit tests already prove
the state machines and rendering (`tui::tests`, `drift_review::tests`);
this driver is what proves the raw terminal event loops on a live system.

Matching strategy: ratatui only rewrites cells that changed between
frames, so the raw output stream fragments text arbitrarily (a screen
switch can emit only the differing characters of a heading). Instead of
grepping the stream, this driver maintains a character grid by
interpreting the cursor-positioning/erase sequences ratatui emits, and
matches patterns against whole grid rows. While waiting it periodically
nudges the pty size (SIGWINCH), which makes ratatui clear and fully
repaint, so the grid converges to the true screen no matter what was on
it before — and it never types while such a nudge may still be in
flight, because a key racing the resize event gets dropped.

Modes:
  wizard                Drive the full migration wizard. Configures the
                        run to match the scripted E2E invocation
                        (`--force --skip-import`, dry-run toggled OFF
                        unless --dry-run is given), waits for the
                        migration to finish, and walks to the final
                        screen.
  wizard-expect-failure Same navigation, defaults left alone (dry-run
                        stays on), for hosts where the migration must
                        fail fast (e.g. a non-OSTree dev box): asserts
                        the Failed screen and a clean exit instead.
  drift                 Drive `bootc-migrate etc-drift --interactive`:
                        uncheck the first entry, confirm, and let the
                        caller assert the written manifest.

Exit status: 0 when the driven flow reached its expected end state,
1 otherwise (with a screen dump for triage).
"""

import argparse
import codecs
import errno
import fcntl
import os
import re
import select
import signal
import struct
import sys
import termios
import time

# One token of terminal output: a CSI sequence, an OSC sequence, a
# charset/keypad escape, or a single character of text.
TOKEN_RE = re.compile(
    r"\x1b\[(?P<csi_params>[0-9;?]*)(?P<csi_final>[A-Za-z])"
    r"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"
    r"|\x1b[()][0-9A-Za-z]"
    r"|\x1b[=>]"
    r"|(?P<ch>[^\x1b])",
    re.DOTALL,
)

ENTER = "\r"
COLS = 220
ROWS = 50


def log(msg: str) -> None:
    print(f"[tui-driver {time.strftime('%H:%M:%S')}] {msg}", flush=True)


class Screen:
    """A minimal terminal-grid emulator covering what ratatui emits:
    cursor positioning (CUP), relative moves, erase display/line, CR/LF,
    and printable text. Styling (SGR) and modes are ignored."""

    def __init__(self, cols: int, rows: int):
        self.cols = cols
        self.rows = rows
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = 0
        self.col = 0
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.pending = ""

    def feed(self, chunk: bytes) -> None:
        text = self.pending + self.decoder.decode(chunk)
        pos = 0
        while pos < len(text):
            # Hold back a trailing partial escape sequence for the next feed.
            if text[pos] == "\x1b":
                m = TOKEN_RE.match(text, pos)
                if not m:
                    break
                self._token(m)
                pos = m.end()
            else:
                self._char(text[pos])
                pos += 1
        self.pending = text[pos:]

    def _token(self, m: re.Match) -> None:
        if m.group("csi_final") is None:
            return  # OSC / charset / keypad: no grid effect
        final = m.group("csi_final")
        params = m.group("csi_params")
        nums = [int(p) for p in params.replace("?", "").split(";") if p.isdigit()]

        def arg(i, default):
            return nums[i] if i < len(nums) else default

        if final in ("H", "f"):
            self.row = min(max(arg(0, 1) - 1, 0), self.rows - 1)
            self.col = min(max(arg(1, 1) - 1, 0), self.cols - 1)
        elif final == "A":
            self.row = max(self.row - arg(0, 1), 0)
        elif final == "B":
            self.row = min(self.row + arg(0, 1), self.rows - 1)
        elif final == "C":
            self.col = min(self.col + arg(0, 1), self.cols - 1)
        elif final == "D":
            self.col = max(self.col - arg(0, 1), 0)
        elif final == "G":
            self.col = min(max(arg(0, 1) - 1, 0), self.cols - 1)
        elif final == "d":
            self.row = min(max(arg(0, 1) - 1, 0), self.rows - 1)
        elif final == "J":
            mode = arg(0, 0)
            if mode == 2:
                for r in range(self.rows):
                    self.grid[r] = [" "] * self.cols
            elif mode == 0:
                self.grid[self.row][self.col:] = [" "] * (self.cols - self.col)
                for r in range(self.row + 1, self.rows):
                    self.grid[r] = [" "] * self.cols
            elif mode == 1:
                for r in range(self.row):
                    self.grid[r] = [" "] * self.cols
                self.grid[self.row][: self.col + 1] = [" "] * (self.col + 1)
        elif final == "K":
            mode = arg(0, 0)
            if mode == 0:
                self.grid[self.row][self.col:] = [" "] * (self.cols - self.col)
            elif mode == 1:
                self.grid[self.row][: self.col + 1] = [" "] * (self.col + 1)
            elif mode == 2:
                self.grid[self.row] = [" "] * self.cols
        # SGR ('m'), modes ('h'/'l'), and the rest: no grid effect.

    def _char(self, ch: str) -> None:
        if ch == "\r":
            self.col = 0
        elif ch == "\n":
            self.row = min(self.row + 1, self.rows - 1)
        elif ch in ("\x07", "\x00"):
            pass
        elif ch == "\b":
            self.col = max(self.col - 1, 0)
        elif ch == "\t":
            self.col = min((self.col // 8 + 1) * 8, self.cols - 1)
        else:
            self.grid[self.row][self.col] = ch
            if self.col < self.cols - 1:
                self.col += 1

    def rows_text(self):
        return ["".join(r) for r in self.grid]

    def text(self) -> str:
        return "\n".join(self.rows_text())


class TuiDriver:
    """Spawn a TUI process on a pty and expect/send against its screen."""

    def __init__(self, argv, transcript_path):
        self.argv = argv
        self.transcript_path = transcript_path
        self.screen = Screen(COLS, ROWS)
        self.rows = ROWS
        # Monotonic time of the last winsize nudge. A key written while
        # the TUI is still handling the resulting SIGWINCH/resize event
        # can be dropped by its input backend (observed reproducibly), so
        # send() refuses to type until WINCH_SETTLE has passed.
        self.last_winch = 0.0
        self.pid, self.fd = os.forkpty()
        if self.pid == 0:  # child
            os.environ["TERM"] = "xterm-256color"
            try:
                os.execvp(argv[0], argv)
            finally:  # pragma: no cover - exec failed
                os._exit(127)
        self._set_winsize(ROWS, COLS)

    def _set_winsize(self, rows: int, cols: int) -> None:
        winsz = struct.pack("HHHH", rows, cols, 0, 0)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, winsz)

    def force_repaint(self) -> None:
        """Nudge the pty size so ratatui clears and repaints the whole
        screen — the grid then reflects the real screen regardless of
        which cells earlier diff-draws skipped. The height alternates
        between ROWS and ROWS-1; every pattern this driver matches sits
        well above the last row, so the one-row difference is invisible
        to the assertions."""
        self.rows = ROWS - 1 if self.rows == ROWS else ROWS
        self._set_winsize(self.rows, COLS)
        self.last_winch = time.monotonic()

    def _drain(self, timeout: float) -> bool:
        """Read whatever the TUI wrote within `timeout`. False on EOF."""
        try:
            ready, _, _ = select.select([self.fd], [], [], timeout)
        except InterruptedError:
            return True
        if not ready:
            return True
        try:
            chunk = os.read(self.fd, 65536)
        except OSError as e:
            if e.errno == errno.EIO:  # pty closed: child exited
                return False
            raise
        if not chunk:
            return False
        self.screen.feed(chunk)
        return True

    def _find(self, patterns):
        rows = self.screen.rows_text()
        for i, pat in enumerate(patterns):
            if any(pat in row for row in rows):
                return i
        return None

    def try_find(self, patterns, timeout: float):
        """Wait for any of `patterns` on the screen; None on timeout.
        EOF is not fatal — the final screen state is still scanned."""
        if isinstance(patterns, str):
            patterns = [patterns]
        start = time.monotonic()
        deadline = start + timeout
        last_heartbeat = start
        # Scan passively first: a fresh screen's own draw usually carries
        # the pattern, and nudging a repaint while the key that triggered
        # the transition is still in flight can make the TUI drop it (see
        # last_winch). Only after a quiet second do we start nudging.
        next_repaint = start + 1.0
        eof = False
        while True:
            which = self._find(patterns)
            if which is not None:
                log(f"matched {patterns[which]!r}")
                return which
            now = time.monotonic()
            if now >= deadline or eof:
                return None
            if now - last_heartbeat >= 30:
                log(f"still waiting for {patterns!r} ({int(deadline - now)}s left)")
                last_heartbeat = now
            if now >= next_repaint:
                self.force_repaint()
                next_repaint = now + 5
            eof = not self._drain(min(0.5, deadline - now))

    def expect(self, patterns, timeout: float, label: str) -> int:
        if isinstance(patterns, str):
            patterns = [patterns]
        which = self.try_find(patterns, timeout)
        if which is None:
            self.fail(f"timeout ({timeout:.0f}s) waiting for {patterns!r} ({label})")
        return which

    def press_until(self, keys: str, patterns, attempts: int, label: str) -> int:
        """Press `keys`, wait briefly for `patterns`, repeat. The TUI
        ignores keys that aren't valid yet (e.g. Enter while the
        migration thread hasn't posted its Done message), so a single
        press can race the state it depends on."""
        if isinstance(patterns, str):
            patterns = [patterns]
        for _ in range(attempts):
            self.send(keys, label)
            which = self.try_find(patterns, 3)
            if which is not None:
                return which
        self.fail(f"{patterns!r} never appeared after {attempts}x {keys!r} ({label})")
        return 1  # unreachable

    WINCH_SETTLE = 1.0

    def send(self, keys: str, label: str) -> None:
        # Never type while a winsize nudge may still be mid-flight in the
        # TUI's event source — a key that races the resize gets dropped.
        settle = self.last_winch + self.WINCH_SETTLE - time.monotonic()
        while settle > 0:
            self._drain(settle)
            settle = self.last_winch + self.WINCH_SETTLE - time.monotonic()
        log(f"sending {keys!r} ({label})")
        os.write(self.fd, keys.encode())
        # Give the 50ms event loop a beat so key order is never a question.
        self._drain(0.2)

    def wait_exit(self, timeout: float) -> int:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid == self.pid:
                self._drain(0.1)
                if os.WIFEXITED(status):
                    return os.WEXITSTATUS(status)
                return 128 + os.WTERMSIG(status)
            self._drain(0.3)
        self.fail(f"TUI did not exit within {timeout:.0f}s")
        return 1  # unreachable

    def save_transcript(self) -> None:
        if not self.transcript_path:
            return
        with open(self.transcript_path, "w", encoding="utf-8") as f:
            f.write(self.screen.text())
        log(f"final screen written to {self.transcript_path}")

    def fail(self, why: str) -> None:
        log(f"FAIL: {why}")
        self.save_transcript()
        print("---- final screen ----", flush=True)
        for row in self.screen.rows_text():
            trimmed = row.rstrip()
            if trimmed:
                print(trimmed, flush=True)
        print("----------------------", flush=True)
        try:
            os.kill(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        sys.exit(1)


def navigate_to_review(driver: TuiDriver, args, configure_live_run: bool) -> None:
    """Welcome → Preflight → SelectImage (custom target) → Options → Review."""
    driver.expect("Step 1 of 5", 30, "welcome screen")
    driver.send(ENTER, "welcome -> preflight")
    # Preflight gathers real system data (repo size scan) before this
    # screen appears, so give it room.
    driver.expect("System Preflight", 180, "preflight screen")
    driver.send(ENTER, "preflight -> select image")
    driver.expect("Select Target Image", 30, "select image screen")

    # Walk past every preset to the "Custom…" row (the cursor clamps at
    # the last row, so overshooting is safe), open the editor, type the
    # target, and confirm it landed before advancing.
    driver.send("j" * 8, "cursor to custom row")
    driver.send(ENTER, "open custom image editor")
    driver.send(args.target_image, "type target image")
    driver.expect(args.target_image, 10, "typed image visible")
    driver.send(ENTER, "close editor")
    driver.send(ENTER, "select image -> options")
    driver.expect("Configure Options", 30, "options screen")

    if configure_live_run:
        # Mirror the scripted invocation the four MVP cells use:
        # dry-run off (unless asked for), skip-import on, force on.
        if not args.dry_run:
            driver.send(" ", "toggle dry-run off")
        driver.send("j", "cursor to skip-import")
        driver.send(" ", "toggle skip-import on")
        driver.send("jjj", "cursor to force")
        driver.send(" ", "toggle force on")
    driver.send("n", "options -> review")
    driver.expect("Review & Run", 30, "review screen")
    # The review screen must show the command we actually configured.
    driver.expect(args.target_image, 10, "target image in review command")


def run_wizard(args) -> None:
    driver = TuiDriver([args.binary, "tui"], args.transcript)
    navigate_to_review(driver, args, configure_live_run=True)
    if not args.dry_run:
        driver.expect("LIVE MIGRATION", 10, "live mode tag")
    driver.send(ENTER, "start migration")
    # "Phase 2 · OCI pull" only exists on the running screen's phase
    # sidebar (the options screen also says "OSTree import", so that
    # label would false-positive before the repaint).
    driver.expect("Phase 2 · OCI pull", 30, "running screen phase sidebar")

    driver.expect("MIGRATION COMPLETED", args.migration_timeout, "completion banner")
    driver.press_until(ENTER, "Migration Complete!", 20, "running -> complete")
    driver.send("q", "exit TUI")
    rc = driver.wait_exit(30)
    driver.save_transcript()
    if rc != 0:
        driver.fail(f"TUI exited with rc={rc}")
    log("wizard flow PASSED")


def run_wizard_expect_failure(args) -> None:
    """Self-test flow for hosts where the migration itself must fail fast
    (e.g. a developer container that is not an OSTree system): proves the
    wizard navigates, launches the run, surfaces the failure screen, and
    exits cleanly."""
    driver = TuiDriver([args.binary, "tui"], args.transcript)
    navigate_to_review(driver, args, configure_live_run=False)
    driver.send(ENTER, "start migration (will fail preflight)")
    driver.expect("Phase 2 · OCI pull", 30, "running screen phase sidebar")
    driver.press_until(ENTER, "Migration Failed", 20, "running -> failed")
    driver.send("q", "exit TUI")
    rc = driver.wait_exit(30)
    driver.save_transcript()
    if rc != 0:
        driver.fail(f"TUI exited with rc={rc}")
    log("wizard self-test (expected failure) PASSED")


def run_drift(args) -> None:
    argv = [args.binary, "etc-drift", "--interactive", "--output", args.output]
    driver = TuiDriver(argv, args.transcript)
    driver.expect("Config Drift Review", 60, "drift review screen")
    # Uncheck the cursor's entry so the manifest provably reflects a real
    # interaction (the harness asserts a `false` decision), then confirm.
    driver.send(" ", "uncheck first entry")
    driver.send(ENTER, "confirm review")
    rc = driver.wait_exit(60)
    driver.save_transcript()
    if rc != 0:
        driver.fail(f"etc-drift exited with rc={rc}")
    log("drift review flow PASSED")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=["wizard", "wizard-expect-failure", "drift"],
                        required=True)
    parser.add_argument("--binary", default="/var/tmp/bootc-migrate")
    parser.add_argument("--target-image", default="",
                        help="target image typed into the wizard (wizard modes)")
    parser.add_argument("--dry-run", action="store_true",
                        help="leave the wizard's dry-run toggle on")
    parser.add_argument("--migration-timeout", type=float, default=2100,
                        help="seconds to wait for the migration banner")
    parser.add_argument("--output", default="/var/tmp/bootc-migrate-etc-drift.json",
                        help="manifest path (drift mode)")
    parser.add_argument("--transcript", default="/var/tmp/tui-transcript.txt")
    args = parser.parse_args()

    if args.mode in ("wizard", "wizard-expect-failure") and not args.target_image:
        parser.error("--target-image is required in wizard modes")

    if args.mode == "wizard":
        run_wizard(args)
    elif args.mode == "wizard-expect-failure":
        run_wizard_expect_failure(args)
    else:
        run_drift(args)


if __name__ == "__main__":
    main()
