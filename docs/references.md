# External references that fuel the roadmap

Curated 2026-07-19. Each entry says *what it teaches us*, not just what it
is. Organized by the milestone it feeds (see ROADMAP.md).

## M0 — MVP hardening (rollback, health)

- **greenboot** — <https://github.com/fedora-iot/greenboot> and the Rust
  rewrite <https://github.com/fedora-iot/greenboot-rs> (Fedora 43 change:
  bootc support). Generic systemd health-check framework:
  `required.d`/`wanted.d` script contract, retry-then-rollback policy.
  *For #22/#26: instead of inventing our own post-migration health gating,
  ship greenboot-compatible check scripts and document greenboot as the
  automatic-rollback layer; our `rollback` subcommand covers the manual
  path. Red Hat's writeup:*
  <https://developers.redhat.com/articles/2024/08/12/greenboot-automate-rollbacks-atomically-updated-systems>

## M2 — Bootloader migration (#65)

- **bootupd** — <https://github.com/coreos/bootupd>. bootc's bootloader
  manager; `bootc install` always runs it. Key facts: it updates GRUB+shim,
  and for systemd-boot it defers to `bootctl` ("would probably just proxy
  that"). *For #65: a migrated system should end in a state bootupd
  recognizes (or explicitly doesn't manage), not a hand-rolled hybrid.
  CRITICAL interaction: bootc's sealed-composefs auto-detection requires
  the image to NOT have bootupd (see M4) — bootloader strategy and backend
  strategy are coupled.*
- **Fedora bootloader phases** —
  <https://fedoraproject.org/wiki/Changes/BootLoaderUpdatesPhase1> (static
  GRUB config, BLS-only, no per-update regeneration; phase 2: bootupd
  builds a full new boot chain shim→grub2→kernel while retaining the
  working chain as fallback). *Their "retain working chain as fallback"
  is exactly our BootNext one-boot-trial design — validated pattern.*
- **openSUSE sdbootutil** —
  <https://en.opensuse.org/Systemd-boot>,
  <https://microos.opensuse.org/blog/2023-12-20-sdboot-fde/>. The most
  complete production GRUB→sd-boot migration in the wild: bootctl wrapper
  managing kernel/cmdline/initrd bookkeeping per snapshot, FDE with
  TPM2 policy prediction (systemd-pcrlock / pcr-oracle), and a documented
  in-place migration (`zypper rm grub2-common; zypper in sdbootutil
  systemd-boot`). Note their 2025 pivot: **GRUB2-BLS became Tumbleweed's
  default** (<https://news.opensuse.org/2025/11/13/tw-grub2-bls/>) — a
  BLS-native GRUB2 is an alternative endpoint that avoids the ESP-resync
  problem entirely. *For #65: consider `grub2-bls` as a cheaper interim
  target on systems where sd-boot is blocked (NVRAM ro, weird ESPs).*
- **kernel-install / BLS spec** — sdbootutil and Fedora both anchor on
  Boot Loader Specification type-1 entries. The standalone path installs
  `/etc/kernel/install.d/95-bootc-migrate-esp-sync.install` as the resync
  drop-in; a path-unit fallback is not needed because the hook receives the
  kernel and initramfs paths directly from `kernel-install`.

## M4 — Native store & generation matrix (#13, #72)

- **bootc composefs-native tracking issue** —
  <https://github.com/bootc-dev/bootc/issues/1190> — THE upstream thread
  to watch for CLI/format changes (the #72 drift came from this work).
- **bootc composefs backend docs** —
  <https://bootc.dev/bootc/experimental-composefs.html> — states the
  auto-detection rule: image has UKI + systemd-boot + **no bootupd** ⇒
  bootc installs with the composefs backend. *Defines the target-image
  shape for a fully-sealed future; also why #65's bootloader choice feeds
  #13's backend selection.*
- **Image sealing with composefs** (Scrivano, 2026-06) —
  <https://scrivano.org/posts/2026-06-05-sealing-with-composefs/> — the
  sealing model in depth: one digest covers content+metadata; the UKI
  circular dependency is broken by emptying /boot before digest
  computation. *Explains what `prepare-boot` does and what a future
  "seal-grade" migration must reproduce.*
- **Ubuntu bootc experiment** —
  <https://github.com/jmarrero/ubuntu-bootc> — two live landmines we will
  inherit: kernel 7.0 regression breaks composefs boot ("Failed to
  execute /sbin/init"; pinned back to 6.17), and Rockcraft/umoci **PAX
  tars fail composefs-rs round-trip** ("Layer has incorrect checksum").
  *For NativeStore: add a PAX-layer detection/skip note to #13; for E2E:
  kernel-version gate on future cells.*
- **containers/storage composefs end-state** —
  <https://github.com/containers/storage/issues/2095> — where podman's
  native composefs storage is headed; converges with our
  `LocalFetchOpt::IfPossible` zero-copy pull path.
- Local evidence base: `docs/cfs-cli-generations.md` (compatibility
  matrix, free in-place upgrade, deterministic EROFS ids, kernel ≥6.12
  file-backed EROFS mount requirement).

## M1/M3 — Re-base engine & cross-base (comparative architectures)

- **rpm-ostree rebase semantics** (Universal Blue docs) —
  <https://universal-blue.discourse.group/t/howto-rebase-to-a-ublue-image-from-fedora/6784>,
  <https://docs.projectbluefin.io/administration/> — the
  `ostree-unverified-registry:` → signed two-step (rebase to unsigned to
  acquire keys/policy, then to signed), and `ujust rebase-helper` UX.
  *For #66/#64: our transport-strip matcher already handles these refs;
  the two-step signing dance belongs in the ImageSwap/OstreeDeploy docs,
  and `ujust`-style guided selection is the #31 UX model.*
- **Vanilla OS ABRoot** — <https://github.com/Vanilla-OS/ABRoot> — the
  other atomicity design: two root partitions with role swap, OCI-image
  transactions, LVM thin provisioning. *Contrast architecture for docs;
  their "local OCI image on top of system image" is a pattern for #68
  hook-shipped customizations. Not a code dependency.*
- **openSUSE transactional-update / MicroOS** — snapshot-based
  transactions (see sdbootutil links above) — the third family; their
  per-snapshot boot-entry bookkeeping is the strongest prior art for
  keeping N deployments bootable simultaneously.

## M5 — Desktop & UX (#68, #15, #31)

- **Mending Wall** — <https://github.com/lawmurray/mendingwall>
  (<https://flathub.org/en/apps/org.indii.mendingwall>) — per-DE settings
  stash/restore daemon; watchlists of dconf keys + config files are the
  authoritative per-DE inventory for our E1 stash.
- **aurorafin-config-mover** —
  <https://github.com/dtg01100/aurorafin-config-mover> — three-phase
  rebase orchestration, DE-scoped flatpak swap from official lists,
  generated `rollback.sh`, do-not-touch list (`~/.var/app`, containers,
  `~/.ssh`).
- **Linux Desktop Migration Tool** —
  <https://codeberg.org/sesivany/linux-desktop-migration-tool> — category
  taxonomy of migratable user state (XDG dirs, flatpaks+data, Toolbx,
  keys/keyrings, GOA, NetworkManager profiles).

## Upstream watch list (subscribe, don't poll)

| What | Where | Why |
|---|---|---|
| bootc releases | <https://github.com/bootc-dev/bootc/releases/> | cfs CLI drift arrives here first |
| composefs-native tracking | <https://github.com/bootc-dev/bootc/issues/1190> | format/CLI direction |
| composefs-rs releases | crates.io `composefs`/`composefs-oci` | NativeStore dependency floor |
| ostree composefs tracking | <https://github.com/ostreedev/ostree/issues/2867> | legacy-side integration |
| greenboot-rs | <https://github.com/fedora-iot/greenboot-rs> | health-check contract for #22/#26 |
| sdbootutil news | <https://news.opensuse.org/tag/sdbootutil/> | sd-boot/FDE migration practice |
