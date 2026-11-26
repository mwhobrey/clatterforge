"""Fail if a retired product claim reappears in maintained documentation or the site.

  python scripts/check-doc-claims.py [--root DIR]

Each entry was true once and is false now. Dated history (ADRs, assessments, the current-state
log) may still quote them and is not scanned; the maintained, user-facing documents below are.
Runs in both the development repository and the public one: paths that do not exist are skipped.
"""
import argparse
from pathlib import Path
import sys

# (retired phrase, what is true now). Matching ignores case.
RETIRED = [
    ('Windows Rust host', 'the main forge runs on Windows, Linux and macOS'),
    ('not the Windows supervisor', 'the supervisor runs on Linux and macOS too'),
    ('Linux supports the Spark', 'Linux runs the main forge as well as Sparks'),
    ('core execution supervisor currently supports Windows', 'the supervisor is cross-platform'),
    ('no configured public repository', 'the public repository exists'),
    ('Public release is being prepared', 'the repository is public'),
    ('Before opening the repository publicly', 'private vulnerability reporting is enabled'),
    ('updates through the package manager', 'the .deb has no update channel yet'),
    ('updates through your package manager', 'the .deb has no update channel yet'),
    ('signed self-updating installers', 'updates are signed; only macOS builds are code-signed'),
    ('Prepare source integration', 'the desktop button names changed'),
    ('Restore original source', 'the desktop button is Restore Originals'),
    ('Recover original source', 'the desktop button is Recover Originals'),
]

MAINTAINED = [
    'README.md', 'CONTRIBUTING.md', 'SECURITY.md',
    'docs/ARCHITECTURE.md', 'docs/RELEASE.md', 'docs/runbook/integration.md',
    'docs/runbook/packaging.md', 'docs/runbook/managed-host.md',
    'docs/public/tree/README.md', 'docs/public/tree/CONTRIBUTING.md', 'docs/public/tree/SECURITY.md',
    'docs/public/tree/docs/ARCHITECTURE.md', 'docs/public/tree/docs/RELEASE.md',
    'docs/public/tree/docs/runbook/integration.md', 'docs/public/tree/docs/runbook/packaging.md',
    'docs/public/tree/docs/runbook/managed-host.md',
    'site/index.html',
]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    problems, scanned = [], 0
    for name in MAINTAINED:
        path = args.root / name
        if not path.is_file():
            continue
        scanned += 1
        for number, line in enumerate(path.read_text(encoding='utf-8').splitlines(), 1):
            lower = line.casefold()
            for phrase, now in RETIRED:
                if phrase.casefold() in lower:
                    problems.append(f'{name}:{number}: "{phrase}" is retired ({now})')
    if problems:
        print('Retired claims found:', file=sys.stderr)
        for problem in problems:
            print('  ' + problem, file=sys.stderr)
        return 1
    print(f'doc claims check passed ({scanned} files)')
    return 0


if __name__ == '__main__':
    sys.exit(main())
