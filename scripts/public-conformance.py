#!/usr/bin/env python3
"""Offline real public-v1 transports; characterization, not v2 certification."""
import argparse
import hashlib
import http.server
import json
import re
from pathlib import Path
import select
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = 'benchmarks/public-conformance/fixture.json'
FIXTURES = {1: FIXTURE, 2: 'benchmarks/public-conformance/fixture-v2.json', 3: 'benchmarks/public-conformance/fixture-v3.json'}
HARNESS_INPUTS = [*FIXTURES.values(), 'scripts/public-conformance.py',
                  'scripts/public-conformance.Dockerfile', 'scripts/test_public_conformance.py']
CARGO_INPUTS = ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml']
LEGACY_DATA = {f'src/bench/data/{name}.json' for name in
               ['locomo1', 'locomo10', 'locomo_conv30', 'locomo_conv41', 'locomo_first']}
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


def git(root, *args):
    return subprocess.check_output(['git', *args], cwd=root)


def checked_file(root, name):
    relative = Path(name)
    if relative.is_absolute() or '..' in relative.parts or str(relative) != name:
        raise RuntimeError(f'input must be a normalized repository-relative file: {name}')
    path = root / name
    if not path.is_file() or any((root / Path(*relative.parts[:i])).is_symlink()
                                for i in range(1, len(relative.parts) + 1)):
        raise RuntimeError(f'missing/nonregular input: {name}')
    if not path.resolve().is_relative_to(root.resolve()):
        raise RuntimeError(f'input escapes source root: {name}')
    return path


def snapshot(source_root, destination, extra=(), expected_commit=None):
    """Single-package tracked source, explicit assets, separately owned harness overlay."""
    source_root = source_root.resolve()
    top = Path(git(source_root, 'rev-parse', '--show-toplevel').decode().strip()).resolve()
    if source_root != top:
        raise RuntimeError('--source-root must be a Git checkout root')
    commit = git(source_root, 'rev-parse', 'HEAD').decode().strip()
    if expected_commit and git(source_root, 'rev-parse', '--verify', expected_commit + '^{commit}').decode().strip() != commit:
        raise RuntimeError('source HEAD differs from --source-commit')
    tracked = {n.decode() for n in git(source_root, 'ls-files', '-z').split(b'\0') if n}
    cargo = tomllib.loads(checked_file(source_root, 'Cargo.toml').read_text())
    if 'workspace' in cargo or 'package' not in cargo:
        raise RuntimeError('workspace snapshots require an explicit future policy')
    def contains_path(value):
        return isinstance(value, dict) and ('path' in value or any(contains_path(v) for v in value.values()))
    def check_dependencies(value):
        if isinstance(value, dict):
            for key, child in value.items():
                if key in ('dependencies', 'dev-dependencies', 'build-dependencies', 'patch', 'replace') and contains_path(child):
                    raise RuntimeError('local path dependencies require an explicit future policy')
                check_dependencies(child)
    check_dependencies(cargo)
    roots = {'src', 'tests', 'examples', 'benches'}
    names = set(CARGO_INPUTS) | set(extra)
    for kind in ('lib', 'bin', 'example', 'test', 'bench'):
        targets = [cargo[kind]] if kind == 'lib' and kind in cargo else cargo.get(kind, [])
        for target in targets:
            path = target.get('path')
            if path:
                checked_file(source_root, path)
                names.add(path)
                # Custom targets can have sibling Rust modules/assets.
                parent = Path(path).parent
                if parent != Path('.'):
                    roots.add(str(parent))
    build = cargo['package'].get('build', 'build.rs' if 'build.rs' in tracked else False)
    if build:
        names.add(build)
        if Path(build).parent != Path('.'):
            roots.add(str(Path(build).parent))
    excluded = {}
    for name in tracked:
        if any(name.startswith(root + '/') for root in roots):
            if name.endswith('.rs'):
                names.add(name)
            elif name in LEGACY_DATA and name not in extra:
                excluded[name] = 'legacy external dataset; not invoked by this synthetic track'
            elif name not in extra:
                raise RuntimeError(f'undeclared target asset; add --source-input {name}')
    for name in names:
        if name not in tracked:
            raise RuntimeError(f'required runtime input is not tracked: {name}')
        if name in HARNESS_INPUTS:
            raise RuntimeError(f'runtime input collides with harness overlay: {name}')
        checked_file(source_root, name)
    # Literal compile-time includes must be explicit, even in excluded data roots.
    # Computed include expressions/build-script inputs remain compiler checks;
    # no static scanner claims to infer arbitrary runtime I/O.
    for name in sorted(names):
        if name.endswith('.rs'):
            code = (source_root / name).read_text()
            for relative in re.findall(r'include(?:_str|_bytes)?!\s*\(\s*"([^"\n]+)"', code):
                included = (source_root / name).parent / relative
                try:
                    required = str(included.resolve().relative_to(source_root))
                except ValueError:
                    raise RuntimeError(f'include escapes source root: {name}: {relative}')
                if required not in names:
                    raise RuntimeError(f'undeclared include; add --source-input {required}')
    patch = git(source_root, 'diff', 'HEAD', '--', *sorted(names))
    if expected_commit and patch:
        raise RuntimeError('frozen runtime inputs have tracked modifications')
    destination.mkdir(parents=True, exist_ok=False)
    hashes, origins, modes = {}, {}, {}
    for name in sorted(names | set(HARNESS_INPUTS)):
        origin = ROOT if name in HARNESS_INPUTS else source_root
        content = checked_file(origin, name).read_bytes()
        if expected_commit and name in names and content != git(source_root, 'show', f'{commit}:{name}'):
            raise RuntimeError(f'frozen runtime file differs from commit: {name}')
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content)
        target.chmod(checked_file(origin, name).stat().st_mode & 0o777)
        modes[name] = oct(target.stat().st_mode & 0o777)
        hashes[name] = sha(target)
        origins[name] = 'harness' if name in HARNESS_INPUTS else 'runtime'
    if git(source_root, 'rev-parse', 'HEAD').decode().strip() != commit or git(source_root, 'diff', 'HEAD', '--', *sorted(names)) != patch:
        raise RuntimeError('runtime changed during source snapshot')
    return {'sourceCommit': commit, 'sourceRoot': str(source_root), 'sourceFilesSha256': hashes,
            'sourceFileOrigins': origins, 'sourceFileModes': modes, 'excludedTrackedInputs': excluded,
            'explicitSourceInputs': sorted(extra), 'frozenSourceCommitRequired': bool(expected_commit),
            'harnessCommit': git(ROOT, 'rev-parse', 'HEAD').decode().strip()}, patch


