"""Publish development commits to the independent public repository, one commit each.

The public repository never shares history with the development checkout. Each development
commit not yet published is exported with `prepare-public-release.py` from a temporary
worktree at that exact commit, committed to a clone of the public repository under a neutral
identity, and verified before anything is pushed:

* after each commit, the public tree equals the export byte for byte and no private deny
  term appears anywhere in the public history (patches and metadata);
* before pushing, `check-privacy.py --history` passes on the final tree and gitleaks passes
  when it is installed.

Commits that change nothing public are recorded and skipped. The development-to-public
mapping lives in a private JSON file (default `<state>/oss-sync/published.json`), so the
next run starts after the last published commit.

By default each change goes through a pull request so CodeRabbit and CI review it before it
reaches `main`: a run opens one PR (branch `sync/<dev sha>`) for the next unpublished change
and stops. Later runs report that PR while it is open; once it is merged (rebase or squash),
the next run records the merged commit and opens the PR for the following change. A PR closed
without merging stops the sync until the operator decides. `--direct` pushes every pending
change straight to `main` instead.

Usage:
  python scripts/sync-public-repo.py --clone <public clone> --privacy-policy <deny list>
      [--message <dev-sha>=<file>] [--no-push] [--direct]
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = {f'GIT_{role}_{field}': value
            for role in ('AUTHOR', 'COMMITTER')
            for field, value in (('NAME', 'Clatterforge contributors'),
                                 ('EMAIL', 'contributors@example.invalid'))}


def state_dir():
    """Same rule as the binary: `.clatterforge`, unless only a pre-rename `.conductor` exists."""
    current, legacy = ROOT / '.clatterforge', ROOT / '.conductor'
    return legacy if not current.exists() and legacy.is_dir() else current


def git(cwd, *args, env=None, text=True):
    result = subprocess.run(['git', '-C', str(cwd), *args], capture_output=True, text=text,
                            env=env, check=False)
    if result.returncode != 0:
        raise RuntimeError(f'git {" ".join(args)} failed: {result.stderr.strip()}')
    return result.stdout


def tree_files(root):
    files = {}
    for path in root.rglob('*'):
        if '.git' in path.relative_to(root).parts or not path.is_file():
            continue
        files[path.relative_to(root).as_posix()] = path.read_bytes()
    return files


def replace_tree(clone, export):
    for entry in clone.iterdir():
        if entry.name == '.git':
            continue
        shutil.rmtree(entry) if entry.is_dir() else entry.unlink()
    shutil.copytree(export, clone, dirs_exist_ok=True)


def deny_terms(policy_path):
    policy = json.loads(policy_path.read_text(encoding='utf-8-sig'))
    terms = [t.casefold() for t in policy.get('literal_terms', []) if t]
    if policy.get('private_email'):
        terms.append(policy['private_email'].casefold())
    return terms


def verify_commit(clone, export, policy_path):
    """Per published commit: exact tree, and no deny term anywhere in the history so far."""
    if tree_files(clone) != tree_files(export):
        raise RuntimeError('public tree differs from the export')
    history = git(clone, 'log', '--all', '-p', '--format=%an%n%ae%n%cn%n%ce%n%B',
                  text=False).decode('utf-8', 'replace').casefold()
    hits = sum(1 for term in deny_terms(policy_path) if term in history)
    if hits:
        raise RuntimeError(f'{hits} private deny term(s) found in public history; nothing pushed')


def verify_final(clone):
    """Before pushing: the pattern gate on the final tree and history metadata, and gitleaks."""
    subprocess.run([sys.executable, '-I', str(ROOT / 'scripts/check-privacy.py'), '--history',
                    '--root', str(clone)], check=True)
    gitleaks = shutil.which('gitleaks')
    if gitleaks:
        subprocess.run([gitleaks, 'detect', '--source', str(clone), '--no-banner', '--redact'],
                       check=True, capture_output=True)
    else:
        print('gitleaks not installed; CI will still run it', file=sys.stderr)


def gh(cwd, *args):
    result = subprocess.run([shutil.which('gh') or 'gh', *args], cwd=cwd, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise RuntimeError(f'gh {" ".join(args[:2])} failed: {result.stderr.strip()}')
    return result.stdout


def resolve_pending_pr(clone, state):
    """Returns True when the sync may continue: no PR is waiting, or the waiting one merged."""
    pending = state.get('pending_pr')
    if not pending:
        return True
    view = json.loads(gh(clone, 'pr', 'view', str(pending['number']),
                         '--json', 'state,mergeCommit,url'))
    if view['state'] == 'OPEN':
        print(f"waiting on {view['url']} for {pending['dev'][:7]}; nothing else opened")
        return False
    if view['state'] != 'MERGED' or not view.get('mergeCommit'):
        raise RuntimeError(f"{view['url']} was closed without merging; reopen it, or remove "
                           f"pending_pr from the state file to publish that change again")
    state['published'].append({'dev': pending['dev'], 'public': view['mergeCommit']['oid']})
    del state['pending_pr']
    git(clone, 'fetch', '-q', 'origin')
    git(clone, 'checkout', '-q', 'main')
    git(clone, 'reset', '-q', '--hard', 'origin/main')
    print(f"recorded {pending['dev'][:7]} -> {view['mergeCommit']['oid'][:7]} from {view['url']}")
    return True


def open_pull_request(clone, sha, message):
    """Pushes the change on its own branch and opens a PR against main."""
    branch = f'sync/{sha[:12]}'
    git(clone, 'push', '-q', 'origin', f'HEAD:refs/heads/{branch}')
    title, _, body = message.partition('\n')
    url = gh(clone, 'pr', 'create', '--base', 'main', '--head', branch,
             '--title', title.strip(), '--body', body.strip() or title.strip()).strip()
    number = int(url.rstrip('/').rsplit('/', 1)[-1])
    return branch, number, url


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--clone', type=Path, required=True, help='clone of the public repo')
    parser.add_argument('--privacy-policy', type=Path, required=True,
                        help='private deny-term policy; never copied anywhere')
    parser.add_argument('--state', type=Path, default=state_dir() / 'oss-sync/published.json')
    parser.add_argument('--message', action='append', default=[], metavar='SHA=FILE',
                        help='public commit message for one development commit, for messages '
                             'that describe private-only files')
    parser.add_argument('--no-push', action='store_true')
    parser.add_argument('--direct', action='store_true',
                        help='push every pending change straight to main instead of opening a PR')
    args = parser.parse_args()
    clone = args.clone.resolve()
    args.privacy_policy = args.privacy_policy.resolve()

    state = json.loads(args.state.read_text(encoding='utf-8')) if args.state.exists() else None
    if not state or not state.get('published'):
        raise RuntimeError(f'{args.state} must record at least one published commit to start from')
    last_dev = state['published'][-1]['dev']
    overrides = {}
    for item in args.message:
        sha, _, path = item.partition('=')
        overrides[git(ROOT, 'rev-parse', sha).strip()] = Path(path).read_text(encoding='utf-8')

    if git(ROOT, 'status', '--porcelain').strip():
        raise RuntimeError('development checkout has uncommitted changes; commit or stash first')
    git(clone, 'fetch', '-q', 'origin')
    if not args.direct:
        if not resolve_pending_pr(clone, state):
            return 0
        args.state.write_text(json.dumps(state, indent=2) + '\n', encoding='utf-8')
        last_dev = state['published'][-1]['dev']
    if git(clone, 'rev-parse', 'HEAD').strip() != git(clone, 'rev-parse', 'origin/main').strip():
        raise RuntimeError('public clone is not at origin/main; reset it before syncing')
    pending = git(ROOT, 'rev-list', '--reverse', f'{last_dev}..HEAD').split()
    if not pending:
        print('nothing to publish')
        return 0

    work = Path(tempfile.mkdtemp(prefix='public-sync-'))
    published = []
    try:
        for sha in pending:
            worktree, export = work / f'wt-{sha[:12]}', work / f'export-{sha[:12]}'
            git(ROOT, 'worktree', 'add', '-q', '--detach', str(worktree), sha)
            try:
                exported = subprocess.run(
                    [sys.executable, '-I', str(worktree / 'scripts/prepare-public-release.py'),
                     '--output', str(export), '--privacy-policy', str(args.privacy_policy)],
                    capture_output=True, text=True)
                if exported.returncode != 0:
                    raise RuntimeError(f'export of {sha[:7]} failed: {exported.stderr.strip()}')
            finally:
                git(ROOT, 'worktree', 'remove', '--force', str(worktree))
            replace_tree(clone, export)
            git(clone, 'add', '-A')
            if not git(clone, 'status', '--porcelain').strip():
                print(f'{sha[:7]}: no public changes, skipped')
                published.append({'dev': sha, 'public': None})
                continue
            message = overrides.get(sha) or git(ROOT, 'log', '-1', '--format=%B', sha)
            subprocess.run(['git', '-C', str(clone), '-c', 'core.hooksPath=',
                            '-c', 'commit.gpgsign=false', 'commit', '-q', '-F', '-'],
                           input=message, text=True, check=True, env={**os.environ, **IDENTITY})
            public_sha = git(clone, 'rev-parse', 'HEAD').strip()
            verify_commit(clone, export, args.privacy_policy)
            print(f'{sha[:7]} -> {public_sha[:7]} {message.splitlines()[0]}')
            if not args.direct:
                # One reviewed change at a time: verify it, open its PR and stop.
                verify_final(clone)
                if args.no_push:
                    print('--no-push: verified locally; no branch pushed, no PR opened')
                    return 0
                branch, number, url = open_pull_request(clone, sha, message)
                state['published'].extend(published)
                state['pending_pr'] = {'dev': sha, 'branch': branch, 'number': number}
                args.state.write_text(json.dumps(state, indent=2) + '\n', encoding='utf-8')
                git(clone, 'reset', '-q', '--hard', 'origin/main')
                print(f'opened {url}; merge it (rebase or squash), then run again')
                return 0
            published.append({'dev': sha, 'public': public_sha})
        if not args.direct:
            # Only skipped (no public change) commits were pending.
            state['published'].extend(published)
            args.state.write_text(json.dumps(state, indent=2) + '\n', encoding='utf-8')
            print('nothing public to open a PR for')
            return 0
        if any(entry['public'] for entry in published):
            verify_final(clone)
        if not args.no_push:
            git(clone, 'push', '-q', 'origin', 'main')
            print('pushed')
            state['published'].extend(published)
            args.state.write_text(json.dumps(state, indent=2) + '\n', encoding='utf-8')
        else:
            print('--no-push: verified locally; mapping not recorded')
    finally:
        shutil.rmtree(work, ignore_errors=True)
        git(ROOT, 'worktree', 'prune')
    return 0


if __name__ == '__main__':
    sys.exit(main())
