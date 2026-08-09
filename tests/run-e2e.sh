RETURN_CMDLINE=$(ssh $SSH_OPTS root@localhost "cat /proc/cmdline")
if ! echo "$RETURN_CMDLINE" | grep -q 'composefs='; then
    echo "FAIL: did not return to composefs (cmdline: $RETURN_CMDLINE)"; exit 1
fi
echo "OK: Returned to composefs cleanly via restored BootOrder."

# --- commit subcommand cleanup test (#25) ---
# Verify the post-commit on-disk layout is byte-shape identical to a fresh
# bootc install of the target image: /sysroot/ostree removed, OSTree BLS
# entries dropped, GRUB2 bits gone (since we migrated to systemd-boot),
# .bootc-aleph.json gone.
step "=== Running commit cleanup test ==="

# Dry-run first — no changes, but the report must list the paths we expect
# to reclaim.
DRYRUN_OUT=$(ssh $SSH_OPTS root@localhost "/var/tmp/bootc-migrate commit --dry-run 2>&1" || true)
echo "$DRYRUN_OUT" | sed 's/^/  /'
for needle in '/sysroot/ostree' '/sysroot/.bootc-aleph.json' 'Would reclaim'; do
    if ! echo "$DRYRUN_OUT" | grep -qF "$needle"; then
        echo "FAIL: commit --dry-run did not mention '$needle'"; exit 1
    fi
done
echo "OK: commit --dry-run lists expected cleanup targets."

# Confirm those paths are still there before the real commit.
PRE_OSTREE=$(ssh $SSH_OPTS root@localhost "test -d /sysroot/ostree && echo yes || echo no")
if [ "$PRE_OSTREE" != "yes" ]; then
    echo "FAIL: /sysroot/ostree absent before commit — dry-run should have been a no-op"; exit 1
fi

# Real commit.
COMMIT_OUT=$(ssh $SSH_OPTS root@localhost "/var/tmp/bootc-migrate commit 2>&1" || {
    echo "FAIL: commit subcommand exited non-zero"; exit 1
})
echo "$COMMIT_OUT" | sed 's/^/  /'
if ! echo "$COMMIT_OUT" | grep -q "Reclaimed:"; then
    echo "FAIL: commit didn't print a Reclaimed summary"; exit 1
fi
echo "OK: commit subcommand ran without error."

# Post-conditions: source-deployment OSTree data is gone.  A fresh bootc
# install retains /sysroot/ostree/bootc for its own container storage, so the
# parent directory itself must not be treated as legacy state.
POST_OSTREE=$(ssh $SSH_OPTS root@localhost "test -d /sysroot/ostree/bootc && ! test -e /sysroot/ostree/repo && ! test -e /sysroot/ostree/deploy && echo clean || echo dirty")
if [ "$POST_OSTREE" != "clean" ]; then
    echo "FAIL: legacy OSTree data remains after commit"; exit 1
fi
echo "OK: legacy OSTree data removed; bootc runtime storage retained."

POST_ALEPH=$(ssh $SSH_OPTS root@localhost "test -e /sysroot/.bootc-aleph.json && echo present || echo absent")
if [ "$POST_ALEPH" != "absent" ]; then
