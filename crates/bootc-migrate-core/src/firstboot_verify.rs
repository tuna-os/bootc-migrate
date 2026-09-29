//! First-boot verify probe (L1): prove the migrated system works, on the
//! system itself.
//!
//! Every staging route installs a small shell probe plus a oneshot unit into
//! the new deployment. On first boot the probe checks the breakage classes a
//! migration can introduce — home-directory ownership, the target's declared
//! system users, a duplicated machine-id — writes a JSON report plus a
//! one-line summary to `/var/lib/bootc-migrate/`, and always exits 0:
//! findings are data for [`crate::rebase_plan`] status and the desktop
//! cleanup prompt (L2), never boot failures.
//!
//! Rendering is pure ([`render_verify_unit`], [`VERIFY_SCRIPT`]); installing
//! is I/O ([`install_verify_probe`]). The script itself is exercised by the
//! E2E health check (section 8) on every migrated boot.

use anyhow::{Context, Result};
use std::path::Path;

/// The one-shot unit's name in the staged `/etc/systemd/system`.
pub const VERIFY_UNIT: &str = "bootc-migrate-verify-firstboot.service";
/// The marker (relative to `/etc`) whose presence arms the unit. Its content
/// is the source machine-id, so the probe can tell a duplicated id from a
/// regenerated one. The unit removes it as its last step: exactly one run.
pub const VERIFY_MARKER: &str = "bootc-migrate/verify-firstboot";
/// The probe script (relative to `/etc`), run by the unit.
pub const VERIFY_SCRIPT_REL: &str = "bootc-migrate/verify-firstboot.sh";
/// One-line summary the E2E health check and `bootc-migrate status` read:
/// `OK`, or `FINDINGS <n>`.
pub const VERIFY_RESULT: &str = "/var/lib/bootc-migrate/verify-result";
/// Full JSON report, one object per finding.
pub const VERIFY_REPORT: &str = "/var/lib/bootc-migrate/verify-report.json";

/// The probe. `set -u`, no bashisms, only coreutils/findutils/grep/awk: it
/// runs before sysinit on whatever the target ships.
pub const VERIFY_SCRIPT: &str = r#"#!/bin/sh
# bootc-migrate first-boot verify probe (L1). Staged into every migrated
# deployment; runs once via bootc-migrate-verify-firstboot.service. Checks
# the breakage classes a migration can introduce, writes a JSON report plus
# a one-line summary, always exits 0 — findings are data, not boot failures.
set -u
OUT_DIR=/var/lib/bootc-migrate
REPORT=$OUT_DIR/verify-report.json
RESULT=$OUT_DIR/verify-result
MARKER=/etc/bootc-migrate/verify-firstboot
FINDINGS_TMP=$(mktemp)
: > "$FINDINGS_TMP"
# note appends "class<TAB>detail" and echoes it to the journal. The count
# is read back from the file at the end: notes also fire from pipeline
# subshells, where a shell variable would not propagate.
note() {
    printf '%s\t%s\n' "$1" "$2" >> "$FINDINGS_TMP"
    echo "verify: [$1] $2"
}
json_escape() {
    sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/\\t/g' | tr -d '\n\r'
}
mkdir -p "$OUT_DIR"

# 1. Home-directory ownership: every human user's top-level home entries
# must belong to them. A /var copy that drops identity (#308) lands them
# root-owned and the desktop breaks.
awk -F: '$3 >= 1000 && $3 < 65534 && $7 !~ /(nologin|false)/ { print $1":"$3":"$6 }' /etc/passwd |
while IFS=: read -r user uid home; do
    [ -d "$home" ] || { note home-missing "$user has no home dir $home"; continue; }
    owner=$(stat -c '%u' "$home")
    [ "$owner" = "$uid" ] || note home-owner "$home owned by uid $owner, expected $uid ($user)"
    # A wholesale identity loss (#308) leaves nearly every top-level entry
    # foreign; a couple of privileged tool dirs (lima, docker) are normal
    # on a healthy desktop, so only five or more is a finding.
    foreign=$(find "$home" -mindepth 1 -maxdepth 1 ! -user "$uid" 2>/dev/null)
    n_foreign=$(printf '%s\n' "$foreign" | sed '/^$/d' | wc -l | tr -d ' ')
    if [ "$n_foreign" -ge 5 ]; then
        bad=$(printf '%s\n' "$foreign" | head -3 | tr '\n' ' ')
        note home-entries "$home has $n_foreign top-level entries not owned by $user (e.g. $bad)"
    fi
