# Migrate to a local image

This page shows how to migrate to a bootc image that you built yourself.
Use it to test an image before you make it public.

## What the tool needs from the target image

A migration reads the target image in two different ways:

- **Podman storage.** Phase 2 runs `podman pull --policy always` on the
  target reference. It then pins that reference to the manifest digest that
  podman selected. The migration stops if podman cannot refresh the image.
  The tool refuses a stale local copy on purpose. The rootfs and the boot
  artifacts must always come from one generation of the image.
- **An HTTP or HTTPS registry endpoint.** The tool uses `curl` to read
  single files from the image through the registry `/v2/` API. The capability
  scan (`bootc-rebase scan`), the cross-base gate, and desktop detection use
  only this path. Phases 4 and 5 use it as a fallback when a file is absent
  from the mounted image.

An image that exists only in podman storage, such as `localhost/my-image`,
satisfies the first requirement but not the second. Therefore, push the
image to a registry that the machine can reach. A registry on your own
network is enough. The registry does not have to be public.

## 1. Build the image

Tag the image with the address of the registry that will serve it. This
example uses a registry on the same machine:

```bash
sudo podman build -t 127.0.0.1:5000/my-dakota:test -f Containerfile .
```

If the migration runs on a different machine than the build, use the LAN
address of the build machine, for example `192.168.1.10:5000/my-dakota:test`.

## 2. Serve the image from a local registry

```bash
sudo podman run -d --name local-registry -p 5000:5000 \
  docker.io/library/registry:2
sudo podman push --tls-verify=false 127.0.0.1:5000/my-dakota:test
```

The registry must stay available for the full migration, and also for later
`bootc upgrade` operations (see [step 6](#6-after-the-reboot)).

The tool selects plain HTTP automatically for `localhost` and for bare IPv4
addresses such as `192.168.1.10:5000`. For any other host name it tries
HTTPS first and then falls back to HTTP.

## 3. Let podman pull over plain HTTP

Phase 2 calls `podman pull` without a `--tls-verify` flag, so podman applies
its own registry policy. On the machine you migrate, mark the local registry
as insecure:

```bash
sudo tee /etc/containers/registries.conf.d/local.conf >/dev/null <<'CONF'
[[registry]]
location = "192.168.1.10:5000"
insecure = true
CONF
```

Replace the location with the address you tagged the image with. Confirm
that the pull works before you migrate:

```bash
sudo podman pull --policy always 192.168.1.10:5000/my-dakota:test
```

## 4. Scan the image

The scan streams a few probe files and reports whether the image can be a
migration target at all:

```bash
sudo bootc-rebase scan 192.168.1.10:5000/my-dakota:test
```

Read the `Compatible: YES/NO` verdict and its reasons. A failure here means
that the registry is unreachable, or that the image is missing something the
migration needs. Fix that first. The cross-base gate also refuses an
`ostree` re-base when it cannot scan the target. An unknown verdict is not
the same as a clean result.

## 5. Dry-run, then migrate

```bash
sudo bootc-migrate --target-image 192.168.1.10:5000/my-dakota:test --dry-run
sudo bootc-migrate --target-image 192.168.1.10:5000/my-dakota:test
```

Everything else is the same as a migration to a published image. Refer to
the walkthrough in the [README](../README.md#usage--end-to-end-walkthrough).

## 6. After the reboot

The deployment `.origin` file records the reference exactly as you typed it
on the command line, as
`ostree-unverified-image:docker://192.168.1.10:5000/my-dakota:test`. It does
not record the digest that Phase 2 pinned. Therefore `bootc upgrade` looks
for that same registry address again later. Two consequences follow:

- Keep the registry available, or the system cannot upgrade.
- Move to a published reference when the test is complete. Run
  `sudo bootc switch <published-image>` from the migrated system.

## Image swap on a machine that already uses composefs

A machine that already boots from composefs does not run the conversion
pipeline. The tool stages the new image with `bootc switch` instead:

```bash
sudo bootc-rebase rebase \
  --source-backend composefs \
  --target-backend composefs \
  --target-image 192.168.1.10:5000/my-dakota:test
```

This route does not scan the target image, so it does not read the image
through the registry API. The target reference goes to `bootc switch`
unchanged. Any reference that your `bootc` accepts is therefore valid here.
This includes a `containers-storage:` reference to an image that only
podman holds, if your `bootc` supports that transport:

```bash
sudo bootc-rebase rebase \
  --source-backend composefs --target-backend composefs \
  --target-image containers-storage:localhost/my-dakota:test
```

Do not add `--de-migrate` to a migration that has no registry access.
Desktop-environment detection reads the target image through the registry,
and it cannot see a local-only image.

## Problems and causes

| Symptom | Cause | Fix |
|---|---|---|
| Phase 2 stops with "podman could not refresh" | Podman cannot reach the registry, or it rejects the TLS configuration | Do [step 3](#3-let-podman-pull-over-plain-http), then pull manually to confirm |
| `scan` or Phase 4 reports "could not reach registry" | The registry is down, or the machine has no route to it | Start the registry again; check the address you tagged the image with |
| The cross-base gate refuses with "Cannot determine whether this is a cross-base re-base" | The scan could not read the target image | Make the registry reachable. Use `--accept-cross-base` only when you already know that the two images share a base |
| `bootc upgrade` fails after a successful migration | The `.origin` file points at a registry that is now gone | Run `sudo bootc switch <published-image>` |
