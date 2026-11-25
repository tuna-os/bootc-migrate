"""Fixtures for the TUI receipt/adapter contract (issue #470). No VM,
no migration: the driver runs against tiny pty child programs."""

import importlib.util
import json
import os
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import tui_receipt as tr  # noqa: E402

_spec = importlib.util.spec_from_file_location("tui_driver", HERE / "tui-e2e-driver.py")
drv = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(drv)

# Child: redraws a fragmented frame (split escape + split UTF-8) at start
# and on every SIGWINCH, like ratatui does.
FRAGMENTED = textwrap.dedent("""
    import os, signal, sys, time
    def w(b):
        os.write(1, b); time.sleep(0.02)
    def frame():
        w(b"\\x1b[2J\\x1b[")
        w(b"3;5HStep 1 of")
        w(b" 5 caf\\xc3")
        w(b"\\xa9\\x1b[4;5H\\x1b[4")
        w(b"1mBRAND\\x1b[0m")
    pending = [True]
    signal.signal(signal.SIGWINCH, lambda *a: pending.__setitem__(0, True))
    for _ in range(600):
        if pending[0]:
            pending[0] = False
            frame()
        time.sleep(0.05)
""")
STATIC_NO_REDRAW = textwrap.dedent("""
    import os, signal, time
    signal.signal(signal.SIGWINCH, signal.SIG_IGN)
    os.write(1, b"\\x1b[2J\\x1b[3;5HStep 1 of 5")
    for _ in range(600): time.sleep(0.05)
""")
NEVER_QUIET = textwrap.dedent("""
    import os, time
    os.write(1, b"\\x1b[3;5HStep 1 of 5")
    for i in range(600):
        os.write(1, b"\\x1b[9;1Htick %d" % i); time.sleep(0.05)
""")
EXITS = "import os; os.write(1, b'\\x1b[3;5HStep 1 of 5')"


def make(script, **kw):
    d = drv.TuiDriver([sys.executable, "-c", script], None, **kw)
    d.QUIET = 0.2
    return d


def receipt(**kw):
    kw.setdefault("scenario", "t")
    kw.setdefault("run_id", "r1")
    kw.setdefault("cols", 220)
    kw.setdefault("rows", 50)
    return tr.Receipt(**kw)


def finish(r, ok=True):
    r.set_cleanup("ok" if ok else "failed")
    return r.to_dict()


