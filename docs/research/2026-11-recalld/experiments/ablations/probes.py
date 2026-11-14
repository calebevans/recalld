"""Synthetic source-informed MODELS, not the Rust engine. Python stdlib only."""
import collections
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import sqlite3
import statistics
import subprocess
import sys
import tempfile

SEEDS = [7, 19, 41, 73, 101]


def metrics(order, relevant, k):
    returned = order[:k]
    ranks = [order.index(i) + 1 for i in relevant if i in order]
    return dict(recall=len(set(returned) & set(relevant)) / len(relevant),
                fill=len(returned) / k, best_relevant_rank=min(ranks, default=None),
                reciprocal_rank_at_k=next((1 / (n + 1) for n, i in enumerate(returned)
                                          if i in relevant), 0))


def prefilter_probe():
    rows = []
    n, k = 2000, 20
    for seed in SEEDS:
        rng = random.Random(seed)
        for query in range(10):
            scores = [rng.random() for _ in range(n)]
            order = sorted(range(n), key=lambda i: -scores[i])
            for fraction in [0.01, 0.05, 0.20]:
                for layout in ['independent', 'eligible_below_rank_200']:
                    pool = order if layout == 'independent' else order[200:]
                    eligible = set(rng.sample(pool, int(n * fraction)))
                    oracle = [i for i in order if i in eligible][:k]
                    assert len(oracle) == k
                    for fetch in [20, 160, 400, 1600]:
                        for variant in ['postfilter', 'prefilter']:
                            if variant == 'postfilter':
                                loaded = order[:fetch]
                                hits = [i for i in loaded if i in eligible][:k]
                            else:
                                loaded = [i for i in order if i in eligible][:fetch]
                                hits = loaded[:k]
                            assert hits == oracle[:len(hits)]
                            if variant == 'prefilter':
                                assert hits == oracle
                            rows.append(dict(seed=seed, query=query, selectivity=fraction,
                                             layout=layout, fetch_k=fetch, variant=variant,
                                             **metrics(hits, oracle, k), returned_ids=hits,
                                             oracle_ids=oracle,
                                             mean_oracle_rank=statistics.mean(oracle.index(i) + 1 for i in hits) if hits else None,
                                             metadata_loads=len(loaded),
                                             modeled_dot_products=n if variant == 'postfilter' else len(eligible)))
    return dict(n=n, limit=k, queries_per_seed=10, rows=rows)


def resolve(candidates, replacement_score, eligible=True, load_failed=False, inherit_existing=False):
    """Stage 8c one terminal replacement. Scores are already computed/boosted."""
    out = dict(candidates)
    inherited = max((score for name, score in candidates if name.startswith('old')), default=0)
    if 'new' in out:
        if inherit_existing:
            out['new'] = max(out['new'], inherited)
    elif eligible and not load_failed and replacement_score is not None:
        out['new'] = max(replacement_score, inherited)
    else:
        return out
    return {name: score for name, score in out.items() if not name.startswith('old')}


def supersession_probe():
    rows = []
    for seed in SEEDS:
        rng = random.Random(seed)
        base = [('old', rng.uniform(.9, .95))] + [(f'f{i}', rng.uniform(.3, .8)) for i in range(10)]
        new_direct = rng.uniform(.15, .25)
        for present in [False, True]:
            candidates = base + ([('new', new_direct)] if present else [])
            for fix in [False, True]:
                after = resolve(candidates, .09, inherit_existing=fix)
                order = sorted(after, key=lambda i: -after[i])
                rows.append(dict(seed=seed, replacement_already_candidate=present,
                                 variant='inherit_existing_counterfactual' if fix else 'source_model',
                                 input_scores=dict(candidates), output_scores=after, order=order,
                                 **metrics(order, ['new'], 5)))
                assert order.count('new') == 1 and 'old' not in order
                assert order.index('new') == (10 if present and not fix else 0)
        for kwargs in [dict(eligible=False), dict(load_failed=True), dict(replacement_score=None)]:
            args = dict(replacement_score=.09) | kwargs
            assert resolve(base, **args) == dict(base)
        assert resolve([('old1', .5), ('old2', .9)], .09) == {'new': .9}
    return dict(limit=5, rows=rows, guards=['ineligible', 'load_failure', 'missing', 'many_to_one'])


