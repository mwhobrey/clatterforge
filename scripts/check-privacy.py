"""Fail on personal identifiers in tracked files and commit metadata.

Pattern-based on purpose: the real private deny terms must never live in a public repository,
so this gate looks for the shapes personal data takes instead (home-directory paths, real email
addresses, tailnet hostnames, tunnel hostnames, private keys). Fixtures must use reserved
example names: `example.invalid`/`example.com`/`.test` emails, `C:/fixtures/...` paths and
`*.example-tailnet.ts.net` hosts.

Usage: python scripts/check-privacy.py [--history] [--root PATH]
"""
import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

# Third-party license texts legitimately carry their authors' addresses.
SKIPPED_PREFIXES = ('licenses/',)
SKIPPED_FILES = {'THIRD_PARTY_NOTICES.md'}
SKIPPED_SUFFIXES = ('.lock',)

PLACEHOLDER_USERS = {'username', 'user', 'you', 'yourname', 'your-name', 'me', 'runneradmin',
                     'public', 'default', 'all users', '%username%', '$env:username', '<user>'}
HOME_PATH = re.compile(
    r'(?:\b[A-Za-z]:(?:\\\\|\\|/)+Users(?:\\\\|\\|/)+|(?<![\w.])/(?:home|Users)/)'
    r'(?P<user>[A-Za-z0-9._%$<>:-]+)', re.IGNORECASE)
EMAIL = re.compile(r'[A-Za-z0-9._%+-]+@(?P<domain>[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,})')
ALLOWED_EMAIL_DOMAIN = re.compile(
    r'(?:^|\.)(?:example\.(?:invalid|com|org|net)|example|invalid|test|localhost|git\.example'
    r'|users\.noreply\.github\.com|github\.com)$', re.IGNORECASE)
# `128x128@2x.png` and similar asset names look like addresses.
FILE_LIKE_TLD = {'png', 'jpg', 'jpeg', 'svg', 'ico', 'gif', 'webp', 'icns', 'json', 'js', 'css'}
TAILNET_HOST = re.compile(r'\b(?P<host>[a-z0-9-]+(?:\.[a-z0-9-]+)*\.ts\.net)\b', re.IGNORECASE)
TUNNEL_HOST = re.compile(r'\b[a-z0-9-]+\.trycloudflare\.com\b', re.IGNORECASE)
PRIVATE_KEY = re.compile(r'-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----')


def findings_in_text(text):
    """Yield (kind, match) for every suspicious span in `text`."""
    for m in HOME_PATH.finditer(text):
        user = m.group('user').rstrip(':').lower()
        if user not in PLACEHOLDER_USERS and not user.startswith('<'):
            yield 'home path', m.group(0)
    for m in EMAIL.finditer(text):
        domain = m.group('domain')
        if domain.rsplit('.', 1)[-1].lower() in FILE_LIKE_TLD:
            continue
        if not ALLOWED_EMAIL_DOMAIN.search(domain):
            yield 'email', m.group(0)
    for m in TAILNET_HOST.finditer(text):
        if 'example' not in m.group('host').lower():
            yield 'tailnet host', m.group(0)
    for m in TUNNEL_HOST.finditer(text):
        yield 'tunnel host', m.group(0)
    for m in PRIVATE_KEY.finditer(text):
        yield 'private key', m.group(0)


def tracked_files(root):
    names = subprocess.check_output(['git', '-C', str(root), 'ls-files', '-z']).decode().split('\0')
    for name in filter(None, names):
        if name in SKIPPED_FILES or name.startswith(SKIPPED_PREFIXES) or name.endswith(SKIPPED_SUFFIXES):
            continue
        path = root / name
        if not path.is_file():
            continue
        try:
            yield name, path.read_text(encoding='utf-8')
        except UnicodeDecodeError:
            continue  # binary assets


def public_commit_emails():
    """Maintainer addresses deliberately public in commit metadata (for example GitHub's merge
    commits). Supplied by the environment, never committed, and never accepted in file content."""
    raw = os.environ.get('PRIVACY_PUBLIC_COMMIT_EMAILS', '')
    return {email.strip().casefold() for email in raw.split(',') if email.strip()}


def history_identities(root):
    out = subprocess.check_output(
        ['git', '-C', str(root), 'log', '--all', '--format=%an <%ae>%n%cn <%ce>']).decode()
    return sorted(set(filter(None, out.splitlines())))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--history', action='store_true',
                        help='also check author/committer emails across all refs')
    args = parser.parse_args()
    problems = []
    for name, text in tracked_files(args.root):
        for kind, match in findings_in_text(text):
            problems.append(f'{name}: {kind}: {match}')
    if args.history:
        allowed = public_commit_emails()
        for identity in history_identities(args.root):
            # Compare the complete email field, not the matched substring: the email
            # pattern can start mid-field (`private!alice@...`), which must not pass.
            field = identity.rpartition('<')[2].rstrip('>').strip().casefold()
            if field in allowed:
                continue
            for kind, match in findings_in_text(identity):
                if kind == 'email':
                    problems.append(f'commit metadata: {kind}: {match}')
    if problems:
        print('Privacy check failed. Replace these with reserved example values:', file=sys.stderr)
        for problem in sorted(set(problems)):
            print('  ' + problem, file=sys.stderr)
        return 1
    print('privacy check passed')
    return 0


if __name__ == '__main__':
    sys.exit(main())
