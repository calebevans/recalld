import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

previous = Path('/tmp/recalld-canonical-harness-test-2ek80er5')
root = Path(tempfile.mkdtemp(prefix='recalld-canonical-harness-supplement-', dir='/tmp'))
out = root / 'output'
out.mkdir()
def sha(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()
manifest = json.loads((previous / 'output/manifest.json').read_text())
for name, expected in manifest['sourceFilesSha256'].items():
    assert sha(previous / 'source' / name) == expected
for name, expected in manifest['outputsSha256'].items():
    assert sha(previous / 'output' / name) == expected
shutil.copyfile('/tmp/recalld-canonical-harness-check-outputs.py', out / 'check-outputs.py')
shutil.copyfile('/tmp/recalld-canonical-2ek-pre-supplement-binaries.json', out / 'prior-binaries.json')
for name in ('run-0', 'run-1', 'run-2', 'probe-disabled', 'probe-enabled'):
    shutil.copytree(previous / 'output' / name, out / name)
script = '''set -eu
python3 scripts/test_offline_evaluation.py > /work/output/runner-tests.log 2>&1
python3 /work/output/check-outputs.py > /work/output/check-outputs.log 2>&1
cargo fmt --all --check > /work/output/fmt.log 2>&1
cargo test --offline --locked --all-features --lib --bins --tests --examples -j1 > /work/output/full-tests.log 2>&1
cargo clippy --offline --locked --all-features --all-targets > /work/output/clippy.log 2>&1
'''
command = ['docker', 'run', '--rm', '--network', 'none', '--cpus', '2', '--memory', '4g',
    '--memory-swap', '4g', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
    '--read-only', '--tmpfs', '/tmp:rw,nosuid,nodev',
    '--mount', f'type=bind,src={previous}/source,dst=/work/source,readonly',
    '--mount', f'type=bind,src={previous}/cargo,dst=/work/cargo',
    '--mount', f'type=bind,src={previous}/target,dst=/work/target',
    '--mount', f'type=bind,src={out},dst=/work/output',
    '-w', '/work/source', '-e', 'HOME=/tmp/home', '-e', 'RUSTUP_TOOLCHAIN=1.94.1',
    '-e', 'CARGO_HOME=/work/cargo', '-e', 'CARGO_TARGET_DIR=/work/target',
    '-e', 'CARGO_BUILD_JOBS=2', '-e', 'CARGO_NET_OFFLINE=true', manifest['imageId'], 'sh', '-c', script]
report = {'sourceCommit': manifest['sourceCommit'], 'sourceFilesSha256': manifest['sourceFilesSha256'],
    'command': command, 'imageId': manifest['imageId'], 'previousManifestSha256': sha(previous / 'output/manifest.json'),
    'driverSha256': sha(Path(__file__)), 'previousRoot': str(previous),
    'targetPolicy': 'Same physical private target and same immutable source from preceding run; no source-root/content switch',
    'status': 'running'}
def save():
    (out / 'manifest.json').write_text(json.dumps(report, indent=2) + '\n')
save()
print(root, flush=True)
result = subprocess.run(command)
report['status'] = 'passed' if result.returncode == 0 else 'failed'
report['exitCode'] = result.returncode
report['outputsSha256'] = {str(p.relative_to(out)): sha(p) for p in out.rglob('*') if p.is_file() and p.name != 'manifest.json'}
save()
print(json.dumps({'root': str(root), 'status': report['status'], 'exitCode': result.returncode}), flush=True)
raise SystemExit(result.returncode)
