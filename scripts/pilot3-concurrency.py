"""Retained actual-CLI concurrency receipts using deterministic loopback providers.

Usage: python scripts/pilot3-concurrency.py BINARY NEW_EVIDENCE_DIRECTORY
Only newly created fixture repositories/processes are used. No paid inference.
"""
import datetime
import hashlib
import http.server
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import threading
import time


if sys.flags.optimize > 0:
    raise SystemExit("Pilot 3 requires Python assertions; do not use -O or -OO.")


BIN = Path(sys.argv[1]).resolve(strict=True)
ROOT = Path(sys.argv[2]).resolve()
ROOT.mkdir(parents=True, exist_ok=False)
DB = ROOT / 'admission.sqlite'
COMMANDS = ROOT / 'commands.jsonl'
ASSIGNMENTS = []
CHECKS = []


def stamp():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def save(path, value):
    path.write_text(json.dumps(value, indent=2), encoding='utf-8')


def command(args, cwd=None, good=True):
    start = stamp()
    p = subprocess.run([str(a) for a in args], cwd=cwd, capture_output=True, text=True, timeout=30)
    with COMMANDS.open('a', encoding='utf-8') as f:
        f.write(json.dumps({'start': start, 'end': stamp(), 'argv': list(map(str, args)),
                            'cwd': str(cwd), 'exit': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr}) + '\n')
    if good:
        assert p.returncode == 0, (args, p.stdout, p.stderr)
    return p


def cli(state, *args, good=True):
    p = command([BIN, '--state', state, *args], good=good)
    return json.loads(p.stdout) if good else p


def git(repo, *args):
    return command(['git', '-C', repo, *args]).stdout.strip()


def rows(path, query, params=()):
    with sqlite3.connect(path.as_uri() + '?mode=ro', uri=True, timeout=5) as db:
        return db.execute(query, params).fetchall()


def read_task(a):
    return json.loads(rows(a.state / 'state.sqlite', 'SELECT record FROM orchestrations WHERE id=?', (a.id,))[0][0])


def job_root(a):
    return a.state / 'orchestration-inference' / a.id


def read_job(a):
    t = read_task(a)
    return json.loads(rows(job_root(a) / 'inference.sqlite', 'SELECT record FROM jobs WHERE id=?',
                           (t['rounds'][-1]['inference_job'],))[0][0])


def wait_for(test, label, seconds=20):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        value = test()
        if value:
            return value
        time.sleep(.02)
    raise AssertionError('Timed out: ' + label)


def check(name, evidence):
    CHECKS.append({'check': name, 'at': stamp(), 'evidence': evidence})
    save(ROOT / 'checks.json', CHECKS)
    print(name, flush=True)


