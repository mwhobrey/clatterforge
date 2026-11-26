"""Unit checks for scripts/check-doc-claims.py."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('check-doc-claims.py')
spec = importlib.util.spec_from_file_location('check_doc_claims', SCRIPT)
claims = importlib.util.module_from_spec(spec)
spec.loader.exec_module(claims)


class RetiredClaimTests(unittest.TestCase):
    def test_claim_wrapped_across_lines_is_found_at_its_first_line(self):
        text = 'Intro.\n\nPublic release is\n  being\tprepared for now.\n'
        found = list(claims.findings('README.md', text))
        self.assertEqual(len(found), 1, found)
        self.assertTrue(found[0].startswith('README.md:3: "Public release is being prepared"'), found)

    def test_case_and_repeats(self):
        text = 'SIGNED SELF-UPDATING INSTALLERS\nthen signed self-updating installers again\n'
        self.assertEqual([f.split(':')[1] for f in claims.findings('x.md', text)], ['1', '2'])

    def test_current_wording_passes(self):
        text = 'Updates are signed; only macOS builds are code-signed. The .deb has no update channel yet.\n'
        self.assertEqual(list(claims.findings('x.md', text)), [])

    def test_cli_exit_codes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'README.md').write_text('All current.\n', encoding='utf-8')
            ok = subprocess.run([sys.executable, '-I', str(SCRIPT), '--root', str(root)],
                                capture_output=True, text=True)
            self.assertEqual(ok.returncode, 0, ok.stderr)
            (root / 'SECURITY.md').write_text('This has no configured\npublic repository.\n', encoding='utf-8')
            bad = subprocess.run([sys.executable, '-I', str(SCRIPT), '--root', str(root)],
                                 capture_output=True, text=True)
            self.assertEqual(bad.returncode, 1)
            self.assertIn('SECURITY.md:1:', bad.stderr)


if __name__ == '__main__':
    unittest.main()
