"""Unit checks for scripts/release/merge-manifests.py (no network, no build)."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parent / 'release' / 'merge-manifests.py'
spec = importlib.util.spec_from_file_location('merge_manifests', SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def manifest(platform, version='0.2.0', notes='', date='2026-11-25T10:00:00Z'):
    return {'version': version, 'notes': notes, 'pub_date': date, 'platforms': {
        platform: {'signature': f'sig-{platform}', 'url': f'https://example.invalid/{platform}'}}}


class MergeTests(unittest.TestCase):
    def test_merges_platforms_with_latest_date_and_first_notes(self):
        merged = module.merge([manifest('windows-x86_64', notes='Release', date='2026-11-25T10:00:00Z'),
                               manifest('linux-x86_64', date='2026-11-25T11:00:00Z')])
        self.assertEqual(merged['version'], '0.2.0')
        self.assertEqual(merged['notes'], 'Release')
        self.assertEqual(merged['pub_date'], '2026-11-25T11:00:00Z')
        self.assertEqual(list(merged['platforms']), ['linux-x86_64', 'windows-x86_64'])

    def test_rejects_mixed_versions_duplicates_and_incomplete_entries(self):
        with self.assertRaises(ValueError):
            module.merge([manifest('windows-x86_64'), manifest('linux-x86_64', version='0.3.0')])
        with self.assertRaises(ValueError):
            module.merge([manifest('linux-x86_64'), manifest('linux-x86_64')])
        bad = manifest('linux-x86_64')
        bad['platforms']['linux-x86_64']['url'] = 'http://example.invalid/insecure'
        with self.assertRaises(ValueError):
            module.merge([bad])
        empty = manifest('linux-x86_64')
        empty['platforms']['linux-x86_64']['signature'] = ' '
        with self.assertRaises(ValueError):
            module.merge([empty])
        with self.assertRaises(ValueError):
            module.merge([])

    def test_cli_writes_output_and_fails_cleanly(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / 'a.json').write_text(json.dumps(manifest('windows-x86_64')), encoding='utf-8')
            (tmp / 'b.json').write_text(json.dumps(manifest('linux-x86_64')), encoding='utf-8')
            ok = subprocess.run([sys.executable, '-I', str(SCRIPT), '--output', str(tmp / 'latest.json'),
                                 str(tmp / 'a.json'), str(tmp / 'b.json')], capture_output=True, text=True)
            self.assertEqual(ok.returncode, 0, ok.stderr)
            self.assertEqual(len(json.loads((tmp / 'latest.json').read_text())['platforms']), 2)
            bad = subprocess.run([sys.executable, '-I', str(SCRIPT), '--output', str(tmp / 'x.json'),
                                  str(tmp / 'a.json'), str(tmp / 'a.json')], capture_output=True, text=True)
            self.assertNotEqual(bad.returncode, 0)
            self.assertIn('cannot merge manifests', bad.stderr)


if __name__ == '__main__':
    unittest.main()
