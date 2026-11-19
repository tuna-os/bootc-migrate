#!/usr/bin/env bash
# e2e-known-flake.sh <e2e-run.log>
#
# Exit 0 only when the run failed for one known reason: the post-migration
# health check found geoclue.service failed with "Result: timeout", and no
# other assertion failed. That is the geoclue lock-order bug in the target
# image (tuna-os/bootc-migrate#368). The e2e workflow retries such a cell once.
# Every other failure exits 1, and the workflow fails the cell at once.
#
# The match is strict on purpose. Any other FAIL:, ERROR: or panic line, a
# second failed unit, or a geoclue failure for a reason other than a timeout
# means the log does not match.
set -euo pipefail

log=${1:?usage: e2e-known-flake.sh <e2e-run.log>}
[ -r "$log" ] || exit 1

# The harness colours its output; compare plain text.
plain=$(sed 's/\x1b\[[0-9;]*[a-zA-Z]//g; s/\x1b[()][0-9A-Za-z]//g' "$log")

geoclue_fail='[health] FAIL: unit failed: geoclue.service'
unhealthy='FAIL: the migrated system is not healthy (see the [health] lines above)'

# 1. The failing lines are exactly the geoclue unit and the health verdict.
fails=$(grep -aE 'FAIL:' <<<"$plain" | sed 's/[[:space:]]*$//' | sort -u)
[ "$fails" = "$(printf '%s\n%s\n' "$geoclue_fail" "$unhealthy" | sort -u)" ] || exit 1

# 2. The health verdict is the last failing line: the run stopped there.
[ "$(grep -aE 'FAIL:' <<<"$plain" | tail -1 | sed 's/[[:space:]]*$//')" = "$unhealthy" ] || exit 1

# 3. No error or panic anywhere else in the run.
if grep -aqE 'ERROR:|panicked|Migration Failed' <<<"$plain"; then
    exit 1
fi

# 4. geoclue failed because systemd timed it out: the first "Active:" line
#    of its status block, after its last FAIL line, says so.
active=$(awk -v f="$geoclue_fail" '
    index($0, f) == 1 { seen = 1; line = ""; next }
    seen && line == "" && /^\[health\][[:space:]]+Active:/ { line = $0 }
    END { print line }' <<<"$plain")
case "$active" in
    *"Active: failed (Result: timeout)"*) exit 0 ;;
    *) exit 1 ;;
esac
