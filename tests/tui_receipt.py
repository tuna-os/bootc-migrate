"""Revision-bound evidence receipts for the PTY TUI oracle (issue #470).

`tests/tui-e2e-driver.py` reconstructs the terminal *text grid* and drives
the TUI through it. That is semantic-state evidence: it proves which screen
was showing and which keys moved it, and it says nothing about colour,
theme or branding, because the grid emulator ignores SGR styling. This
module turns a driver run into a machine-checkable receipt and keeps the
four kinds of claim apart:

  text_grid       the reconstructed grid showed the expected state
  interaction     keys were sent, and the expected transitions followed
  backend_intent  the UI state was correlated with a backend/file assertion
  pixel           a terminal-raster assertion bound to the same run/state

A capability is only ``pass`` when a check of that kind ran and passed.
``not_run`` and ``unobservable`` are never green: if a scenario *claims* a
capability (``claims``), the receipt verdict is ``fail`` unless that
capability passed. A dry-run or mock journey is labelled as such and can
never set ``qualifies_actual_migration``.

Standard library only (the driver runs on the system under test, python
3.9+). The module is import-safe: it never touches a pty.
"""

import hashlib
import json
import os
import re
import time

SCHEMA = "bootc-migrate.tui-receipt/v1"
HARNESS_VERSION = "1"

TEXT_GRID = "text_grid"
INTERACTION = "interaction"
BACKEND_INTENT = "backend_intent"
PIXEL = "pixel"
CAPABILITIES = (TEXT_GRID, INTERACTION, BACKEND_INTENT, PIXEL)

PASS = "pass"
FAIL = "fail"
NOT_RUN = "not_run"
UNOBSERVABLE = "unobservable"
STATUSES = (PASS, FAIL, NOT_RUN, UNOBSERVABLE)

# Only a run against the real system can qualify an actual migration.
MODE_LIVE = "live"
MODE_DRY_RUN = "dry-run"
MODE_MOCK = "mock"
MODES = (MODE_LIVE, MODE_DRY_RUN, MODE_MOCK)

# Exported evidence is allowlisted by kind and bounded in size.
EVIDENCE_KINDS = {
    "text_grid": "txt",
    "transcript": "txt",
    "input_trace": "json",
    "pixels": "ppm",
}
MAX_EVIDENCE_BYTES = 256 * 1024

# Keys that carry no user data and may be recorded verbatim in the input
# trace. Everything else (typed text, passphrases) is recorded as a
# length + hash only.
SAFE_KEYS = {
    "\r": "ENTER",
    "\x1b": "ESC",
    "\x1b[A": "UP",
    "\x1b[B": "DOWN",
    "\x1b[4~": "END",
    " ": "SPACE",
    "j": "j",
    "k": "k",
    "b": "b",
    "q": "q",
}


class ReceiptError(Exception):
    """The receipt is absent or not parseable."""


def sha256_hex(data) -> str:
    if isinstance(data, str):
        data = data.encode("utf-8", "replace")
    return hashlib.sha256(data).hexdigest()


def file_sha256(path):
    try:
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for block in iter(lambda: f.read(1 << 20), b""):
                h.update(block)
        return h.hexdigest()
    except OSError:
        return None


_CONTROL_RE = re.compile(r"[\x00-\x08\x0b-\x1f\x7f]")


