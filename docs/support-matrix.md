# Support matrix — what CI has proven, by axis

A re-base depends on six axes: **backend**, **bootloader**, **LUKS**,
**UKI**, **package manager** and **DE**. This file records what the
E2E matrix in [e2e-tests.yml](../.github/workflows/e2e-tests.yml) has proven
about each axis — and refuses to record anything else.

## The evidence rule

Every status below carries its proof. There are exactly three statuses:

- **PROVEN** — green on a `main` E2E run, cited by run id and date.
  Green on a PR branch is signal, not proof: PR code is not the product.
- **RED** — red on a `main` E2E run, cited the same way. A red cell proves
  "does not work here". It does not prove "cannot work". Until you triage
  it, the cause can be the route, the harness or the image.
- **UNPROVEN** — no cell, or no green on `main`. Unknown. Do not write
  "supported", "unsupported", "works" or "does not work" about these.
  This applies to docs, TUI copy and refusal messages. The honest word is
  "unknown", plus what would prove it.

Reference runs used below (both `e2e-tests.yml`):

- `main` 33891928677 (2026-09-04): the last main run when we wrote this.
  It shows all gating cells green, except LVM-on-LUKS (red).
- PR 36215098705 (2026-09-26): recent branch run, cited only to show
  flakiness where a cell disagrees with `main`.

Non-gating cells disagree between runs (image-swap, opensuse, one tunaOS
cell each flipped). A status changes only on a new `main` run. When you
update this file, update the reference runs above.

Unit tests prove only pure logic, for example route tables, karg filters
and ESP classifiers. They never move a status past UNPROVEN here. The claim
is that the system boots, and only a cell that boots proves it.

## Backend (source × target route)

Source of truth for the table shape:
[rebase_plan.rs](../crates/bootc-migrate-core/src/rebase_plan.rs) `ROUTES`.

| Route | Strategy | Status | Proof |
|---|---|---|---|
| ostree → composefs | CoreMigration | **PROVEN** | `bluefin -> dakota` btrfs + ext4 cells green on main 33891928677 (gating) |
| composefs → composefs | ImageSwap | **RED** (`main`); green once on a PR branch | `dakota -> utah` red on main 33891928677, green on PR 36215098705 — flaky, unproven |
| ostree → ostree | OstreeDeploy | **PROVEN** | `bluefin ostree re-base` green on main 33891928677 (gating) |
| composefs → ostree | OstreeInstall | **RED** | `dakota -> fedora-bootc` red on main 33891928677 *and* on PR 36215098705 — untriaged |

What RED means for the two red routes: nobody triaged the failure, so
this file makes no claim about *why*. The image-swap red does not prove
"Utah cannot boot composefs". The OstreeInstall red does not prove that the
route has a defect. The cause of each can be the harness or the image. Triage first,
claim after.

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
| KeepSource on ImageSwap (systemd-boot stays) | **UNPROVEN** | route is RED; nothing separable is proven |
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
`prepare-root.conf` enables composefs. It does not tell if the initramfs
has the composefs module (the scan shows that Utah's does not). It also
tells nothing about UKI. The RED/flaky image-swap cell must show if Utah
boots as a composefs deployment. Refer to the backend table, not the scan.

## Package manager (lineage input)

The scanner reads each image's package manager
([scan.rs](../crates/bootc-migrate-core/src/scan.rs) `BaseInfo`). Two images
with the same manager are one family, whatever the value of `ID_LIKE`. A
pair with no evidence (Dakota ships no manager) gives a warning and keeps
the standard merge ([cross_family.rs](../crates/bootc-migrate-core/src/cross_family.rs)).

| Combination | Status | Proof |
|---|---|---|
| Different manager, cross-family policy (`→ opensuse-bootc`) | **RED** (latest); green on `main` | green on main 33891928677, red on PR 36215098705 — flaky, unproven |
| Same-manager shortcut | **UNPROVEN** at E2E level | unit-tested only; no cell isolates it |
| No-manager Unknown path (Dakota as source) | **UNPROVEN** at E2E level | exercised only inside RED/flaky cells (image-swap, OstreeInstall) |

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

Real-host evidence (2026-09-29/30) does not change the tags above. Only
a green `main` run can change them. The route staged with exit 0 on a
developer laptop (Dakota composefs/systemd-boot/btrfs-on-LUKS →
Utah:testing), and the host booted Utah. Two manual steps were necessary,
and issues track both:

- GRUB cannot unlock the LUKS root. The boot used a manual ESP kernel and
  a BLS `ostree=` entry through systemd-boot (#305).
- The carried `/var` needed an ownership/mtime repair (#308, fixed on
  `ux/ostreeinstall-progress`) and a new machine-id (#309).

Overall: **UNPROVEN**. On a real host today, it is an experiment, not
a supported migration. What would promote it, in order:

1. Triage the RED `dakota -> fedora-bootc` OstreeInstall cell — a green
   route with a small target first.
2. Add a `dakota -> utah` OstreeInstall cell (or re-target the
   `fedora-bootc` one once Utah fits the job time budget) and get it green.
3. Decide what to do with the image-swap cell. If Utah-as-composefs stays
   RED, stop the claim for that route for Utah. Do not keep a red
   non-gating cell as decoration.
4. LUKS coverage for the route last — it is the hardest axis to add
   (swtpm + fisherman recipe per `docs/luks-testing.md`).

## Maintenance

- This file follows `main` E2E runs, not PR runs. After a green `main` run,
  update the reference runs and flip whatever it proved.
- A "supported" claim anywhere (README, TUI copy, refusal text) must name
  its cell here. No cell, no claim — write "unknown" and link the gap.
- A flaky cell (green/red across runs) keeps the worse status. It keeps
  it until you fix the flake or the cell stays green.
