#!/usr/bin/env bash
set -euo pipefail
variant=${1:?baseline or fixed}
registry=${2:?prepared Cargo registry directory}
target=${3:?scratch Cargo target directory}
evidence=$(cd -- "$(dirname -- "$0")" && pwd)
case "$variant" in
  baseline) revision=211b1bd409570d4cb7fc594ecc90d4fad56a3293 ;;
  fixed) revision=8cbb7344b1c45cc444b32781fa6c52d0c6f88ceb ;;
  *) exit 2 ;;
esac
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
mkdir -p "$scratch/source" "$target"
git archive "$revision" | tar -x -C "$scratch/source"
if [ "$variant" = baseline ]; then cat "$evidence/probes.rs" >> "$scratch/source/src/decay/sweep.rs"; fi
docker run --rm --network none --cpus 2 --memory 4g --cap-drop ALL --security-opt no-new-privileges \
  --mount "type=bind,src=$scratch/source,dst=/source,readonly" \
  --mount "type=bind,src=$registry,dst=/cargo/registry,readonly" \
  --mount "type=bind,src=$target,dst=/target" \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e CARGO_BUILD_JOBS=2 \
  -e RUSTUP_TOOLCHAIN=1.94.1 -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
  recalld-ci:1.94 bash -c 'set -e; cp -R /source /tmp/review; cd /tmp/review; cargo test --offline --locked --lib independent_review_regressions -- --nocapture'
