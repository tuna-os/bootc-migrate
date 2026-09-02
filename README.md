# bootc-migrate

[![CI](https://github.com/tuna-os/bootc-migrate/actions/workflows/ci.yml/badge.svg)](https://github.com/tuna-os/bootc-migrate/actions/workflows/ci.yml)
[![E2E](https://github.com/tuna-os/bootc-migrate/actions/workflows/e2e-tests.yml/badge.svg?branch=main)](https://github.com/tuna-os/bootc-migrate/actions/workflows/e2e-tests.yml?query=branch%3Amain)

In-place migration utility that converts an OSTree-backend bootc system
(e.g. Bluefin) into a ComposeFS-backend bootc system (e.g. Dakota), without
reinstalling and without losing `/home`, `/var`, `/etc` customizations,
flatpaks, container storage, or user accounts.

## Migrate Bluefin → Dakota (quick start)

The common case: **Bluefin stable (btrfs) → Dakota stable**. Five steps. Your
old OSTree deployment stays in the boot menu as a fallback the whole time.

> ⚠️ **Back up anything you can't afford to lose first.** This rewrites how your
> system boots. It preserves `/home`, `/var`, `/etc`, flatpaks, container
> storage, and user accounts — but treat it as risky until you've rebooted and
> confirmed everything works. It's reversible until you run `commit` (step 5).

**1. Get the migrator.** Download the latest prebuilt binary (x86_64; for arm64
swap in `aarch64-unknown-linux-gnu`):

> **Release-naming note.** This repo was renamed from `bootc-migrate-composefs`
> to `bootc-migrate` after v0.2.0. `release.yml` already publishes under the
> new name, so from **v0.5.0** the tarball, the binary inside it, and the
> container image are all `bootc-migrate`. The commands below reflect that.
> To install the older v0.2.0 artifacts, substitute `bootc-migrate-composefs`
> in the paths.

```bash
curl -fsSL -o bmc.tar.gz \
  https://github.com/tuna-os/bootc-migrate/releases/latest/download/bootc-migrate-x86_64-unknown-linux-gnu.tar.gz
tar xzf bmc.tar.gz
sudo install -m755 bootc-migrate /usr/local/bin/bootc-migrate
```

<details><summary>…or pull the container image</summary>

A minimal image ships the same binary, useful when GitHub Releases is rate-limited/blocked, or to `COPY --from=` it into another Containerfile:

```bash
podman create --name bmc-extract ghcr.io/tuna-os/bootc-migrate:latest
podman cp bmc-extract:/usr/local/bin/bootc-migrate .
podman rm bmc-extract
sudo install -m755 bootc-migrate /usr/local/bin/bootc-migrate
```
</details>

<details><summary>…or build from source (needs Rust)</summary>

```bash
git clone https://github.com/tuna-os/bootc-migrate
cd bootc-migrate
cargo build --release
sudo install -m755 target/release/bootc-migrate /usr/local/bin/
```
</details>

**2. Dry-run** — makes no changes, just checks your system is ready:

```bash
sudo bootc-migrate \
  --target-image ghcr.io/projectbluefin/dakota:stable --dry-run
```

**3. Migrate** (~5–25 min depending on cache/network):

```bash
sudo bootc-migrate \
  --target-image ghcr.io/projectbluefin/dakota:stable
```

**4. Reboot** — the new composefs entry is the default. If anything looks wrong,
pick the old **Bluefin / OSTree** entry in the boot menu to get straight back.

```bash
sudo systemctl reboot
```

**5. Confirm, then make it permanent:**

```bash
cat /proc/cmdline | grep -o 'composefs=[0-9a-f]*'   # confirms composefs boot
sudo bootc-migrate commit                 # one-way; removes the OSTree fallback
```

> ⚠️ **Note:** Phase 4 copies `/var` to the composefs side. After migration,
> the two `/var` trees are **independent** — changes you make on the composefs
> side won't be reflected if you roll back to OSTree (and vice versa). Commit
> only when you're satisfied with the new system.

That's it. For flags, rollback, troubleshooting, and the full phase-by-phase
breakdown, see [Usage — end-to-end walkthrough](#usage--end-to-end-walkthrough).
On **Bluefin LTS** (XFS) or systems with **LVM / LUKS / a dedicated `/var`
partition**, the tool handles those automatically — see
[docs/filesystem-support.md](docs/filesystem-support.md).

> **Status: CI-validated, released, and proven on real hardware.** Seven E2E
> scenarios — btrfs, ext4, LUKS+XFS, LVM-on-LUKS with a dedicated `/var`, two
> ostree re-bases, and a TUI-driven migration — run in CI on every push to
> `main` (migration, commit, deep-clean, and `bootc status` /
> `upgrade --check` all green). Prebuilt binaries are on the
> [Releases](https://github.com/tuna-os/bootc-migrate/releases) page. Don't point this at a machine you can't
> reinstall, but the core path is stable.

## Interactive wizard (TUI)

Prefer a guided walkthrough over flags? Run the tool with no `--target-image`
(or `tui` explicitly) to launch a terminal wizard that walks through target
image selection, options, a plain-English review of what's about to happen,
and a live phase-by-phase progress view with scrollable logs:

```bash
sudo bootc-migrate tui
```

![bootc-migrate TUI wizard](docs/images/tui-review.png)

The wizard defaults to `--dry-run` and only builds the equivalent CLI
invocation shown on the Review screen — it doesn't need root just to browse;
root is required once you press Enter to actually run a migration.

## Architecture

```mermaid
flowchart TB
    %% ── Source: what we migrate from ────────────────────────────
    subgraph SRC["Source &middot; OSTree-backed Bluefin"]
        direction TB
        S_USR["<b>/</b> &mdash; OSTree hardlink farm<br/>/usr/etc &middot; /ostree/repo object store"]
        S_ETC["<b>/etc</b><br/>live, 3-way-merge source"]
        S_VAR["<b>/ostree/deploy/&lt;n&gt;/var</b><br/>user state"]
        S_BOOT["<b>/boot/loader/entries</b><br/>GRUB BLS &middot; ostree-*"]
    end

    %% ── The migration tool: six phases, one command ─────────────
    subgraph BIN["bootc-migrate &middot; 6 phases (0&ndash;5)"]
        direction TB
        P0["<b>Phase 0 &middot; Preflight</b><br/>ESP size &middot; NVRAM &middot; reflink"]
        P1["<b>Phase 1 &middot; OSTree import</b> (optional)<br/>reflink objects &rarr; composefs store"]
        P2["<b>Phase 2 &middot; OCI pull</b><br/>target image &rarr; composefs store"]
        P3["<b>Phase 3 &middot; EROFS seal</b><br/>build &middot; seal &middot; capture config digest"]
        P4["<b>Phase 4 &middot; Stage deploy</b><br/>3-way /etc merge (sealed mount)<br/>symlink prune &middot; identity-DB union<br/>/var copy &middot; .origin (tini)"]
        P5["<b>Phase 5 &middot; Bootloader</b><br/>sd-boot from sealed mount<br/>BLS entries on ESP &middot; NVRAM"]
        P0 --> P1 --> P2 --> P3 --> P4 --> P5
    end

    %% ── Target: what we migrate to ──────────────────────────────
    subgraph DST["Target &middot; ComposeFS-backed Dakota"]
        direction TB
        D_CFS["<b>/composefs/</b><br/>images/ (EROFS) &middot; objects/ &middot; streams/"]
        D_STATE["<b>/state/deploy/&lt;verity&gt;/</b><br/>etc/ &middot; &lt;verity&gt;.origin"]
        D_VAR["<b>/state/os/default/var</b><br/>bind-mounted as /var by initramfs"]
        D_BOOT["<b>/EFI/Linux/bootc_composefs-&lt;verity&gt;/</b><br/>vmlinuz &middot; initrd<br/>/loader/entries/*.conf &middot; systemd-bootx64.efi"]
    end

    %% ── Data flows across the lanes ─────────────────────────────
    S_USR -- "ostree object reflinks" --> P1
    P2 --> D_CFS
    S_ETC -- "current / old / new merge" --> P4
    P4 --> D_STATE
    S_VAR -- "verbatim copy<br/>(containers &middot; flatpaks &middot; machine-id)" --> D_VAR
    P3 -- "sealed config digest<br/>(not rootfs verity)" --> P4
    P3 --> P5
    P5 -- "copy from sealed mount" --> D_BOOT
    P5 -. "efibootmgr &middot; Linux Boot Manager" .-> NVRAM(["UEFI NVRAM"])

    RUN["<b>Booted Dakota</b><br/>/ = composefs overlay (RO)<br/>/etc writable &larr; state/<br/>/var writable &larr; state/os/default/var"]
    DST -. "reboot &rarr; systemd-boot &rarr; kernel<br/>cmdline composefs=&lt;verity&gt;" .-> RUN

    %% ── Lane colours ────────────────────────────────────────────
    classDef src fill:#e3f0ff,stroke:#3b82c4,color:#0b2545;
    classDef bin fill:#fff4e0,stroke:#d9920b,color:#5a3a00;
    classDef dst fill:#e4f7e7,stroke:#3ca34a,color:#06311a;
    classDef run fill:#f3e8ff,stroke:#8b5cf6,color:#2e1065;

    class S_USR,S_ETC,S_VAR,S_BOOT src;
    class P0,P1,P2,P3,P4,P5 bin;
    class D_CFS,D_STATE,D_VAR,D_BOOT dst;
    class RUN,NVRAM run;
```

**Key insight:** Phase 3 runs `bootc internals cfs oci seal` which prints the
sealed manifest's config digest. Phases 4 and 5 pass that **sealed config
digest** (not the rootfs verity) to `bootc cfs oci mount` — the overlay then
exposes real file content for `/etc`, kernel, initrd, systemd-boot, and kernel
modules, eliminating the need to re-stream OCI layers at runtime.

## What it does

Six phases (numbered 0–5 to match the console output), run as one command:

- **Phase 0 — Preflight** — free-space, reflink/CoW, UEFI, NVRAM-writable, ESP
  capacity.
- **Phase 1 — OSTree import** *(optional)* — reflinks existing OSTree file
  objects into the composefs object store so the pull in Phase 2 is mostly
  dedup. Skipped with `--skip-import`.
- **Phase 2 — OCI pull** — `bootc internals cfs oci pull` of the target bootc
  image into the composefs store.
- **Phase 3 — EROFS seal** — builds and seals the EROFS image, capturing the
  sealed config digest that Phases 4 and 5 mount.
- **Phase 4 — Stage deploy** — 3-way `/etc` merge (read from the sealed mount,
  no registry streaming), identity-DB line-union, dangling `/usr/*` symlink
  pruning, `/home` ↔ `/var/home` compatibility for native-home targets, `/var`
  preservation (copy or in-place dedicated subvolume), and `.origin`
  (boot_digest, manifest_digest) written via tini.
- **Phase 5 — Bootloader** — copies `systemd-bootx64.efi` from the sealed mount
  to the ESP (no registry streaming), verifies that the target kernel resolves
  the current wireless devices, writes BLS entries (including a durable
  stateroot mount argument for a dedicated `/var`), and registers `Linux Boot
  Manager` in UEFI NVRAM. The original GRUB entry is left as a rollback escape
  hatch.

After a successful reboot into the composefs entry, `bootc-migrate
commit` removes the OSTree fallback and makes composefs permanent.

## Usage — end-to-end walkthrough

> **Before you start.** This tool rewrites bootloader state and either reuses
> or copies the entire `/var`. Don't run it on a machine you can't reinstall
> in a pinch.
> Until you run `commit`, it's reversible — but a fresh backup is still
> cheap insurance.

### 1. Decide your target

The migration takes a `--target-image` — the composefs-backed bootc image
you want to end up on. Today the validated path is **Bluefin → Dakota**:

```
ghcr.io/projectbluefin/dakota:stable     # default target
```

If you're migrating a different OSTree-backed system (Aurora, Silverblue),
point `--target-image` at the composefs-flavored equivalent.

### 2. Check readiness with a dry-run

```bash
sudo bootc-migrate \
  --target-image ghcr.io/projectbluefin/dakota:stable \
  --dry-run
```

Things to confirm in the report:

- `Booted OSTree backend: Yes` — required; if `No` the tool refuses to run.
- `UEFI Boot Mode: Yes` + `NVRAM writable: Yes` — required for the
  systemd-boot path; on BIOS-only or locked NVRAM pass `--bootloader grub2`.
- `ESP Free Space: ≥ 150 MB` — we copy `systemd-bootx64.efi` from the
  target image onto the ESP.
- `Reflink (CoW) Support: Yes` — btrfs and XFS both support reflink.
- `ComposeFS free space: ≥ 1.1 × ostree_repo_size` — the composefs object
  store is built by reflinking your existing OSTree objects.

Optionally, preview what Phase 4's `/etc` merge will see before running it:

```bash
sudo bootc-migrate etc-drift
```

Lists every path where your live `/etc` has diverged from the OSTree factory
default (added/modified/removed/type-changed), read-only. Useful for
spotting a stale customization you no longer need before it carries forward
into the migrated system.

### 3. Run the migration

```bash
sudo bootc-migrate \
  --target-image ghcr.io/projectbluefin/dakota:stable
```

Expect ~5–10 minutes on warm caches, ~15–25 minutes on a cold pull. Six
phase headers (0–5) print as it goes:

| Phase | What's happening | Why it might take a while |
|---|---|---|
| **0 — Preflight** | Same checks as `--dry-run` | seconds |
| **1 — OSTree import** *(optional)* | Reflinks existing OSTree file objects into the composefs object store so Phase 2 mostly dedups | tens of seconds to a few minutes; skip with `--skip-import` |
| **2 — OCI pull** | `bootc internals cfs oci pull` of the target image | minutes (network-bound) |
| **3 — EROFS image** | Builds + fs-verity-signs the composefs metadata image | seconds |
| **4 — Stage deploy** | 3-way `/etc` merge (from sealed mount), dangling-symlink prune, identity-DB line-union, `/var` copy or dedicated-subvolume preservation, `.origin` file written | ~1 minute |
| **5 — Bootloader** | Copies systemd-boot from mounted image, writes BLS entries, registers NVRAM | ~30s |

When it ends with `=== MIGRATION COMPLETED ===` the on-disk state is
ready. Reboot:

```bash
sudo systemctl reboot
```

### 4. Validate the composefs boot

Log in (your existing accounts and SSH keys still work) and check:

```bash
cat /proc/cmdline                                       # must contain composefs=<hex>
bootc status                                            # should report the composefs deployment
bootc status --json | jq .status.booted.composefs        # non-null
```

Spend a day on it. Run your usual workflow — flatpaks, dnf, containers,
homebrew, GNOME extensions, whatever. Everything that lived under `/home`,
`/var`, and `/etc` on Bluefin should be where you left it. If something
is missing or broken, you can roll back (see below).

A login banner (`/etc/motd.d/85-bootc-migrate`) reminds you to run
`commit` on every login until you do, so a live migration doesn't sit
forgotten in this dual-boot state indefinitely. It clears itself once
`commit` runs — or once `undo` runs, since at that point there's nothing
left to commit.

### 5. Make it permanent (one-way)

Once you trust the new system:

```bash
sudo bootc-migrate commit
```

This removes the OSTree fallback from the ESP, drops GRUB2 boot artifacts,
and reclaims ~14 GiB of OSTree object store. The systemd-boot entry becomes
the sole default with timeout 0.

### Flags

| Flag                  | Purpose                                                            |
| --------------------- | ------------------------------------------------------------------ |
| `--dry-run`           | Print every action; touch nothing                                  |
| `--skip-import`       | Skip phase 1 (faster when target image is mostly new content)      |
| `--bootloader grub2`  | Stay on GRUB2 instead of installing systemd-boot                   |
| `--skip-preflight`    | Bypass preflight checks (don't, unless you know exactly why)       |
| `--force`             | Proceed past non-fatal warnings                                    |

### Move system Steam into Flatpak Steam

After installing and launching `com.valvesoftware.Steam` once, its per-user
data can absorb a system Steam installation without re-downloading games:

```bash
bootc-migrate system-to-flatpak-steam --dry-run
bootc-migrate system-to-flatpak-steam
```

Run it as the desktop user, **without** `sudo`, after closing Steam and all
Steam games. It uses filesystem renames only—never a recursive copy—to move
`steamapps`, `userdata`, and `config` from `~/.local/share/Steam` into Flatpak
Steam's data directory. The pre-existing Flatpak versions and both library
registries are retained in a timestamped rollback directory under
`~/.var/app/com.valvesoftware.Steam/`.

The command deliberately leaves the native Steam runtime files and unrelated
non-Steam folders such as `~/Games` alone. Add a Flatpak filesystem override
for `~/Games` separately if Steam shortcuts need it.

### Rollback / recovery

Until you run `commit`, the migration is **reversible**. The previous OSTree
deployment stays bootable:

- Phase 5 only *adds* the systemd-boot composefs entry; it never deletes the
  existing `/boot/loader/entries/ostree-*.conf` files.
- The original `/ostree/deploy/<n>/deploy/<commit>.0/` rootfs and
  `/ostree/deploy/<n>/var/` stay on disk.
- Phase 4 either copies `/var` to `state/os/default/var` or, for a dedicated
  filesystem/Btrfs subvolume, reuses it in place through a persistent BLS mount
  argument. The original data is not deleted during migration.
- We push `Linux Boot Manager` (systemd-boot) to the front of NVRAM `BootOrder`
  but the `Fedora` shim entry (which boots GRUB → OSTree) remains listed.

#### Automatic rollback subcommand

To return to the original OSTree deployment directly from the command line:

```bash
sudo bootc-migrate rollback --reboot
# or via the universal re-base CLI:
sudo bootc-rebase rollback --reboot
```

This verifies prerequisites, re-orders UEFI `BootOrder` so the OSTree entry (Fedora/GRUB) takes top priority, and reboots immediately into the OSTree deployment.

#### Manual firmware recovery

If the system fails to boot into composefs or NVRAM state is interrupted:

1. Power on; tap the firmware boot-menu key (commonly **F12**, **F8**, or **Esc**).
2. Pick the `Fedora` entry. GRUB will show the original `ostree:0` menu.
3. Boot it. You land on the pre-migration system with its `/var` and `/etc` intact.

Or, from a working composefs login, one-shot:

```bash
sudo efibootmgr -v | grep -E 'Fedora|Linux Boot Manager'
sudo efibootmgr --bootnext <Boot####-of-Fedora>
sudo systemctl reboot
```

Pre-migration diagnostic snapshots and logs are automatically recorded under `/var/log/bootc-migrate/` on every run (`preflight-*.json` and `migration.log`) so boot configuration can be manually reconstructed if NVRAM is ever wiped.

After running `bootc-migrate commit`, the OSTree fallback is removed
from the ESP and rollback becomes a fresh install. The E2E test exercises the
full round-trip (composefs → OSTree → composefs) on every run.

### What's preserved

Validated end-to-end (21+ assertions per run; see `tests/run-e2e.sh`):

- **/var data** — `/var/lib/*`, `/var/log/*`, `/var/cache/*`, containers,
  flatpak system installs, machine-id, hidden dirs and symlinks
- **User homes** — `/var/home/<user>/`, dotfiles, project trees, SSH keys
  (with `.ssh` mode preserved so StrictModes still accepts your keys),
  wallpapers, GNOME extensions, dconf user db, glib gsettings keyfile,
  homebrew Cellar, per-user flatpak installs. When an OSTree source uses
  `/home -> /var/home` and the target has a native `/home`, both absolute path
  spellings remain valid through an ordered compatibility bind mount.
- **/etc state** — `/etc/sudoers.d/*`, `/etc/hosts` edits, custom
  `sshd_config.d/*`, custom config files added under `/etc/`, in-place
  edits to image-shipped files (`/etc/hostname`), `/etc` symlinks
- **Accounts** — `/etc/passwd`, `/etc/shadow`, `/etc/group` line-union
  merged so users you added survive *and* users the target image needs
  (messagebus, polkitd, …) get added

What's intentionally *not* carried forward:

- OSTree/rpm-ostree state markers (`.updated`, `.rpm-ostree-shadow-mode-fixed2.stamp`)
- GRUB2 config files (`grub2.cfg`, `grub2-efi.cfg`, `/etc/grub.d/`) — the
  target uses systemd-boot
- Source-image `/etc` files the target image removed (e.g. `sshd_config.d/40-redhat-crypto-policies.conf`
  which references `/etc/crypto-policies/` paths Dakota doesn't ship)

### Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Phase 0 refuses with "System is not booted into an OSTree deployment" | You're already on composefs (or a non-bootc system) | Nothing to do |
| Phase 2 fails with ENOSPC mid-pull | `/sysroot/composefs` is tight on the 1.1× heuristic | Free space or grow the partition, then rerun |
| Post-reboot `cat /proc/cmdline` shows `ostree=` not `composefs=` | Firmware ignored the new NVRAM entry, or OVMF loaded `Fedora\shim` instead | Use firmware boot menu to pick `Linux Boot Manager`; if that fails, fall back to OSTree and report the firmware quirk |
| `bootc status` says "No manifest_digest in origin" | You're on an old build of this tool | Update to `main` — version info is on the first line of the migration log |
| SSH key auth broken post-migration | Permissions changed during /var copy | Boot OSTree fallback and `chmod 700 ~/.ssh; chmod 600 ~/.ssh/authorized_keys` |
| GNOME boots but session settings (wallpaper, accent) look wrong | dconf database needs recompile | `dconf update` as your user, or log out + back in |
| Phase 5 refuses because the target kernel has no module alias for a wireless device | The image omits the driver for Wi-Fi hardware present on the source system | Fix or update the target image. Use `--force` only when alternate networking is available and losing Wi-Fi is acceptable |
| Migration went wrong and you want to undo it | Something failed mid-migration | Run `sudo bootc-migrate undo` (removes composefs boot artifacts, keeps object store) or `sudo bootc-migrate undo --full` (full cleanup including object store); then reboot into OSTree |

## Requirements

- Booted on an OSTree-backed bootc system (Bluefin, Aurora, Silverblue…)
- UEFI firmware with writable NVRAM (for the systemd-boot path; GRUB2 fallback
  works on BIOS)
- Btrfs or XFS sysroot with reflink/CoW support
- ESP with ≥150 MB free
- ≥ `1.1 × ostree_repo_size` free on `/sysroot/composefs` (no reflink: 1.5×)
- Outbound registry access for `bootc internals cfs oci pull`
  (Phase 2 fetches the target image; Phases 4–5 read artifacts from the sealed
  mount, so no runtime registry access is needed after Phase 2)

## Building

```
cargo build --release
```

Drops a single binary at `target/release/bootc-migrate`.
Requires Rust 1.85+ and a Linux host with `libxkbcommon-dev`.

## End-to-end tests

A QEMU-based E2E harness lives in `tests/run-e2e.sh`. It installs Bluefin
into a disk image, runs the migration against a registry mirror of the
Dakota target image, reboots, and validates the full round-trip.

```
sudo ./tests/run-e2e.sh
```

Overridable via env: `BASE_IMAGE`, `TARGET_IMAGE`, `DISK_SIZE`,
`FILESYSTEM`, `SKIP_SETUP`, `E2E_MODE`.

The CI matrix runs seven cells (see `.github/workflows/e2e-tests.yml`, which
is authoritative):

| Cell | Base → target | Filesystem | Disk |
|---|---|---|---|
| composefs migration | bluefin:stable → dakota:stable | btrfs | 40G |
| composefs migration | bluefin:lts → dakota:stable | ext4 | 40G |
| composefs migration | bluefin:lts → dakota:stable | xfs+crypt | 40G |
| composefs migration | bluefin:lts → dakota:stable | xfs+lvm+crypt | 60G |
| ostree re-base | bluefin:stable → dakota:stable | btrfs | 40G |
| ostree re-base, GNOME→KDE (non-gating) | bluefin:stable → aurora:stable | btrfs | 40G |
| TUI-driven migration | bluefin:stable → dakota:stable | btrfs | 40G |

Only the two `xfs*` cells exercise the ext4-loopback composefs store (XFS has
no fs-verity); btrfs and ext4 seal in place.

## Layout

A Cargo workspace with three crates (see [ROADMAP.md](ROADMAP.md) for why):

- `crates/bootc-migrate-core` — the capability library everything else is
  built from: phases, preflight/readiness, `/etc` merge (`mergetc`), OSTree
  object scan, registry streaming, transaction (`commit`/`undo`), target-image
  capability scan (`scan`), cross-base UID/GID remap (`remap`), UEFI boot-entry
  audit (`boot_audit`), DE detection (`de_detect`) and config stash/restore
  (`de_migrate`), types.
- `crates/bootc-migrate` — **the protected MVP binary** described
  above. CLI surface (clap), `commit`/`undo`/`rollback` subcommands, the TUI
  wizard. Its E2E cells are untouchable regression gates — this binary's
  behavior doesn't change as new capability lands in `bootc-rebase`.
- `crates/bootc-rebase` — the universal re-base engine binary; see below.
- `tests/run-e2e.sh` — QEMU E2E harness exercising both binaries.

## `bootc-rebase` — the universal re-base engine

`bootc-migrate` above does one proven thing: OSTree → ComposeFS.
`bootc-rebase` is the generalization — a routing table over
**backend × strategy** that will eventually cover every bootc re-base shape
(same-backend image swaps, cross-backend conversions, bootloader changes,
cross-distro-family moves, desktop-environment switches). It's newer and less
battle-tested than the MVP binary; treat subcommands marked *skeleton* or
*read-only* below as previews, not yet full features.

```bash
cargo build --release -p bootc-rebase
```

| Subcommand | What it does | Status |
|---|---|---|
| `scan <image>` | Registry-streamed capability probe of a target image — composefs/ostree readiness, fs-verity requirement, transient root/etc, bootloader payload, desktops, base OS identity, sysusers, initramfs flavor, filesystem expectation, and a `Compatible: YES/NO` verdict with reasons. `--json` for machine output. | Done |
| `rebase --target-image <image>` | Re-base the running system, routing on `--source-backend`/`--target-backend` through the strategy table below. `--plan` prints the route, selected phases, and bootloader policy, then exits without touching the system. | Implemented for ostree→composefs (the MVP pipeline), composefs→composefs (image swap), and ostree→ostree (native `bootc switch`, with cross-base UID/GID remap when host and target disagree on distro family — pass `--accept-cross-base` to proceed past the report) |
| `rollback [--reboot]` | Re-order UEFI `BootOrder` back to the previous deployment. | Done |
| `boot-entries [--json] [--interactive] [--rename-branding] [--apply] [--undo]` | Enumerate and classify UEFI boot entries: dead (loader path missing), generic-label, duplicate, firmware-managed, plus which are protected and why. **Dry-run by default** — a bare invocation is the read-only audit. `--interactive` opens a checklist (protected entries are unselectable), `--rename-branding` proposes renaming the booted entry to `PRETTY_NAME`, `--apply` writes the result to NVRAM after a typed confirmation and a restorable snapshot, and `--undo` replays that snapshot. | Audit and the cleanup **planner** are unit-tested (protections, the last-bootable-entry guard, and the "every entry looks dead ⇒ the ESP is wrong" refusal). The gating OSTree re-base E2E cell also renames a live OVMF NVRAM entry, applies the plan, undoes it, and asserts byte-identical `efibootmgr -v` output. This covers the executor and snapshot restore; it does **not** cover the separate, unimplemented GRUB2→systemd-boot flip ([#31](https://github.com/tuna-os/bootc-migrate/issues/31), [#189](https://github.com/tuna-os/bootc-migrate/issues/189)) |
| `de-migrate stash\|restore` | Move a user's desktop-environment config (GNOME dconf/gnome-shell, KDE kdeglobals/plasma, COSMIC, niri, XFCE) into or out of a stash directory around a cross-DE re-base — union of paths per issue [#68](https://github.com/tuna-os/bootc-migrate/issues/68), never deletes. `--run-hooks` executes `pre-switch.d`/`post-switch.d` scripts with `REBASE_FROM_DE`/`REBASE_TO_DE`/`REBASE_STASH_DIR`/`REBASE_HOME` set. `--dry-run` previews without touching anything. | Done. Also runs automatically inside `rebase --de-migrate`; this subcommand remains the manual escape hatch for images shipping several desktops (which detection refuses to guess between) |
| `rebase --de-migrate` | Detects the desktop environment the target image ships (registry-streamed session files, session binaries, and display-manager default session — no `podman pull`) and the one this host runs. When they differ, stashes every human account's outgoing DE config before staging and re-exposes any stash a previous re-base in the other direction left behind, running the `pre-switch.d`/`post-switch.d` hooks around each. | Done, unit-tested; **off by default** — a re-base never touches per-user desktop state unless asked to. The non-gating Bluefin→Aurora E2E cell passes `--de-migrate`, seeds GNOME config, and asserts that the cross-DE plan and stash are created. Because target desktop detection uses the registry scan, this evidence depends on that exploratory cell completing successfully ([#68](https://github.com/tuna-os/bootc-migrate/issues/68), [#188](https://github.com/tuna-os/bootc-migrate/issues/188)) |
| `migrate-bootloader --to systemd-boot` | GRUB2 → systemd-boot conversion, standalone of a backend re-base. | **Not implemented** — the subcommand exists and always refuses; only the pure BLS-entry/kernel-arg/entry-token logic it will use has landed. Live ESP populate + NVRAM cutover + the kernel-install resync hook (without which a flipped system would silently boot stale kernels) are deliberately deferred pending explicit sign-off and a dedicated E2E cell — see [#65](https://github.com/tuna-os/bootc-migrate/issues/65) for the full implementation plan |

`rebase`'s routing table (`crates/bootc-rebase/src/routing.rs` is the single
source of truth the CLI consults before touching anything):

| From ↓ \ To → | ostree | composefs |
|---|---|---|
| **ostree** | `OstreeDeploy` (native `bootc switch`) | `CoreMigration` (this repo's proven phase 0–5 pipeline) |
| **composefs** | planned, not implemented | `ImageSwap` |

## Roadmap

Full milestone plan, current status per issue, and design decisions live in
[ROADMAP.md](ROADMAP.md) — start there for "what's next" and "why did we
choose X over Y."

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for setup and
[REVIEW.md](REVIEW.md) for the code-review expectations. Run `just check` (clippy,
rustfmt, unit tests, shellcheck) before opening a PR. AI-assisted contributions
should follow [AGENTS.md](AGENTS.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual-licensed as above, without any additional terms or conditions.
