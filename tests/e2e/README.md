# E2E tests

This directory is a pointer, not a parallel suite. The project's real
end-to-end coverage already exists one level up and predates this marker;
duplicating it here would just fork two copies of the same assertions out
of sync with each other.

## Where the suite actually lives

| What | Where |
|---|---|
| Harness (partition → install → migrate → reboot → assert, QEMU-driven) | [`tests/run-e2e.sh`](../run-e2e.sh) |
| Post-reboot health/assertion checks | [`tests/e2e-health.sh`](../e2e-health.sh) |
| TUI wizard driver (drives the interactive migration over a pty) | [`tests/tui-e2e-driver.py`](../tui-e2e-driver.py) |
| CI matrix that runs every cell on each PR | [`.github/workflows/e2e-tests.yml`](../../.github/workflows/e2e-tests.yml) |
| Single-scenario CI entry point | [`.github/workflows/e2e-single.yml`](../../.github/workflows/e2e-single.yml) |
| Full matrix, gating rules, and test pyramid writeup | [`docs/testing.md`](../../docs/testing.md) |

Current gating cells include Bluefin → Dakota across btrfs/ext4/xfs+LUKS/
xfs+LVM+LUKS, the TUI-driven migration (`tests/tui-e2e-driver.py`), the
`composefs-to-ostree` and `image-swap` backend-switch routes (Dakota →
fedora-bootc, Dakota → Utah), and the cross-family `/etc` policy gate — see
`docs/testing.md` for the full, current matrix and status of each cell.

## Running a cell locally

```sh
sudo E2E_MODE=composefs-migrate FILESYSTEM=btrfs ./tests/run-e2e.sh
```

See `tests/run-e2e.sh` (top-of-file comments) and `docs/testing.md` for the
full set of `E2E_MODE` / `FILESYSTEM` / `E2E_TEST_MODE` combinations, and
`just e2e-failures` to grep a failed run's phase banners.

## Why this directory exists

This repo is a Rust CLI/TUI tool, not a web app — `playwright`/`cypress`
config would be the wrong tool here. The automated ACMM prerequisite check
(#225) looks for one of a fixed list of E2E marker paths and doesn't know
about `tests/run-e2e.sh`, so this file exists to satisfy that check
truthfully: by pointing at the real, CI-gated E2E suite instead of forking
a second one.
