"""Merge per-platform updater manifests (each build script's latest.json) into one latest.json.

  python scripts/release/merge-manifests.py --output latest.json windows.json linux.json ...

Every input must name the same version and different platforms; a platform appearing twice,
a missing signature or URL, or mixed versions is an error rather than a silent overwrite.
"""
import argparse
import json
from pathlib import Path
import sys


def merge(manifests):
    if not manifests:
        raise ValueError('no manifests to merge')
    versions = {manifest['version'] for manifest in manifests}
    if len(versions) != 1:
        raise ValueError(f'manifests disagree on version: {sorted(versions)}')
    platforms = {}
    for manifest in manifests:
        for name, entry in manifest['platforms'].items():
            if name in platforms:
                raise ValueError(f'platform {name} appears in more than one manifest')
            if not entry.get('signature', '').strip() or not entry.get('url', '').startswith('https://'):
                raise ValueError(f'platform {name} needs a signature and an https url')
            platforms[name] = {'signature': entry['signature'], 'url': entry['url']}
    notes = next((manifest['notes'] for manifest in manifests if manifest.get('notes')), '')
    return {
        'version': versions.pop(),
        'notes': notes,
        'pub_date': max(manifest['pub_date'] for manifest in manifests),
        'platforms': dict(sorted(platforms.items())),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('manifests', type=Path, nargs='+')
    args = parser.parse_args()
    try:
        merged = merge([json.loads(path.read_text(encoding='utf-8-sig')) for path in args.manifests])
    except (ValueError, KeyError) as error:
        sys.exit(f'cannot merge manifests: {error}')
    args.output.write_text(json.dumps(merged, indent=2) + '\n', encoding='utf-8')
    print(f"{args.output}: {merged['version']} for {', '.join(merged['platforms'])}")


if __name__ == '__main__':
    main()
