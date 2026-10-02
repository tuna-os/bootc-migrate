# External references that fuel the roadmap

Curated 2026-07-19. Each entry explains the lesson it provides.
Organized by the milestone it feeds (see ROADMAP.md).

## M0 — MVP hardening (rollback, health)

- **greenboot** — <https://github.com/fedora-iot/greenboot> and the Rust
  rewrite <https://github.com/fedora-iot/greenboot-rs> (Fedora 43 change:
  bootc support). Generic framework for systemd health checks:
  `required.d`/`wanted.d` script contract, retry-then-rollback policy.
  *For #22/#26: ship check scripts compatible with greenboot.
  Document greenboot for automatic rollback.
  Our `rollback` subcommand provides manual rollback. Red Hat's writeup:*
  <https://developers.redhat.com/articles/2024/08/12/greenboot-automate-rollbacks-atomically-updated-systems>

## M2 — Bootloader migration (#65)

- **bootupd** — <https://github.com/coreos/bootupd>. bootc's bootloader
  manager. `bootc install` runs it. It updates GRUB and shim.
  For systemd-boot, it defers to `bootctl`. For #65, a migrated system must
  use a state that bootupd recognizes or does not manage.
  Important interaction: bootc auto-detection for sealed composefs needs an image
  without bootupd (see M4).
- **Fedora bootloader phases** —
  <https://fedoraproject.org/wiki/Changes/BootLoaderUpdatesPhase1>.
  Phase 1 uses static GRUB with BLS. Phase 2 builds a new boot chain with bootupd.
  It keeps the previous chain as a fallback. This pattern validates our
  one-boot trial design.
- **openSUSE sdbootutil** —
  <https://en.opensuse.org/Systemd-boot>,
  <https://microos.opensuse.org/blog/2023-12-20-sdboot-fde/>. Complete GRUB to
  sd-boot tool. The `bootctl` wrapper manages kernels and initrd files per snapshot.
  It handles FDE with TPM2 policies and in-place migration. In 2025, GRUB2-BLS
  became the Tumbleweed default (<https://news.opensuse.org/2025/11/13/tw-grub2-bls/>).
  GRUB2 with BLS does not have ESP sync issues. For #65, consider `grub2-bls` where
  sd-boot fails.
- **kernel-install / BLS specification** — tools use type-1 Boot Loader
  Specification entries. Our resync hook will use a `kernel-install` drop-in first.

## M4 — Native store & generation matrix (#13, #72)

- **bootc composefs-native issue** —
  <https://github.com/bootc-dev/bootc/issues/1190> — upstream thread to monitor
  for CLI format changes.
- **Documentation for bootc composefs backend** —
  <https://bootc.dev/bootc/experimental-composefs.html>. The auto-detection
  rule: image has UKI, systemd-boot, and no bootupd.
- **Image seals with composefs** (Scrivano, 2026-06) —
  <https://scrivano.org/posts/2026-06-05-sealing-with-composefs/>. Details of
  image seals: one digest will cover content and metadata. The process clears `/boot`
  before digest calculation. This explains what `prepare-boot` does.
- **Ubuntu bootc experiment** —
  <https://github.com/jmarrero/ubuntu-bootc>. Kernel 7.0 breaks composefs boot
  (pinned to 6.17). Also, PAX archives fail during checksum checks in composefs-rs.
- **Storage goals for composefs** —
  <https://github.com/containers/storage/issues/2095>. Plans for native storage in podman.
- Local evidence: `docs/cfs-cli-generations.md` (matrix, upgrade steps, and
  kernel 6.12 mount needs).

## M1/M3 — Re-base engine & cross-base (comparative architectures)

- **rpm-ostree rebase flow** (Universal Blue docs) —
  <https://universal-blue.discourse.group/t/howto-rebase-to-a-ublue-image-from-fedora/6784>,
  <https://docs.projectbluefin.io/administration/>. Shows rebase to unsigned image
  to get keys, then rebase to signed image. The code handles these refs for ImageSwap.
- **Vanilla OS ABRoot** — <https://github.com/Vanilla-OS/ABRoot>. Dual root
  partitions with role swaps, OCI transactions, and LVM thin volumes.
- **openSUSE transactional-update / MicroOS** — snapshot transactions. Boot
  entry lists per snapshot allow multiple bootable deployments.

## M5 — Desktop & UX (#68, #15, #31)

- **The `mendingwall` utility** — <https://github.com/lawmurray/mendingwall>,
  <https://flathub.org/en/apps/org.indii.mendingwall>. Daemon for desktop settings.
  Watchlists of dconf keys define the inventory for desktop stashes.
- **aurorafin-config-mover** —
  <https://github.com/dtg01100/aurorafin-config-mover> — three-phase
  rebase orchestration, DE-scoped flatpak swap from official lists,
  generated `rollback.sh`, do-not-touch list (`~/.var/app`, containers,
  `~/.ssh`).
- **Migration tool for Linux desktops** —
  <https://codeberg.org/sesivany/linux-desktop-migration-tool>. Taxonomy of
  user state: XDG directories, Flatpaks, keys, and network profiles.

## Upstream watch list (subscribe, don't poll)

| What | Where | Why |
|---|---|---|
| bootc releases | <https://github.com/bootc-dev/bootc/releases/> | cfs CLI drift arrives here first |
| composefs-native tracking | <https://github.com/bootc-dev/bootc/issues/1190> | format/CLI direction |
| composefs-rs releases | crates.io `composefs`/`composefs-oci` | NativeStore dependency floor |
| ostree composefs tracking | <https://github.com/ostreedev/ostree/issues/2867> | legacy-side integration |
| greenboot-rs | <https://github.com/fedora-iot/greenboot-rs> | health-check contract for #22/#26 |
| sdbootutil news | <https://news.opensuse.org/tag/sdbootutil/> | sd-boot/FDE migration practice |
