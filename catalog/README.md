# TUI image catalog

`catalog/sources.json` is the reviewed list of primary choices, image families,
desktop and hardware tag patterns, and target backends. Edit it when an upstream
image family or tag rule changes. CI probes candidate tags in GHCR and adds
the ones currently published. A primary choice stays visible but becomes
unselectable if its tag disappears. Utah stays listed without a reference until
its first release.

`python3 catalog/generate.py --probe` composes `images.json` using `skopeo`
manifest checks. The `Image catalog` workflow runs on source changes and daily,
then publishes the JSON to the `catalog-feed` branch. The migrator fetches that
branch at startup, caches the last valid copy, and uses the packaged JSON
snapshot when the network is unavailable. The picker supports arrows, Page Up,
Page Down, Home, End, and first-letter jumps for the larger edition list.

The backend field determines the migration route: `composefs` uses
`bootc-migrate`, while `ostree` uses `bootc-rebase`. Check this against the
upstream image's boot configuration before adding an entry. Registry presence
alone does not prove a specific host can boot an image; preflight and the
migration engine still perform their own checks.

Source references:

- [Project Bluefin images](https://docs.projectbluefin.io/images/)
- [TunaOS variants and tags](https://github.com/tuna-os/tunaOS#images-and-variants)
- [Zirconium repository](https://github.com/zirconium-dev/zirconium)
- [Bazzite rebase guide](https://docs.bazzite.gg/Installing_and_Managing_Software/Updates_Rollbacks_and_Rebasing/rebase_guide/)