class DriverFixtures(unittest.TestCase):
    def tearDown(self):
        for d in getattr(self, "drivers", []):
            try:
                os.kill(d.pid, 9)
                os.waitpid(d.pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass

    def spawn(self, script, **kw):
        d = make(script, **kw)
        self.drivers = getattr(self, "drivers", []) + [d]
        return d

    def test_fragmented_redraw_settles_and_matches(self):
        r = receipt()
        d = self.spawn(FRAGMENTED, receipt=r)
        self.assertEqual(d.try_find("Step 1 of 5 café", 5), 0)
        value, settled = d.fresh_check(lambda: d.row_containing("BRAND"), 8)
        self.assertTrue(settled)
        self.assertIsNotNone(value)
        d.note_state("welcome")
        self.assertEqual(r.transitions[0]["state"], "welcome")
        self.assertIn("matched", [w["outcome"] for w in r.waits])

    def test_missing_redraw_is_unobservable_not_stale_green(self):
        r = receipt()
        d = self.spawn(STATIC_NO_REDRAW, receipt=r)
        self.assertEqual(d.try_find("Step 1 of 5", 3), 0)
        # The stale grid contains the text, but no fresh frame arrived.
        value, settled = d.fresh_check(lambda: d.row_containing("Step 1 of 5"), 1.5)
        self.assertFalse(settled)
        self.assertIsNone(value)
        status = r.check("welcome-fresh", tr.TEXT_GRID, tr.PASS, requires_fresh=True,
                         settled=settled)
        self.assertEqual(status, tr.UNOBSERVABLE)

    def test_settle_timeout_when_screen_never_goes_quiet(self):
        d = self.spawn(NEVER_QUIET, receipt=receipt())
        d.try_find("Step 1 of 5", 3)
        value, settled = d.fresh_check(lambda: True, 1.5)
        self.assertFalse(settled)
        self.assertIsNone(value)
        self.assertEqual(d.receipt.waits[-1]["outcome"], "timeout")

    def test_child_exit_reported(self):
        d = self.spawn(EXITS, receipt=receipt())
        self.assertIsNone(d.try_find("never on screen", 3))
        self.assertEqual(d.receipt.waits[-1]["outcome"], "child_exit")
        _, settled = d.fresh_check(lambda: True, 2)
        self.assertFalse(settled)

    def test_wrong_state_times_out_with_receipt_wait(self):
        d = self.spawn(FRAGMENTED, receipt=receipt())
        self.assertIsNone(d.try_find("Select Target Image", 1))
        self.assertEqual(d.receipt.waits[-1]["outcome"], "timeout")

    def test_send_records_sanitized_input(self):
        r = receipt()
        d = self.spawn(FRAGMENTED, receipt=r)
        d.WINCH_SETTLE = 0
        d.send("\r", "advance")
        d.send("hunter2", "type passphrase", secret=True)
        keys = [i["keys"] for i in r.inputs]
        self.assertEqual(keys[0], "ENTER")
        self.assertNotIn("hunter2", json.dumps(r.to_dict()))
        self.assertTrue(keys[1].startswith("<redacted 7 chars"))


class ReceiptContract(unittest.TestCase):
    def good(self, **kw):
        r = receipt(**kw)
        r.transition("welcome", "g1")
        r.transition("review", "g2")
        r.check("welcome", tr.TEXT_GRID, tr.PASS)
        r.check("advance", tr.INTERACTION, tr.PASS)
        return r

    def test_positive_receipt_verifies(self):
        d = finish(self.good())
        self.assertEqual(d["verdict"], tr.PASS)
        self.assertEqual(tr.verify_receipt(
            d, required_checks=["welcome", "advance"],
            expected_transitions=["welcome", "review"]), [])

    def test_absent_receipt(self):
        self.assertEqual(tr.verify_receipt(None), ["receipt absent"])
        with tempfile.TemporaryDirectory() as t, self.assertRaises(tr.ReceiptError):
            tr.load_receipt(os.path.join(t, "nope.json"))

    def test_binary_identity_mismatch(self):
        with tempfile.NamedTemporaryFile() as f:
            f.write(b"binary-a")
            f.flush()
            d = finish(self.good(binary=f.name, source_revision="abc"))
        self.assertEqual(tr.verify_receipt(d, tr.sha256_hex(b"binary-a"), "abc"), [])
        self.assertIn("binary identity mismatch",
                      tr.verify_receipt(d, tr.sha256_hex(b"binary-b")))
        self.assertIn("source revision mismatch", tr.verify_receipt(d, None, "def"))

    def test_skipped_check_and_out_of_order(self):
        d = finish(self.good())
        self.assertTrue(any("skipped" in p for p in
                            tr.verify_receipt(d, required_checks=["backend-file"])))
        self.assertTrue(any("out of order" in p for p in
                            tr.verify_receipt(d, expected_transitions=["review", "welcome"])))

    def test_failed_and_unobservable_checks_fail_receipt(self):
        r = self.good()
        r.check("fresh", tr.TEXT_GRID, tr.PASS, requires_fresh=True, settled=False)
        d = finish(r)
        self.assertEqual(d["verdict"], tr.FAIL)
        self.assertTrue(tr.verify_receipt(d))

    def test_forged_pass_without_settle_is_rejected(self):
        d = finish(self.good())
        d["checks"].append({"id": "x", "capability": tr.TEXT_GRID, "status": tr.PASS,
                            "required": True, "fresh_required": True, "settled": False})
        self.assertTrue(any("without a settled" in p for p in tr.verify_receipt(d)))

    def test_dry_run_never_qualifies_migration(self):
        r = self.good(mode=tr.MODE_DRY_RUN)
        r.check("intent", tr.BACKEND_INTENT, tr.PASS)
        d = finish(r)
        self.assertFalse(d["qualifies_actual_migration"])
        live = self.good(mode=tr.MODE_LIVE)
        live.check("intent", tr.BACKEND_INTENT, tr.PASS)
        self.assertTrue(finish(live)["qualifies_actual_migration"])
        d["qualifies_actual_migration"] = True  # forged
        self.assertTrue(any("claims to qualify" in p for p in tr.verify_receipt(d)))

    def test_missing_cleanup_fails(self):
        d = self.good().to_dict()
        self.assertIn("cleanup not completed", tr.verify_receipt(d))

    def test_first_failure_evidence_bounded_and_allowlisted(self):
        with tempfile.TemporaryDirectory() as t:
            r = receipt(export_dir=t)
            r.register_secret("s3cret")
            r.record_failure("boom s3cret", "grid s3cret\x1b", "x" * (tr.MAX_EVIDENCE_BYTES + 5))
            r.record_failure("second", "other", "other")
            self.assertEqual(r.first_failure["why"], "boom <redacted>")
            self.assertTrue(r.first_failure["transcript"]["truncated"])
            names = sorted(os.listdir(t))
            self.assertEqual(len(names), 3)
            self.assertNotIn("s3cret", Path(t, names[0]).read_text())
            with self.assertRaises(ValueError):
                r.add_evidence("env_dump", "x", "x")


# ---- pixel scope boundary -------------------------------------------

def synthetic_raster(stream, run_id, state_seq, grid_sha, cell=(4, 8)):
    """TEST-ONLY stand-in for an independent terminal rasterizer: paints
    each cell's SGR background (40-47) into RGB. It parses the raw ANSI
    itself and shares no code with the driver's Screen."""
    palette = {41: (205, 0, 0), 42: (0, 205, 0), 44: (0, 0, 238)}
    cols, rows = 40, 6
    bg = [[(0, 0, 0)] * cols for _ in range(rows)]
    row = col = 0
    cur = (0, 0, 0)
    i = 0
    while i < len(stream):
        if stream[i] == "\x1b":
            end = stream.index("m", i) if stream[i:].startswith("\x1b[") and "m" in stream[i:] \
                and "H" not in stream[i:stream.index("m", i)] else None
            if end is not None:
                code = int(stream[i + 2:end] or 0)
                cur = palette.get(code, (0, 0, 0)) if code else (0, 0, 0)
                i = end + 1
                continue
            h = stream.index("H", i)
            row, col = [int(x) - 1 for x in stream[i + 2:h].split(";")]
            i = h + 1
            continue
        bg[row][col] = cur
        col += 1
        i += 1
    cw, ch = cell
    rgb = bytearray()
    for y in range(rows * ch):
        for x in range(cols * cw):
            rgb += bytes(bg[y // ch][x // cw])
    return tr.Raster(run_id, state_seq, grid_sha,
                     {"name": "synthetic-test-rasterizer", "version": "0",
                      "font": "none (cell fills only)"},
                     cols * cw, rows * ch, cw, ch, bytes(rgb))


class PixelScope(unittest.TestCase):
    RED = "\x1b[1;1H\x1b[41mBRAND\x1b[0m"
    BLUE = "\x1b[1;1H\x1b[44mBRAND\x1b[0m"

    def grid_of(self, stream):
        s = drv.Screen(40, 6)
        s.feed(stream.encode())
        return s.text()

    def test_text_oracle_cannot_tell_styles_apart(self):
        self.assertEqual(self.grid_of(self.RED), self.grid_of(self.BLUE))

    def run_claim(self, shown, rendered, claims):
        r = receipt(claims=claims)
        r.transition("brand", self.grid_of(shown))
        r.check("brand-text", tr.TEXT_GRID, tr.PASS)
        r.check("driver", tr.INTERACTION, tr.PASS)
        if rendered is not None:
            raster = synthetic_raster(rendered, "r1", 1, r.transitions[0]["grid_sha256"])
            r.check_pixel_cell("brand-bg-red", raster, 0, 1, (205, 0, 0))
        return finish(r)

    def test_wrong_styling_passes_text_but_fails_pixel_claim(self):
        d = self.run_claim(self.BLUE, self.BLUE, (tr.TEXT_GRID, tr.INTERACTION, tr.PIXEL))
        self.assertEqual(d["capabilities"][tr.TEXT_GRID], tr.PASS)
        self.assertEqual(d["capabilities"][tr.PIXEL], tr.FAIL)
        self.assertEqual(d["verdict"], tr.FAIL)

    def test_right_styling_passes_pixel_claim(self):
        d = self.run_claim(self.RED, self.RED, (tr.TEXT_GRID, tr.INTERACTION, tr.PIXEL))
        self.assertEqual(d["verdict"], tr.PASS)
        self.assertEqual(tr.verify_receipt(d), [])
        self.assertEqual(d["pixel_bindings"][0]["renderer"]["name"], "synthetic-test-rasterizer")

    def test_missing_pixel_evidence_fails_required_claim(self):
        d = self.run_claim(self.RED, None, (tr.TEXT_GRID, tr.INTERACTION, tr.PIXEL))
        self.assertEqual(d["capabilities"][tr.PIXEL], tr.NOT_RUN)
        self.assertEqual(d["verdict"], tr.FAIL)
        self.assertIn("pixel claim has no raster evidence", tr.verify_receipt(d))

    def test_text_only_claim_leaves_pixels_explicitly_not_run(self):
        d = self.run_claim(self.RED, None, (tr.TEXT_GRID, tr.INTERACTION))
        self.assertEqual(d["verdict"], tr.PASS)
        self.assertEqual(d["capabilities"][tr.PIXEL], tr.NOT_RUN)

    def test_raster_must_be_bound_to_run_state_and_renderer(self):
        r = receipt(claims=(tr.PIXEL,))
        r.transition("brand", self.grid_of(self.RED))
        good = r.transitions[0]["grid_sha256"]
        for bad in (synthetic_raster(self.RED, "other-run", 1, good),
                    synthetic_raster(self.RED, "r1", 1, "0" * 64),
                    synthetic_raster(self.RED, "r1", 2, good)):
            self.assertEqual(r.check_pixel_cell("c", bad, 0, 1, (205, 0, 0)), tr.FAIL)
        anon = synthetic_raster(self.RED, "r1", 1, good)
        anon.renderer = {}
        self.assertEqual(r.check_pixel_cell("c", anon, 0, 1, (205, 0, 0)), tr.FAIL)


if __name__ == "__main__":
    unittest.main()
