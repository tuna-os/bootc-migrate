#!/usr/bin/env python3
"""Select E2E scenarios from JSON, preserving the baseline rotation."""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

FULL_EVENTS = {"schedule", "workflow_dispatch"}
TUI_PREFIX = "crates/bootc-migrate/src/tui/"
TUI_PATHS = {
    "crates/bootc-migrate/src/main.rs",
    "crates/bootc-migrate/src/drift_review.rs",
    "crates/bootc-migrate/Cargo.toml",
    "Cargo.toml",
    "Cargo.lock",
    "tests/tui-e2e-driver.py",
    "tests/run-e2e.sh",
    ".github/e2e-scenarios.yml",
    ".github/workflows/e2e-tests.yml",
    ".github/workflows/ci.yml",
    "scripts/select_e2e.py",
    "tests/test_select_e2e.py",
}
ZERO_SHA = "0" * 40
# Local tree comparisons should finish quickly; never leave the planner hung.
GIT_TIMEOUT_SECONDS = 60


def affects_tui(paths):
    """Match whole repository-relative paths, including removed files."""
    return any(path.startswith(TUI_PREFIX) or path in TUI_PATHS for path in paths)


def select_scenarios(scenarios, event, run_number, size, paths):
    """Keep the baseline ordered slice and append at most one existing TUI cell."""
    if not isinstance(scenarios, list) or not scenarios:
        raise ValueError("scenarios must be a nonempty list")
    if any(not isinstance(cell, dict) or not cell.get("name") for cell in scenarios):
        raise ValueError("each scenario must have a name")
    if event in FULL_EVENTS:
        return {"scenarios": scenarios, "reason": "Full scheduled or manual matrix"}
    if event not in {"pull_request", "push"}:
        raise ValueError(f"unsupported event: {event}")
    if type(run_number) is not int or run_number < 1:
        raise ValueError("run number must be a positive integer")
    if type(size) is not int or size < 1:
        raise ValueError("round-robin size must be a positive integer")
    gating = [cell for cell in scenarios if cell.get("allow_failure") is not True]
    if not gating:
        raise ValueError("no gating scenarios")
    picked = [
        gating[(run_number * size + offset) % len(gating)]
        for offset in range(min(size, len(gating)))
    ]
    reason = "Baseline round-robin slice"
    if affects_tui(paths):
        tui = [cell for cell in scenarios if cell.get("mode") == "tui-migrate"]
        if len(tui) != 1 or tui[0].get("allow_failure") is True:
            raise ValueError("TUI changes require exactly one gating tui-migrate scenario")
        if tui[0] not in picked:
            picked.append(tui[0])
        reason += "; TUI-related paths require tui-migrate"
    return {"scenarios": picked, "reason": reason}


def event_diff(event, payload):
    """Use the full PR branch diff or the entire push, never just HEAD~1."""
    if event in FULL_EVENTS:
        return None
    if not isinstance(payload, dict):
        raise ValueError("event payload must be an object")
    try:
        if event == "pull_request":
            base = payload["pull_request"]["base"]["sha"]
            head = payload["pull_request"]["head"]["sha"]
        elif event == "push":
            base, head = payload["before"], payload["after"]
        else:
            raise ValueError(f"unsupported event: {event}")
    except (KeyError, TypeError) as error:
        raise ValueError("event payload is missing base/head commit IDs") from error
    for sha in (base, head):
        if not isinstance(sha, str) or re.fullmatch(r"[0-9a-f]{40}", sha) is None:
            raise ValueError("event commit IDs must be full Git SHA-1 hashes")
    if head == ZERO_SHA or (event == "pull_request" and base == ZERO_SHA):
        raise ValueError("event contains a null commit ID")
    if base == ZERO_SHA:
        # A newly created branch has no previous tree: inspect every tracked path.
        return ["ls-tree", "-r", "--name-only", "-z", head]
    separator = "..." if event == "pull_request" else ".."
    # Disabling rename detection includes both sides of moves into/out of TUI paths.
    return [
        "diff", "--no-ext-diff", "--no-textconv", "--name-only", "--no-renames",
        "-z", f"{base}{separator}{head}", "--",
    ]


def changed_paths(event, payload, repo=Path(".")):
    """Read NUL-delimited Git paths; missing history or Git failures stop planning."""
    arguments = event_diff(event, payload)
    if arguments is None:
        return []
    result = subprocess.run(
        ["git", *arguments], cwd=repo, check=True, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, timeout=GIT_TIMEOUT_SECONDS,
    )
    return [path.decode("utf-8", errors="surrogateescape")
            for path in result.stdout.split(b"\0") if path]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", required=True)
    parser.add_argument("--event-path", type=Path, required=True)
    parser.add_argument("--run-number", type=int, required=True)
    parser.add_argument("--size", type=int, required=True)
    arguments = parser.parse_args()
    try:
        scenarios = json.load(sys.stdin)
        # Full runs do not need a diff or the PR/push event fields.
        payload = ({} if arguments.event in FULL_EVENTS
                   else json.loads(arguments.event_path.read_text()))
        paths = changed_paths(arguments.event, payload)
        plan = select_scenarios(
            scenarios, arguments.event, arguments.run_number, arguments.size, paths,
        )
    except subprocess.TimeoutExpired:
        print(f"E2E planning failed: git exceeded {GIT_TIMEOUT_SECONDS} seconds", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as error:
        print(f"E2E planning failed: git exited {error.returncode}: "
              f"{error.stderr.decode('utf-8', errors='replace').strip()}", file=sys.stderr)
        return 1
    except (ValueError, OSError) as error:
        print(f"E2E planning failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(plan, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
