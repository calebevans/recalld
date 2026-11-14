#!/usr/bin/env python3
"""Source boundary checks; no Cargo compilation, providers or personal checkout writes."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('public_conformance', Path(__file__).with_name('public-conformance.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


def git(root, *args):
    return subprocess.check_output(['git', '-c', 'user.name=Fixture', '-c',
        'user.email=fixture@example.invalid', *args], cwd=root, stderr=subprocess.STDOUT)


def write(root, name, content):
    p = root / name
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content)


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.runtime = self.base / 'runtime'
        self.harness = self.base / 'harness'
        for root in [self.runtime, self.harness]:
            root.mkdir()
            git(root, 'init', '-q')
        self.original_root = runner.ROOT
        runner.ROOT = self.harness
        self.addCleanup(setattr, runner, 'ROOT', self.original_root)
        for name in runner.HARNESS_INPUTS:
            write(self.harness, name, 'harness overlay\n')
        git(self.harness, 'add', '.')
        git(self.harness, 'commit', '-qm', 'harness fixture')
        write(self.runtime, 'Cargo.toml', '[package]\nname="fixture"\nversion="0.1.0"\n')
        write(self.runtime, 'Cargo.lock', '# fixture lock\n')
        write(self.runtime, 'rust-toolchain.toml', '[toolchain]\nchannel="1.94.1"\n')
        write(self.runtime, 'src/lib.rs', 'pub fn fixture() {}\n')
        self.commit()

    def commit(self):
        git(self.runtime, 'add', '.')
        git(self.runtime, 'commit', '-qm', 'runtime fixture')
        return git(self.runtime, 'rev-parse', 'HEAD').decode().strip()

    def snapshot(self, **kwargs):
        return runner.snapshot(self.runtime, self.base / 'snapshot', **kwargs)

    def test_explicit_targets_assets_and_separate_overlay(self):
        p = self.runtime / 'Cargo.toml'
        p.write_text(p.read_text() + '\n[[example]]\nname="custom"\npath="tools/custom/main.rs"\n')
        write(self.runtime, 'tools/custom/main.rs', 'mod helper; fn main() {}')
        write(self.runtime, 'tools/custom/helper.rs', 'const GOLD: &str = include_str!("golden.json");')
        write(self.runtime, 'tools/custom/golden.json', '{"expected":1}')
        write(self.runtime, 'tests/check.rs', '#[test] fn works() {}')
        write(self.runtime, 'tests/data/result.bin', 'synthetic bytes')
        (self.runtime / 'tests/data/result.bin').chmod(0o755)
        write(self.runtime, 'examples/automatic.rs', 'fn main() {}')
        write(self.runtime, 'build.rs', 'fn main() {}')
        write(self.runtime, 'scripts/public-conformance.py', 'wrong runtime harness')
        self.commit()
        m, _ = self.snapshot(extra=['tools/custom/golden.json', 'tests/data/result.bin'])
        self.assertEqual((self.base / 'snapshot/tests/data/result.bin').stat().st_mode & 0o777, 0o755)
        self.assertEqual(m['sourceFileModes']['tests/data/result.bin'], '0o755')
        names = m['sourceFilesSha256']
        for name in ['tools/custom/main.rs', 'tools/custom/helper.rs', 'tools/custom/golden.json',
                     'tests/check.rs', 'tests/data/result.bin', 'examples/automatic.rs', 'build.rs']:
            self.assertIn(name, names)
            self.assertEqual(m['sourceFileOrigins'][name], 'runtime')
        self.assertEqual((self.base / 'snapshot/scripts/public-conformance.py').read_text(), 'harness overlay\n')
        for name, digest in names.items():
            self.assertEqual(runner.sha(self.base / 'snapshot' / name), digest)

    def test_excludes_unknown_untracked_and_unrelated_tracked_files(self):
        for name in ['notes.json', '.env', '.cargo/config.toml']:
            write(self.runtime, name, 'not an input')
        self.commit()
        for name in ['src/untracked.rs', '.validation/secret', '.test-scratch/cache', 'target/file']:
            write(self.runtime, name, 'not an input')
        m, _ = self.snapshot()
        self.assertEqual(set(m['sourceFilesSha256']), set(runner.CARGO_INPUTS + runner.HARNESS_INPUTS + ['src/lib.rs']))

    def test_missing_declared_target_and_unlisted_asset_fail(self):
        p = self.runtime / 'Cargo.toml'
        p.write_text(p.read_text() + '\n[[test]]\nname="missing"\npath="checks/missing.rs"\n')
        with self.assertRaisesRegex(RuntimeError, 'missing/nonregular'):
            self.snapshot()
        write(self.runtime, 'checks/missing.rs', 'fn main() {}')
        write(self.runtime, 'checks/expected.json', '{}')
        self.commit()
        with self.assertRaisesRegex(RuntimeError, 'undeclared target asset'):
            self.snapshot()

    def test_literal_include_requires_tracked_explicit_asset(self):
        write(self.runtime, 'src/lib.rs', 'const X: &str = include_str!("../assets/value.txt");')
        write(self.runtime, 'assets/value.txt', 'synthetic')
        self.commit()
        with self.assertRaisesRegex(RuntimeError, 'undeclared include'):
            self.snapshot()
        self.snapshot(extra=['assets/value.txt'])

    def test_missing_untracked_and_symlink_inputs_fail(self):
        with self.assertRaisesRegex(RuntimeError, 'not tracked'):
            self.snapshot(extra=['missing.txt'])
        (self.runtime / 'src/lib.rs').unlink()
        (self.runtime / 'src/lib.rs').symlink_to(self.harness / runner.HARNESS_INPUTS[0])
        with self.assertRaisesRegex(RuntimeError, 'missing/nonregular'):
            self.snapshot()

    def test_frozen_commit_rejects_dirty_or_wrong_head(self):
        commit = git(self.runtime, 'rev-parse', 'HEAD').decode().strip()
        write(self.runtime, 'src/lib.rs', 'pub fn changed() {}')
        with self.assertRaisesRegex(RuntimeError, 'tracked modifications'):
            self.snapshot(expected_commit=commit)
        self.commit()
        with self.assertRaisesRegex(RuntimeError, 'differs from'):
            self.snapshot(expected_commit=commit)
        self.snapshot(expected_commit='HEAD')

    def test_workspace_path_dependencies_and_escaping_paths_fail(self):
        with self.assertRaisesRegex(RuntimeError, 'not tracked'):
            self.snapshot(extra=['../outside'])
        write(self.runtime, 'Cargo.toml', '[workspace]\nmembers=[]\n')
        with self.assertRaisesRegex(RuntimeError, 'workspace'):
            self.snapshot()
        write(self.runtime, 'Cargo.toml', '[package]\nname="x"\nversion="0.1.0"\n[patch.crates-io]\nx={path="../outside"}\n')
        with self.assertRaisesRegex(RuntimeError, 'path dependencies'):
            self.snapshot()


if __name__ == '__main__':
    unittest.main()