class Receipt:
    """Accumulates one driver run's evidence. Not thread-safe (the driver
    is single-threaded)."""

    def __init__(self, scenario, run_id, attempt=1, binary=None,
                 source_revision=None, mode=MODE_LIVE, claims=(TEXT_GRID, INTERACTION),
                 term="xterm-256color", locale=None, cols=0, rows=0,
                 export_dir=None, clock=time.monotonic):
        if mode not in MODES:
            raise ValueError(f"unknown mode {mode!r}")
        for c in claims:
            if c not in CAPABILITIES:
                raise ValueError(f"unknown capability {c!r}")
        self.clock = clock
        self.t0 = clock()
        self.scenario = scenario
        self.run_id = run_id
        self.attempt = attempt
        self.mode = mode
        self.claims = list(claims)
        self.binary_path = binary
        self.binary_sha256 = file_sha256(binary) if binary else None
        self.source_revision = source_revision
        self.environment = {
            "term": term,
            "locale": locale or os.environ.get("LC_ALL") or os.environ.get("LANG") or "",
            "cols": cols,
            "rows": rows,
        }
        self.transitions = []
        self.waits = []
        self.inputs = []
        self.checks = []
        self.evidence = []
        self.pixel_bindings = []
        self.first_failure = None
        self.cleanup = None
        self.export_dir = export_dir
        self._secrets = []

    # -- recording ---------------------------------------------------

    def _now(self):
        return round(self.clock() - self.t0, 3)

    def transition(self, state_id, grid_text=None):
        """Record that the screen identified by `state_id` was observed.
        The grid hash lets a pixel capture be bound to this exact state."""
        entry = {"seq": len(self.transitions) + 1, "state": state_id, "t": self._now()}
        if grid_text is not None:
            entry["grid_sha256"] = sha256_hex(grid_text)
        self.transitions.append(entry)
        return entry["seq"]

    def wait(self, label, outcome, elapsed, polls=None):
        self.waits.append({"label": label, "outcome": outcome,
                           "elapsed": round(elapsed, 3), "polls": polls, "t": self._now()})

    def register_secret(self, secret):
        if secret:
            self._secrets.append(secret)

    def input(self, keys, label, secret=False):
        """Record a keystroke without leaking typed data."""
        if not secret and keys in SAFE_KEYS:
            shown = SAFE_KEYS[keys]
        else:
            shown = f"<redacted {len(keys)} chars sha256:{sha256_hex(keys)[:12]}>"
            if secret:
                self.register_secret(keys)
        self.inputs.append({"seq": len(self.inputs) + 1, "keys": shown,
                            "label": self.sanitize(label),
                            "after_transition": len(self.transitions), "t": self._now()})

    def check(self, check_id, capability, status, required=True, detail="",
              requires_fresh=False, settled=None):
        """Record an assertion outcome. A check that requires a fresh,
        settled observation is ``unobservable`` unless `settled` is True,
        whatever `status` the stale grid would have produced."""
        if capability not in CAPABILITIES:
            raise ValueError(f"unknown capability {capability!r}")
        if status not in STATUSES:
            raise ValueError(f"unknown status {status!r}")
        if requires_fresh and settled is not True:
            status = UNOBSERVABLE
            detail = (detail + "; " if detail else "") + "frame never settled"
        self.checks.append({"id": check_id, "capability": capability, "status": status,
                            "required": required, "detail": self.sanitize(detail),
                            "fresh_required": requires_fresh,
                            "settled": settled, "t": self._now()})
        return status

    def add_evidence(self, kind, data, name):
        """Hash `data`, keep at most MAX_EVIDENCE_BYTES, and (when an
        export dir is set) write it under a generated allowlisted name."""
        if kind not in EVIDENCE_KINDS:
            raise ValueError(f"evidence kind {kind!r} is not allowlisted")
        raw = data.encode("utf-8", "replace") if isinstance(data, str) else bytes(data)
        if kind != "pixels":
            raw = self.sanitize(raw.decode("utf-8", "replace")).encode("utf-8")
        full_hash = sha256_hex(raw)
        kept = raw[:MAX_EVIDENCE_BYTES]
        ref = {"kind": kind, "name": re.sub(r"[^A-Za-z0-9_-]+", "-", name)[:40],
               "sha256": full_hash, "bytes": len(raw), "truncated": len(kept) < len(raw),
               "file": None}
        if self.export_dir:
            os.makedirs(self.export_dir, exist_ok=True)
            fname = f"{len(self.evidence) + 1:02d}-{ref['name']}.{EVIDENCE_KINDS[kind]}"
            with open(os.path.join(self.export_dir, fname), "wb") as f:
                f.write(kept)
            ref["file"] = fname
        self.evidence.append(ref)
        return ref

    def record_failure(self, why, grid_text="", transcript=""):
        """Keep the first failure only, with bounded diagnostics."""
        if self.first_failure is not None:
            return
        self.first_failure = {
            "why": self.sanitize(why),
            "t": self._now(),
            "grid": self.add_evidence("text_grid", grid_text, "first-failure-grid"),
            "transcript": self.add_evidence("transcript", transcript, "first-failure-transcript"),
            "input_trace": self.add_evidence(
                "input_trace", json.dumps(self.inputs, indent=1), "first-failure-inputs"),
        }

    def set_cleanup(self, outcome, detail=""):
        self.cleanup = {"outcome": outcome, "detail": self.sanitize(detail)}

    def sanitize(self, text):
        for s in self._secrets:
            text = text.replace(s, "<redacted>")
        return _CONTROL_RE.sub("?", text)

    # -- pixel claims ------------------------------------------------

    def bind_raster(self, raster):
        """Register a terminal-raster capture so a pixel check can be
        bound to this run and one recorded state."""
        ref = self.add_evidence("pixels", raster.to_ppm(), f"raster-{raster.state_seq}")
        self.pixel_bindings.append({
            "run_id": raster.run_id,
            "state_seq": raster.state_seq,
            "grid_sha256": raster.grid_sha256,
            "renderer": dict(raster.renderer),
            "raster_sha256": ref["sha256"],
            "width": raster.width,
            "height": raster.height,
        })
        return ref

    def check_pixel_cell(self, check_id, raster, row, col, expected_rgb, tolerance=0):
        """Assert the cell at (row, col) is rendered with `expected_rgb`
        background in an independent raster of the same run and state.
        Fails (never skips) if the raster is not bound to this run, is
        stale against the recorded state, or lacks renderer identity."""
        problem = self._raster_problem(raster)
        if problem:
            return self.check(check_id, PIXEL, FAIL, detail=problem)
        self.bind_raster(raster)
        got = raster.cell_rgb(row, col)
        ok = all(abs(a - b) <= tolerance for a, b in zip(got, expected_rgb))
        return self.check(check_id, PIXEL, PASS if ok else FAIL,
                          detail=f"cell ({row},{col}) rgb={got} expected={tuple(expected_rgb)}")

    def _raster_problem(self, raster):
        if raster.run_id != self.run_id:
            return "raster belongs to a different run"
        if not raster.renderer.get("name") or not raster.renderer.get("font"):
            return "raster lacks renderer/font identity"
        if not (1 <= raster.state_seq <= len(self.transitions)):
            return "raster is not bound to a recorded state"
        state = self.transitions[raster.state_seq - 1]
        if state.get("grid_sha256") != raster.grid_sha256:
            return "raster was not taken of the recorded state's grid"
        return None

    # -- output ------------------------------------------------------

    def capability_summary(self):
        out = {}
        for cap in CAPABILITIES:
            statuses = [c["status"] for c in self.checks if c["capability"] == cap]
            if FAIL in statuses:
                out[cap] = FAIL
            elif UNOBSERVABLE in statuses:
                out[cap] = UNOBSERVABLE
            elif PASS in statuses:
                out[cap] = PASS
            else:
                out[cap] = NOT_RUN
        return out

    def verdict(self):
        caps = self.capability_summary()
        if any(c["required"] and c["status"] != PASS for c in self.checks):
            return FAIL
        if any(caps[c] != PASS for c in self.claims):
            return FAIL
        if self.cleanup is None or self.cleanup.get("outcome") != "ok":
            return FAIL
        return PASS

    def to_dict(self):
        caps = self.capability_summary()
        return {
            "schema": SCHEMA,
            "harness_version": HARNESS_VERSION,
            "identity": {
                "binary_sha256": self.binary_sha256,
                "source_revision": self.source_revision,
            },
            "scenario": self.scenario,
            "run_id": self.run_id,
            "attempt": self.attempt,
            "mode": self.mode,
            "environment": self.environment,
            "claims": self.claims,
            "capabilities": caps,
            # Only a live run with a passing backend-intent correlation may
            # speak for the real migration; the text grid never can.
            "qualifies_actual_migration": (
                self.mode == MODE_LIVE and caps[BACKEND_INTENT] == PASS),
            "transitions": self.transitions,
            "waits": self.waits,
            "inputs": self.inputs,
            "checks": self.checks,
            "evidence": self.evidence,
            "pixel_bindings": self.pixel_bindings,
            "first_failure": self.first_failure,
            "cleanup": self.cleanup,
            "verdict": self.verdict(),
        }

    def write(self, path):
        with open(path, "w", encoding="utf-8") as f:
            json.dump(self.to_dict(), f, indent=1, sort_keys=True)
            f.write("\n")


