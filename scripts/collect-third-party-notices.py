"""Collect declared dependency license texts without copying registry paths or user data."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import re
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor


UPSTREAM_CACHE = {}


def upstream_texts(package, package_root):
    repository = urllib.parse.urlparse(package.get('repository') or '')
    parts = repository.path.strip('/').split('/')[:2]
    vcs_file = package_root / '.cargo_vcs_info.json'
    if (repository.scheme != 'https' or repository.hostname != 'github.com'
            or repository.username or repository.password or len(parts) != 2
            or not all(re.fullmatch(r'[A-Za-z0-9_.-]+', part) and part not in ('.', '..') for part in parts)
            or not vcs_file.is_file()):
        return []
    revision = json.loads(vcs_file.read_text())['git']['sha1']
    if not re.fullmatch(r'[0-9a-f]{40}', revision):
        raise RuntimeError('Invalid published VCS revision')
    repo = '/'.join(parts).removesuffix('.git')
    key = (repo.casefold(), revision)
    if key not in UPSTREAM_CACHE:
        def fetch(name):
            url = f'https://raw.githubusercontent.com/{repo}/{revision}/{name}'
            try:
                with urllib.request.urlopen(url, timeout=20) as response:
                    data = response.read(1024 * 1024 + 1)
            except urllib.error.HTTPError as error:
                if error.code == 404:
                    return None
                raise
            if len(data) > 1024 * 1024:
                raise RuntimeError('Upstream license exceeds 1 MiB')
            return data, url
        with urllib.request.urlopen(f'https://api.github.com/repos/{repo}/git/trees/{revision}', timeout=20) as response:
            tree = json.load(response)
        names = [item['path'] for item in tree['tree'] if item['type'] == 'blob'
                 and item['path'].lower().startswith(('license', 'licence', 'copying', 'copyright', 'notice'))]
        with ThreadPoolExecutor(max_workers=4) as pool:
            UPSTREAM_CACHE[key] = [result for result in pool.map(fetch, names) if result]
    return UPSTREAM_CACHE[key]


def collect(root, toolchain):
    graphs = [
        ('Cargo.toml', 'x86_64-pc-windows-msvc'),
        ('Cargo.toml', 'x86_64-unknown-linux-gnu'),
        ('gui/Cargo.toml', 'wasm32-unknown-unknown'),
        ('gui/src-tauri/Cargo.toml', 'x86_64-pc-windows-msvc'),
    ]
    packages, documents, provenance, unresolved = {}, {}, {}, []
    for manifest, target in graphs:
        metadata = json.loads(subprocess.check_output(
            ['cargo', f'+{toolchain}', 'metadata', '--locked', '--format-version', '1',
             '--manifest-path', str(root / manifest), '--filter-platform', target], cwd=root))
        for package in metadata['packages']:
            if not package.get('source'):
                continue
            key = (package['name'], package['version'])
            if key in packages:
                continue
            package_root = Path(package['manifest_path']).resolve().parent
            files = [path for path in package_root.iterdir()
                     if path.is_file() and path.name.lower().startswith(
                         ('license', 'licence', 'copying', 'copyright', 'notice'))]
            if package.get('license_file'):
                files.append(package_root / package['license_file'])
            refs = set()
            texts = []
            for path in sorted(set(files)):
                resolved = path.resolve()
                if not resolved.is_relative_to(package_root) or not resolved.is_file():
                    raise RuntimeError(f'Invalid license source for {key}')
                data = resolved.read_bytes()
                if len(data) > 1024 * 1024:
                    raise RuntimeError(f'Oversized license source for {key}; review it explicitly')
                texts.append((data, f'{package["name"]} {package["version"]}: {resolved.name} from published crate'))
            if not texts:
                texts = upstream_texts(package, package_root)
            if not texts and key == ('content_disposition', '0.4.0') and package['license'] == 'MIT OR 0BSD':
                texts = [((root / 'licenses/0BSD.txt').read_bytes(),
                          '0BSD alternative declared at https://github.com/y0zong/content-disposition/blob/62330e3d606cbe32219300422f5922f55bedb3a2/Cargo.toml; canonical terms: https://opensource.org/license/0bsd')]
            for data, origin in texts:
                text = '\n'.join(line.rstrip() for line in data.decode('utf-8-sig').splitlines()).rstrip() + '\n'
                digest = hashlib.sha256(text.encode()).hexdigest()
                documents[digest] = text
                provenance.setdefault(digest, set()).add(origin)
                refs.add(digest)
            if not package.get('license') or not refs:
                unresolved.append((key, package.get('license') or 'UNKNOWN'))
            packages[key] = (package['license'], sorted(refs))
    return packages, documents, provenance, unresolved


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--toolchain', default='1.90.0')
    parser.add_argument('--output', type=Path, default=Path('THIRD_PARTY_NOTICES.md'))
    parser.add_argument('--allow-missing', action='store_true',
                        help='Produce a source inventory with explicit unresolved binary-redistribution gaps')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    packages, documents, provenance, unresolved = collect(root, args.toolchain)
    lines = ['# Third-party notices', '',
             'Dependency license texts for the declared Windows core/desktop, Linux edge and WASM builds.',
             'Each dependency retains its own terms; the project license does not replace them.',
             'The inventory can include build-time and platform-specific packages. It does not assert that every package is linked into every artifact.',
             'Generated by `python scripts/collect-third-party-notices.py` from the committed lockfiles.', '',
             'Line endings and trailing whitespace in collected texts are normalized; license wording and attribution are retained.', '',
             'Package sources are available at the linked version-specific crates.io pages.', '',
             '## Unresolved license text', '']
    if unresolved:
        lines += ['Bundled binary redistribution is blocked until these missing upstream texts/attributions are resolved.',
                  'Declared metadata alone is not a complete notice. This source repository does not vendor these crates.', '']
        lines += [f'- `{name} {version}`: declared `{license_name}`, no license text in the published crate or pinned upstream root.'
                  for (name, version), license_name in unresolved]
    else:
        lines += ['No missing dependency license texts in this inventory.']
    lines += ['', '## Package index', '', '| Package | Version | Declared license | License texts |',
             '|---|---|---|---|']
    for (name, version), (license_name, refs) in sorted(packages.items()):
        links = ', '.join(f'[{ref[:16]}](#license-{ref})' for ref in refs)
        lines.append(f'| [{name}](https://crates.io/crates/{name}/{version}) | {version} | {license_name} | {links or "UNRESOLVED"} |')
    lines += ['', '## License texts', '']
    for digest, text in sorted(documents.items()):
        lines += [f'<a id="license-{digest}"></a>', f'### {digest}', '',
                  'Sources: ' + '; '.join(sorted(provenance[digest])), '',
                  '``````text', text.rstrip(), '``````', '']
    output = args.output if args.output.is_absolute() else root / args.output
    output.write_text('\n'.join(lines), encoding='utf-8')
    print(json.dumps({'packages': len(packages), 'license_texts': len(documents),
                      'unresolved': len(unresolved), 'output': output.name}))
    if unresolved and not args.allow_missing:
        raise SystemExit('Missing upstream license texts; see the generated unresolved list. Binary redistribution remains blocked.')


if __name__ == '__main__':
    main()
