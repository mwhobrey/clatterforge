"""Unit checks for the bundle-PR mapping in scripts/sync-public-repo.py (local Git only)."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parent / 'sync-public-repo.py'
spec = importlib.util.spec_from_file_location('sync_public_repo', SCRIPT)
sync = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sync)


def git(root, *args):
    return subprocess.run(['git', '-C', str(root), '-c', 'user.name=Fixture',
                           '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgsign=false',
                           *args], check=True, capture_output=True, text=True).stdout.strip()


class BundleMappingTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        git(self.root, 'init', '-q')
        self.base = self.commit('base')
        self.pending = {'base': self.base, 'entries': [
            {'dev': 'a' * 40, 'changed': True},
            {'dev': 'b' * 40, 'changed': False},
            {'dev': 'c' * 40, 'changed': True}]}

    def tearDown(self):
        self.tmp.cleanup()

    def commit(self, name):
        (self.root / name).write_text(name, encoding='utf-8')
        git(self.root, 'add', name)
        git(self.root, 'commit', '-qm', name)
        return git(self.root, 'rev-parse', 'HEAD')

    def test_rebase_merge_maps_each_change_to_its_own_commit(self):
        first, second = self.commit('one'), self.commit('two')
        self.assertEqual(sync.bundle_mapping(self.root, self.pending, second), [
            {'dev': 'a' * 40, 'public': first},
            {'dev': 'b' * 40, 'public': None},
            {'dev': 'c' * 40, 'public': second}])

    def test_squash_merge_maps_every_change_to_the_squash_commit(self):
        squash = self.commit('squash')
        mapped = sync.bundle_mapping(self.root, self.pending, squash)
        self.assertEqual([entry['public'] for entry in mapped], [squash, None, squash])

    def test_unexpected_commit_count_is_refused(self):
        for name in ('one', 'two', 'three'):
            last = self.commit(name)
        with self.assertRaises(RuntimeError):
            sync.bundle_mapping(self.root, self.pending, last)


if __name__ == '__main__':
    unittest.main()