class Provider:
    def __init__(self, name, mode='success', prompt=101, output=11, cached=7):
        self.name, self.mode = name, mode
        self.prompt, self.output, self.cached = prompt, output, cached
        self.gate = threading.Event()
        self.entered = threading.Event()
        self.idle = threading.Event()
        self.calls = 0
        self.events = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                assert self.path == '/v1/models'
                data = json.dumps({'data': [{'id': 'fixture:model', 'digest': 'a' * 64}],
                                   'runtime_version': 'pilot3-fixture-v1'}).encode()
                self.send_response(200)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_POST(self):
                assert self.path == '/v1/chat/completions'
                json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                outer.calls += 1
                n = outer.calls
                outer.events.append({'event': 'dispatch', 'round': n, 'at': stamp()})
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.end_headers()

                def emit(v):
                    self.wfile.write(('data: ' + (v if isinstance(v, str) else json.dumps(v)) + '\n\n').encode())
                    self.wfile.flush()

                try:
                    emit({'choices': [], 'usage': {'prompt_tokens': outer.prompt * n}})
                    outer.entered.set()
                    assert outer.gate.wait(45), 'Fixture gate deadline'
                    if outer.mode == 'provider_failure':
                        emit({'error': {'message': 'Injected deterministic provider failure'}})
                    else:
                        wrong = outer.mode == 'verification_failure' or (outer.mode == 'repair' and n == 1)
                        code = 'def add(a, b):\n    return a - b\n' if wrong else 'def add(a, b):\n    return a + b\n'
                        edit = json.dumps({'summary': 'Implement integer addition',
                                           'files': [{'path': 'src/calc.py', 'contents': code}]})
                        emit({'model': 'fixture:model', 'choices': [{'index': 0, 'delta': {'content': edit}, 'finish_reason': None}]})
                        emit({'model': 'fixture:model', 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]})
                        usage = {'prompt_tokens': outer.prompt * n, 'completion_tokens': outer.output * n}
                        if outer.cached is not None:
                            usage['prompt_tokens_details'] = {'cached_tokens': outer.cached * n}
                        emit({'choices': [], 'usage': usage})
                        emit('[DONE]')
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    outer.events.append({'event': 'client_disconnected', 'at': stamp()})
                finally:
                    outer.events.append({'event': 'handler_idle', 'round': n, 'at': stamp()})
                    outer.idle.set()
                    save(ROOT / (outer.name + '-provider-events.json'), outer.events)

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()

    def close(self):
        self.gate.set()
        self.server.shutdown()
        self.server.server_close()
        self.worker.join()


