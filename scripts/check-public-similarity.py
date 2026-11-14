#!/usr/bin/env python3
"""Fail a public fixture-v3 regression on any recorded contract violation."""
import hashlib
import json
from pathlib import Path
import sys


def check(root):
    manifest = json.loads((root / 'manifest.json').read_text())
    for name, expected in manifest['outputsSha256'].items():
        assert hashlib.sha256((root / name).read_bytes()).hexdigest() == expected, name
    summary = json.loads((root / 'summary.json').read_text())
    assert summary['fixtureVersion'] == 3
    episodes = summary['episodes']
    assert {(e['transport'], e['replay']) for e in episodes} == {
        (mode, repeat) for mode in ['stdio', 'http-mcp', 'rest'] for repeat in [1, 2, 3]}
    assert len(episodes) == 9
    failures = []
    for episode in episodes:
        name = f"{episode['transport']}-{episode['replay']}"
        failures.extend(f"{name}: {v}" for v in episode['knownConformanceViolations'])
        exposure = json.loads((root / name / 'exposure-window.json').read_text())
        assert len(exposure['cases']) == 6
        for case in exposure['cases']:
            if case['returnedIds'] != case['expectedIds'] or set(case['changedIds']) != set(case['expectedIds']):
                failures.append(f"{name}: raw {case['case']}/{case['scope']} IDs differ")
        restart = json.loads((root / name / 'exposure-restart.json').read_text())
        assert restart['before'] == restart['after'], name
    if failures or not summary['conformancePassed']:
        raise SystemExit('\n'.join(failures) or 'conformancePassed false')
    print('PASS: nine episodes; 54 true-mode cases; preserved output hashes and restart state')


if __name__ == '__main__':
    check(Path(sys.argv[1]))
