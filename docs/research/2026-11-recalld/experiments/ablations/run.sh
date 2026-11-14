#!/usr/bin/env bash
# Run from any directory. No bind mounts, network, pulls, or installed dependencies.
set -euo pipefail
cd "$(dirname "$0")/../../../../.."
out=docs/research/2026-11-recalld/experiments/ablations
image=python@sha256:4209fae09a3e20afd863e528bfddc235da5fd5977983e1996cd54cc9a72e6267
docker image inspect "$image" > "$out/image-inspect.json"
git rev-parse HEAD > "$out/source-commit.txt"
git diff -- src/search src/rif src/bench/locomo.rs > "$out/source-working-tree.diff"
# Input files are streamed into disposable /tmp, never mounted or modified on host.
COPYFILE_DISABLE=1 tar -cf - "$out/probes.py" src/search/*.rs src/rif/*.rs src/bench/locomo.rs benchmarks/*/bench_results.json |
  docker run --rm -i --pull never --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges --pids-limit 64 --memory 512m --cpus 1 \
    --tmpfs /tmp:rw,nosuid,nodev,size=128m --workdir /tmp \
    --env PYTHONDONTWRITEBYTECODE=1 --env PYTHONHASHSEED=0 \
    "$image" sh -ec 'tar --no-same-owner -xf -; python docs/research/2026-11-recalld/experiments/ablations/probes.py' > "$out/results.json"
