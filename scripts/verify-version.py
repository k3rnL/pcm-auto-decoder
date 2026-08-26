#!/usr/bin/env python3
"""Verify every PCM Auto Decoder version surface available before release."""

from __future__ import annotations

import argparse
import pathlib
import subprocess
import sys
import tomllib


PROJECT = "pcm-auto-decoder"


def fail(message: str) -> None:
    print(f"version verification failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--tag")
    parser.add_argument("--release-title")
    parser.add_argument("--archive-name")
    parser.add_argument("--target")
    args = parser.parse_args()

    repository = pathlib.Path(__file__).resolve().parents[1]
    with (repository / "Cargo.toml").open("rb") as handle:
        manifest = tomllib.load(handle)
    with (repository / "Cargo.lock").open("rb") as handle:
        lock = tomllib.load(handle)

    package = manifest.get("package", {})
    if package.get("name") != PROJECT:
        fail(f"Cargo package name is {package.get('name')!r}, expected {PROJECT!r}")
    version = package.get("version")
    if not isinstance(version, str) or not version:
        fail("Cargo.toml has no package version")

    lock_versions = {
        entry.get("version")
        for entry in lock.get("package", [])
        if entry.get("name") == PROJECT
    }
    if lock_versions != {version}:
        fail(f"Cargo.lock versions are {sorted(lock_versions)!r}, expected only {version!r}")

    output = subprocess.run(
        [str(args.binary), "--version"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()
    expected_output = f"{PROJECT} {version}"
    if output != expected_output:
        fail(f"binary reports {output!r}, expected {expected_output!r}")

    expected_tag = f"v{version}"
    if args.tag and args.tag != expected_tag:
        fail(f"tag is {args.tag!r}, expected {expected_tag!r}")
    if args.release_title and args.release_title != f"PCM Auto Decoder {expected_tag}":
        fail(
            f"release title is {args.release_title!r}, "
            f"expected {'PCM Auto Decoder ' + expected_tag!r}"
        )
    if args.archive_name:
        if not args.target:
            fail("--archive-name requires --target")
        expected_archive = (
            f"{PROJECT}-{expected_tag}-debian-trixie-{args.target}.tar.gz"
        )
        if args.archive_name != expected_archive:
            fail(f"archive is {args.archive_name!r}, expected {expected_archive!r}")

    print(f"verified {PROJECT} {version}")


if __name__ == "__main__":
    main()
