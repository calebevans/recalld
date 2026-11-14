#!/usr/bin/env python3
"""Offline real public-v1 transports; characterization, not v2 certification."""
import argparse
import hashlib
import http.server
import json
from pathlib import Path
import select
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = 'benchmarks/public-conformance/fixture.json'
FIXED = ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', FIXTURE,
         'scripts/public-conformance.py', 'scripts/public-conformance.Dockerfile']
PROTOCOL = '2025-06-18'


def sha(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()


def save(p, value):
    p.write_text(json.dumps(value, sort_keys=True, indent=2) + '\n')


def append(p, value):
    with p.open('a') as f:
        f.write(json.dumps(value, sort_keys=True) + '\n')


def coverage(ids, mappings, labels):
    found = {s for i in ids for s in mappings[i]}
    return len(found & set(labels)) / len(labels) if labels else None


def host(args):
    if not args.cache or not args.build_cache:
        raise SystemExit('--cache and --build-cache required; copied, never shared live')
    scratch = Path(tempfile.mkdtemp(prefix='recalld-public-conformance-', dir='/tmp'))
    output = scratch / 'output'
    output.mkdir()
    print(f'Scratch: {scratch}', flush=True)
    tracked = subprocess.check_output(['git', 'ls-files', '-z', '--', 'src'], cwd=ROOT)
    names = sorted(set(FIXED + [n.decode() for n in tracked.split(b'\0') if n.endswith(b'.rs')]))
    hashes = {}
    for name in names:
        p = ROOT / name
        if p.is_symlink() or not p.is_file() or not p.resolve().is_relative_to(ROOT.resolve()):
            raise RuntimeError(f'missing/nonregular input: {name}')
        target = scratch / 'source' / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(p, target)
        hashes[name] = sha(target)
    shutil.copytree(args.cache / 'registry', scratch / 'cargo/registry')
    shutil.copytree(args.build_cache, scratch / 'target')
    image = json.loads(subprocess.check_output(['docker', 'image', 'inspect', args.image]))[0]
    (output / 'source.patch').write_bytes(subprocess.check_output(['git', 'diff', 'HEAD', '--', *names], cwd=ROOT))
    manifest = {'manifestVersion': 1, 'track': 'public-v1-characterization',
        'sourceCommit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        'sourceFilesSha256': hashes, 'dirtyPatchSha256': sha(output / 'source.patch'),
        'imageId': image['Id'], 'imageDigests': image.get('RepoDigests', []),
        'fixtureSha256': hashes[FIXTURE], 'protocolVersion': PROTOCOL, 'contractVersion': 'legacy-v1',
        'clock': 'actual wall clock; no public manual clock', 'snapshots': 'A1 interfaces unavailable',
        'seed': 7, 'freshReplaysPerTransport': 3, 'transports': ['stdio', 'http-mcp', 'rest'],
        'limits': 'network none; 2 CPUs; 4 GiB; 2 build jobs', 'status': 'preparing'}
    command = ['docker', 'run', '--rm', '--network', 'none', '--cpus', '2', '--memory', '4g',
        '--memory-swap', '4g', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges', '--read-only',
        '--tmpfs', '/tmp:rw,nosuid,nodev', '--mount', f'type=bind,src={scratch},dst=/work',
        '--workdir', '/work/source', '--env', 'HOME=/tmp/home', '--env', 'CARGO_HOME=/work/cargo',
        '--env', 'CARGO_TARGET_DIR=/work/target', '--env', 'CARGO_BUILD_JOBS=2',
        '--env', 'RUSTUP_TOOLCHAIN=1.94.1', image['Id'], 'bash', '-c', '''set -eu
mkdir -p /tmp/home
rustc -Vv > /work/output/rustc.txt
python3 --version > /work/output/python.txt
cat /sys/fs/cgroup/cpu.max > /work/output/cgroup-cpu.txt
cat /sys/fs/cgroup/memory.max > /work/output/cgroup-memory.txt
cargo fmt --all -- --check > /work/output/fmt.log 2>&1
cargo test --offline --locked --lib --all-features > /work/output/tests.log 2>&1
cargo clippy --offline --locked --all-targets --all-features > /work/output/clippy.log 2>&1
cargo build --offline --locked --bin recalld > /work/output/build.log 2>&1
python3 scripts/public-conformance.py --inside
''']
    manifest['containerCommand'] = command
    save(output / 'manifest.json', manifest)
    try:
        with (output / 'container.log').open('w') as log:
            subprocess.run(command, check=True, stdout=log, stderr=subprocess.STDOUT)
        manifest['status'] = 'completed_characterization'
    except BaseException as exc:
        manifest['status'], manifest['error'] = 'failed', str(exc)
        raise
    finally:
        manifest['outputsSha256'] = {str(p.relative_to(output)): sha(p) for p in sorted(output.rglob('*'))
                                    if p.is_file() and p.name != 'manifest.json'}
        save(output / 'manifest.json', manifest)
        print(f"{manifest['status']}: {output}", flush=True)


class Stub(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        status = 200
        try:
            req = json.loads(raw)
            assert self.path == '/v1/embeddings' and req['model'] == 'public-fixture-v1'
            assert req['dimensions'] == 4 and req['encoding_format'] == 'float'
            vectors = [self.server.inputs[text] for text in req['input']]
            body = {'data': [{'index': i, 'embedding': v} for i, v in enumerate(vectors)],
                    'usage': {'prompt_tokens': 0, 'total_tokens': 0}}
        except (ValueError, KeyError, TypeError, AssertionError):
            status, body = 400, {'error': {'message': 'unrecorded fixture input/profile; no live fallback'}}
        append(self.server.log_path, {'path': self.path, 'requestBody': raw.decode(), 'status': status})
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Public:
    def __init__(self, mode, directory, out, stub_port):
        self.mode, self.directory, self.out = mode, directory, out
        self.serial, self.starts, self.session = 0, 0, None
        with socket.socket() as s:
            s.bind(('127.0.0.1', 0))
            self.port = s.getsockname()[1]
        (directory / 'home').mkdir()
        # Existing direct fallback: parent is a regular file, daemon cannot spawn.
        (directory / 'disabled-daemon').write_text('not a socket directory\n')
        self.env = {'PATH': '/usr/local/bin:/usr/bin:/bin', 'HOME': str(directory / 'home'),
            'TZ': 'UTC', 'RUST_LOG': 'info', 'OPENAI_API_KEY': 'synthetic-fixture-only',
            'RECALLD_DAEMON_SOCKET': str(directory / 'disabled-daemon/socket')}
        config = f'''timezone = "UTC"
[storage]
data_dir = "{directory}/data"
[embedding]
provider = "openai"
model_name = "public-fixture-v1"
api_key_env = "OPENAI_API_KEY"
base_url = "http://127.0.0.1:{stub_port}"
dimensions = 4
document_prefix = ""
query_prefix = ""
[decay]
disable_sweep = true
decay_rate_multiplier = 0.0
[graph]
autolink_enabled = false
[rif]
enabled = false
[cache]
warm_file_enabled = false
'''
        (directory / 'config.toml').write_text(config)
        (out / 'config.toml').write_text(config)
        self.proc = None

    def trace(self, request, response, start, status=None):
        append(self.out / 'trace.jsonl', {'diagnosticTraceVersion': 1, 'step': self.serial,
            'transport': self.mode, 'processStart': self.starts, 'request': request, 'response': response,
            'httpStatus': status, 'latencyUs': (time.perf_counter_ns() - start) / 1000})

    def http(self, method, path, body=None, headers=None):
        self.serial += 1
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(f'http://127.0.0.1:{self.port}{path}', data=data, method=method,
            headers={'Content-Type': 'application/json', **(headers or {})})
        start = time.perf_counter_ns()
        append(self.out / 'attempts.jsonl', {'step': self.serial, 'method': method, 'path': path, 'body': body})
        try:
            response = urllib.request.urlopen(req, timeout=15)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            raw = response.read().decode()
            try:
                value = json.loads(raw) if raw else None
            except ValueError:
                value = {'unparsedBody': raw}
            status, hdr = response.status, dict(response.headers)
        self.trace({'method': method, 'path': path, 'body': body}, value, start, status)
        return status, value, hdr

    def rpc(self, method, params, notification=False):
        self.serial += 1
        req = {'jsonrpc': '2.0', 'method': method, 'params': params}
        if not notification:
            req['id'] = self.serial
        if self.mode == 'stdio':
            start = time.perf_counter_ns()
            append(self.out / 'attempts.jsonl', {'step': self.serial, 'request': req})
            self.proc.stdin.write((json.dumps(req) + '\n').encode())
            self.proc.stdin.flush()
            if notification:
                self.trace(req, None, start)
                return
            ready, _, _ = select.select([self.proc.stdout], [], [], 20)
            if not ready:
                raise RuntimeError('stdio timeout; no unkeyed retry')
            value = json.loads(self.proc.stdout.readline())
            self.trace(req, value, start)
        else:
            headers = {'MCP-Protocol-Version': PROTOCOL}
            if self.session:
                headers['Mcp-Session-Id'] = self.session
            status, value, hdr = self.http('POST', '/mcp', req, headers)
            assert status in (200, 202), (status, value)
            for key, val in hdr.items():
                if key.lower() == 'mcp-session-id':
                    self.session = val
            if notification:
                return
        assert value['id'] == req['id'] and 'error' not in value, value
        return value['result']

    def start(self):
        self.starts += 1
        self.session = None
        self.log = (self.out / f'server-{self.starts}.log').open('wb')
        cmd = ['/work/target/debug/recalld', '--config', str(self.directory / 'config.toml'),
               '--data-dir', str(self.directory / 'data')]
        cmd += ['mcp'] if self.mode == 'stdio' else ['serve', '--bind', f'127.0.0.1:{self.port}']
        self.proc = subprocess.Popen(cmd, cwd=self.directory, env=self.env, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE if self.mode == 'stdio' else self.log, stderr=self.log, bufsize=0)
        if self.mode != 'stdio':
            deadline = time.monotonic() + 20
            while True:
                assert self.proc.poll() is None, 'server exited during startup'
                try:
                    with socket.create_connection(('127.0.0.1', self.port), timeout=.2):
                        break
                except OSError:
                    if time.monotonic() > deadline:
                        raise RuntimeError('HTTP listen timeout')
                    time.sleep(.05)
        if self.mode != 'rest':
            result = self.rpc('initialize', {'protocolVersion': PROTOCOL, 'capabilities': {},
                'clientInfo': {'name': 'public-conformance', 'version': '1'}})
            assert result['protocolVersion'] == PROTOCOL
            self.rpc('notifications/initialized', {}, notification=True)
            names = {t['name'] for t in self.rpc('tools/list', {})['tools']}
            assert {'store_memory', 'recall_memories', 'get_memory', 'reinforce_memory', 'forget_memory'} <= names

    def stop(self):
        if self.proc is None:
            return
        if self.proc.poll() is None:
            if self.mode == 'stdio':
                self.proc.stdin.close()
            else:
                self.proc.send_signal(signal.SIGINT)
                deadline = time.monotonic() + 10
                while self.proc.poll() is None:
                    if 'API server exited cleanly' in (self.out / f'server-{self.starts}.log').read_text():
                        break
                    if time.monotonic() > deadline:
                        raise RuntimeError('HTTP shutdown timeout')
                    time.sleep(.05)
                if self.proc.poll() is None:
                    # v1 serve waits again for system shutdown after HTTP returns.
                    time.sleep(.1)
                    self.proc.send_signal(signal.SIGTERM)
            self.proc.wait(timeout=15)
        assert self.proc.returncode == 0, self.proc.returncode
        self.log.close()
        if self.mode == 'stdio':
            assert 'falling back to direct mode' in (self.out / f'server-{self.starts}.log').read_text()

    def call(self, action, args):
        if self.mode != 'rest':
            value = self.rpc('tools/call', {'name': action, 'arguments': args})
            if value.get('isError', False):
                return False, value
            return True, json.loads(value['content'][0]['text'])
        payload = dict(args)
        memory = payload.pop('id', None)
        paths = {'store_memory': ('POST', '/memories'), 'create_namespace': ('POST', '/namespaces'),
            'recall_memories': ('POST', '/search'), 'reinforce_memory': ('POST', f'/memories/{memory}/reinforce'),
            'forget_memory': ('DELETE', f'/memories/{memory}')}
        if action == 'get_memory':
            flag = str(payload.get('recordAccess', True)).lower()
            method, path, payload = 'GET', f'/memories/{memory}?recordAccess={flag}', None
        else:
            method, path = paths[action]
        status, value, _ = self.http(method, path, payload)
        return 200 <= status < 300, value.get('data', value) if isinstance(value, dict) else value

    def good(self, action, args):
        ok, result = self.call(action, args)
        assert ok, (action, args, result)
        return result

    def get(self, memory):
        return self.good('get_memory', {'id': memory, 'recordAccess': False})

    def recall(self, namespace=None):
        args = {'query': 'amber', 'recordAccess': False, 'limit': 20}
        if namespace is not None:
            args['namespace'] = namespace
        value = self.good('recall_memories', args)
        return [h['memory']['id'] for h in value['hits']] if self.mode == 'rest' else [h['id'] for h in value['memories']]


def files_at(directory):
    # Physical read-window observation, not a canonical snapshot/barrier.
    return {str(p.relative_to(directory)): sha(p) for p in sorted(directory.rglob('*')) if p.is_file()}


def durable(files):
    # SQLite's WAL-index read marks are coordination state, not durable content.
    # Preserve the complete hashes in read-window.json; exclude only this sidecar.
    return {name: digest for name, digest in files.items() if name != 'fts.db-shm'}


def episode(mode, repeat, fixture, stub_port, output):
    out = output / f'{mode}-{repeat}'
    out.mkdir()
    directory = Path(tempfile.mkdtemp(prefix=f'public-{mode}-', dir='/tmp'))
    client = Public(mode, directory, out, stub_port)
    ids, mappings, violations = {}, {}, []
    try:
        client.start()
        client.good('create_namespace', {'name': 'foreign', 'embeddingDim': 4})
        for source in fixture['sources']:
            args = {'summary': source['summary'], 'checkDuplicates': False}
            if 'fullText' in source:
                args['fullText'] = source['fullText']
            if source['namespace'] != 'default':
                args['namespace'] = source['namespace']
            memory = client.good('store_memory', args)['id']
            assert memory not in mappings
            ids[source['summary']], mappings[memory] = memory, [source['sourceId']]
            fetched = client.get(memory)
            assert fetched['namespace'] == source['namespace']
            assert fetched.get('fullText') == source.get('fullText')
            append(out / 'turn-ledger.jsonl', {'sourceId': source['sourceId'], 'status': 'accepted',
                'contentSha256': hashlib.sha256(json.dumps({'summary': source['summary'], 'fullText': source.get('fullText')}, sort_keys=True).encode()).hexdigest(), 'memoryId': memory})
        save(out / 'source-mappings.json', mappings)
        before = {name: client.get(i) for name, i in ids.items()}
        disk_before = files_at(directory / 'data')
        ranks = client.recall()
        assert ranks == client.recall('default') and ids['amber'] in ranks and ids['foreign'] not in ranks
        assert ids['foreign'] in client.recall('foreign')
        for _ in range(3):
            client.recall()
            for i in ids.values():
                client.get(i)
        time.sleep(.2)  # bounded quiet check; explicitly NOT readiness
        after = {name: client.get(i) for name, i in ids.items()}
        disk_after = files_at(directory / 'data')
        save(out / 'read-window.json', {'before': before, 'after': after,
            'filesBefore': disk_before, 'filesAfter': disk_after, 'quietMs': 200,
            'excludedFromDurableComparison': ['fts.db-shm']})
        assert before == after, 'false reads changed public records'
        assert durable(disk_before) == durable(disk_after), 'false reads changed durable files'
        bad_inputs = [('store_memory', {'summary': 7}),
            ('store_memory', {'summary': 'fixture-miss'}),
            ('store_memory', {'summary': 'amber', 'tags': 'wrong-type'}),
            ('recall_memories', {'query': 'amber', 'recordAccess': 'false'}),
            ('recall_memories', {'query': 'amber', 'recordAccess': None}),
            ('get_memory', {'id': ids['amber'], 'recordAccess': 'invalid'}),
            ('reinforce_memory', {'id': ids['amber'], 'quality': 0}),
            ('reinforce_memory', {'id': ids['amber'], 'quality': 5})]
        for action, args in bad_inputs:
            prior = files_at(directory / 'data')
            ok, result = client.call(action, args)
            assert not ok, (action, args, result)
            assert durable(files_at(directory / 'data')) == durable(prior), 'rejected action changed durable files'
        prior = client.get(ids['olive'])
        files_before = files_at(directory / 'data')
        ok, result = client.call('reinforce_memory', {'id': ids['olive'], 'quality': 'bad'})
        updated = client.get(ids['olive'])
        files_after = files_at(directory / 'data')
        changed = durable(files_before) != durable(files_after)
        save(out / 'malformed-quality.json', {'accepted': ok, 'response': result,
            'before': prior, 'after': updated, 'filesBefore': files_before, 'filesAfter': files_after})
        if ok or changed or prior != updated:
            violations.append({'case': 'present-string-quality', 'accepted': ok, 'durableFilesChanged': changed})
        stability = client.get(ids['amber'])['stability']
        client.good('reinforce_memory', {'id': ids['amber'], 'quality': 4})
        # With multiplier zero, retrievability is one: grades 2..4 have no growth.
        assert client.get(ids['amber'])['stability'] == stability
        client.good('reinforce_memory', {'id': ids['amber'], 'quality': 1})
        weakened = client.get(ids['amber'])
        assert 0 < weakened['stability'] < stability and weakened['summary'] == 'amber'
        client.good('forget_memory', {'id': ids['birch']})
        deleted = client.get(ids['birch'])
        assert deleted['phase'].lower() == 'tombstone' and not deleted['summary'] and not deleted.get('fullText')
        ranks_before_restart = client.recall()
        assert ids['birch'] not in ranks_before_restart
        before_restart = {name: client.get(i) for name, i in ids.items()}
        client.stop()
        client.start()
        after_restart = {name: client.get(i) for name, i in ids.items()}
        save(out / 'restart.json', {'before': before_restart, 'after': after_restart})
        assert before_restart == after_restart, 'public state changed across restart'
        assert client.recall() == ranks_before_restart
        assert ids['birch'] not in client.recall()
        summary = {'transport': mode, 'replay': repeat, 'freshSources': len(ids),
            'falseReadPublicStateUnchanged': True, 'falseReadDurableFilesUnchanged': True,
            'defaultScopeMatchesExplicit': True, 'rejectedRequestsWithoutDurableFileChanges': len(bad_inputs),
            'grade4PreservesStabilityAtZeroDecay': True, 'grade1WeakensWithoutDeleting': True, 'deleteContentAbsent': True, 'restartPublicStateEqual': True,
            'extractionEvidenceRetention': coverage(list(mappings), mappings, fixture['evidenceSources']),
            'retrievalSourceRecallAt20': coverage(ranks[:20], mappings, fixture['evidenceSources']),
            'normalizedRanks': [mappings[i][0] for i in ranks],
            'normalizedRanksAfterDeleteAndRestart': [mappings[i][0] for i in ranks_before_restart], 'knownConformanceViolations': violations,
            'semanticQuality': 'not measured'}
        save(out / 'summary.json', summary)
        return summary
    finally:
        client.stop()


def inside():
    assert Path('/.dockerenv').is_file() and ROOT == Path('/work/source')
    fixture = json.loads((ROOT / FIXTURE).read_text())
    assert fixture['profile'] == {'model': 'public-fixture-v1', 'dimensions': 4,
                                  'documentPrefix': '', 'queryPrefix': ''}
    assert coverage(['a', 'b'], {'a': ['s'], 'b': ['s']}, ['s', 't']) == .5
    assert coverage([], {}, []) is None
    output = Path('/work/output')
    stub = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Stub)
    stub.inputs = {s['embeddingInput']: s['vector'] for s in fixture['sources']}
    stub.inputs.update(fixture['extraEmbeddingInputs'])
    stub.log_path = output / 'embedding-requests.jsonl'
    threading.Thread(target=stub.serve_forever, daemon=True).start()
    summaries = []
    try:
        for mode in ['stdio', 'http-mcp', 'rest']:
            for repeat in range(1, 4):
                summary = episode(mode, repeat, fixture, stub.server_port, output)
                summaries.append(summary)
                print(json.dumps(summary), flush=True)
        for mode in ['stdio', 'http-mcp', 'rest']:
            runs = [{k: v for k, v in s.items() if k != 'replay'} for s in summaries if s['transport'] == mode]
            assert runs[0] == runs[1] == runs[2]
        assert all(s['normalizedRanks'] == summaries[0]['normalizedRanks'] for s in summaries)
        requests = [json.loads(l) for l in stub.log_path.read_text().splitlines()]
        misses = [r for r in requests if r['status'] != 200]
        assert len(misses) == 9 and all(r['status'] == 400 and
            json.loads(r['requestBody'])['input'] == ['fixture-miss'] for r in misses)
        assert len(requests) > len(misses), 'no successful fixture embeddings'
        save(output / 'summary.json', {'status': 'characterization-complete', 'episodes': summaries,
            'conformancePassed': not any(s['knownConformanceViolations'] for s in summaries),
            'embeddingRequests': len(requests), 'expectedFailClosedMisses': len(misses), 'freshReplaysPerTransport': 3,
            'claim': 'public-v1 diagnostic; no v2/A1/A6 completion'})
    finally:
        stub.shutdown()
        stub.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--cache', type=Path)
    parser.add_argument('--build-cache', type=Path)
    parser.add_argument('--image', default='recalld-ci:1.94')
    args = parser.parse_args()
    inside() if args.inside else host(args)


if __name__ == '__main__':
    main()
