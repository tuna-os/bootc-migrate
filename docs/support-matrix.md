# Support matrix — what CI has proven, by axis

Six factors decide if a re-base works: **backend**, **bootloader**,
**LUKS**, **UKI**, **package manager**, **DE**. This file records what the
E2E matrix in [e2e-tests.yml](../.github/workflows/e2e-tests.yml) has proven
about each axis — and refuses to record anything else.

## The evidence rule

Every status below carries its proof. There are exactly three statuses:

- **PROVEN** — green on a `main` E2E run, cited by run id and date.
  Green on a PR branch is signal, not proof: PR code is not the product.
- **RED** — red on a `main` E2E run, cited the same way. A red cell proves
  "does not work here", not "cannot work". The failure can be the route,
  the harness, or the image, until triaged.
- **UNPROVEN** — no cell, or no green on `main`. Unknown. Do not write
  "supported", "unsupported", "works", or "does not work". Write "unknown"
  and state what will prove it.

Reference runs used below (both `e2e-tests.yml`):

- `main` 33891928677 (2026-09-04): last main run at publication time.
  Gating cells passed, except LVM-on-LUKS (red).
- PR 36215098705 (2026-09-26): recent branch run, cited only to show
  flakiness where a cell disagrees with `main`.

Non-gating cells disagree between runs (opensuse, one tunaOS
cell each flipped). A status changes only on a new `main` run. When you
update this file, update the reference runs above.

Unit tests check the logic of route tables, karg filters, and ESP classifiers.
They do not promote a status past UNPROVEN. Only a booted cell proves a claim.

## Backend (source × target route)

Source of truth for the table shape:
[rebase_plan.rs](../crates/bootc-migrate-core/src/rebase_plan.rs) `ROUTES`.

