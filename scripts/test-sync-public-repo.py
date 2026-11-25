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
        self.pending = {'base': self.base, 'title': 'Sync 2 changes', 'entries': [
            {'dev': 'a' * 40, 'changed': True, 'subject': 'one'},
            {'dev': 'b' * 40, 'changed': False, 'subject': None},
            {'dev': 'c' * 40, 'changed': True, 'subject': 'two'}]}

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

    def test_unrelated_commit_on_main_before_the_merge_is_not_mapped(self):
        self.commit('unrelated')
        first, second = self.commit('one'), self.commit('two')
        mapped = sync.bundle_mapping(self.root, self.pending, second)
        self.assertEqual([entry['public'] for entry in mapped], [first, None, second])

    def test_squash_merge_maps_every_change_to_the_squash_commit(self):
        self.commit('unrelated')
        squash = self.commit('Sync 2 changes (#46)')
        mapped = sync.bundle_mapping(self.root, self.pending, squash)
        self.assertEqual([entry['public'] for entry in mapped], [squash, None, squash])

    def test_unrecognized_merge_is_refused(self):
        for name in ('two', 'one'):
            last = self.commit(name)
        with self.assertRaises(RuntimeError):
            sync.bundle_mapping(self.root, self.pending, last)


class IdentityExemptionTests(unittest.TestCase):
    def test_only_complete_identity_fields_are_exempt(self):
        with tempfile.TemporaryDirectory() as tmp:
            root, policy = Path(tmp) / 'repo', Path(tmp) / 'policy.json'
            policy.write_text('{"literal_terms": ["secretname"], '
                              '"public_identities": ["Secretname Public"]}', encoding='utf-8')
            git(root.parent, 'init', '-q', str(root))
            (root / 'a.txt').write_text('a', encoding='utf-8')
            git(root, 'add', 'a.txt')
            subprocess.run(['git', '-C', str(root), '-c', 'user.name=Secretname Public',
                            '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgsign=false',
                            'commit', '-qm', 'clean'], check=True)
            sync.verify_commit(root, root, policy)
            (root / 'b.txt').write_text('b', encoding='utf-8')
            git(root, 'add', 'b.txt')
            git(root, 'commit', '-qm', 'thanks Secretname Public')
            with self.assertRaises(RuntimeError):
                sync.verify_commit(root, root, policy)


if __name__ == '__main__':
    unittest.main()
