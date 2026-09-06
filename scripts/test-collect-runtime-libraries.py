#!/usr/bin/env python3
"""Exercise closure collection with real ELF files, without running them."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("collector", Path(__file__).with_name("collect-runtime-libraries.py"))
collector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collector)


class ClosureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bundle = self.root / "bundle"
        self.libdir = self.bundle / "lib"
        self.prefix = self.root / "prefix"
        self.system = self.root / "system"
        for directory in (self.libdir, self.prefix, self.system):
            directory.mkdir(parents=True)

    def library(self, directory, name, code, dependencies=()):
        source = directory / (name + ".c")
        source.write_text(code)
        target = directory / name
        subprocess.run(["cc", "-shared", "-fPIC", str(source), "-Wl,-soname," + name,
                        *map(str, dependencies), "-o", str(target)], check=True)
        return target

    def test_transitive_prefix_and_builder_libraries_are_copied(self):
        leaf = self.library(self.system, "libleaf.so.1", "int leaf(void) { return 7; }")
        versioned_leaf = leaf.with_name("libleaf.so.1.2")
        leaf.rename(versioned_leaf)
        leaf.symlink_to(versioned_leaf.name)
        middle = self.library(self.prefix, "libmiddle.so.1", "int leaf(void); int middle(void) { return leaf(); }", [leaf])
        self.library(self.bundle, "plugin.so", "int middle(void); int plugin(void) { return middle(); }", [middle])
        result = collector.collect(self.bundle, self.prefix, self.libdir, [self.system])
        self.assertEqual(set(result["copied"]), {"libmiddle.so.1", "libleaf.so.1"})
        self.assertEqual((self.libdir / leaf.name).read_bytes(), leaf.read_bytes())
        self.assertFalse((self.libdir / leaf.name).is_symlink())
        self.assertEqual(result["copied"][middle.name]["origin"], "cerbero")
        self.assertEqual(result["copied"][leaf.name]["origin"], "builder")
        # Once assembled, closure checking requires no builder dependencies.
        collector.collect(self.bundle, self.root / "absent", self.libdir, [])

    def test_unresolved_transitive_dependency_fails(self):
        leaf = self.library(self.prefix, "libmissing.so.1", "int missing(void) { return 7; }")
        self.library(self.bundle, "plugin.so", "int missing(void); int plugin(void) { return missing(); }", [leaf])
        leaf.unlink()
        with self.assertRaisesRegex(ValueError, "unresolved DT_NEEDED.*libmissing"):
            collector.collect(self.bundle, self.prefix, self.libdir, [])

    def test_explicit_dlopen_library_includes_its_dependencies(self):
        leaf = self.library(self.system, "libleaf.so.1", "int leaf(void) { return 1; }")
        self.library(self.system, "libvulkan.so.1", "int leaf(void); int loader(void) { return leaf(); }", [leaf])
        result = collector.collect(self.bundle, self.prefix, self.libdir,
                                   [self.system], ["libvulkan.so.1"])
        self.assertEqual(set(result["copied"]), {"libvulkan.so.1", "libleaf.so.1"})

    def test_host_boundaries_do_not_exclude_generic_graphics_loaders(self):
        for name in ("libc.so.6", "libpthread.so.0", "ld-linux-x86-64.so.2"):
            self.assertTrue(collector.GLIBC.fullmatch(name))
        for name in ("libcuda.so.1", "libGLX_mesa.so.0", "libvulkan_intel.so", "iris_dri.so"):
            self.assertTrue(collector.DRIVER.fullmatch(name))
        for name in ("libdrm.so.2", "libGL.so.1", "libEGL.so.1", "libvulkan.so.1", "libva.so.2"):
            self.assertFalse(collector.GLIBC.fullmatch(name) or collector.DRIVER.fullmatch(name))


if __name__ == "__main__":
    unittest.main()
