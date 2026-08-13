# Minimal OCI image distributing the bootc-migrate binary.
#
# This tool needs to run directly on the host (root, /sysroot, EFI vars) — it
# is not meant to run *inside* this container. The image exists so the binary
# can be pulled without hitting GitHub Release rate limits/auth, or referenced
# from another Containerfile:
#
#   podman create --name bmc-extract ghcr.io/tuna-os/bootc-migrate-composefs:latest
#   podman cp bmc-extract:/usr/local/bin/bootc-migrate-composefs .
#   podman rm bmc-extract
#
#   # or, in another image's build:
#   COPY --from=ghcr.io/tuna-os/bootc-migrate-composefs:latest \
#       /usr/local/bin/bootc-migrate-composefs /usr/local/bin/bootc-migrate
#
# Release-naming note: the repo was renamed from `bootc-migrate-composefs` to
# `bootc-migrate` after the v0.2.0 release, and the published image and binary
# still carry the old name until the next (post-rename) release.
#
# Built by CI (.github/workflows/release.yml) from the per-arch release
# binaries — see ctx/linux/$TARGETARCH/ staged there before this build.
FROM gcr.io/distroless/cc-debian12
ARG TARGETARCH
COPY linux/${TARGETARCH}/bootc-migrate /usr/local/bin/bootc-migrate
ENTRYPOINT ["/usr/local/bin/bootc-migrate"]
