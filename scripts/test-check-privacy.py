"""Unit checks for the pattern-based privacy gate."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    'check_privacy', Path(__file__).with_name('check-privacy.py'))
check_privacy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_privacy)


def kinds(text):
    return sorted(kind for kind, _ in check_privacy.findings_in_text(text))


class PrivacyPatternTests(unittest.TestCase):
    def test_real_identifiers_are_flagged(self):
        # Samples are assembled at runtime so this file itself never contains a match for
        # this gate, gitleaks or any other scanner run over the repository.
        users = 'Us' + 'ers'
        self.assertEqual(kinds('checkout C:\\' + users + '\\alice\\dev\\repo'), ['home path'])
        self.assertEqual(kinds('path C:/' + users + '/alice/dev'), ['home path'])
        self.assertEqual(kinds('see /ho' + 'me/alice/.config and /' + users + '/alice/x'),
                         ['home path', 'home path'])
        self.assertEqual(kinds('mail alice.smith@' + 'gmail.com'), ['email'])
        self.assertEqual(kinds('host laptop.tail1234.ts' + '.net:8788'), ['tailnet host'])
        self.assertEqual(kinds('https://quiet-river-1.trycloud' + 'flare.com'), ['tunnel host'])
        self.assertEqual(kinds('-----BEGIN OPENSSH ' + 'PRIVATE KEY-----'), ['private key'])

    def test_reserved_fixtures_and_lookalikes_pass(self):
        for text in [
            'user.email=fixture@example.invalid',
            'contact test@example.com or git@git.example',
            '49699333+dependabot[bot]@users.noreply.github.com',
            'icons/128x128@2x.png',
            'C:/fixtures/sample-app and C:\\Users\\USERNAME\\AppData',
            'devbox.example-tailnet.ts.net:8788',
            'cd ~/project && ls /usr/local/bin',
            'scope /home/<user>/repo',
        ]:
            self.assertEqual(kinds(text), [], text)


class PublicCommitEmailTests(unittest.TestCase):
    def run_gate(self, root, allowed):
        env = {**os.environ, 'PRIVACY_PUBLIC_COMMIT_EMAILS': allowed}
        return subprocess.run([sys.executable, '-I', str(Path(__file__).with_name('check-privacy.py')),
                               '--history', '--root', str(root)], capture_output=True, text=True, env=env)

    def test_allowance_covers_exact_commit_metadata_only(self):
        email = 'alice.smith@' + 'gmail.com'
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            git = ['git', '-C', str(root), '-c', 'user.name=Alice', '-c', f'user.email={email}']
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            (root / 'a.txt').write_text('nothing personal', encoding='utf-8')
            subprocess.run([*git, 'add', 'a.txt'], check=True)
            subprocess.run([*git, 'commit', '-qm', 'one'], check=True)
            self.assertNotEqual(self.run_gate(root, '').returncode, 0)
            self.assertNotEqual(self.run_gate(root, 'other@' + 'gmail.com').returncode, 0)
            self.assertEqual(self.run_gate(root, ' ALICE.Smith@' + 'gmail.com ').returncode, 0)
            (root / 'a.txt').write_text('mail ' + email, encoding='utf-8')
            subprocess.run([*git, 'commit', '-qam', 'two'], check=True)
            failed = self.run_gate(root, email)
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn('a.txt: email', failed.stderr)


if __name__ == '__main__':
    unittest.main()
