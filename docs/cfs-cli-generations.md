# cfs CLI generations and the store compatibility matrix

Upstream bootc replaced its embedded composefs tools. `bootc internals cfs`
is now `cfsctl` from composefs-rs (the `--help` output shows `cfsctl oci …`).
This removed the `oci create-image` and `oci seal` subcommands that phase 3
uses. Image creation moved to `pull --bootable` and `prepare-boot`. The system
will seal images automatically. This broke the pipeline when bluefin:stable shipped it (issue #72).

This document records what each generation can and cannot do, how the
migrator survives the transition, and the empirical evidence behind every
claim. Tests reproduced all results on 2026-07-19 with
`quay.io/fedora/fedora-bootc:42` (legacy) and `quay.io/fedora/fedora-bootc:44`
(new-gen) against a shared store on an ext4 loopback with fs-verity.

## The two generations

| | legacy | new-generation |
|---|---|---|
| marker | `oci --help` lists `create-image` | no `create-image` (has `compute-id`, `prepare-boot`, `fsck`, `varlink`) |
| creation | explicit `oci create-image <config>` | folded into `pull --bootable` / `prepare-boot` |
| sealing | explicit `oci seal <config>` → sealed config splitstream | implicit: config **label** `containers.composefs.fsverity` = EROFS object id |
| `oci mount` identifier | **sealed config digest** (looks up `streams/oci-config-<digest>`) | **tag name or manifest digest** (resolves tag/manifest → config splitstream → EROFS named ref) |
| known ships | bluefin:lts, fedora-bootc:42 | bluefin:stable, dakota:stable, fedora-bootc:44 |

Probe method (`crates/bootc-migrate-core/src/composefs.rs`): run
`… cfs oci --help`, grep for `create-image`. The fail-behavior is
deliberately asymmetric:

- **host probe fails → legacy.** Old hosts without the subcommand must keep
  their unchanged fast path.
- **container-image probe fails → NOT legacy.** A failed `podman run`
  (ENOSPC, network) must not act as a legacy verdict. That error once
  caused a wrong "dakota is legacy" conclusion in CI.

## Store writer selection (the #73 delegation ladder)

The target image's bootc defines the store format when it reads the store at boot.
New-gen bootc reads legacy-format stores (see matrix), so any legacy-CLI bootc
is a valid writer. `BootcCliStore::pull_image` picks:

1. **host bootc**, if the host uses the legacy CLI. This is the fast path
   and reflinks blobs on btrfs;
2. **the target image's bootc** via podman, if it probes legacy (store
   written by its own runtime reader);
3. **a pinned legacy builder** — `quay.io/fedora/fedora-bootc:42`,
   overridable with `BMC_CFS_BUILDER` when the pin ages out;
4. otherwise: report a hard error for #72 and the native backend.

The migrator records `delegate_image` so all three phases use the same writer.
The delegate pulls and runs `create-image`/`seal`.

## Compatibility matrix (empirical)

Store written by the **legacy** CLI, operated on by the **new-gen** CLI:

| new-gen operation | result |
|---|---|
| `oci images` | ✅ listed (legacy pull tags with the FULL ref *including transport*: `docker://quay.io/…`) |
| `oci fsck` | ✅ completely clean |
| boot-time read (`bootc status` / `upgrade --check`) | ✅ proven by the green LTS→dakota E2E cells |
| `oci mount <sealed-config-digest>` | ❌ parsed as a manifest digest → `Opening ref 'streams/oci-manifest-<…>': No such file` |
| `oci mount <tag or manifest digest>` | ❌ `No composefs EROFS image linked — try re-pulling the image` |

The mount failures have one cause. Legacy `create-image` commits the EROFS to
`images/`, but never writes the named ref in the config splitstream (`IMAGE_REF_KEY`)
that new-gen resolution needs.

### The in-place upgrade (and why it is free)

The error message's advice is literal — **a new-gen re-pull over the legacy
store is the upgrade**:

- imported **0 new objects, 0 B stored** (everything deduped);
- rewrote config+manifest splitstreams with the EROFS named ref;
- After the upgrade, mount-by-tag and mount-by-manifest both resolve the EROFS;
- Both resolve to the sha512 object ID from the legacy `create-image`. EROFS
  generation is deterministic. The `.origin` files, BLS entries, and `composefs=`
  karg stay valid;
- The sealed config stream from the legacy CLI remains intact.

Programmatic equivalent: `composefs_oci::upgrade_repo` (composefs-rs ≥0.7),
documented in-crate as the migration path for "repositories created by older
versions of composefs-rs (e.g. bootc ≤ 1.15.x)".

Note for local tests: new-gen `oci mount` uses file-backed EROFS mounts
(kernel ≥ 6.12). On older kernels it fails with `Block device required`
even when resolution succeeds; production bluefin kernels are fine.

## Phase 4/5 implication (open work)

`mount_image()` in `bootc-migrate-core::migration` passes the **sealed
config digest** to the **host** bootc. On a new-gen host, that digest
resolves nothing. The fast path for composefs fails. Phases 4 and 5 (/etc merge
source, boot-artifact extraction) fall back to podman paths. These paths work,
but they are slower and do not deduplicate data.

The evidence shows how to fix this: update `mount_image` for both generations
with the probe. A legacy host keeps the sealed-config identifier. A new-gen host
re-pulls from `containers-storage:` and mounts by tag or manifest digest.

## The native backend (issue #13, PR #74)

`NativeStore` (feature `composefs-native`) writes the store with the
composefs / composefs-oci crates directly — no CLI to drift against, typed
digests instead of scraped stdout. Design notes that came out of the
empirical work:

- the store's fs-verity flavour is **sha512** (`.origin` carries
  `sha512:<hex>`; `composefs=` takes the bare hex);
- `create_image` routes through `upgrade_repo` so the EROFS is built *and
  linked* — a native-written store is mountable by new-gen from birth;
- The writer places the sealed config splitstream under `oci-config-sha256:<digest>`.
  This matches legacy CLI naming, so legacy hosts can mount it;
- selection (probe target generation → `NativeStore` vs `BootcCliStore`)
  is deliberately not wired yet; it lands with the generation-aware
  `mount_image`.

## Reproducing the experiments

```sh
# verity-capable scratch filesystem (root ext4 often lacks -O verity)
truncate -s 4G img && mkfs.ext4 -q -O verity img
sudo mount -o loop img /mnt/x && sudo mkdir /mnt/x/store

# legacy writer
sudo podman run --rm --privileged -v /mnt/x/store:/store quay.io/fedora/fedora-bootc:42 \
  bash -c 'bootc internals cfs --repo /store oci pull docker://quay.io/fedora/fedora-minimal:42
           bootc internals cfs --repo /store oci create-image sha256:<config>
           bootc internals cfs --repo /store oci seal sha256:<config>'

# new-gen reader/upgrader
sudo podman run --rm --privileged -v /mnt/x/store:/store quay.io/fedora/fedora-bootc:44 \
  bash -c 'bootc internals cfs --repo /store oci fsck
           bootc internals cfs --repo /store oci pull docker://quay.io/fedora/fedora-minimal:42
           bootc internals cfs --repo /store oci mount <tag-or-manifest> /tmp/m'
```
