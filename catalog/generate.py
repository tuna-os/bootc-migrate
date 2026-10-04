#!/usr/bin/env python3
"""Compose the published TUI catalog from the reviewed source manifest."""
import argparse
import json
import re
import subprocess
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def valid_reference(ref):
    return bool(re.fullmatch(r"ghcr\.io/[A-Za-z0-9._/:-]+", ref)) and len(ref) <= 200


def inspect_published(ref):
    if not ref:
        return False
    try:
        result = subprocess.run(
            ["skopeo", "inspect", "--raw", f"docker://{ref}"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=45, check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def compose(source, probe=False):
    if source.get("version") != 1:
        raise ValueError("unsupported source version")
    core = source["images"]
    if not core or len(core) > 512:
        raise ValueError("invalid image count")
    seen = set()
    for image in core:
        name, ref = image["name"], image["image"]
        if not name or len(name) > 64 or any(ord(c) < 32 for c in name):
            raise ValueError("invalid name")
        if image["backend"] not in ("ostree", "composefs"):
            raise ValueError("invalid backend")
        if ref and not valid_reference(ref):
            raise ValueError("invalid image reference")
        if image["published"] and (not ref or ref in seen):
            raise ValueError("missing or duplicate published image")
        if ref:
            seen.add(ref)

    extras = []
    for family in source.get("families", []):
        name, repo, backend = (family[key] for key in ("name", "repository", "backend"))
        if backend not in ("ostree", "composefs") or not valid_reference(repo + ":latest"):
            raise ValueError("invalid family")
        for desktop in family["desktops"]:
            for suffix in family["suffixes"]:
                tag = desktop + suffix
                ref = repo + ":" + tag
                if ref in seen:
                    continue
                if not valid_reference(ref):
                    raise ValueError("invalid generated reference")
                seen.add(ref)
                extras.append({"name": f"{name} {tag}", "image": ref,
                               "backend": backend, "published": True})
    if len(core) + len(extras) > 512:
        raise ValueError("catalog too large")

    if probe:
        candidates = core + extras
        with ThreadPoolExecutor(max_workers=12) as pool:
            results = list(pool.map(inspect_published, (item["image"] for item in candidates)))
        if not results[0]:
            raise RuntimeError("Dakota stable registry probe failed; refusing to publish an empty feed")
        core = [{**item, "published": bool(item["published"] and result)}
                for item, result in zip(core, results[:len(core)])]
        extras = [item for item, result in zip(extras, results[len(core):]) if result]
    else:
        extras = []  # Offline composition leaves the reviewed snapshot intact.
    return {"version": 1, "images": core + extras}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=ROOT / "images.json")
    parser.add_argument("--probe", action="store_true", help="verify published tags in GHCR")
    args = parser.parse_args()
    catalog = compose(json.loads((ROOT / "sources.json").read_text()), args.probe)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(catalog, indent=2) + "\n")


if __name__ == "__main__":
    main()