| Route | Strategy | Status | Proof |
|---|---|---|---|
| ostree → composefs | CoreMigration | **PROVEN** | `bluefin -> dakota` btrfs + ext4 cells green on main 33891928677 (gating) |
| composefs → composefs | ImageSwap | **UNPROVEN** | no active cell on main (exploratory `dakota -> utah` cell deleted per #304: Utah initramfs lacks composefs module) |
| ostree → ostree | OstreeDeploy | **PROVEN** | `bluefin ostree re-base` green on main 33891928677 (gating) |
| composefs → ostree | OstreeInstall | **RED** | `dakota -> fedora-bootc` red on main 33891928677 *and* on PR 36215098705 — untriaged |

Meaning of RED for the red route: CI did not triage the failure. No claim
explains *why*. The OstreeInstall failure does not prove a broken route. The
cause can be harness or image.

Route availability by frontend (mechanism fact, not a support claim):

| Route | `bootc-migrate` | `bootc-rebase` | TUI wizard |
|---|---|---|---|
| CoreMigration | `--target-image` | `-t …` (auto) | full wizard |
| ImageSwap | automatic fallback when already composefs | `-t … --target-backend composefs` | same wizard, swap path |
| OstreeDeploy | no | `-t … --target-backend ostree` on ostree hosts | no |
| OstreeInstall | no (`--target-backend` does not exist) | `-t … --target-backend ostree` on composefs hosts | no |

## Bootloader

| Combination | Status | Proof |
|---|---|---|
| CoreMigration → systemd-boot (UEFI) | **PROVEN** | every green CoreMigration cell boots it (main 33891928677) |
| CoreMigration → GRUB2 (`--bootloader grub2`) | **UNPROVEN** | no cell sets it (`grub.rs` is a placeholder; the logic lives in `phase5_setup_bootloader` with no E2E) |
| KeepSource on OstreeDeploy (GRUB stays GRUB) | **PROVEN** | gating `bluefin ostree re-base` green on main 33891928677 |
| KeepSource on ImageSwap (systemd-boot stays) | **UNPROVEN** | no active cell (image-swap cell deleted per #304) |
| OstreeInstall GRUB-first + "Linux Boot Manager" rollback entry | **UNPROVEN** | cell is RED; bootloader vs route failure not separated |
| Standalone `migrate-bootloader` (#65) | refuses by design | pure BLS/karg logic is unit-tested; live ESP/NVRAM mutation is not implemented and no CLI path runs it — no boot claim exists to prove |
| BIOS/CSM (non-UEFI) hosts, any route | **UNPROVEN** | harness boots OVMF/UEFI only; no BIOS cell |
| GRUB2-BLS as an endpoint (openSUSE default) | **UNPROVEN** | reference note in `docs/references.md` only |

## LUKS / disk layout

| Combination | Status | Proof |
|---|---|---|
| CoreMigration, plain (btrfs, ext4) | **PROVEN** | gating cells green on main 33891928677 |
| CoreMigration, LUKS root (`xfs+crypt`, TPM2 unlock) | **PROVEN** | gating cell green on main 33891928677 |
| CoreMigration, LVM-on-LUKS + separate `/var` | **RED** (`main`); green on a PR branch | red on main 33891928677, green on PR 36215098705 — flaky, unproven |
| CoreMigration, encrypted `/boot` | **UNPROVEN** | E2E layout assumes unencrypted `/boot`; no code, no cell |
| CoreMigration, btrfs-on-LUKS | **UNPROVEN** | no code path, no cell |
| CoreMigration, detached LUKS headers | **UNPROVEN** | no code path, no cell |
| CoreMigration, passphrase (non-TPM2) unlock | **UNPROVEN** | harness unlocks via swtpm only; no sendkey/passphrase cell |
| LUKS of any kind on ImageSwap / OstreeDeploy | **UNPROVEN** | no cells; both rely on `bootc switch` native behavior, which no cell exercises under encryption |
| LUKS of any kind on OstreeInstall | **UNPROVEN** | `carry_over_kargs` keeps `rd.luks.*`/`rd.lvm.lv` in code, but the route's only cell is RED on unencrypted disk — encryption adds an unproven axis to a red route |

## UKI (boot artifact format)

| Combination | Status | Proof |
|---|---|---|
| BLS Type 1 (`loader/entries/*.conf` + kernel/initrd on ESP) for composefs | **PROVEN** | every green composefs-boot cell boots it (main 33891928677) |
| UKI Type 2 (`.efi` unified image) for composefs | **UNPROVEN, no implementation** | zero `uki` matches in `crates/`; SPECIFICATION.md §3 plans it, nothing builds/installs/detects `.efi` artifacts |
| Upstream auto-detection rule (UKI + systemd-boot + no bootupd ⇒ composefs backend) | reference only | cited in `docs/references.md` from bootc docs; not implemented as logic anywhere |

Consequence for the Utah question: "composefs-capable" in scan output means
`prepare-root.conf` enables composefs. The scan does not show that the
initramfs contains the composefs module (Utah does not have it). ImageSwap
stages Utah's own initramfs without regeneration. A composefs deployment cannot
boot without composefs initramfs support. We deleted the exploratory
`dakota -> utah` image-swap cell in #304. Dakota -> Utah uses the
OstreeInstall route (#302).

## Package manager (lineage input)

The scanner reads the package manager of each image
([scan.rs](../crates/bootc-migrate-core/src/scan.rs) `BaseInfo`). Two images
with the same manager belong to one family. A pair without evidence
(Dakota has none) warns and keeps the standard merge
([cross_family.rs](../crates/bootc-migrate-core/src/cross_family.rs)).

| Combination | Status | Proof |
|---|---|---|
| Different manager, cross-family policy (`→ opensuse-bootc`) | **RED** (latest); green on `main` | green on main 33891928677, red on PR 36215098705 — flaky, unproven |
| Same-manager shortcut | **UNPROVEN** at E2E level | unit-tested only; no cell isolates it |
| No-manager Unknown path (Dakota as source) | **UNPROVEN** at E2E level | exercised only inside RED/flaky cells (OstreeInstall) |

## DE (desktop environment)

| Combination | Status | Proof |
|---|---|---|
| Same DE, any green route | **PROVEN** | gating cells (all GNOME→GNOME) green on main 33891928677 |
| Cross-DE GNOME → KDE, no `--de-migrate` (aurora) | **PROVEN** (non-gating) | green on main 33891928677 |
| Cross-DE + `--de-migrate` stash/restore (tunaOS ring, 4 cells) | **PROVEN** (non-gating) | all four green on main 33891928677 |
| `--de-migrate` on CoreMigration / ImageSwap / OstreeInstall | **UNPROVEN** | `--de-migrate` is exercised only in `ostree-rebase` E2E mode |

## The Dakota → Utah migration, per axis

The concrete target: Dakota (composefs, systemd-boot, BLS-1, btrfs-on-LUKS,
no package manager, GNOME) → Utah (ostree, GRUB via bootupd, GNOME).

| Axis | Status for this pair |
|---|---|
| Backend (composefs → ostree) | **RED** with `fedora-bootc:44`; **no cell** with Utah — unproven for this pair |
| Bootloader (systemd-boot → GRUB-first + rollback entry) | **UNPROVEN** (only exercised inside the RED cell) |
| LUKS (btrfs-on-LUKS source) | **UNPROVEN** (no btrfs-on-LUKS cell on any route) |
| UKI | N/A — ostree target boots BLS/GRUB, not a UKI question |
| Package manager (none → ?) | **UNPROVEN** (Unknown-lineage path, no green E2E) |
| DE (GNOME → GNOME) | same-DE path, proven on other routes only |

Real-host test on 2026-09-29: the stage step exited 0, but the reboot opened a
GRUB shell. Cause: the disk has no separate `/boot` partition. GRUB targets a
filesystem inside LUKS2+argon2id, which GRUB cannot unlock. Secure Boot was
disabled, and shim executed GRUB. A successful stage step does not ensure boot
on this layout. Issue #305 tracks this problem.

Overall: **UNPROVEN**. A test on a real host today is an experiment, not
a supported migration. Promotion needs this sequence:

1. Triage the RED `dakota -> fedora-bootc` OstreeInstall cell — a green
   route with a small target first.
2. Add a `dakota -> utah` OstreeInstall cell (or re-target the
   `fedora-bootc` one once Utah fits the job time budget) and get it green (#302).
3. Decide the image-swap cell fate: decided in #304. We deleted the cell
   because Utah lacks composefs support in its initramfs. Issue #302 tracks
   Dakota -> Utah on the OstreeInstall route.
4. LUKS coverage for the route last — it is the hardest axis to add
   (swtpm + fisherman recipe per `docs/luks-testing.md`).

## Maintenance

- This file follows `main` E2E runs, not PR runs. After a green `main` run,
  update the reference runs and flip whatever it proved.
- A "supported" claim anywhere (README, TUI copy, refusal text) must name
  its cell here. No cell, no claim — write "unknown" and link the gap.
- Flaky cells keep the worse status until a fix makes the cell green.
