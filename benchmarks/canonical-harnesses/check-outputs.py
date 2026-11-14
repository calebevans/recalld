import hashlib
import json
from pathlib import Path
import runpy
import subprocess

root = Path('/work/output')
runner = runpy.run_path('/work/source/scripts/offline-evaluation.py')
stable = runner['verify_replays'](root, 3)
disabled = json.loads((root / 'probe-disabled/summary.json').read_text())
enabled = json.loads((root / 'probe-enabled/summary.json').read_text())
for key in ('result_sha256', 'canonical_state_sha256'):
    assert disabled[key] == enabled[key], key
assert (root / 'probe-disabled/events.jsonl').stat().st_size == 0
events = [json.loads(line) for line in (root / 'probe-enabled/events.jsonl').read_text().splitlines()]
assert events
guard = subprocess.run(['python3', '/work/source/scripts/pipeline-performance.py',
    '--anchor', '/nonexistent-anchor', '--candidate', '/nonexistent-candidate',
    '--root', '/tmp/historical-must-not-exist', '--source', 'fixture',
    '--anchor-source', 'fixture', '--mode', 'characterization'], capture_output=True, text=True)
assert guard.returncode != 0 and 'frozen 16c574b probe' in guard.stderr, guard
assert not Path('/tmp/historical-must-not-exist').exists()
(root / 'historical-driver-refusal.txt').write_text(guard.stderr)
summary = {
    'claim': 'deterministic fixture/result projections only; not deterministic canonical-state/reset/replay',
    'processes': 3, 'identicalArtifacts': stable,
    'ledgerChecks': 'full retained receipts; identity/reference/count/contiguous generations; all semantic fields compared except explicitly checked generated store/operation IDs',
    'perProcessRecords': 100, 'perProcessInsertReceipts': 100,
    'perProcessFinalGeneration': '104', 'falseRecordAndControlChecks': True,
    'probeRecords': 1000, 'probeDimensions': 384, 'probeEnabledEvents': len(events),
    'probeDisabledEvents': 0, 'probeResultsAndCanonicalLogicalHashesEqual': True,
    'probePhysicalHashesAcrossRestart': [disabled['canonical_file_sha256'], enabled['canonical_file_sha256']],
    'probePhysicalHashesAcrossRestartExpectedEqual': False,
    'probePerProcessFalseQueryBytesUnchanged': True,
    'historicalDriverRejectedBeforeEffects': True,
    'performanceGate': False, 'wholeCoreFreezeOrReset': False,
}
(root / 'output-checks.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))
