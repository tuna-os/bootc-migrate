# OSTree → ComposeFS Migration Tool — just tasks
# https://github.com/tuna-os/bootc-migrate

default:
    @just --list

# === Build & Test ===

# Build (set PROFILE_FLAG=--release for CI)
build:
    cargo build {{env_var_or_default('PROFILE_FLAG', '')}}

# Build in release profile (convenience alias)
build-release:
    cargo build --release

# Run all unit tests (set PROFILE_FLAG=--release for CI)
test:
    cargo test {{env_var_or_default('PROFILE_FLAG', '')}}

# Run tests with output
test-verbose:
    cargo test {{env_var_or_default('PROFILE_FLAG', '')}} -- --nocapture

# Run a single test (usage: just test-one test_name)
test-one test_name:
    cargo test {{test_name}} -- --nocapture

# Check compilation without producing binary (fast)
cargo-check:
    cargo check

# === E2E Tests ===

# Run full E2E migration test.
# All parameters are env-var driven; set PROFILE_FLAG=--release for CI.
# Defaults: Bluefin stable → Dakota stable (btrfs, 20G disk).
e2e: build test
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="{{env_var_or_default('BASE_IMAGE', 'ghcr.io/projectbluefin/bluefin:stable')}}" \
      TARGET_IMAGE="{{env_var_or_default('TARGET_IMAGE', 'ghcr.io/projectbluefin/dakota:stable')}}" \
      DISK_SIZE="{{env_var_or_default('DISK_SIZE', '20G')}}" \
      FILESYSTEM="{{env_var_or_default('FILESYSTEM', 'btrfs')}}" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-run.log

# Bluefin LTS → Dakota (xfs + loopback workaround).
e2e-lts: build test
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="ghcr.io/projectbluefin/bluefin:lts" \
      TARGET_IMAGE="ghcr.io/projectbluefin/dakota:stable" \
      DISK_SIZE="20G" \
      FILESYSTEM="xfs" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-lts.log

# Bluefin LTS → Dakota (xfs + LUKS encryption + loopback).
e2e-luks: build test
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="ghcr.io/projectbluefin/bluefin:lts" \
      TARGET_IMAGE="ghcr.io/projectbluefin/dakota:stable" \
      DISK_SIZE="40G" \
      FILESYSTEM="xfs+crypt" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-luks.log

# Bluefin → Dakota, driven through the TUI wizard + drift checklist
# (E2E_MODE=tui-migrate; see tests/tui-e2e-driver.py).
e2e-tui: build test
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="ghcr.io/projectbluefin/bluefin:stable" \
      TARGET_IMAGE="ghcr.io/projectbluefin/dakota:stable" \
      DISK_SIZE="40G" \
      E2E_MODE="tui-migrate" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-tui.log

# Bluefin LTS → Dakota (LVM-on-LUKS, separate /var LV + loopback).
e2e-lvm: build test
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="ghcr.io/projectbluefin/bluefin:lts" \
      TARGET_IMAGE="ghcr.io/projectbluefin/dakota:stable" \
      DISK_SIZE="40G" \
      FILESYSTEM="xfs+lvm+crypt" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-lvm.log

# Run E2E with composefs boot log_level=debug
e2e-debug: build
    @echo "=== Running E2E with composefs systemd debug logging ==="
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="{{env_var_or_default('BASE_IMAGE', 'ghcr.io/projectbluefin/bluefin:stable')}}" \
      TARGET_IMAGE="{{env_var_or_default('TARGET_IMAGE', 'ghcr.io/projectbluefin/dakota:stable')}}" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-run.log; rc=$$?; \
      echo "=== Quick diagnostics ==="; \
      echo "--- dbus/messagebus/polkit/logind ---"; \
      grep -iE 'dbus|messagebus|polkit|logind|machine.id' e2e-run.log | tail -30; \
      echo "--- all FAILED ---"; \
      grep -E 'FAILED|DEPEND' e2e-run.log | tail -30; \
      exit $$rc

# === Linting ===

# Run every check CI runs (clippy, rustfmt, unit tests, shellcheck)
check: clippy fmt-check test test-all-features lint-shell

# Unit tests + clippy with every cargo feature enabled (feature-gated code
# like composefs-native is invisible to the default build — this is its
# only automated gate).
test-all-features:
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features {{env_var_or_default('PROFILE_FLAG', '')}}

# Probe watched bootc images for cfs CLI generation drift (see #72).
drift-canary:
    ./tests/drift-canary.sh

# Line-coverage summary (requires cargo-llvm-cov + llvm-tools-preview).
coverage:
    cargo llvm-cov --workspace --all-features --summary-only

# Coverage with the regression floor CI enforces (see docs/testing.md).
coverage-check:
    cargo llvm-cov --workspace --all-features --summary-only --fail-under-lines 26

# Browsable HTML coverage report (target/llvm-cov/html/index.html).
coverage-html:
    cargo llvm-cov --workspace --all-features --html

# Format all Rust code.
fmt:
    cargo fmt --all

# Verify Rust code is formatted (no changes applied).
fmt-check:
    cargo fmt --all -- --check

# Run clippy with warnings denied.
clippy:
    cargo clippy --all-targets -- -D warnings

# Check dependency licenses and sources with cargo-deny.
deny:
    cargo deny check bans sources licenses

# Run all linters (shellcheck, rustfmt, clippy). Retained as an alias for `check`.
lint: lint-shell lint-rust

# Lint shell scripts with shellcheck (warnings + errors only, skip info/style)
lint-shell:
    @echo "=== shellcheck ==="
    shellcheck --severity=warning tests/run-e2e.sh

# Lint Rust code (format + clippy)
lint-rust: fmt-check clippy

# === Interactive E2E Steps ===
# Run individual phases of the E2E test for debugging and iteration.

