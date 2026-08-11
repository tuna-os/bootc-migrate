# Contributing

Thanks for your interest in improving `bootc-migrate`.

> **Note:** this tool performs an in-place, hard-to-reverse migration of a real
> system. Treat changes to the migration phases (`src/migration/`) with extra
> care and exercise them through the end-to-end suite before merging.

## Development setup

You need a recent stable Rust toolchain (the crate targets edition 2024,
`rust-version = 1.88.0`) and [`just`](https://github.com/casey/just).

```console
$ cargo build
$ just check        # clippy + rustfmt + unit tests + shellcheck — run before every PR
```

### IDE setup

Standard `rust-analyzer` works out of the box. The crate uses `clippy` with
several extra lints enabled (see `Cargo.toml`); run `just check` rather than
`cargo clippy` alone to get the full lint set.

---

## Running the end-to-end tests

The E2E harness boots a real QEMU VM, runs the full migration, reboots, and
validates the result. It needs additional host packages:

```bash
# Fedora/RHEL
sudo dnf install qemu-system-x86_64 edk2-ovmf podman cryptsetup lvm2 swtpm

# Ubuntu/Debian
sudo apt install qemu-system-x86 ovmf podman cryptsetup-bin lvm2 swtpm
```

You also need root (for loop mounts and pflash) and outbound registry access
(`ghcr.io`) to pull the Bluefin/Dakota images. On a fresh machine, seed the
local registry cache first (saves ~8 GB of re-pulls on every run):

```console
$ just registry-start   # start a local OCI registry on localhost:5000
$ just registry-cache   # pull Bluefin + Dakota; push to local registry
```

### E2E scenarios

| Recipe | What it tests | Disk | Notes |
|--------|--------------|------|-------|
| `just e2e` | Bluefin stable → Dakota (btrfs, x86_64) | 20 GB | Default; fastest |
| `just e2e-lts` | Bluefin LTS → Dakota (XFS + ext4 loopback) | 20 GB | LTS base |
| `just e2e-luks` | Bluefin LTS → Dakota (XFS + LUKS + swtpm) | 40 GB | Encrypted root |
| `just e2e-lvm` | Bluefin LTS → Dakota (LVM-on-LUKS, separate `/var`) | 40 GB | Most complex |

Run the default scenario:

```console
$ just e2e
```

Watch progress in another terminal:

```console
$ just watch          # tails the latest .log; exits on errors or idle timeout
```

Or ssh into the running VM to poke around:

```console
$ just e2e-ssh        # opens an interactive SSH session to port 2222
```

### Debugging a failed E2E run

```console
$ just e2e-failures   # grep log for failures/errors
$ just e2e-composefs  # grep for composefs-related boot messages
$ just e2e-tail       # tail the QEMU serial console (high-signal lines only)
$ just e2e-status     # show disk.raw status + QEMU/SSH availability
```

To reproduce a failure starting from after the migration (skipping setup):

```console
$ SKIP_SETUP=1 just e2e-reboot-test
```

### Using Corral VMs for interactive testing

[Corral](https://github.com/tuna-os/corral) is a VM manager that provisions
KubeVirt (or local QEMU) VMs from bootc container images. It's useful for
interactive TUI testing and exploratory debugging where the scripted QEMU
harness is too rigid.

**Setup** — install the `corral` binary (see Corral's README), then create a
Bluefin VM for testing:

```console
$ corral create tui-e2e --image ghcr.io/projectbluefin/bluefin:stable \
    --cpu 2 --memory 4Gi --disk 40Gi --efi
$ corral start tui-e2e
```

**SSH into the VM:**

```console
$ corral ssh tui-e2e --user root
```

**Deploy a local build to the VM** (cross-compile or build on the VM):

```bash
# Option 1: Build on the VM (Rust must be installed in the VM)
tar czf /tmp/bmc-src.tar.gz --exclude=target --exclude=.git .
base64 /tmp/bmc-src.tar.gz | corral ssh tui-e2e --user root -c \
  "base64 -d > /tmp/src.tar.gz && mkdir -p /tmp/bmc && \
   tar xzf /tmp/src.tar.gz -C /tmp/bmc && cd /tmp/bmc && \
   cargo build --release && \
   cp target/release/bootc-migrate /usr/local/bin/"

# Option 2: If architectures match, just ship the binary
base64 target/release/bootc-migrate | corral ssh tui-e2e --user root -c \
  "base64 -d > /usr/local/bin/bootc-migrate && \
   chmod +x /usr/local/bin/bootc-migrate"
```

**Capture TUI screenshots** (the VM won't have tmux on an immutable OS, but
Python3 is available for PTY capture):

```bash
corral ssh tui-e2e --user root -c "python3 << 'EOF'
import pty, os, time, select, re, struct, fcntl, termios, sys
rows, cols = 30, 100
pid, fd = pty.fork()
if pid == 0:
    ws = struct.pack('HHHH', rows, cols, 0, 0)
    fcntl.ioctl(sys.stdout.fileno(), termios.TIOCSWINSZ, ws)
    os.environ['TERM'] = 'xterm-256color'
    os.execvp('bootc-migrate', ['bootc-migrate'])
else:
    ws = struct.pack('HHHH', rows, cols, 0, 0)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, ws)
    time.sleep(2)
    out = b''
    while select.select([fd],[],[],1)[0]:
        out += os.read(fd, 65536)
    os.write(fd, b'q'); time.sleep(0.5)
    os.write(fd, b'h'); time.sleep(0.3)
    os.write(fd, b'\r'); time.sleep(0.5)
    os.waitpid(pid, 0)
    text = re.sub(r'\x1b\[[0-9;]*[a-zA-Z]', '', out.decode('utf-8', errors='replace'))
    print(text)
EOF"
```

**Corral VMs in CI** — the CI matrix uses the scripted QEMU harness
(`tests/run-e2e.sh`) for reproducibility. Corral VMs are for developer
convenience only and are not required to contribute.

### Cleaning up after E2E

```console
$ just cleanup        # kill QEMU, prune podman, remove disk.raw and .log files
```

---

## Adding a new E2E scenario

1. Add a new recipe in `justfile` modelled on `e2e-luks` or `e2e-lvm`.
2. Add the scenario to the CI matrix in `.github/workflows/e2e-tests.yml`
   (follow the existing `include:` pattern, set `name`, `filesystem`, `disk-size`, and any env overrides).
3. Update the CI matrix table in [AGENTS.md](AGENTS.md).
4. Document the scenario in [docs/filesystem-support.md](docs/filesystem-support.md).

---

## Before you open a PR

- `just check` passes (clippy + rustfmt + unit tests + shellcheck — this is
  what CI's `validate` job runs).
- `cargo deny check` passes if you touched dependencies.
- Commits follow the `component: Summary` convention described in
  [REVIEW.md](REVIEW.md); fixups are squashed before merge.
- New non-trivial logic has unit tests (prefer table-driven, per REVIEW.md),
  and migration-path changes are exercised by at least the default E2E scenario.
- If your change affects the kernel command line, boot artifacts, or any phase
  output, run the full E2E matrix locally or wait for CI to do it on your PR.

## Code review

Please read [REVIEW.md](REVIEW.md) — it describes the testing, code-quality, and
commit-message expectations applied here. AI-assisted contributions must follow
[AGENTS.md](AGENTS.md) (no automatic `Signed-off-by`; add an `Assisted-by:`
trailer).

---

## Dependency update policy

Dependency updates come through [Renovate](https://docs.renovatebot.com/) (see
`renovate.json`). Patch-level updates are auto-merged if CI is green. Minor and
major updates get a PR for human review. When reviewing Renovate PRs:

- Check the changelog / release notes for breaking changes.
- Verify `cargo deny check` still passes.
- Run `just check` locally if the changed crate is a key dependency (`rustix`,
  `clap`, `anyhow`, `serde_json`).

---

## License

By contributing, you agree that your contributions are dual-licensed under the
[MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licenses.