class Raster:
    """A terminal-raster capture from an independent renderer. The text
    grid emulator never produces one."""

    def __init__(self, run_id, state_seq, grid_sha256, renderer, width, height,
                 cell_w, cell_h, rgb):
        self.run_id = run_id
        self.state_seq = state_seq
        self.grid_sha256 = grid_sha256
        self.renderer = renderer  # {"name", "version", "font", ...}
        self.width = width
        self.height = height
        self.cell_w = cell_w
        self.cell_h = cell_h
        self.rgb = rgb  # width*height*3 bytes, row-major

    def cell_rgb(self, row, col):
        x = col * self.cell_w + self.cell_w // 2
        y = row * self.cell_h + self.cell_h // 2
        i = (y * self.width + x) * 3
        return tuple(self.rgb[i:i + 3])

    def to_ppm(self):
        return f"P6\n{self.width} {self.height}\n255\n".encode() + bytes(self.rgb)


def load_receipt(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        raise ReceiptError(f"receipt {path} is absent") from None
    except (OSError, ValueError) as e:
        raise ReceiptError(f"receipt {path} unreadable: {e}") from None


def verify_receipt(receipt, expect_binary_sha256=None, expect_revision=None,
                   required_checks=(), expected_transitions=()):
    """Independent gate over a finished receipt. Returns a list of problems
    (empty means the receipt supports its claims). `receipt` may be None
    (absent). Never trusts the receipt's own ``verdict``."""
    if not receipt:
        return ["receipt absent"]
    problems = []
    if receipt.get("schema") != SCHEMA:
        problems.append(f"unknown schema {receipt.get('schema')!r}")
    if receipt.get("harness_version") != HARNESS_VERSION:
        problems.append(f"harness version {receipt.get('harness_version')!r} "
                        f"!= {HARNESS_VERSION!r}")
    ident = receipt.get("identity") or {}
    if expect_binary_sha256 is not None and ident.get("binary_sha256") != expect_binary_sha256:
        problems.append("binary identity mismatch")
    if expect_revision is not None and ident.get("source_revision") != expect_revision:
        problems.append("source revision mismatch")

    checks = {c["id"]: c for c in receipt.get("checks", [])}
    for cid in required_checks:
        c = checks.get(cid)
        if c is None:
            problems.append(f"required check {cid!r} skipped (no outcome)")
        elif c["status"] != PASS:
            problems.append(f"required check {cid!r} is {c['status']}")
    for c in checks.values():
        if c.get("required") and c["status"] != PASS:
            problems.append(f"check {c['id']!r} is {c['status']}")
        if c.get("fresh_required") and c["status"] == PASS and c.get("settled") is not True:
            problems.append(f"check {c['id']!r} passed without a settled frame")

    states = [t["state"] for t in receipt.get("transitions", [])]
    pos = 0
    for want in expected_transitions:
        try:
            pos = states.index(want, pos) + 1
        except ValueError:
            problems.append(f"transition {want!r} missing or out of order")
            break

    caps = receipt.get("capabilities") or {}
    for cap in receipt.get("claims", []):
        if caps.get(cap) != PASS:
            problems.append(f"claimed capability {cap!r} is {caps.get(cap, NOT_RUN)}")
    if PIXEL in receipt.get("claims", []) and not receipt.get("pixel_bindings"):
        problems.append("pixel claim has no raster evidence")
    if receipt.get("mode") != MODE_LIVE and receipt.get("qualifies_actual_migration"):
        problems.append(f"{receipt.get('mode')} run claims to qualify a real migration")
    cleanup = receipt.get("cleanup")
    if not cleanup or cleanup.get("outcome") != "ok":
        problems.append("cleanup not completed")
    return problems
