#!/usr/bin/env python3
"""Post-run validation of archived events from the frozen A5 workload, not a rerun."""
import collections
import hashlib
import json
from pathlib import Path
import sys

SEEDS = [7, 19, 41, 73, 101]
STAGES = set('parse_namespace embedding canonical_read strength_projection hard_filter vector fts fusion entity candidate_assembly graph candidate_filter effective_strength rif rank graph_filter_rank_supersession response exposure residual'.split())
DURATIONS = collections.Counter({'storage.read.queue': 3, **{s: 1 for s in 'storage.read.wait canonical.namespace_read storage.read.hold vector.lock_wait vector.lock_hold fts.lock_wait fts.lock_hold engine.search'.split()}})
FIELDS = {
    'duration': {'stage', 'outcome', 'elapsed_ns'},
    'exclusive_pipeline': {'stage', 'elapsed_ns'},
    'canonical_read': {'examined', 'records', 'encoded_record_bytes'},
    'eligibility': {'examined', 'eligible', 'rejected'},
    'vector_scan': {'examined', 'dot_products', 'admitted'},
    'fts_legacy_rows': {'examined', 'returned', 'sqlite_internal_work_known'},
    'graph_work': {'known'},
    'selection': {'vector_admitted', 'fts_admitted', 'entity_admitted', 'union_candidates', 'pre_limit', 'returned', 'receipt_count'},
    'pipeline_accounting': {'elapsed_ns', 'accounted_ns'},
    'final_admission': {'accepted', 'receipt_count'},
}