class Assignment:
    def __init__(self, name, provider, repo, state=None, rounds=1):
        self.name, self.provider = name, provider
        self.state = state or ROOT / (name + '-state')
        target = {'id': provider.name, 'provider': 'openai_local',
                  'endpoint': f'http://127.0.0.1:{provider.server.server_port}/v1/',
                  'model': 'fixture:model', 'context_limit': 32768, 'output_limit': 512,
                  'capabilities': {'streaming': True, 'tools': False, 'required_tool_choice': False,
                                   'parallel_tools': False, 'structured_output': False, 'media': False, 'reasoning_control': False},
                  'capability_source': 'deterministic isolated fixture runtime', 'priority': 0,
                  'resource_id': provider.name,
                  'api_pricing': {'currency': 'USD', 'input_micros_per_million': 2000000,
                                  'output_micros_per_million': 4000000, 'cached_input_micros_per_million': 1000000}}
        config = {'schema_version': 1, 'default_target': provider.name, 'hardware_id': 'deterministic-fixtures',
                  'admission_db': str(DB), 'max_active_per_resource': 1, 'registry_max_age_ms': 60000,
                  'fallback_before_dispatch': False, 'targets': [target]}
        spec = {'repository': str(repo), 'objective': 'Fix integer addition in src/calc.py only',
                'acceptance': 'Protected Python tests for positive, negative and zero addition pass',
                'allowed_files': ['src/calc.py'], 'context_files': [], 'verification_files': ['verify.py'],
                'verify_executable': str(Path(sys.executable).resolve()),
                'verify_args': ['-B', 'verify.py'] + (['--hold'] if provider.mode == 'verify_cancel' else []),
                'target': provider.name, 'max_rounds': rounds, 'max_output_tokens': 512,
                'generation_timeout_ms': 60000, 'verification_timeout_ms': 10000, 'reasoning': None}
        self.config = ROOT / (name + '-config.json')
        self.spec = ROOT / (name + '-spec.json')
        save(self.config, config)
        save(self.spec, spec)
        p = cli(self.state, 'orchestrate-prepare', '--spec', self.spec, '--config', self.config)
        save(ROOT / (name + '-prepared.json'), p)
        # Fixture-only approval of the exact captured source/worktree/verifier/provider digest.
        assert p['task']['assignment']['spec']['allowed_files'] == ['src/calc.py']
        self.id = p['task']['id']
        self.approved = cli(self.state, 'orchestrate-approve', self.id, '--revision', p['task']['revision'],
                            '--digest', p['assignment_digest'], '--by', 'Pilot 3 simulated scoped fixture approval')
        save(ROOT / (name + '-approved.json'), self.approved)
        self.workspace = Path(self.approved['task']['assignment']['workspace'])
        self.process = None
        ASSIGNMENTS.append(self)

    def start(self):
        self.out = (ROOT / (self.name + '-run.stdout.json')).open('w', encoding='utf-8')
        self.err = (ROOT / (self.name + '-run.stderr.txt')).open('w', encoding='utf-8')
        self.process = subprocess.Popen([str(BIN), '--state', str(self.state), 'orchestrate-run', self.id,
                                         '--revision', str(self.approved['task']['revision'])], stdout=self.out, stderr=self.err)
        save(ROOT / (self.name + '-launch.json'), {'at': stamp(), 'pid': self.process.pid, 'id': self.id,
                                                  'approved_revision': self.approved['task']['revision']})

    def collect(self, expected):
        if self.process:
            exit_code = self.process.wait(timeout=25)
            self.out.close()
            self.err.close()
            assert (exit_code == 0) == (expected == 'awaiting_review'), (self.name, exit_code)
        t = cli(self.state, 'orchestrate-show', self.id)
        s = cli(self.state, 'orchestrate-show', self.id, '--summary')
        save(ROOT / (self.name + '-result.json'), t)
        save(ROOT / (self.name + '-summary.json'), s)
        events = cli(self.state, 'orchestrate-events', self.id)
        save(ROOT / (self.name + '-events.json'), events)
        assert t['task']['status'] == expected and s['status'] == expected, (self.name, t)
        assert s['execution_id'] == self.id and s['target'] == self.provider.name
        assert t['task']['owner_token'] is None
        if expected == 'awaiting_review':
            assert s['verification'] == 'succeeded'
            assert s['review'] == 'unavailable'
        if expected != 'unknown' and t['task'].get('timing'):
            assert s['duration_ms'] is not None and s['duration_ms'] >= 0
        elif not t['task'].get('timing'):
            assert s['started_at'] is None and s['duration_ms'] is None
        if self.workspace.exists():
            assert self.workspace.resolve() == self.workspace
            assert self.workspace.is_relative_to(self.state)
            assert git(self.workspace, 'rev-parse', 'HEAD') == t['task']['assignment']['repository']['base']
            changed = git(self.workspace, 'status', '--porcelain')
            assert changed in ('M src/calc.py', ''), changed
            assert (self.workspace / 'verify.py').read_bytes() == (repo / 'verify.py').read_bytes()
            save(ROOT / (self.name + '-boundary.json'), {'workspace': str(self.workspace),
                  'git_common_dir': git(self.workspace, 'rev-parse', '--git-common-dir'), 'status': changed})
        for n, r in enumerate(t['task']['rounds']):
            for key in ['response', 'patch']:
                if r.get(key):
                    artifact = r[key]
                    path = Path(artifact['path'])
                    assert path.is_relative_to(self.state)
                    assert hashlib.sha256(path.read_bytes()).hexdigest() == artifact['hash']
            v = r.get('verification')
            if v:
                assert v['tree_terminated']
                assert Path(v['cwd']) == self.workspace
        return s

    def mutate(self, operation):
        t = read_task(self)
        return cli(self.state, operation, self.id, '--revision', t['revision'])

    def deny_replay(self):
        t = read_task(self)
        before = self.provider.calls
        p = cli(self.state, 'orchestrate-run', self.id, '--revision', t['revision'], good=False)
        assert p.returncode and self.provider.calls == before
        assert read_task(self) == t


def active(a):
    if not a.provider.entered.is_set():
        return False
    t = read_task(a)
    j = read_job(a)
    return t['status'] == 'generating' and j['status'] == 'running' and j['generations'][-1]['usage']['prompt_tokens'] == a.provider.prompt


def resources(a, filename):
    v = cli(a.state, 'inference-resources', '--config', a.config)
    save(ROOT / filename, v)
    return v


