#!/usr/bin/env python3
"""Complete a private ELF runtime using the build prefix and builder libraries.

Never execute inspected binaries. Generic graphics loaders are bundled; glibc,
its loader, and hardware-specific driver implementations remain host-owned.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

GLIBC = re.compile(r"(?:ld-linux[^/]*|lib(?:c|m|mvec|dl|pthread|rt|resolv|util|anl|BrokenLocale|thread_db|nss_[^.]+)\.so(?:\..*)?)$")
DRIVER = re.compile(r"(?:lib(?:cuda|nvcuvid|nvidia[^/]*|GLX_nvidia|GLX_mesa|EGL_mesa|vulkan_(?:intel|radeon|nouveau|lvp|virtio))\.so(?:\..*)?|[^/]+_(?:dri|drv_video)\.so)$")


def is_elf(path):
    # Split debug files retain an ELF header but have no loadable closure.
    if path.name.endswith((".debug", ".debuginfo")) or not path.is_file():
        return False
    with path.open("rb") as stream:
        return stream.read(4) == b"\x7fELF"


def needed(path):
    output = subprocess.check_output(["readelf", "--dynamic", "--wide", str(path)], text=True)
    return re.findall(r"\(NEEDED\).*?\[([^]]+)\]", output)


def library_index(roots):
    result = {}
    for root in roots:
        if root.is_dir():
            for path in sorted(root.rglob("*.so*")):
                if path.is_file():
                    result.setdefault(path.name, path)
    return result


def collect(bundle, prefix, libdir, system_roots, required_libraries=()):
    bundle, prefix, libdir = bundle.resolve(), prefix.resolve(), libdir.resolve()
    if not libdir.is_relative_to(bundle):
        raise ValueError("runtime library directory must be inside the bundle")
    libdir.mkdir(parents=True, exist_ok=True)
    prefix_libraries = library_index([prefix])
    system_libraries = library_index(system_roots)
    queue = [path for path in sorted(bundle.rglob("*")) if not path.is_symlink() and is_elf(path)]
    visited, copied, external = set(), {}, {}
    def require(name, requiring):
        if Path(name).name != name or name in (".", ".."):
            raise ValueError(f"non-portable DT_NEEDED {name!r} in {requiring}")
        reason = "host-glibc" if GLIBC.fullmatch(name) else "host-gpu-driver" if DRIVER.fullmatch(name) else None
        if reason:
            external[name] = reason
            return
        destination = libdir / name
        if not destination.is_file():
            source = prefix_libraries.get(name) or system_libraries.get(name)
            if source is None or not is_elf(source):
                raise ValueError(f"unresolved DT_NEEDED {name!r} required by {requiring}")
            # Copy the actual file under its SONAME, without builder symlinks.
            shutil.copy2(source.resolve(), destination)
            copied[name] = {
                "origin": "cerbero" if source.is_relative_to(prefix) else "builder",
                "source": str(source.relative_to(prefix)) if source.is_relative_to(prefix) else str(source),
                "sha256": hashlib.sha256(destination.read_bytes()).hexdigest(),
            }
        queue.append(destination)

    # Some normal application dependencies (e.g. Vulkan) are loaded with dlopen
    # and therefore cannot be discovered by inspecting DT_NEEDED alone.
    for name in required_libraries:
        require(name, "explicit dynamic dependency")
    while queue:
        binary = queue.pop()
        identity = binary.resolve()
        if identity in visited:
            continue
        visited.add(identity)
        for name in needed(binary):
            require(name, binary.relative_to(bundle))
    manifest = {"copied": dict(sorted(copied.items())), "host_requirements": dict(sorted(external.items()))}
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", required=True, type=Path)
    parser.add_argument("--prefix", required=True, type=Path)
    parser.add_argument("--libdir", required=True, type=Path)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--require-library", action="append", default=[])
    args = parser.parse_args()
    # This runs only in the already verified Ubuntu x86_64 builder. Include
    # private directories such as pulseaudio/ for transitive SONAMEs absent
    # from ldconfig's public cache. Only needed or explicitly requested libraries
    # are copied.
    manifest = collect(args.bundle, args.prefix, args.libdir,
                       [Path("/usr/lib/x86_64-linux-gnu"), Path("/lib/x86_64-linux-gnu"), Path("/usr/lib")], args.require_library)
    args.manifest.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Completed ELF closure: {len(manifest['copied'])} libraries copied")


if __name__ == "__main__":
    main()