def enabled(path, summary, manifest):
    expected = manifest['warmup'] + manifest['samples'] + summary['throughput_completed']
    samples = [json.loads(line)['latency_ns'] for line in (path.parent / 'samples.jsonl').read_text().splitlines()]
    assert len(samples) == manifest['samples']
    digest = hashlib.sha256()
    requests = events = 0
    fields = collections.defaultdict(list)
    stage_sum = collections.Counter()
    adapter_sum = collections.Counter()
    counter_ranges = {}
    with path.open('rb') as source:
        for line in source:
            digest.update(line)
            row = json.loads(line)
            assert set(row) == {'level', 'fields', 'target', 'span', 'spans'}, 'unexpected envelope field'
            assert row['level'] == 'TRACE' and row['target'] == 'recalld::performance'
            span = {'event_version': 1, 'operation': 'engine.search', 'record_access': False,
                    'trace_id': requests + 1, 'name': 'performance_request'}
            assert row['span'] == span and row['spans'] == [span], 'orphan/reordered/incorrect parent span'
            value = row['fields']; kind = value['kind']
            assert kind in FIELDS and set(value) == FIELDS[kind] | {'kind'}, 'unexpected payload field'
            for key, item in value.items():
                if key not in {'kind', 'stage', 'outcome'}:
                    assert isinstance(item, (int, bool)) and item >= 0, 'unexpected payload type'
            fields[kind].append(value)
            events += 1
            if kind == 'duration':
                assert value['stage'] in DURATIONS
                assert value['outcome'] == ('released' if value['stage'].endswith(('hold', '.hold')) else 'ok')
            if kind == 'exclusive_pipeline':
                assert value['stage'] in STAGES
            if kind != 'duration' or value['stage'] != 'engine.search':
                continue
            # One completed request; no sample/event loss can be hidden by a mean.
            assert set(fields) == set(FIELDS)
            for key in FIELDS.keys() - {'duration', 'exclusive_pipeline'}:
                assert len(fields[key]) == 1, key
            exclusive = {v['stage']: v['elapsed_ns'] for v in fields['exclusive_pipeline']}
            assert len(fields['exclusive_pipeline']) == len(STAGES) and set(exclusive) == STAGES
            assert collections.Counter(v['stage'] for v in fields['duration']) == DURATIONS
            accounting = fields['pipeline_accounting'][0]
            assert sum(exclusive.values()) == accounting['accounted_ns'] == accounting['elapsed_ns']
            root_ns = value['elapsed_ns']
            assert accounting['elapsed_ns'] <= root_ns
            duration = {v['stage']: v['elapsed_ns'] for v in fields['duration']}
            assert all(v['elapsed_ns'] <= root_ns for v in fields['duration'])
            assert duration['canonical.namespace_read'] <= duration['storage.read.hold'] <= exclusive['canonical_read']
            assert duration['vector.lock_hold'] <= exclusive['vector']
            assert duration['fts.lock_hold'] <= exclusive['fts']
            assert fields['canonical_read'][0] == dict(kind='canonical_read', examined=1000, records=1000, encoded_record_bytes=93000)
            assert fields['eligibility'][0] == dict(kind='eligibility', examined=1000, eligible=1000, rejected=0)
            assert fields['vector_scan'][0] == dict(kind='vector_scan', examined=10000, dot_products=1000, admitted=160)
            assert fields['fts_legacy_rows'][0] == dict(kind='fts_legacy_rows', examined=160, returned=160, sqlite_internal_work_known=False)
            assert fields['graph_work'][0] == dict(kind='graph_work', known=False)
            assert fields['final_admission'][0] == dict(kind='final_admission', accepted=True, receipt_count=0)
            selection = fields['selection'][0]
            assert selection['vector_admitted'] == selection['fts_admitted'] == 160
            assert selection['entity_admitted'] == selection['receipt_count'] == 0
            assert selection['returned'] == 20
            assert 160 <= selection['union_candidates'] == selection['pre_limit'] <= 320
            for name in ['union_candidates', 'pre_limit']:
                old = counter_ranges.setdefault(name, [selection[name], selection[name]])
                old[0] = min(old[0], selection[name]); old[1] = max(old[1], selection[name])
            sample_index = requests - manifest['warmup']
            if 0 <= sample_index < manifest['samples']:
                assert root_ns <= samples[sample_index], 'root time exceeds caller latency including sink flush'
                stage_sum.update(exclusive)
                stage_sum['pipeline_total'] += accounting['elapsed_ns']
                stage_sum['root_total'] += root_ns
                stage_sum['caller_total'] += samples[sample_index]
                for item in fields['duration']:
                    adapter_sum[item['stage']] += item['elapsed_ns']
            requests += 1
            fields.clear()
    assert not fields and requests == expected and requests > 0, 'missing/truncated enabled traces'
    assert events == requests * 38, 'wrong event count'
    return {'valid': True, 'expected_requests': expected, 'observed_requests': requests,
            'events': events, 'events_per_request': 38, 'sha256': digest.hexdigest(),
            'bytes': path.stat().st_size, 'counter_ranges': counter_ranges,
            'sample_stage_mean_ns': {k: v / len(samples) for k, v in stage_sum.items()},
            'sample_nested_duration_mean_ns': {k: v / len(samples) for k, v in adapter_sum.items()},
            'checks': ['exact_nonzero_request_and_event_counts', 'single_engine_parent_worker_continuity',
                       'exclusive_partition_and_nested_bounds', 'root_bounded_by_caller_time',
                       'exact_fixture_counters', 'strict_payload_allowlist_no_content_or_memory_ids']}


def main():
    root = Path(sys.argv[1])
    manifest = json.loads((root / 'manifest.json').read_text())
    result = {'source': manifest['candidate_source'], 'anchor': manifest['anchor_source'], 'arms': {}, 'valid': True}
    for seed in SEEDS:
        for arm in 'ADE':
            path = root / f'seed-{seed}-{arm}' / 'raw' / 'events.jsonl'
            try:
                assert json.loads((path.parent.parent / 'exit.json').read_text())['returncode'] == 0
                summary = json.loads((path.parent / 'summary.json').read_text())
                if arm == 'E':
                    data = enabled(path, summary, manifest)
                else:
                    assert path.stat().st_size == 0, 'disabled/anchor emitted events'
                    data = {'valid': True, 'events': 0, 'bytes': 0, 'sha256': hashlib.sha256(b'').hexdigest()}
            except (AssertionError, KeyError, ValueError, OSError) as error:
                data = {'valid': False, 'error': str(error)}
                result['valid'] = False
            result['arms'][f'{seed}-{arm}'] = data
            print(f'{seed}-{arm}: valid={data["valid"]} events={data.get("events")} {data.get("error", "")}', flush=True)
            (root / 'trace-qa.json').write_text(json.dumps(result, indent=2) + '\n')
    if not result['valid']:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