def acknowledge(a):
    a.provider.gate.set()
    assert a.provider.idle.wait(10), 'Must observe fixture idleness before acknowledgement'
    leases = resources(a, a.name + '-before-release.json')
    own = [r for r in leases if r['resource'] == a.provider.name]
    assert len(own) == 1 and own[0]['quarantined']
    cli(a.state, 'inference-release-resource', '--config', a.config, '--resource', a.provider.name,
        '--by', 'Pilot 3 fixture operator', '--reason', 'Owned fixture handler returned; observed idle, no other requests; no model server involved')
    assert not any(r['resource'] == a.provider.name for r in resources(a, a.name + '-after-release.json'))


def pair(case, mode='success'):
    pa = Provider(case + '-a', mode)
    pb = Provider(case + '-b', prompt=202, output=22, cached=None)
    a = Assignment(case + '-a', pa, repo, rounds=2 if mode == 'repair' else 1)
    b = Assignment(case + '-b', pb, repo)
    # Initialize shared SQLite metadata before simultaneous first opens.
    initial = cli(a.state, 'inference-resources', '--config', a.config)
    save(ROOT / (case + '-initial-resources.json'), initial)
    assert not initial
    a.start()
    b.start()
    wait_for(lambda: active(a) and active(b), case + ' both actually generating')
    sample = {'observed_at': stamp(), 'tasks': [read_task(a), read_task(b)], 'jobs': [read_job(a), read_job(b)],
              'reservations': resources(a, case + '-active-resources.json')}
    save(ROOT / (case + '-overlap.json'), sample)
    assert len(sample['reservations']) == 2
    assert a.id != b.id and a.workspace != b.workspace
    assert not a.workspace.is_relative_to(b.workspace) and not b.workspace.is_relative_to(a.workspace)
    live = read_task(a)
    assert cli(a.state, 'orchestrate-reconcile', a.id, '--revision', live['revision'], good=False).returncode
    check(case + ': two active approved executions, distinct worktrees, two bounded leases', [a.id, b.id])
    return a, b


repo = ROOT / 'fixture-repository'
(repo / 'src').mkdir(parents=True)
for name, text in {'README.md': 'Pilot 3 deterministic addition fixture.\n',
                   'AGENTS.md': 'Only edit approved src/calc.py. Preserve protected verify.py.\n',
                   'src/calc.py': 'def add(a, b):\n    return 0\n',
                   'verify.py': 'import runpy, sys, time\nif "--hold" in sys.argv:\n    print("owned fixture verifier active", flush=True)\n    time.sleep(45)\nf = runpy.run_path("src/calc.py")["add"]\nassert f(3,4)==7\nassert f(-5,2)==-3\nassert f(0,0)==0\nprint("3 independent arithmetic assertions passed")\n'}.items():
    (repo / name).write_text(text, encoding='utf-8')
git(repo, 'init', '-b', 'main')
git(repo, 'config', 'core.autocrlf', 'false')
git(repo, 'add', '.')
git(repo, '-c', 'user.name=Conductor fixture', '-c', 'user.email=fixture@example.invalid', '-c', 'core.hooksPath=',
    '-c', 'commit.gpgsign=false', 'commit', '-m', 'Isolated immutable Pilot 3 arithmetic baseline')
assert command([sys.executable, '-B', 'verify.py'], cwd=repo, good=False).returncode != 0
save(ROOT / 'run.json', {'started_at': stamp(), 'binary': str(BIN), 'binary_sha256': hashlib.sha256(BIN.read_bytes()).hexdigest(),
                       'fixture_base': git(repo, 'rev-parse', 'HEAD'), 'kind': 'actual orchestrator CLI, deterministic HTTP provider; no live model'})

