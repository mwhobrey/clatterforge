"""Prepare an independent public-source repository; never rewrite the development repository."""
import argparse
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib


ROOT_FILES = {'.coderabbit.yaml', '.gitattributes', '.gitignore', 'Cargo.toml', 'Cargo.lock', 'LICENSE', 'NOTICE',
              'THIRD_PARTY_NOTICES.md'}
SOURCE_ROOTS = ('.github/', 'src/', 'edge/', 'protocol/', 'gui/', 'tests/', 'scripts/', 'licenses/', 'examples/')
PUBLIC_DOCS = 'docs/public/tree/'
MANIFESTS = ('Cargo.toml', 'edge/Cargo.toml', 'protocol/Cargo.toml', 'gui/Cargo.toml',
             'gui/src-tauri/Cargo.toml')


def git(root, *args):
    return subprocess.check_output(['git', '-C', str(root), *args])


def public_path(name):
    if name.startswith(PUBLIC_DOCS):
        return name[len(PUBLIC_DOCS):]
    if Path(name).name == 'AGENTS.md':
        return None
    if name in ROOT_FILES or name.startswith(SOURCE_ROOTS):
        return name
    return None


def privacy_check(files, policy):
    terms = [term.casefold() for term in policy.get('literal_terms', []) if term]
    if policy.get('private_email'):
        terms.append(policy['private_email'].casefold())
    failures = []
    for name, data in files.items():
        if any(part.casefold() in {'.git', '.conductor', '.clatterforge', 'target'}
               for part in Path(name).parts):
            failures.append(name)
            continue
        if re.search(r'(^|/)\.env($|\.(?!example$))|\.(sqlite|db|pem|pfx|p12|key)$', name, re.I):
            failures.append(name)
            continue
        if any(term in name.casefold() for term in terms):
            failures.append(name)
            continue
        if any(term.encode('utf-8') in data.lower() for term in terms):
            failures.append(name)
            continue
        try:
            text = data.decode('utf-8')
        except UnicodeError:
            continue
        if any(term in text.casefold() for term in terms):
            failures.append(name)
    if failures:
        raise RuntimeError('Public source privacy check failed in: ' + ', '.join(sorted(set(failures))))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--privacy-policy', type=Path, required=True,
                        help='Private deny terms; this file is never copied to the output')
    parser.add_argument('--initialize-git', action='store_true')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    output = args.output.resolve()
    if output.exists():
        raise RuntimeError('Output must not exist; preserve prior candidates')
    if output == root or root.is_relative_to(output):
        raise RuntimeError('Output must not contain the development checkout')
    tracked = git(root, 'ls-files', '--stage', '-z').decode().split('\0')
    files = {}
    destinations = set()
    selected = []
    for entry in filter(None, tracked):
        header, name = entry.split('\t', 1)
        mode, oid, stage = header.split()
        destination = public_path(name)
        if destination is None:
            continue
        if mode not in ('100644', '100755') or stage != '0':
            raise RuntimeError('Only resolved regular source files can be exported: ' + name)
        source = root / name
        if source.is_symlink() or not source.resolve().is_relative_to(root):
            raise RuntimeError('Source path escapes checkout: ' + name)
        destination_path = Path(destination)
        if destination_path.is_absolute() or '..' in destination_path.parts:
            raise RuntimeError('Public path escapes output: ' + destination)
        if destination.casefold() in destinations:
            raise RuntimeError('Duplicate public path: ' + destination)
        destinations.add(destination.casefold())
        selected.append((destination, oid))
    content = subprocess.check_output(['git', '-C', str(root), 'cat-file', '--batch'],
                                      input=''.join(oid + '\n' for _, oid in selected).encode())
    stream = io.BytesIO(content)
    for destination, oid in selected:
        actual_oid, kind, size = stream.readline().decode().strip().split()
        if actual_oid != oid or kind != 'blob':
            raise RuntimeError('Invalid indexed public source: ' + destination)
        files[destination] = stream.read(int(size))
        if stream.read(1) != b'\n':
            raise RuntimeError('Invalid Git object framing')
    for required in ('README.md', 'AGENTS.md', 'CONTRIBUTING.md', 'SECURITY.md', 'LICENSE',
                     'NOTICE', 'THIRD_PARTY_NOTICES.md', 'docs/runbook/00_INDEX.md'):
        if required not in files:
            raise RuntimeError('Missing curated public file: ' + required)
    licenses = {tomllib.loads(files[name].decode())['package'].get('license') for name in MANIFESTS}
    if len(licenses) != 1 or None in licenses:
        raise RuntimeError('First-party package licenses must be explicit and consistent')
    policy = json.loads(args.privacy_policy.read_text(encoding='utf-8-sig'))
    privacy_check(files, policy)
    output.mkdir(parents=True)
    for name, data in files.items():
        destination = (output / name).resolve()
        if not destination.is_relative_to(output):
            raise RuntimeError('Public path escapes output: ' + name)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
    if args.initialize_git:
        git(output, 'init', '--template=', '-b', 'main')
        git(output, 'config', 'core.autocrlf', 'false')
        git(output, 'add', '--all')
        environment = os.environ.copy()
        for role in ('AUTHOR', 'COMMITTER'):
            environment[f'GIT_{role}_NAME'] = 'Clatterforge contributors'
            environment[f'GIT_{role}_EMAIL'] = 'contributors@example.invalid'
        subprocess.run(['git', '-C', str(output), '-c', 'core.hooksPath=',
                        '-c', 'commit.gpgsign=false', 'commit', '-m',
                        ':tada: feat: introduce Clatterforge supervised source alpha'],
                       env=environment, check=True, stdout=subprocess.PIPE)
        if git(output, 'rev-list', '--all', '--count').strip() != b'1':
            raise RuntimeError('Public candidate must have one independent initial commit')
        if git(output, 'remote').strip():
            raise RuntimeError('Public candidate must not have a configured remote')
        metadata = git(output, 'log', '--all', '--format=%B%n%an%n%ae%n%cn%n%ce')
        privacy_check({'commit-metadata': metadata}, policy)
    print(json.dumps({'files': len(files), 'license': next(iter(licenses)),
                      'initial_history': args.initialize_git, 'output': str(output)}))


if __name__ == '__main__':
    main()
