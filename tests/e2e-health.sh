#!/usr/bin/env bash
# Post-reboot health check for a migrated system. Runs INSIDE the VM, piped
# over SSH by tests/run-e2e.sh (assert_system_healthy) after every reboot
# into a migrated deployment.
#
# The per-mode assertions in run-e2e.sh prove that user data survived. This
# script proves that the system that carries it works: boot completed, no
# unit failed, the system bus and logind answer, the display manager runs,
# every account the target image declares exists, and, on an SELinux target,
# nothing is unlabeled or mislabeled. The Dakota -> Utah bugs (#267) were
# both of the kind this catches, and were caught only because they also
# happened to break sshd.
#
# Inputs (environment):
#   E2E_EXPECT_DM             display-manager unit the target must run
#                             (e.g. gdm, sddm). Empty: any, if graphical.
#   E2E_ALLOWED_FAILED_UNITS  space-separated glob patterns of units allowed
#                             to be failed; each must carry a reason where it
#                             is set (the matrix cell or the default below).
#
# Exit status: 0 when every check passes, 1 otherwise. All checks run before
# exiting so one run reports every problem.
set -u

fail=0
bad() { echo "FAIL: $*"; fail=1; }
ok() { echo "OK: $*"; }

# Units that fail in the CI VM for reasons unrelated to any migration.
# Each entry needs a reason.
DEFAULT_ALLOWED_FAILED_UNITS=""

# ---- 1. Boot completed ------------------------------------------------------
state=$(timeout 300 systemctl is-system-running --wait 2>/dev/null)
echo "  system state: ${state:-<none>}"
case "$state" in
    running) ok "boot completed (running)" ;;
    degraded) echo "  boot completed degraded; failed units are checked below" ;;
    *) bad "boot did not complete (systemctl is-system-running: ${state:-<none>})" ;;
esac

# ---- 2. Failed units ---------------------------------------------------------
allowed="$DEFAULT_ALLOWED_FAILED_UNITS ${E2E_ALLOWED_FAILED_UNITS:-}"
failed_units=$(systemctl list-units --state=failed --no-legend --plain 2>/dev/null | awk '{print $1}')
unexpected=0
for unit in $failed_units; do
    permitted=0
    for pattern in $allowed; do
        # shellcheck disable=SC2254 # patterns are globs on purpose
        case "$unit" in $pattern) permitted=1 ;; esac
    done
    if [ "$permitted" = 1 ]; then
        echo "  failed unit on the allowlist: $unit"
    else
        unexpected=1
        bad "unit failed: $unit"
        systemctl status --no-pager --lines 15 "$unit" 2>&1 | sed 's/^/    /'
    fi
done
[ "$unexpected" = 0 ] && ok "no unexpected failed units"

# ---- 3. System bus and logind ----------------------------------------------
if systemctl is-active --quiet dbus.service || systemctl is-active --quiet dbus-broker.service; then
    ok "system D-Bus is active"
else
    bad "system D-Bus (dbus.service / dbus-broker.service) is not active"
fi
busctl list --system >/dev/null 2>&1 || bad "the system bus does not answer (busctl list)"
if loginctl list-sessions >/dev/null 2>&1; then
    ok "logind answers over the bus"
else
    bad "logind does not answer (loginctl list-sessions)"
fi

# ---- 4. Graphical session ----------------------------------------------------
default_target=$(systemctl get-default 2>/dev/null)
echo "  default target: ${default_target:-<none>}"
if [ "$default_target" = graphical.target ]; then
    systemctl is-active --quiet graphical.target \
        && ok "graphical.target reached" \
        || bad "graphical.target was not reached"
    dm=$(systemctl show -p Id --value display-manager.service 2>/dev/null)
    echo "  display manager: ${dm:-<none>}"
    if [ -z "$dm" ] || [ "$dm" = display-manager.service ]; then
        bad "graphical.target is the default but no display manager is enabled"
    elif systemctl is-active --quiet display-manager.service; then
        ok "display manager $dm is active"
    else
        bad "display manager $dm is not active"
        systemctl status --no-pager --lines 15 display-manager.service 2>&1 | sed 's/^/    /'
    fi
    if [ -n "${E2E_EXPECT_DM:-}" ] && [ "$dm" != "${E2E_EXPECT_DM%.service}.service" ]; then
        bad "display manager is $dm, expected ${E2E_EXPECT_DM%.service}.service"
    fi
elif [ -n "${E2E_EXPECT_DM:-}" ]; then
    bad "a display manager (${E2E_EXPECT_DM}) was expected but the default target is ${default_target:-<none>}"
fi

# ---- 5. Accounts the target image declares ----------------------------------
# Every user and group in the image's sysusers.d must resolve. A migration
# that replaces the target's passwd/group with the source's drops these
# (Dakota -> Utah lost `dbus`).
missing=""
for f in /usr/lib/sysusers.d/*.conf; do
    [ -f "$f" ] || continue
    # Only the local /etc override (same basename) replaces a vendor file.
    [ -f "/etc/sysusers.d/$(basename "$f")" ] && f="/etc/sysusers.d/$(basename "$f")"
    while read -r type name _; do
        case "$name" in *%*|"") continue ;; esac
        case "$type" in
            u|u!) getent passwd "$name" >/dev/null || missing="$missing user:$name"
                  getent group "$name" >/dev/null || missing="$missing group:$name" ;;
            g) getent group "$name" >/dev/null || missing="$missing group:$name" ;;
        esac
    done < <(grep -Ev '^\s*(#|$)' "$f")
done
if [ -n "$missing" ]; then
    bad "accounts the target image declares are missing:$missing"
else
    ok "every user and group in the target's sysusers.d resolves"
fi

# ---- 6. SELinux --------------------------------------------------------------
if command -v getenforce >/dev/null 2>&1 && [ "$(getenforce 2>/dev/null)" = Enforcing ]; then
    echo "  SELinux: Enforcing"
    denials=$( { journalctl -b --no-pager -o cat 2>/dev/null
                 ausearch -m AVC,USER_AVC -ts boot 2>/dev/null; } \
               | grep -E 'avc: +denied' | sort -u)
    if [ -n "$denials" ]; then
        echo "  AVC denials this boot:"
        echo "$denials" | head -20 | sed 's/^/    /'
    fi
    # An unlabeled_t target is never the policy's intent: it means a file was
    # written without a label (the Dakota -> Utah class of bug).
    if echo "$denials" | grep -q 'tcontext=[^ ]*:unlabeled_t:'; then
        bad "SELinux denied access to unlabeled files (a migrated tree was not labelled)"
    else
        ok "no SELinux denials against unlabeled files"
    fi
    # Files whose label differs from what the policy assigns. -n: report only.
    mislabeled=$(restorecon -Rnv /etc /var/home 2>/dev/null | grep -c 'Would relabel')
    if [ "${mislabeled:-0}" -gt 0 ]; then
        bad "$mislabeled path(s) under /etc or /var/home are mislabeled for the target's policy"
        restorecon -Rnv /etc /var/home 2>/dev/null | grep 'Would relabel' | head -20 | sed 's/^/    /'
    else
        ok "/etc and /var/home carry the labels the target's policy assigns"
    fi
else
    echo "  SELinux: not enforcing; label checks skipped"
fi

exit "$fail"
