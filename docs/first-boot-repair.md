# First-boot repair

A migration can damage a system in ways that a check finds only after the
first boot. Some of these breakage classes have a known safe fix. The
first-boot repair applies that fix on the migrated system, one time, before
any user logs in. Issue #309 tracks this work.

## What the migration stages

The `CoreMigration` route (OSTree to composefs) and the `OstreeInstall` route
(composefs to OSTree) write these items into the new deployment:

- An identity manifest at `/var/lib/bootc-migrate/identity-manifest.tsv`.
  It records the owner, group, mode and modification time of each entry
  under the live `/var/home`. The migration writes it before the reboot.
- A copy of the bootc-migrate binary at `/etc/bootc-migrate/bootc-migrate-repair`.
  The target image does not include bootc-migrate.
- The unit `bootc-migrate-repair-firstboot.service` and the marker
  `/etc/bootc-migrate/repair-firstboot` that starts it.
- A desktop autostart entry that shows the result one time for each user.

The other routes do not stage the repair yet.

## When the unit runs

The unit runs after these units, if the migration staged them:

- `bootc-migrate-cross-family-firstboot.service`
- `bootc-migrate-verify-firstboot.service` (the verify probe)

It runs before `systemd-user-sessions.service` and `display-manager.service`.
It runs one time. Its last step removes the marker and the staged binary,
also when a repair fails.

## Classes that the repair fixes

| Class | Check | Fix |
|---|---|---|
| `ownership` | An entry in the identity manifest has a different owner, group or modification time on disk | Set the owner, group and modification time from the manifest. If the change removes setuid or setgid bits, put them back. |
| `machine-id` | `/etc/machine-id` is the same as the source machine-id | Make `/etc/machine-id` empty. The next boot makes a new ID. This class is active only on the `OstreeInstall` route. The `CoreMigration` merge keeps the machine-id on purpose. |
| `flatpak` | The owner of a Flatpak installation, or of its `repo`, `app` or `runtime` directory, is not the correct user. For the system installation, the correct owner is root. | Run `flatpak repair --system`, or `flatpak repair --user` as the user. |
| `selinux-label` | The verify report has a `selinux-label` finding for a path | Run `restorecon -F` on that path only. The command is not recursive. |

The ownership repair reads the manifest. It does not copy files again. It
does not follow symbolic links. If the parent directory of an entry resolves
to a location outside `/var/home`, the repair does not change that entry.

## Findings that the repair only reports

If no class in the table applies to a verify result, the repair does not
change it. The repair log
lists it under `report_only`. Examples are `missing-user` and `home-missing`.

The repair does not delete deployments, ESP files or snapshots.

## Logs

The repair writes these files to `/var/lib/bootc-migrate/`:

| File | Content |
|---|---|
| `repair-log.json` | The status of each class, the actions, and the findings that the repair only reports |
| `repair-result` | One line: `CLEAN`, `DISABLED <reason>`, or `REPAIRED <n> FAILED <n> REPORTED <n>` |
| `repair-verify-before.json` | The verify report before the repair |
| `repair-verify-after.json` | The verify report after the repair |

These files exist only when the verify probe is in the deployment.
The repair runs the probe again after it applies the fixes. The log also lists
the findings that the repair resolved.

The unit output goes to the journal:

```bash
journalctl -b -u bootc-migrate-repair-firstboot.service
```

To see what a repair would change, without a change, run:

```bash
sudo bootc-migrate repair --dry-run
```

## How to disable the repair

Before the reboot, run this command:

```bash
sudo bootc-migrate repair --disable
```

It writes `/etc/bootc-migrate/repair.disabled` into each staged deployment
that has the repair marker. The unit does not start when this file exists.
To start the repair again, run `sudo bootc-migrate repair --enable`.

At boot, add this kernel argument to stop the unit:

```text
bootc_migrate.repair=0
```

To stop one or more classes, use one of these methods:

- Add a kernel argument, for example
  `bootc_migrate.repair.skip=flatpak,selinux-label`.
- Write one class name on each line of `/etc/bootc-migrate/repair.skip`.

The log shows the status `skipped` for each class that you stop.
