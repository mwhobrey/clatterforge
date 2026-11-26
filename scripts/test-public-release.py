"""Exercise privacy and independent-history boundaries using disposable repositories."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


EXPORTER = Path(__file__).with_name('prepare-public-release.py')


def git(root, *args):
    return subprocess.check_output(['git', '-C', str(root), *args], stderr=subprocess.PIPE)


class PublicReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / 'private-development'
        self.root.mkdir()
        self.write('scripts/prepare-public-release.py', EXPORTER.read_text())
        git(self.root, 'init', '--template=', '-b', 'private-history')
        git(self.root, 'config', 'core.autocrlf', 'false')
        self.write('private-notes.md', 'private-host-fixture and private-account@example.invalid')
        self.write('AGENTS.md', 'private-host-fixture instructions')
        self.commit('Private development history')
        for name in ('README.md', 'AGENTS.md', 'CONTRIBUTING.md', 'SECURITY.md',
                     'docs/runbook/00_INDEX.md'):
            self.write('docs/public/tree/' + name, 'Public documentation\n')
        for name in ('LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md'):
            self.write(name, 'Public license material\n')
        for name in ('Cargo.toml', 'edge/Cargo.toml', 'protocol/Cargo.toml',
                     'gui/Cargo.toml', 'gui/src-tauri/Cargo.toml'):
            self.write(name, '[package]\nname="fixture"\nlicense="Apache-2.0"\n')
        self.write('src/lib.rs', '// public code\n')
        self.write('.github/workflows/ci.yml', 'name: CI\n')
        self.write('.github/dependabot.yml', 'version: 2\n')
        self.commit('Reviewed source')
        self.write('untracked-secret.txt', 'private-host-fixture')
        self.write('.conductor/state.sqlite', 'private-host-fixture')
        self.write('.clatterforge/state.sqlite', 'private-host-fixture')
        self.policy = self.base / 'private-policy.json'
        self.policy.write_text(json.dumps({'literal_terms': ['private-host-fixture'],
                                          'private_email': 'private-account@example.invalid'}))
        self.output = self.base / 'public'

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8')

    def commit(self, message):
        git(self.root, 'add', '--all')
        git(self.root, '-c', 'user.name=Private fixture',
            '-c', 'user.email=private-account@example.invalid', '-c', 'core.hooksPath=',
            '-c', 'commit.gpgsign=false', 'commit', '-m', message)

    def export(self):
        environment = os.environ.copy()
        environment['GIT_AUTHOR_EMAIL'] = 'private-account@example.invalid'
        environment['GIT_COMMITTER_EMAIL'] = 'private-account@example.invalid'
        return subprocess.run([sys.executable, str(self.root / 'scripts/prepare-public-release.py'),
                               '--output', str(self.output), '--privacy-policy', str(self.policy),
                               '--initialize-git'], capture_output=True, text=True, env=environment)

    def test_private_history_and_local_state_are_not_reachable_from_public_git(self):
        before = git(self.root, 'rev-parse', 'HEAD')
        result = self.export()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(git(self.output, 'rev-list', '--all', '--count').strip(), b'1')
        self.assertEqual(git(self.output, 'remote'), b'')
        self.assertEqual(git(self.output, 'for-each-ref', '--format=%(refname)').strip(), b'refs/heads/main')
        self.assertEqual(git(self.root, 'rev-parse', 'HEAD'), before)
        objects = git(self.output, 'rev-list', '--objects', '--all').splitlines()
        for entry in objects:
            oid = entry.split(b' ', 1)[0].decode()
            data = git(self.output, 'cat-file', '-p', oid)
            self.assertNotIn(b'private-host-fixture', data)
            self.assertNotIn(b'private-account@example.invalid', data)
        self.assertFalse((self.output / 'private-notes.md').exists())
        self.assertFalse((self.output / 'untracked-secret.txt').exists())
        self.assertFalse((self.output / '.conductor').exists())
        self.assertFalse((self.output / '.clatterforge').exists())
        self.assertTrue((self.output / '.github/workflows/ci.yml').exists())
        self.assertFalse((self.output / '.github/dependabot.yml').exists())
        self.assertEqual((self.output / 'AGENTS.md').read_text(), 'Public documentation\n')
        second = self.export()
        self.assertNotEqual(second.returncode, 0)
        self.assertIn('Output must not exist', second.stderr)

    def test_private_content_fails_before_creating_output(self):
        self.write('src/lib.rs', '// private-host-fixture\n')
        git(self.root, 'add', 'src/lib.rs')
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('privacy check failed', result.stderr)
        self.assertFalse(self.output.exists())

    def test_unstaged_content_cannot_silently_change_reviewed_index_snapshot(self):
        self.write('src/lib.rs', '// private-host-fixture\n')
        result = self.export()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.output / 'src/lib.rs').read_text(), '// public code\n')

    def test_index_symlink_is_refused_even_without_os_symlink_support(self):
        self.write('gui/linked.txt', '../../private-notes.md')
        oid = git(self.root, 'hash-object', '-w', 'gui/linked.txt').decode().strip()
        git(self.root, 'update-index', '--add', '--cacheinfo', '120000,' + oid + ',gui/linked.txt')
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Only resolved regular source files', result.stderr)
        self.assertFalse(self.output.exists())

    def test_tracked_credential_file_is_refused(self):
        self.write('gui/.env', 'SYNTHETIC_CREDENTIAL=placeholder\n')
        git(self.root, 'add', 'gui/.env')
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('privacy check failed', result.stderr)
        self.assertFalse(self.output.exists())

    def test_private_metadata_in_binary_file_is_refused(self):
        path = self.root / 'gui/fixture.png'
        path.write_bytes(b'\x89PNG\xff\x00private-host-fixture')
        git(self.root, 'add', 'gui/fixture.png')
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('privacy check failed', result.stderr)
        self.assertFalse(self.output.exists())

    def test_colliding_curated_path_is_refused(self):
        self.write('docs/public/tree/src/lib.rs', '// duplicate source\n')
        git(self.root, 'add', 'docs/public/tree/src/lib.rs')
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Duplicate public path', result.stderr)
        self.assertFalse(self.output.exists())


if __name__ == '__main__':
    unittest.main()