done

# 2. The target's declared system users resolve. An image swap from another
# distribution can replace the target's passwd with the source's; Utah lost
# its dbus user that way and D-Bus never started.
for f in /usr/lib/sysusers.d/*.conf; do
    [ -f "$f" ] || continue
    grep -Ev '^\s*(#|$)' "$f" | while read -r type name _; do
        case "$type" in
            u|u!) case "$name" in *%*|"") continue ;; esac
                getent passwd "$name" >/dev/null || note missing-user "$name (from $f)" ;;
        esac
    done
done

# 3. machine-id must not still be the source's: a duplicated id shadows the
# new system's journal and confuses anything keyed on it. The marker holds
# the source id the migration recorded.
if [ -f "$MARKER" ]; then
    src_id=$(cat "$MARKER")
    live_id=$(cat /etc/machine-id 2>/dev/null || true)
    if [ -n "$src_id" ] && [ "$src_id" = "$live_id" ]; then
        note machine-id "live machine-id $live_id still equals the source's"
    fi
fi

{
    printf '{\n  "generated": "%s",\n  "findings": [\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    first=1
    tab=$(printf '\t')
    while IFS="$tab" read -r class detail; do
        [ "$first" = 1 ] || printf ',\n'
        first=0
        printf '    {"class": "%s", "detail": "%s"}' "$class" "$(printf '%s' "$detail" | json_escape)"
    done < "$FINDINGS_TMP"
    printf '\n  ]\n}\n'
} > "$REPORT"
n=$(wc -l < "$FINDINGS_TMP" | tr -d ' ')
if [ "$n" = 0 ]; then echo OK > "$RESULT"; else echo "FINDINGS $n" > "$RESULT"; fi
rm -f "$FINDINGS_TMP"
echo "verify: $n finding(s); report at $REPORT"
exit 0
"#;

/// Render the oneshot unit. It runs after the cross-family first-boot unit
/// when one is staged (remap and relabel first, verify after: an `After=`
/// on an absent unit is ignored) and before sysinit, like the unit it
/// follows.
pub fn render_verify_unit() -> String {
    format!(
        "[Unit]\n\
         Description=bootc-migrate first-boot verify probe\n\
         ConditionPathExists=/etc/{VERIFY_MARKER}\n\
         DefaultDependencies=no\n\
         After=local-fs.target {cross}\n\
         Before=sysinit.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=no\n\
         ExecStart=/bin/sh /etc/{VERIFY_SCRIPT_REL}\n\
         ExecStart=/usr/bin/rm -f /etc/{VERIFY_MARKER}\n\
         \n\
         [Install]\n\
         WantedBy=sysinit.target\n",
        cross = crate::cross_family::FIRSTBOOT_UNIT,
    )
}

/// Stage the probe into `etc_dir`: the script (mode 755), the enabled unit,
/// and the marker holding the live source machine-id (empty when the host
/// has none, in which case the probe skips the id check).
pub fn install_verify_probe(etc_dir: &Path) -> Result<()> {
    let script_path = etc_dir.join(VERIFY_SCRIPT_REL);
    if let Some(parent) = script_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&script_path, VERIFY_SCRIPT)
        .with_context(|| format!("failed to write {}", script_path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
    let source_id = std::fs::read_to_string("/etc/machine-id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    crate::cross_family::install_firstboot_unit_as(
        etc_dir,
        VERIFY_UNIT,
        VERIFY_MARKER,
        format!("{source_id}\n").as_bytes(),
        &render_verify_unit(),
    )
    .context("failed to install the verify first-boot unit")?;
    Ok(())
}

// ---- Desktop cleanup prompt (L2) -------------------------------------------
// Autostarted once per desktop user after a migration. Offers `commit`
// when one is pending, surfaces first-boot verify findings otherwise.
// Silent unless there is something to say; stamps after showing so it
// never nags twice.

/// The autostart entry (relative to `/etc`).
pub const PROMPT_DESKTOP_REL: &str = "xdg/autostart/bootc-migrate-cleanup.desktop";
/// The prompt script (relative to `/etc`).
pub const PROMPT_SCRIPT_REL: &str = "bootc-migrate/cleanup-prompt.sh";

/// The autostart entry. `NoDisplay`: it is a one-shot prompt, not an app.
pub const PROMPT_DESKTOP: &str = "[Desktop Entry]\n\
     Type=Application\n\
     Name=Complete system migration\n\
     Comment=Offer to commit a finished bootc migration, or surface its first-boot findings\n\
     Exec=/bin/sh /etc/bootc-migrate/cleanup-prompt.sh\n\
     NoDisplay=true\n\
     X-GNOME-Autostart-enabled=true\n";

/// The prompt script. `BOOTC_MIGRATE_STATE_DIR` overrides the state dir so
/// tests run it against fixtures; production leaves it unset.
pub const PROMPT_SCRIPT: &str = r#"#!/bin/sh
# bootc-migrate desktop cleanup prompt (L2). Autostarted once per desktop
# user after a migration. Silent unless there is something to say; stamps
# after showing (even on "Later") so it never nags twice.
set -u
STATE_DIR=${BOOTC_MIGRATE_STATE_DIR:-/var/lib/bootc-migrate}
STAMP_DIR=${XDG_STATE_HOME:-$HOME/.local/state}/bootc-migrate
STAMP=$STAMP_DIR/cleanup-prompted
[ -f "$STAMP" ] && exit 0

result=$(cat "$STATE_DIR/verify-result" 2>/dev/null || echo MISSING)
commit_pending=0
# Same three conditions as `status`: our own report, booted the staged
# composefs deployment, legacy content still on disk.
if [ -f "$STATE_DIR/report.json" ] && grep -q 'composefs=' /proc/cmdline 2>/dev/null; then
    # Any non-dot entry besides the target's own bootc/ storage.
    for e in /sysroot/ostree/*; do
        [ -e "$e" ] || continue
        case "$e" in */bootc) continue ;; esac
        commit_pending=1; break
    done