try:
    a, b = pair('success', 'repair')
    a.provider.gate.set()
    b.provider.gate.set()
    sa, sb = a.collect('awaiting_review'), b.collect('awaiting_review')
    assert sa['tokens']['input']['total'] == 303 and sa['tokens']['output']['total'] == 33
    assert sa['tokens']['cached_input']['total'] == 21 and sa['repair_attempts'] == 1
    assert sa['estimated_api_cost']['amount'] == '0.000717'
    assert sb['tokens']['input']['total'] == 202 and sb['repair_attempts'] == 0
    assert sb['tokens']['cached_input']['total'] is None and sb['estimated_api_cost'] is None
    assert not resources(a, 'success-released.json')
    check('independent repair, usage, fictional cost and missing metrics; completion releases both resources', [sa, sb])
    a.provider.close()
    b.provider.close()

    a, b = pair('cancel')
    invalid = json.loads(a.config.read_text())
    invalid['max_active_per_resource'] = 2
    invalid_path = ROOT / 'unsupported-capacity-config.json'
    save(invalid_path, invalid)
    rejected = cli(ROOT / 'invalid-config-state', 'orchestrate-prepare', '--spec', a.spec, '--config', invalid_path, good=False)
    assert rejected.returncode and 'one admission slot per resource' in rejected.stderr
    assert a.provider.calls == 1 and b.provider.calls == 1
    check('configuration rejects unsupported per-resource capacity two before dispatch', invalid_path.name)
    # Same state root must reject before worktree creation or provider dispatch.
    pending = Assignment('cancel-pending', a.provider, repo, state=a.state)
    denied = cli(pending.state, 'orchestrate-run', pending.id, '--revision', pending.approved['task']['revision'], good=False)
    assert denied.returncode and 'another orchestration owns' in denied.stderr
    assert not pending.workspace.exists() and a.provider.calls == 1
    pending.mutate('orchestrate-cancel')
    pending.collect('cancelled')
    pending.deny_replay()
    check('state-root maximum one enforced; pending approved work cancelled without dispatch', pending.id)
    # Another root with the SAME actual fixture resource must not dispatch.
    busy = Assignment('cancel-busy', a.provider, repo)
    busy.start()
    busy.collect('unknown')
    j = read_job(busy)
    assert j['status'] == 'ready' and not j['generations'] and a.provider.calls == 1
    cli(job_root(busy), 'inference-cancel', j['id'], '--revision', j['revision'])
    assert read_job(busy)['status'] == 'cancelled'
    busy.deny_replay()
    check('shared target rejects occupied capacity; undispatched ready inference cancelled; no replay', busy.id)
    before = read_task(b)
    a.mutate('orchestrate-cancel')
    sa = a.collect('cancelled')
    assert read_task(b) == before and b.process.poll() is None
    assert sa['tokens']['input']['total'] == 101 and sa['tokens']['output']['total'] is None
    assert sa['estimated_api_cost'] is None and sa['verification'] is None
    assert (a.workspace / 'src/calc.py').read_bytes() == (repo / 'src/calc.py').read_bytes()
    a.deny_replay()
    t = read_task(a)
    assert cli(a.state, 'orchestrate-reconcile', a.id, '--revision', t['revision'], good=False).returncode
    acknowledge(a)
    # A fresh approved assignment can use the released slot, preserving cancelled evidence.
    fresh = Assignment('cancel-fresh', a.provider, repo, state=a.state)
    fresh.start()
    fresh.collect('awaiting_review')
    b.provider.gate.set()
    b.collect('awaiting_review')
    assert not resources(a, 'cancel-all-released.json')
    check('cancelled execution retains partial evidence; peer unchanged; idle acknowledgement releases slot for fresh work', a.id)
    a.provider.close()
    b.provider.close()

    a, b = pair('verifier-cancel', 'verify_cancel')
    a.provider.gate.set()
    partial_stdout = a.state / 'orchestration-artifacts' / a.id / '0/verify.stdout.part'
    wait_for(lambda: read_task(a)['status'] == 'verifying' and partial_stdout.exists()
             and b'owned fixture verifier active' in partial_stdout.read_bytes(), 'native verifier printed active marker')
    before = read_task(b)
    a.mutate('orchestrate-cancel')
    sa = a.collect('cancelled')
    assert sa['verification'] == 'cancelled' and read_task(a)['rounds'][0]['patch']
    log = Path(read_task(a)['rounds'][0]['verification']['stdout']['artifact']['path'])
    assert b'owned fixture verifier active' in log.read_bytes()
    assert (a.workspace / 'src/calc.py').read_text() == 'def add(a, b):\n    return a + b\n'
    assert read_task(b) == before and b.process.poll() is None
    assert not any(r['resource'] == a.provider.name for r in resources(a, 'verifier-cancel-released.json'))
    a.deny_replay()
    b.provider.gate.set()
    b.collect('awaiting_review')
    assert not resources(a, 'verifier-cancel-all-released.json')
    check('native verifier cancellation retains isolated applied patch and logs, terminates owned tree, leaves peer healthy', [a.id, b.id])
    a.provider.close()
    b.provider.close()

    for case, mode in [('verification-failure', 'verification_failure'), ('provider-failure', 'provider_failure')]:
        a, b = pair(case, mode)
        a.provider.gate.set()
        sa = a.collect('needs_work')
        assert active(b) and b.process.poll() is None
        if mode == 'verification_failure':
            assert sa['verification'] == 'failed' and read_task(a)['rounds'][0]['patch']
            assert not any(r['resource'] == a.provider.name for r in resources(a, case + '-released.json'))
        else:
            assert sa['verification'] is None and sa['tokens']['input']['total'] == 101
            acknowledge(a)
        a.deny_replay()
        b.provider.gate.set()
        b.collect('awaiting_review')
        assert not resources(a, case + '-all-released.json')
        check(case + ': affected execution stopped; independent healthy execution passed', [a.id, b.id])
        a.provider.close()
        b.provider.close()

    a, b = pair('owner-recovery')
    # Only the Popen process created by this harness is killed, while it owns no verifier.
    assert read_task(a)['rounds'][0]['verifier_process'] is None
    a.process.kill()
    a.process.wait(timeout=10)
    a.out.close()
    a.err.close()
    a.process = None
    recovered = a.mutate('orchestrate-reconcile')
    assert recovered['task']['status'] == 'unknown' and read_job(a)['status'] == 'unknown'
    sa = a.collect('unknown')
    assert sa['ended_at'] is None and sa['duration_ms'] is None and sa['recovery']['replayed'] is False
    assert sa['tokens']['input']['total'] == 101 and sa['repair_attempts'] == 0
    a.deny_replay()
    assert active(b)
    acknowledge(a)
    b.provider.gate.set()
    b.collect('awaiting_review')
    assert not resources(a, 'recovery-all-released.json')
    check('dead owned fixture owner reconciled to unknown; evidence retained; no duplicate dispatch; peer succeeded', [a.id, b.id])
    a.provider.close()
    b.provider.close()

    assert git(repo, 'status', '--porcelain') == ''
    assert (repo / 'src/calc.py').read_text() == 'def add(a, b):\n    return 0\n'
    worktrees = git(repo, 'worktree', 'list', '--porcelain')
    (ROOT / 'worktrees.txt').write_text(worktrees, encoding='utf-8')
    save(ROOT / 'manifest.json', {'finished_at': stamp(), 'assignments': [
        {'name': a.name, 'execution_id': a.id, 'state': str(a.state), 'workspace': str(a.workspace),
         'status': read_task(a)['status']} for a in ASSIGNMENTS], 'checks': len(CHECKS) + 1,
         'source_checkout_clean': True, 'fixture_only': True, 'no_automatic_integration': True})
    check('all persisted worktrees scoped; protected tests and source checkout preserved; no integration', str(ROOT))
except BaseException:
    import traceback
    (ROOT / 'FAILURE.txt').write_text(traceback.format_exc(), encoding='utf-8')
    raise
finally:
    for a in ASSIGNMENTS:
        a.provider.gate.set()
    # Do not destroy evidence or automatically retry failed/unknown work.