# Show current E2E state (disk, QEMU, SSH, checkpoint)
e2e-status:
    @echo "=== E2E State ==="
    @echo -n "disk.raw: "; [ -f disk.raw ] && echo "present ($(stat -c%s disk.raw) bytes)" || echo "missing"
    @echo -n "pre-migration ckpt: "; [ -f disk.raw.pre-migration ] && echo present || echo missing
    @echo -n "post-migration ckpt: "; [ -f disk.raw.post-migration ] && echo present || echo missing
    @echo -n "QEMU: "; pgrep -f 'qemu-system.*disk.raw' > /dev/null && echo running || echo stopped
    @echo -n "SSH (port 2222): "; timeout 3 bash -c 'echo > /dev/tcp/localhost/2222' 2>/dev/null && echo open || echo closed

# Open interactive SSH to the E2E VM
e2e-ssh:
    ssh -i test_key -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@localhost

# Tail the QEMU serial console log (filtered high-signal lines)
e2e-tail:
    @tail -F -q -n 0 qemu.log 2>/dev/null \
      | sed -u 's/\x1b\[[0-9;]*[a-zA-Z]//g' \
      | grep --line-buffered -E '\[FAILED\]|Failed to start|panic|Out of memory|kernel BUG|Kernel panic|sshd|login:|Welcome to|GRUB|Booting|systemd-boot|composefs|=== Phase|=== MIGRATION|bootc-migrate|Linux Boot Manager|dbus|messagebus|polkit|machine.id|emergency' || true

# Host-side .raw disk scan only (no migration, no boot).
# Requires: disk.raw (post-migration).
e2e-scan:
    @[ -f disk.raw ] || { echo "ERROR: disk.raw not found"; exit 1; }
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      FILESYSTEM="{{env_var_or_default('FILESYSTEM', 'xfs')}}" \
      E2E_SCAN_ONLY=true \
      ./tests/run-e2e.sh 2>&1 | tee e2e-scan.log

# Reboot-test: launch QEMU from checkpoint, wait for composefs boot, validate.
# Requires: disk.raw (post-migration) or disk.raw.post-migration checkpoint.
e2e-reboot-test:
    @[ -f disk.raw ] || [ -f disk.raw.post-migration ] || { echo "ERROR: no disk.raw or post-migration checkpoint"; exit 1; }
    @[ -f disk.raw ] || cp disk.raw.post-migration disk.raw
    sudo -E env PATH="{{env_var_or_default('PATH', '/usr/bin:/usr/sbin:/usr/local/bin')}}" \
      BASE_IMAGE="{{env_var_or_default('BASE_IMAGE', 'ghcr.io/projectbluefin/bluefin:lts')}}" \
      TARGET_IMAGE="{{env_var_or_default('TARGET_IMAGE', 'ghcr.io/projectbluefin/dakota:stable')}}" \
      FILESYSTEM="{{env_var_or_default('FILESYSTEM', 'xfs')}}" \
      ./tests/run-e2e.sh 2>&1 | tee e2e-boot-test.log

# === Diagnostics ===

# View the last E2E log
e2e-log:
    @less e2e-run.log

# Watch the latest E2E log with auto-exit on errors
watch log="":
	@LOG="{{log}}"; \
	if [ -z "$LOG" ]; then \
	  LOG=$(ls -t *.log 2>/dev/null | head -1); \
	  if [ -z "$$LOG" ]; then echo "No log found"; exit 1; fi; \
	fi; \
	echo "Watching: $LOG"; exec ./watcher.sh "$LOG" 30 300

# Grep E2E log for failures
e2e-failures:
    @grep --color=always -E 'FAILED|error|Error|WARNING|panic|blocker' e2e-run.log || echo "No failures found"

# Grep E2E log for dbus-related messages
e2e-dbus:
    @grep --color=always -i 'dbus\|messagebus\|polkit\|logind\|machine.id' e2e-run.log || echo "No dbus messages"

# Grep full QEMU log for dbus failures (unfiltered serial console)
e2e-qemu-dbus:
    @grep --color=always -iE 'dbus|messagebus|polkit|logind|machine.id|FAILED' qemu.log | tail -60 || echo "No qemu.log"

# Grep E2E log for composefs-boot messages (kernel cmdline, mounts, etc.)
e2e-composefs:
    @grep --color=always -E 'composefs=|overlay|erofs|subvol|fstab|BOOT_IMAGE|Kernel command line' e2e-run.log || echo "No composefs messages"

# === Registry (for fast E2E pulls) ===

# Start local OCI registry for E2E
registry-start:
    sudo podman run -d --name e2e-registry --network=host docker.io/library/registry:2

# Cache images in local registry
registry-cache:
    sudo podman tag ghcr.io/projectbluefin/bluefin:stable 127.0.0.1:5000/bluefin:stable
    sudo podman tag ghcr.io/projectbluefin/dakota:stable  127.0.0.1:5000/dakota:stable
    sudo podman push --tls-verify=false 127.0.0.1:5000/bluefin:stable
    sudo podman push --tls-verify=false 127.0.0.1:5000/dakota:stable

# === Cleanup ===

# Kill stale QEMU processes and free disk space
cleanup:
    @echo "Killing stale QEMU processes..."
    -sudo kill $$(pgrep -f 'qemu-system.*disk.raw') 2>/dev/null
    @echo "Freeing podman storage..."
    -sudo podman system prune -af
    @echo "Removing E2E artifacts..."
    -rm -f disk.raw disk.raw.pre-migration qemu.log test_key test_key.pub e2e-run.log
    @echo "Done."

# Clean build artifacts
clean-build:
    cargo clean

# === Git ===

# Commit with conventional commit format
commit msg:
    git add -A && git commit -m "{{msg}}" && git push
