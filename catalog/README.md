# Published catalogs

`catalog/sources.json` is the reviewed list of primary choices, image families,
desktop and hardware tag patterns, and target backends. Edit it when an upstream
image family or tag rule changes. CI probes candidate tags in GHCR and adds
the ones currently published. A primary choice stays visible but becomes
unselectable if its tag disappears. Utah stays listed without a reference until
its first release.

`python3 catalog/generate.py --probe` composes `images.json` using `skopeo`
manifest checks. The `Published catalogs` workflow runs on source changes and
daily, then publishes both JSON files to the `catalog-feed` branch:

- `images.json`: <https://raw.githubusercontent.com/tuna-os/bootc-migrate/catalog-feed/images.json>
- `apps.json`: <https://raw.githubusercontent.com/tuna-os/bootc-migrate/catalog-feed/apps.json>

The migrator fetches the image feed at startup, caches the last valid copy, and
uses the packaged JSON snapshot when the network is unavailable. The picker
supports arrows, Page Up, Page Down, Home, End, and first-letter jumps for the
larger edition list.

The backend field determines the migration route: `composefs` uses
`bootc-migrate`, while `ostree` uses `bootc-rebase`. Check this against the
upstream image's boot configuration before adding an entry. Registry presence
alone does not prove a specific host can boot an image; preflight and the
migration engine still perform their own checks.

## Application replacement catalog

`catalog/apps.json` is the shared application-to-Flatpak mapping for the
package-host software report and wootc. Each record has a stable app `id`, a
Flatpak ID, a portability classification, a user-facing note, and aliases under
`match` for Windows display names and rpm, deb, and pacman package names.

Package aliases are exact and unique within their package ecosystem. Windows
display-name aliases are lowercase substrings; consumers match them
case-insensitively and choose the longest match. `portability` is `portable`
when app state can move as files, `signin` when account sign-in restores the
useful state, and `none` when the Flatpak is a replacement rather than a state
migration target.

The schema is versioned. Consumers must reject versions they do not implement.
The Rust reader in `bootc_migrate_core::app_mapping` validates the bundled file
and provides deterministic lookup for all four source inventories.

Source references:

- [Project Bluefin images](https://docs.projectbluefin.io/images/)
- [TunaOS variants and tags](https://github.com/tuna-os/tunaOS#choose-your-image)
- [Zirconium repository](https://github.com/zirconium-dev/zirconium)
- [Bazzite rebase guide](https://docs.bazzite.gg/Installing_and_Managing_Software/Updates_Rollbacks_and_Rebasing/rebase_guide/)
