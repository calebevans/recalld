"""Runner boundary regression tests; execute in the isolated Rust container."""
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest

runner = runpy.run_path(str(Path(__file__).with_name("offline-evaluation.py")))


class SourceSnapshotTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "checkout"
        self.root.mkdir()
        self.output = Path(self.temp.name) / "snapshot"
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        for name in (*runner["FIXED_INPUTS"], "src/lib.rs", ".validation/tracked.txt", "notes.txt"):
            self.write(name, "fixture\n")
        self.write("Cargo.toml", '[package]\nname="fixture"\nversion="0.1.0"\nedition="2024"\n')
        self.git("add", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "commit", "--no-gpg-sign", "-qm", "fixture")

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.root, check=True)

    def write(self, name, value):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value)

    def test_only_allowlisted_inputs_and_patch_with_exact_hashes(self):
        for name in (".env", ".validation/tracked.txt", ".validation/secret", ".test-scratch/secret",
                     "target/secret", "notes.txt", "src/untracked.rs", ".cargo/config.toml"):
            self.write(name, "DO_NOT_COPY_CANARY\n")
        self.write("src/lib.rs", "allowed source edit\n")
        hashes = runner["snapshot_source"](self.root, self.output)
        self.assertEqual(set(hashes), set(runner["FIXED_INPUTS"]) | {"src/lib.rs"})
        for name, digest in hashes.items():
            self.assertEqual(digest, runner["sha"](self.output / name))
        patch = runner["source_patch"](self.root, hashes)
        self.assertIn(b"allowed source edit", patch)
        self.assertNotIn(b"DO_NOT_COPY_CANARY", patch)
        self.assertEqual({str(p.relative_to(self.output)) for p in self.output.rglob("*") if p.is_file()}, set(hashes))

    def test_actual_extra_example_modules_and_cli_targets_are_included(self):
        for name in ("examples/performance_probe.rs", "examples/support/mod.rs", "tests/text_migration_cli.rs"):
            self.write(name, "// tracked canonical consumer\n")
        self.git("add", "examples", "tests")
        hashes = runner["snapshot_source"](self.root, self.output)
        self.assertIn("examples/support/mod.rs", hashes)
        self.assertIn("examples/performance_probe.rs", hashes)
        self.assertIn("tests/text_migration_cli.rs", hashes)

    def test_unknown_target_asset_or_explicit_cargo_target_fails_closed(self):
        self.write("tests/fixture.secret", "must not silently omit or copy\n")
        self.git("add", "tests")
        with self.assertRaises(RuntimeError):
            runner["snapshot_source"](self.root, self.output)
        self.git("rm", "-f", "tests/fixture.secret")
        self.write("Cargo.toml", '[package]\nname="fixture"\nversion="0.1.0"\n[[test]]\nname="custom"\npath="other/check.rs"\n')
        with self.assertRaises(RuntimeError):
            runner["snapshot_source"](self.root, self.output)

    def test_missing_required_input_fails_closed(self):
        (self.root / "Cargo.lock").unlink()
        with self.assertRaises(RuntimeError):
            runner["snapshot_source"](self.root, self.output)

    def test_symlink_input_fails_closed(self):
        path = self.root / "src/lib.rs"
        path.unlink()
        path.symlink_to(self.root / "notes.txt")
        with self.assertRaises(RuntimeError):
            runner["snapshot_source"](self.root, self.output)

    def test_symlink_parent_even_inside_checkout_fails_closed(self):
        (self.root / "src").rename(self.root / "elsewhere")
        (self.root / "src").symlink_to(self.root / "elsewhere", target_is_directory=True)
        with self.assertRaises(RuntimeError):
            runner["snapshot_source"](self.root, self.output)


if __name__ == "__main__":
    unittest.main()
