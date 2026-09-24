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
#   E2E_ALLOWED_MISLABELED    space-separated glob patterns of paths whose
#                             new mislabel is reported, not failed. Only for
#                             an image whose own policy is inconsistent; the
#                             matrix cell carries the reason.
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
        # Server images (fedora-bootc) default to graphical.target and ship
        # no display manager. Only a cell that expects one fails here; a
        # desktop that lost its display manager in the /etc merge is caught
        # by E2E_EXPECT_DM.
        if [ -n "${E2E_EXPECT_DM:-}" ]; then
            bad "no display manager is enabled, expected ${E2E_EXPECT_DM%.service}.service"
        else
            echo "  no display manager is enabled (none expected)"
        fi
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
    while read -r type name id _; do
        case "$name" in *%*|"") continue ;; esac
        case "$type" in
            u|u!)
                getent passwd "$name" >/dev/null || missing="$missing user:$name"
                # sysusers.d(5): "uid:gid" or "-:group" names the primary
                # group, and then no group of the user's name is created
                # (`u sync 5:0` uses root's group). Otherwise it is.
                case "$id" in
                    *:*) primary="${id#*:}"
                         getent group "$primary" >/dev/null || missing="$missing group:$primary" ;;
                    *) getent group "$name" >/dev/null || missing="$missing group:$name" ;;
                esac ;;
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
    # Files whose label differs from what the policy assigns. -n: report
    # only. Paths the base already had mislabeled before the migration
    # (recorded by capture_health_baseline) are reported, not failed.
    baseline=/var/lib/e2e-health/label-baseline
    mislabeled=$(restorecon -Rnv /etc /var/home 2>/dev/null \
        | sed -n 's/^Would relabel \([^ ]*\) from.*/\1/p' | sort -u)
    if [ -s "$baseline" ]; then
        new_mislabeled=$(comm -23 <(printf '%s\n' "$mislabeled" | sed '/^$/d') "$baseline")
        echo "  $(comm -12 <(printf '%s\n' "$mislabeled" | sed '/^$/d') "$baseline" | wc -l) mislabeled path(s) were already mislabeled on the base (not counted)"
    else
        new_mislabeled=$(printf '%s\n' "$mislabeled" | sed '/^$/d')
    fi
    if [ -n "$new_mislabeled" ] && [ -n "${E2E_ALLOWED_MISLABELED:-}" ]; then
        kept=""
        allowed_count=0
        while IFS= read -r path; do
            permitted=0
            for pattern in $E2E_ALLOWED_MISLABELED; do
                # shellcheck disable=SC2254 # patterns are globs on purpose
                case "$path" in $pattern) permitted=1 ;; esac
            done
            if [ "$permitted" = 1 ]; then
                allowed_count=$((allowed_count + 1))
            else
                kept="$kept$path"$'\n'
            fi
        done <<< "$new_mislabeled"
        echo "  $allowed_count new mislabeled path(s) match E2E_ALLOWED_MISLABELED (not counted)"
        new_mislabeled=$(printf '%s' "$kept" | sed '/^$/d')
    fi
    if [ -n "$new_mislabeled" ]; then
        bad "$(echo "$new_mislabeled" | wc -l) path(s) under /etc or /var/home are mislabeled for the target's policy and were not on the base"
        restorecon -Rnv /etc /var/home 2>/dev/null | grep -F -f <(echo "$new_mislabeled" | head -20 | sed 's/$/ from/') | head -20 | sed 's/^/    /'
        # A home directory the policy maps to default_t means the policy's
        # /var/home -> /home equivalence (file_contexts.subs*) or its home
        # contexts (file_contexts.homedirs) did not survive the /etc merge.
        if echo "$new_mislabeled" | grep -q '^/var/home/'; then
            fc=/etc/selinux/targeted/contexts/files
            echo "  policy home-directory mapping:"
            matchpathcon /var/home /home 2>&1 | sed 's/^/    /'
            for f in "$fc"/file_contexts.subs "$fc"/file_contexts.subs_dist; do
                [ -f "$f" ] && echo "    $f:" && grep -E 'home' "$f" | sed 's/^/      /'
            done
            ls -l "$fc"/file_contexts.homedirs* 2>&1 | sed 's/^/    /'
            grep -c . "$fc"/file_contexts.homedirs 2>&1 | sed 's/^/    homedirs lines: /'
            [ -d /usr/etc/selinux ] && diff -rq /usr/etc/selinux /etc/selinux 2>&1 | head -20 | sed 's/^/    vendor diff: /'
        fi
    else
        ok "the migration left no path under /etc or /var/home mislabeled"
    fi
else
    echo "  SELinux: not enforcing; label checks skipped"
fi

exit "$fail"
