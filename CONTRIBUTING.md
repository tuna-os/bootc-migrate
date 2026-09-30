# Contributing

Thank you for your interest in `bootc-migrate`.

> **Note:** this tool does an in-place migration of a real system.
> Treat changes to the migration phases (`src/migration/`) with care.
> Run the end-to-end test suite before you merge changes.

## Development setup

You need a stable Rust toolchain (edition 2024, `rust-version = 1.88.0`)
and [`just`](https://github.com/casey/just).

```console
$ cargo build
$ just check        # clippy + rustfmt + unit tests + shellcheck — run before every PR
```

### IDE setup

Standard `rust-analyzer` works out of the box. The crate uses `clippy` with
extra lints (see `Cargo.toml`). Run `just check` instead of `cargo clippy`
to get the full lint set.

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
| `just e2e-tui` | Bluefin stable → Dakota, driven through the TUI wizard | 40 GB | `E2E_MODE=tui-migrate` |

These are local recipes that differ from the CI matrix.
`just e2e-lts` runs XFS at 20 GB to exercise the loopback store.
CI's LTS cell runs `ext4` at 40 GB.
The seven-cell CI matrix lives in `.github/workflows/e2e-tests.yml`, which is authoritative.
`README.md` reproduces it.
Do not sync these two tables into one; they answer different questions.

Run the default scenario:

```console
$ just e2e
```

Watch progress in another terminal:

```console
$ just watch          # tails the latest .log; exits on errors or idle timeout
```

Or connect to the VM with SSH:

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

To reproduce a failure after the migration without setup:

```console
$ SKIP_SETUP=1 just e2e-reboot-test
```

### Using Corral VMs for interactive testing

[Corral](https://github.com/tuna-os/corral) provisions KubeVirt (or local QEMU) VMs from bootc container images.
Corral is useful for interactive TUI tests when the scripted QEMU harness is too rigid.

**Setup** — install the `corral` binary (see Corral's README), then create a Bluefin VM:

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
- Commits follow the `component: Summary` format in [REVIEW.md](REVIEW.md). Squash fixups before merge.
- Add unit tests for new logic (prefer table-driven tests, per [REVIEW.md](REVIEW.md)).
- Exercise migration-path changes with the default E2E scenario.
- If your change affects kernel args or boot artifacts, run the E2E matrix or wait for CI.
- The change satisfies the **Definition of Done** ([REVIEW.md](REVIEW.md)).
  Every claim matches the diff. All required checks pass on the head commit.

## Code review

Read [REVIEW.md](REVIEW.md). It describes the Definition of Done (DoD),
test requirements, and commit conventions. AI contributions must follow
[AGENTS.md](AGENTS.md) (no automatic `Signed-off-by`; add an `Assisted-by:` trailer,
and complete all validation steps).

---

## Dependency update policy

Dependency updates come through [Renovate](https://docs.renovatebot.com/) (see
`renovate.json`). Renovate applies patch updates when CI passes.
Minor and major updates get a PR for human review. When you review Renovate PRs:

- Check the release notes for incompatible changes.
- Verify `cargo deny check` still passes.
- Run `just check` locally if the crate is a key dependency (`rustix`, `clap`, `anyhow`, `serde_json`).

---

## License

By your contribution, you agree to dual-license your work under the
[MIT](https://github.com/tuna-os/bootc-migrate/blob/main/LICENSE-MIT) and [Apache-2.0](https://github.com/tuna-os/bootc-migrate/blob/main/LICENSE-APACHE) licenses.
