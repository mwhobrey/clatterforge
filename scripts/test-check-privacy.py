"""Unit checks for the pattern-based privacy gate."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    'check_privacy', Path(__file__).with_name('check-privacy.py'))
check_privacy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_privacy)


def kinds(text):
    return sorted(kind for kind, _ in check_privacy.findings_in_text(text))


class PrivacyPatternTests(unittest.TestCase):
    def test_real_identifiers_are_flagged(self):
        self.assertEqual(kinds(r'checkout C:\Users\alice\dev\repo'), ['home path'])
        self.assertEqual(kinds('path C:/Users/alice/dev'), ['home path'])
        self.assertEqual(kinds('see /home/alice/.config and /Users/alice/x'),
                         ['home path', 'home path'])
        self.assertEqual(kinds('mail alice.smith@gmail.com'), ['email'])
        self.assertEqual(kinds('host laptop.tail1234.ts.net:8788'), ['tailnet host'])
        self.assertEqual(kinds('https://quiet-river-1.trycloudflare.com'), ['tunnel host'])
        self.assertEqual(kinds('-----BEGIN OPENSSH PRIVATE KEY-----'), ['private key'])

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


if __name__ == '__main__':
    unittest.main()