def plasticity(a):
    if a < .10:
        return 1.0
    if a < .45:
        return 1 - .15 * (1 - ((a - .275) / .175) ** 2)
    return 1 + .05 * min((a - .45) / .55, 1)


def composite(branch, signal, effective_r):
    # Fixed entity overlap=.5, FTS absent; entity recall=.5.
    if branch == 'direct':
        return .90 * signal + .05 * .5
    if branch == 'graph':
        return .70 * signal + .30 * .5
    if branch == 'entity':
        return .40 * .5 + .20 * .5 + .10 * effective_r
    return .10 * effective_r


def rif_probe():
    rows = []
    assert math.isclose(plasticity(.275), .85)
    assert plasticity(.05) == 1 and plasticity(.7) > 1
    for seed in SEEDS:
        rng = random.Random(seed)
        strengths = {'target': .9} | {f'leaf{i}': .8 for i in range(6)} | {f'other{i}': rng.uniform(.70, .85) for i in range(8)}
        # One undirected associative edge per leaf. No parallel edges or self loops.
        adjacency = {i: [] for i in strengths}
        for i in range(6):
            adjacency['target'].append(f'leaf{i}')
            adjacency[f'leaf{i}'].append('target')
        pooled = [j for i in strengths for j in adjacency[i]]
        for branch in ['direct', 'graph', 'entity', 'metadata']:
            for variant in ['off', 'as_wired', 'dedup_neighbors', 'dedup_plus_cap']:
                effective = dict(strengths)
                events = collections.Counter()
                neighbors = list(dict.fromkeys(pooled)) if variant.startswith('dedup') else pooled
                if variant != 'off':
                    for source in strengths:
                        scored = [(j, .9 * .3 * strengths[j]) for j in neighbors if j in adjacency[source]]
                        for target, activation in sorted(scored, key=lambda x: -x[1])[:100]:
                            factor = plasticity(activation)
                            if factor < 1:
                                events[target] += 1
                                effective[target] *= factor
                                if variant == 'dedup_plus_cap':
                                    effective[target] = max(effective[target], strengths[target] * .75)
                scores = {}
                for name in strengths:
                    # Metadata-only has only metadata candidates; other fixtures
                    # use low-scoring direct leaves as retrieved triggers.
                    scores[name] = composite('direct', .1, effective[name]) if name.startswith('leaf') and branch != 'metadata' else composite(branch, strengths[name], effective[name])
                order = sorted(scores, key=lambda i: -scores[i])
                assert events['target'] == {'off': 0, 'as_wired': 36, 'dedup_neighbors': 6, 'dedup_plus_cap': 6}[variant]
                if branch in ['direct', 'graph']:
                    assert order[0] == 'target'
                rows.append(dict(seed=seed, branch=branch, variant=variant, order=order,
                                 scores=scores, input_strengths=strengths, effective_r=effective, suppression_events=dict(events),
                                 target_cumulative_multiplier=effective['target'] / strengths['target'],
                                 **metrics(order, ['target'], 5)))
    return dict(limit=5, edge_weight=.9, gamma=.3, max_neighbors=100, rows=rows)


def sqlite_child(db, mode, cut):
    con = sqlite3.connect(db)
    con.execute('PRAGMA synchronous=FULL')
    if mode == 'transaction':
        con.execute('BEGIN IMMEDIATE')
    if cut == 0:
        os._exit(73)
    for step, table in enumerate(['memory', 'tag_projection', 'edge_projection'], 1):
        con.execute(f'INSERT INTO {table} VALUES (1, ?)', ('synthetic',))
        if mode == 'separate_commits':
            con.commit()
        if cut == step:
            os._exit(73)
    if mode == 'transaction':
        con.commit()
    # cut 4 explicitly exercises abrupt exit after commit.
    os._exit(73)