fi
if [ "$commit_pending" = 0 ] && { [ "$result" = OK ] || [ "$result" = MISSING ]; }; then
    exit 0
fi

# Headless, or no dialog tool: say it once for the journal and leave the
# stamp alone — the next graphical login still gets the prompt.
if [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
    echo "bootc-migrate: migration follow-up pending (verify: $result, commit: $commit_pending); run 'bootc-migrate status'." >&2
    exit 0
fi
DIALOG=""
if command -v zenity >/dev/null 2>&1; then DIALOG=zenity
elif command -v kdialog >/dev/null 2>&1; then DIALOG=kdialog
fi
[ -n "$DIALOG" ] || {
    echo "bootc-migrate: migration follow-up pending but no dialog tool found; run 'bootc-migrate status'." >&2
    exit 0
}

BIN=""
for c in bootc-migrate /usr/local/bin/bootc-migrate /var/tmp/bootc-migrate; do
    if command -v "$c" >/dev/null 2>&1; then BIN=$c; break; fi
done

stamp() { mkdir -p "$STAMP_DIR"; touch "$STAMP"; }

if [ "$commit_pending" = 1 ] && [ -n "$BIN" ]; then
    TEXT="The migration finished and the old system is still on disk as a fallback. Commit the new system as permanent? This deletes the old deployment; it cannot be undone."
    if [ "$DIALOG" = zenity ]; then
        zenity --question --title="Complete system migration" --width=450 \
            --text="$TEXT" --ok-label="Commit" --cancel-label="Later"
        ans=$?
    else
        kdialog --title "Complete system migration" --yesno "$TEXT" --yes-label "Commit" --no-label "Later"
        ans=$?
    fi
    [ "$ans" = 0 ] && pkexec "$BIN" commit
    stamp
    exit 0
fi

# Findings, or a pending commit with no binary to run it: inform, with the
# manual command for the commit case.
if [ "$commit_pending" = 1 ]; then
    TEXT="A migration commit is pending (the old system is still on disk), but bootc-migrate was not found to run it. Run 'bootc-migrate commit' as root when ready."
else
    TEXT="The post-migration check reported: $result. See /var/lib/bootc-migrate/verify-report.json, or run 'bootc-migrate status'."
fi
if [ "$DIALOG" = zenity ]; then
    zenity --info --title="Migration follow-up" --width=450 --text="$TEXT"
else
    kdialog --title "Migration follow-up" --msgbox "$TEXT"
fi
stamp
exit 0
"#;

/// Stage the prompt into `etc_dir`: the script (mode 755) and the autostart
/// entry. Runs at every staging route alongside the L1 probe.
pub fn install_cleanup_prompt(etc_dir: &Path) -> Result<()> {
    let script_path = etc_dir.join(PROMPT_SCRIPT_REL);
    if let Some(parent) = script_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&script_path, PROMPT_SCRIPT)
        .with_context(|| format!("failed to write {}", script_path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
    let desktop_path = etc_dir.join(PROMPT_DESKTOP_REL);
    if let Some(parent) = desktop_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&desktop_path, PROMPT_DESKTOP)
        .with_context(|| format!("failed to write {}", desktop_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn verify_unit_orders_after_remap_and_before_sysinit() {
        let unit = render_verify_unit();
        assert!(unit.contains(&format!("ConditionPathExists=/etc/{VERIFY_MARKER}")));
        assert!(
            unit.contains(&format!(
                "After=local-fs.target {}",
                crate::cross_family::FIRSTBOOT_UNIT
            )),
            "verify must run after the remap/relabel unit when one is staged:\n{unit}"
        );
        assert!(unit.contains("Before=sysinit.target"));
        assert!(unit.contains(&format!("ExecStart=/bin/sh /etc/{VERIFY_SCRIPT_REL}")));
        assert!(unit.contains(&format!("ExecStart=/usr/bin/rm -f /etc/{VERIFY_MARKER}")));
        assert!(unit.contains("WantedBy=sysinit.target"));
    }

    #[test]
    fn install_writes_script_unit_marker_and_enablement() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let etc = dir.path().join("etc");

        install_verify_probe(&etc).unwrap();

        let script = etc.join(VERIFY_SCRIPT_REL);
        assert_eq!(std::fs::read_to_string(&script).unwrap(), VERIFY_SCRIPT);
        assert_eq!(
            std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let unit_path = etc.join("systemd/system").join(VERIFY_UNIT);
        assert_eq!(
            std::fs::read_to_string(&unit_path).unwrap(),
            render_verify_unit()
        );
        let link = etc
            .join("systemd/system/sysinit.target.wants")
            .join(VERIFY_UNIT);
        assert_eq!(
            std::fs::read_link(&link).unwrap().to_string_lossy(),
            format!("../{VERIFY_UNIT}")
        );
        let expected_id = std::fs::read_to_string("/etc/machine-id")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        assert_eq!(
            std::fs::read_to_string(etc.join(VERIFY_MARKER)).unwrap(),
            format!("{expected_id}\n")
        );
    }

    #[test]
    fn probe_script_shape() {
        // Static contract the E2E health check (section 8) relies on: the
        // probe writes both files, never fails the boot, and covers the
        // three breakage classes.
        for needle in [
            "verify-report.json",
            "verify-result",
            "exit 0",
            "home-entries",
            "missing-user",
            "machine-id",
        ] {
            assert!(VERIFY_SCRIPT.contains(needle), "script lost {needle}");
        }
        assert!(VERIFY_SCRIPT.starts_with("#!/bin/sh\n"));
    }

    #[test]
    fn prompt_installs_script_and_autostart_entry() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let etc = dir.path().join("etc");

        install_cleanup_prompt(&etc).unwrap();

        let script = etc.join(PROMPT_SCRIPT_REL);
        assert_eq!(std::fs::read_to_string(&script).unwrap(), PROMPT_SCRIPT);
        assert_eq!(
            std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let desktop = std::fs::read_to_string(etc.join(PROMPT_DESKTOP_REL)).unwrap();
        assert_eq!(desktop, PROMPT_DESKTOP);
        assert!(desktop.contains("Exec=/bin/sh /etc/bootc-migrate/cleanup-prompt.sh"));
        assert!(desktop.contains("NoDisplay=true"));
    }

    #[test]
    fn prompt_script_shape() {
        // Static contract: silent unless something is pending, never nags
        // twice, degrades without a display or dialog tool.
        for needle in [
            "cleanup-prompted",
            "BOOTC_MIGRATE_STATE_DIR",
            "WAYLAND_DISPLAY",
            "zenity",
            "kdialog",
            "pkexec",
        ] {
            assert!(PROMPT_SCRIPT.contains(needle), "script lost {needle}");
        }
        assert!(PROMPT_SCRIPT.starts_with("#!/bin/sh\n"));
    }
}