def host(args):
    if not args.cache or not args.build_cache:
        raise SystemExit('--cache and --build-cache required; copied, never shared live')
    scratch = Path(tempfile.mkdtemp(prefix='recalld-public-conformance-', dir='/tmp'))
    output = scratch / 'output'
    output.mkdir()
    print(f'Scratch: {scratch}', flush=True)
    source, patch = snapshot(args.source_root or ROOT, scratch / 'source', args.source_input, args.source_commit)
    shutil.copytree(args.cache / 'registry', scratch / 'cargo/registry')
    shutil.copytree(args.build_cache, scratch / 'target')
    image = json.loads(subprocess.check_output(['docker', 'image', 'inspect', args.image]))[0]
    (output / 'source.patch').write_bytes(patch)
    manifest = {'manifestVersion': 1, 'track': 'public-v1-characterization',
        **source, 'dirtyPatchSha256': sha(output / 'source.patch'),
        'imageId': image['Id'], 'imageDigests': image.get('RepoDigests', []),
        'fixtureVersion': args.fixture_version, 'fixturePath': FIXTURES[args.fixture_version],
        'fixtureSha256': source['sourceFilesSha256'][FIXTURES[args.fixture_version]], 'protocolVersion': PROTOCOL, 'contractVersion': 'legacy-v1',
        'clock': 'actual wall clock; no public manual clock', 'snapshots': 'A1 interfaces unavailable',
        'validationTestThreads': args.test_threads,
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
python3 scripts/test_public_conformance.py > /work/output/runner-tests.log 2>&1
cat /sys/fs/cgroup/cpu.max > /work/output/cgroup-cpu.txt
cat /sys/fs/cgroup/memory.max > /work/output/cgroup-memory.txt
cargo metadata --offline --locked --no-deps --format-version 1 > /work/output/cargo-targets.json
cargo fmt --all -- --check > /work/output/fmt.log 2>&1
cargo test --offline --locked --all-features --lib --bins --tests --examples -j 1 > /work/output/tests.log 2>&1
cargo clippy --offline --locked --all-targets --all-features > /work/output/clippy.log 2>&1
cargo build --offline --locked --bin recalld > /work/output/build.log 2>&1
python3 scripts/public-conformance.py --inside --fixture-version FIXTURE_VERSION
'''.replace('FIXTURE_VERSION', str(args.fixture_version))]
    if args.test_threads is not None:
        command[command.index(image['Id']):command.index(image['Id'])] = ['--env', f'RUST_TEST_THREADS={args.test_threads}']
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
            'forget_memory': ('DELETE', f'/memories/{memory}'),
            'find_similar_memories': ('POST', f'/similar/{memory}')}
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


def vector_window(client, fixture, ids, mappings, output, phase):
    """Actual public vector paths; no private index injection or provider bypass stub."""
    extension = fixture['extension']
    source = ids[extension['similaritySource']]
    neighbor = ids[extension['expectedNeighbor']]
    repeats = extension['initialRepeats'] if phase == 'initial' else extension['restartRepeats']
    before = {name: client.get(memory) for name, memory in ids.items()}
    files_before = files_at(client.directory / 'data')
    provider_before = (output / 'embedding-requests.jsonl').read_text().splitlines()
    results, rejected = [], []
    for _ in range(repeats):
        response = client.good('find_similar_memories', {'id': source, 'namespace': 'default',
            'recordAccess': False, 'limit': 20, 'minScore': 0.1})
        hits = response['hits'] if client.mode == 'rest' else response['memories']
        returned = [h['memory']['id'] if client.mode == 'rest' else h['id'] for h in hits]
        assert returned and returned[0] == neighbor and source not in returned
        assert ids['foreign'] not in returned and len(returned) == len(set(returned))
        assert all(before[next(name for name, memory in ids.items() if memory == i)]['namespace'] == 'default'
                   for i in returned)
        if phase == 'restart':
            assert ids['birch'] not in returned
        result = {'similarityRanks': [mappings[i][0] for i in returned],
            'similarityWireNamespaces': [h['memory']['namespace'] if client.mode == 'rest' else h['namespace'] for h in hits]}
        if fixture['fixtureVersion'] >= 3:
            result['similarityScoreThresholdPassed'] = all(h['score'] >= 0.1 for h in hits)
            result['similarityNamespacesPassed'] = all(n == 'default' for n in result['similarityWireNamespaces'])
        if client.mode == 'rest':
            status, response, _ = client.http('POST', '/search', {'embedding': extension['vectorQuery'],
                'namespace': 'default', 'recordAccess': False, 'limit': 20})
            assert status == 200, (status, response)
            returned = [h['memory']['id'] for h in response['hits']]
            assert returned[:2] == [source, neighbor] and ids['foreign'] not in returned
            assert all(h['memory']['namespace'] == 'default' for h in response['hits'])
            if phase == 'restart':
                assert ids['birch'] not in returned
            result['vectorOnlyRanks'] = [mappings[i][0] for i in returned]
        results.append(result)
    ok, response = client.call('find_similar_memories', {'id': source, 'namespace': 'foreign',
        'recordAccess': False, 'limit': 20})
    assert not ok, response
    rejected.append('foreign-namespace-assertion')
    if client.mode == 'rest':
        for name, vector in [('wrong-dimension', [1, 0]), ('zero-norm', [0, 0, 0, 0])]:
            status, response, _ = client.http('POST', '/search', {'embedding': vector,
                'namespace': 'default', 'recordAccess': False, 'limit': 20})
            assert 400 <= status < 500, (status, response)
            rejected.append(name)
    if fixture['fixtureVersion'] >= 3 and client.mode == 'rest':
        strength_results = {}
        for minimum in [0.5, 1.1]:
            status, response, _ = client.http('POST', '/search', {'embedding': extension['vectorQuery'],
                'namespace': 'default', 'recordAccess': False, 'limit': 20, 'minStrength': minimum})
            assert status == 200, (status, response)
            found = [h['memory']['id'] for h in response['hits']]
            assert (source in found and neighbor in found) if minimum == 0.5 else found == []
            strength_results[str(minimum)] = [mappings[i][0] for i in found]
        save(client.out / f'strength-filter-{phase}.json', strength_results)
    time.sleep(.2)  # diagnostic quiet window; not A1 readiness
    after = {name: client.get(memory) for name, memory in ids.items()}
    files_after = files_at(client.directory / 'data')
    provider_after = (output / 'embedding-requests.jsonl').read_text().splitlines()
    save(client.out / f'vector-window-{phase}.json', {'fixtureVersion': fixture['fixtureVersion'], 'before': before,
        'after': after, 'filesBefore': files_before, 'filesAfter': files_after,
        'excludedFromDurableComparison': ['fts.db-shm'], 'quietMs': 200,
        'providerRequestCountBefore': len(provider_before), 'providerRequestCountAfter': len(provider_after),
        'results': results, 'rejectedCases': rejected})
    assert before == after and durable(files_before) == durable(files_after), 'vector/similar false reads changed state'
    assert provider_before == provider_after, 'explicit vectors/similarity invoked embedding provider'
    assert all(r == results[0] for r in results), 'repeated vector/similarity ranks changed'
    return {'similarityCalls': repeats, 'vectorOnlyCalls': repeats if client.mode == 'rest' else 0,
        'rejectedCases': rejected, 'publicAndDurableStateUnchanged': True, 'embeddingProviderCalls': 0,
        **results[0]}


def exposure_window(client, fixture, ids, output):
    """Declared true-mode arm; public observations, not A1 receipt inspection."""
    cases, violations = [], []
    for source_name, neighbor_name, namespace in fixture['exposureChecks']['scopePairs']:
        source, neighbor = ids[source_name], ids[neighbor_name]
        for case in fixture['exposureChecks']['cases']:
            before = {name: client.get(i) for name, i in ids.items()}
            files_before = files_at(client.directory / 'data')
            time.sleep(1.1)  # public MCP timestamps can have second precision
            args = {'id': source, 'recordAccess': True, 'limit': 20}
            # Omitted namespace must use source scope, even for foreign records.
            if case == 'threshold':
                args['minScore'] = 0.1
            elif case == 'limit':
                args['limit'] = 1
            else:
                args['minScore'] = 0.99
            response = client.good('find_similar_memories', args)
            hits = response['hits'] if client.mode == 'rest' else response['memories']
            returned = [h['memory']['id'] if client.mode == 'rest' else h['id'] for h in hits]
            expected = [] if case == 'empty' else [neighbor]
            deadline = time.monotonic() + 5
            while True:
                after = {name: client.get(i) for name, i in ids.items()}
                changed = [ids[name] for name in ids if after[name] != before[name]]
                if not expected or all(i in changed for i in expected) or time.monotonic() >= deadline:
                    break
                time.sleep(.02)
            time.sleep(.2)  # bounded legacy async drain observation, not a barrier
            after = {name: client.get(i) for name, i in ids.items()}
            changed = [ids[name] for name in ids if after[name] != before[name]]
            files_after = files_at(client.directory / 'data')
            failures = []
            if returned != expected:
                failures.append('similarity-result-ids')
            if any((h['memory'] if client.mode == 'rest' else h)['namespace'] != namespace for h in hits):
                failures.append('canonical-namespace')
            if set(changed) != set(expected):
                failures.append('only-returned-ids-exposed')
            if not expected and durable(files_before) != durable(files_after):
                failures.append('empty-result-durable-write')
            for name, memory in ids.items():
                if memory in expected:
                    if after[name]['lastAccessedAt'] == before[name]['lastAccessedAt']:
                        failures.append('returned-anchor-not-updated')
                    if after[name]['stability'] != before[name]['stability']:
                        failures.append('exposure-changed-stability')
            item = {'case': case, 'scope': namespace, 'request': args, 'response': response,
                'expectedIds': expected, 'returnedIds': returned, 'changedIds': changed,
                'before': before, 'after': after, 'filesBefore': files_before, 'filesAfter': files_after,
                'failures': failures}
            cases.append(item)
            violations.extend({'case': case, 'scope': namespace, 'failure': f} for f in failures)
    save(client.out / 'exposure-window.json', {'cases': cases, 'violations': violations,
        'limitation': 'actual clock; bounded async observation; public anchors not full canonical event/receipt inspection'})
    # Exposure state, including observed anchors, survives graceful restart.
    before_restart = {name: client.get(i) for name, i in ids.items()}
    client.stop()
    client.start()
    after_restart = {name: client.get(i) for name, i in ids.items()}
    save(client.out / 'exposure-restart.json', {'before': before_restart, 'after': after_restart})
    assert before_restart == after_restart
    return {'cases': len(cases), 'violations': violations, 'restartPublicStateEqual': True}


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
        vector_initial = vector_window(client, fixture, ids, mappings, output, 'initial') if fixture['fixtureVersion'] >= 2 else None
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
        if fixture['fixtureVersion'] >= 2:
            assert not ok and not changed and prior == updated, 'fixture v2 requires strict quality rejection'
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
        vector_restart = vector_window(client, fixture, ids, mappings, output, 'restart') if fixture['fixtureVersion'] >= 2 else None
        summary = {'transport': mode, 'replay': repeat, 'freshSources': len(ids),
            'falseReadPublicStateUnchanged': True, 'falseReadDurableFilesUnchanged': True,
            'defaultScopeMatchesExplicit': True, 'rejectedRequestsWithoutDurableFileChanges': len(bad_inputs),
            'grade4PreservesStabilityAtZeroDecay': True, 'grade1WeakensWithoutDeleting': True, 'deleteContentAbsent': True, 'restartPublicStateEqual': True,
            'extractionEvidenceRetention': coverage(list(mappings), mappings, fixture['evidenceSources']),
            'retrievalSourceRecallAt20': coverage(ranks[:20], mappings, fixture['evidenceSources']),
            'normalizedRanks': [mappings[i][0] for i in ranks],
            'normalizedRanksAfterDeleteAndRestart': [mappings[i][0] for i in ranks_before_restart], 'knownConformanceViolations': violations,
            'semanticQuality': 'not measured'}
        if fixture['fixtureVersion'] >= 2:
            summary['fixtureVersion'] = fixture['fixtureVersion']
            summary['vectorWindows'] = {'initial': vector_initial, 'restart': vector_restart}
        if fixture['fixtureVersion'] >= 3:
            for phase, window in summary['vectorWindows'].items():
                for check in ['similarityScoreThresholdPassed', 'similarityNamespacesPassed']:
                    if not window[check]:
                        violations.append({'case': phase, 'failure': check})
            exposure = exposure_window(client, fixture, ids, output)
            summary['exposure'] = exposure
            violations.extend(exposure['violations'])
        save(out / 'summary.json', summary)
        return summary
    finally:
        client.stop()


def inside(fixture_version):
    assert Path('/.dockerenv').is_file() and ROOT == Path('/work/source')
    fixture = json.loads((ROOT / FIXTURES[fixture_version]).read_text())
    assert fixture['fixtureVersion'] == fixture_version
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
        save(output / 'summary.json', {'status': 'characterization-complete', 'fixtureVersion': fixture_version, 'episodes': summaries,
            'conformancePassed': not any(s['knownConformanceViolations'] for s in summaries),
            'embeddingRequests': len(requests), 'expectedFailClosedMisses': len(misses), 'freshReplaysPerTransport': 3,
            'claim': 'public-v1 diagnostic; no v2/A1/A6 completion'})
    finally:
        stub.shutdown()
        stub.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--fixture-version', type=int, choices=sorted(FIXTURES), default=1, help='fixture revision; both use legacy API v1')
    parser.add_argument('--source-root', type=Path, help='runtime Git checkout; harness stays here')
    parser.add_argument('--source-commit', help='require exact clean runtime HEAD and copied bytes')
    parser.add_argument('--source-input', action='append', default=[], help='additional exact tracked runtime file; repeat for assets')
    parser.add_argument('--cache', type=Path)
    parser.add_argument('--build-cache', type=Path)
    parser.add_argument('--image', default='recalld-ci:1.94')
    parser.add_argument('--test-threads', type=int, help='Rust test concurrency; recorded in manifest, does not alter replay count')
    args = parser.parse_args()
    if args.test_threads is not None and args.test_threads < 1:
        parser.error('--test-threads must be positive')
    if args.source_root and not args.source_commit:
        parser.error('--source-root requires --source-commit to freeze the runtime')
    inside(args.fixture_version) if args.inside else host(args)


if __name__ == '__main__':
    main()
