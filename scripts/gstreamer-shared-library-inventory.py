#!/usr/bin/env python3
"""Emit a fail-closed canonical inventory for bundled shared libraries."""

import json
import os
from pathlib import Path
import re
import stat
import sys


SHARED_LIBRARY = re.compile(r"^(lib[A-Za-z0-9_.+-]+\.so)(?:\.[0-9]+){0,3}$")


def fail(message: str) -> None:
    raise SystemExit(f"shared-library-inventory: {message}")


def canonical_name(name: str) -> str | None:
    match = SHARED_LIBRARY.fullmatch(name)
    return match.group(1) if match else None


def scan_directory(root: Path, relative_dir: str, prefix: str) -> list[dict[str, str]]:
    relative_path = Path(relative_dir)
    if relative_path.is_absolute() or any(part in {"", ".", ".."} for part in relative_path.parts):
        fail(f"library directory is not a safe relative path: {relative_dir}")
    directory = root / relative_path
    try:
        directory_stat = directory.lstat()
        directory.resolve(strict=True).relative_to(root)
    except (OSError, ValueError):
        fail(f"library directory is missing: {relative_dir}")
    if stat.S_ISLNK(directory_stat.st_mode) or not stat.S_ISDIR(directory_stat.st_mode):
        fail(f"library directory is not a real directory: {relative_dir}")

    entries: list[dict[str, str]] = []
    regular_names: dict[str, str] = {}
    try:
        children = sorted(os.scandir(directory), key=lambda entry: entry.name)
    except OSError:
        fail(f"cannot enumerate library directory: {relative_dir}")
    for child in children:
        canonical = canonical_name(child.name)
        if canonical is None:
            continue
        try:
            mode = child.stat(follow_symlinks=False).st_mode
        except OSError:
            fail(f"cannot inspect shared-library entry: {prefix}{child.name}")
        path = f"{prefix}{child.name}"
        if stat.S_ISREG(mode):
            if canonical in regular_names:
                fail(f"multiple regular files have canonical name: {prefix}{canonical}")
            regular_names[canonical] = child.name
            entries.append({"path": path, "canonical_path": f"{prefix}{canonical}", "kind": "file"})
            continue
        if not stat.S_ISLNK(mode):
            fail(f"unsupported shared-library entry type: {path}")
        try:
            link_target = os.readlink(child.path)
        except OSError:
            fail(f"cannot read shared-library symlink: {path}")
        if (
            not link_target
            or os.path.isabs(link_target)
            or link_target != os.path.basename(link_target)
            or link_target in {".", ".."}
        ):
            fail(f"shared-library symlink target is not a same-directory basename: {path}")
        try:
            resolved = (directory / link_target).resolve(strict=True)
            resolved.relative_to(directory.resolve(strict=True))
            resolved_mode = resolved.stat().st_mode
        except (OSError, ValueError):
            fail(f"shared-library symlink is dangling or escapes its directory: {path}")
        if not stat.S_ISREG(resolved_mode):
            fail(f"shared-library symlink target is not regular: {path}")
        if canonical_name(resolved.name) != canonical:
            fail(f"shared-library symlink changes canonical name: {path}")
        entries.append(
            {
                "path": path,
                "canonical_path": f"{prefix}{canonical}",
                "kind": "symlink",
                "link_target": link_target,
            }
        )
    return entries


def main() -> None:
    if len(sys.argv) != 4:
        fail("usage: ROOT PUBLIC_LIBDIR PRIVATE_LIBDIR")
    root = Path(sys.argv[1])
    try:
        root = root.resolve(strict=True)
    except OSError:
        fail("runtime root is missing")
    public_dir, private_dir = sys.argv[2:]
    entries = scan_directory(root, public_dir, "")
    entries.extend(scan_directory(root, private_dir, "pulseaudio/"))
    canonical_paths = sorted({entry["canonical_path"] for entry in entries})
    print(json.dumps({"canonical_paths": canonical_paths, "entries": entries}, sort_keys=True))


if __name__ == "__main__":
    main()