def atomicity_probe():
    rows = []
    for repeat in range(3):
        for mode in ['transaction', 'separate_commits']:
            for cut in range(5):
                with tempfile.TemporaryDirectory(dir='/tmp') as scratch:
                    db = str(Path(scratch) / 'canonical.sqlite')
                    with sqlite3.connect(db) as con:
                        assert con.execute('PRAGMA journal_mode=DELETE').fetchone()[0] == 'delete'
                        for table in ['memory', 'tag_projection', 'edge_projection']:
                            con.execute(f'CREATE TABLE {table} (id INTEGER PRIMARY KEY, value TEXT NOT NULL)')
                    proc = subprocess.run([sys.executable, __file__, '--sqlite-child', db, mode, str(cut)])
                    assert proc.returncode == 73
                    with sqlite3.connect(db) as con:
                        counts = [con.execute(f'SELECT COUNT(*) FROM {table}').fetchone()[0] for table in ['memory', 'tag_projection', 'edge_projection']]
                        integrity = con.execute('PRAGMA integrity_check').fetchone()[0]
                    expected = ([1] * 3 if cut == 4 else [0] * 3) if mode == 'transaction' else [int(i < cut) for i in range(3)]
                    assert counts == expected and integrity == 'ok'
                    rows.append(dict(repeat=repeat, mode=mode, interrupt_after_step=cut,
                                     child_exit=proc.returncode, counts=counts,
                                     consistent=len(set(counts)) == 1, integrity=integrity))
    return dict(rows=rows, tables=['memory', 'tag_projection', 'edge_projection'])


def audit_benchmarks():
    rows = []
    for path in sorted(Path('benchmarks').glob('*/bench_results.json')):
        data = json.loads(path.read_bytes())
        conversations = data['per_conversation']
        questions = [q for c in conversations for q in c['questions']]
        actual_correct = sum(q['correct'] for q in questions)
        checks = dict(qa_matches=len(questions) == data['total_qa'],
                      correct_matches=actual_correct == data['total_correct'],
                      conversations_match=len(conversations) == data['total_conversations'],
                      conversation_counts_match=all(len(c['questions']) == c['qa_total'] and sum(q['correct'] for q in c['questions']) == c['qa_correct'] for c in conversations),
                      category_counts_match=all(sum(q['category'] == cat for q in questions) == stats['total'] and sum(q['correct'] for q in questions if q['category'] == cat) == stats['correct'] for cat, stats in data['categories'].items()))
        # Never emit question, answer, conversation ID, or other narrative content.
        rows.append(dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
                         **{key: data[key] for key in ['total_qa', 'total_correct', 'total_conversations', 'model', 'ingest_model', 'judge_model', 'skip_adversarial', 'stress_test', 'top_k', 'avg_retrieval_ms']},
                         recomputed_accuracy=actual_correct / len(questions), checks=checks,
                         question_field_names=sorted(set().union(*(q.keys() for q in questions))),
                         categories=data['categories']))
    assert len(rows) == 6
    return rows


def summarize(rows, keys, values):
    groups = collections.defaultdict(list)
    for row in rows:
        groups[tuple(row[k] for k in keys)].append(row)
    return [dict(zip(keys, group)) | dict(n=len(items)) |
            {v: dict(mean=statistics.mean(r[v] for r in items), min=min(r[v] for r in items), max=max(r[v] for r in items)) for v in values}
            for group, items in groups.items()]


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--sqlite-child':
        sqlite_child(sys.argv[2], sys.argv[3], int(sys.argv[4]))
    filtering = prefilter_probe()
    supersession = supersession_probe()
    rif = rif_probe()
    atomicity = atomicity_probe()
    sources = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(Path('src').rglob('*.rs'))}
    output = dict(label='SYNTHETIC MODELS, NOT FULL ENGINE', seeds=SEEDS,
                  runtime=dict(python=sys.version, sqlite=sqlite3.sqlite_version, platform=platform.platform(), machine=platform.machine()),
                  source_sha256=sources,
                  prefilter=filtering, supersession=supersession, rif=rif, atomicity=atomicity,
                  benchmark_audit=audit_benchmarks(),
                  summaries=dict(prefilter=summarize(filtering['rows'], ['selectivity', 'layout', 'fetch_k', 'variant'], ['recall', 'fill', 'metadata_loads', 'modeled_dot_products']),
                                 prefilter_per_seed=summarize(filtering['rows'], ['seed', 'selectivity', 'layout', 'fetch_k', 'variant'], ['recall', 'fill']),
                                 supersession=summarize(supersession['rows'], ['replacement_already_candidate', 'variant'], ['recall', 'fill', 'best_relevant_rank']),
                                 rif=summarize(rif['rows'], ['branch', 'variant'], ['recall', 'fill', 'best_relevant_rank', 'target_cumulative_multiplier'])),
                  checks='all executable assertions passed')
    print(json.dumps(output, indent=2, sort_keys=True, allow_nan=False))
