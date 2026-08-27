# Instructions for AI agents

This project follows the conventions of the wider bootc-dev / composefs-rs
ecosystem. If you are an LLM or an LLM-assisted tool contributing here, follow
the guidance below.

## CRITICAL instructions for generating commits

### Signed-off-by

Human review is required for all code that is generated or assisted by a large
language model. If you are an LLM, you MUST NOT add a `Signed-off-by` trailer to
automatically generated git commits. Only an explicit human action or request
should add a `Signed-off-by`. If you open a pull request and the DCO check fails,
tell the human to review the code and explain how to add a signoff.

### Attribution

When generating substantial amounts of code, you SHOULD include an
`Assisted-by: TOOLNAME (MODELNAME)` trailer. For example,
`Assisted-by: Claude Code (Opus 4.8)`.

## Code guidelines

[REVIEW.md](REVIEW.md) describes expectations around testing, code quality,
commit messages, and commit organization. After each commit — and especially
when you believe a task is complete — you are strongly encouraged to review your
change against those guidelines (a subagent review is a good way to do this),
alongside looking for any other issues. The same applies when reviewing others'
code.

Key project-specific points (see REVIEW.md for the full list):

- Prefer `rustix` over `libc`; `unsafe` is denied via `[lints.rust]` and must be
  carefully justified if ever reintroduced.
- Keep parsing separate from I/O so logic stays unit-testable; prefer
  table-driven tests.
- Run `just check` (clippy, rustfmt, unit tests, shellcheck) before opening a PR.

## Monitoring E2E runs

Use `watcher.sh` instead of polling loops. It tails the e2e log, exits on fatal
error patterns or idle timeout:

```bash
# Watch the latest .log file with default timeout (300s idle)
./watcher.sh e2e-luks.log 30 300
```

Also available via just: `just watch log="e2e-luks.log"`

## CI matrix

| Scenario | Base | Target | Filesystem | Disk size |
|----------|------|--------|------------|-----------|
| btrfs + composefs | bluefin:stable | dakota:stable | btrfs | 20G |
| xfs + loopback | bluefin:lts | dakota:stable | xfs+ext4loop | 20G |
| LUKS + xfs | bluefin:lts | dakota:stable | xfs+crypt | 40G |
| LVM-on-LUKS + /var | bluefin:lts | dakota:stable | xfs+lvm+crypt | 40G |
| TUI-driven migration (`E2E_MODE=tui-migrate`) | bluefin:stable | dakota:stable | btrfs | 40G |

## Two-candidate CI races

When evaluating competing implementations (e.g. monolith M vs modular P), push
both branches and dispatch CI on each. The watcher script can monitor any local
run; for CI use `gh run view --json jobs` to poll per-scenario results.

## Interactive testing with Corral VMs

[Corral](https://github.com/tuna-os/corral) manages KubeVirt/QEMU VMs
provisioned from bootc container images. Use it for interactive TUI testing,
exploratory debugging, and validating changes on real OSTree systems without
running the full scripted E2E harness.

Key commands:

```bash
corral list                              # show all VMs
corral ssh tui-e2e --user root           # interactive shell
corral ssh tui-e2e --user root -c "CMD"  # run a command
```

To deploy a local build to a corral VM:

```bash
# Tar + base64 source over SSH, build on the VM
tar czf /tmp/src.tar.gz --exclude=target --exclude=.git .
base64 /tmp/src.tar.gz | corral ssh <vm> --user root -c \
  "base64 -d > /tmp/src.tar.gz && mkdir -p /tmp/bmc && \
   tar xzf /tmp/src.tar.gz -C /tmp/bmc && cd /tmp/bmc && \
   cargo build --release && cp target/release/bootc-migrate /usr/local/bin/"
```

The `tui-e2e` VM (Bluefin stable, UEFI, XFS) is pre-provisioned for TUI
development and pending-transaction testing. Note: corral VMs are for developer
convenience; CI uses the scripted `tests/run-e2e.sh` harness.

## Follow other guidelines

Read [README.md](README.md) and [CONTRIBUTING.md](CONTRIBUTING.md) and follow
the contribution guidance there.
