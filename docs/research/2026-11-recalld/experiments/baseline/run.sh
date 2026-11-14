#!/usr/bin/env bash
set -euo pipefail

# Reproduce the recorded commit in a NEW scratch tree; preserve archived evidence.
ARTIFACT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(git -C "$ARTIFACT_DIR" rev-parse --show-toplevel)
SOURCE_COMMIT=5aa4692c7d326d2ef10712939eb01484a520e891
IMAGE=rust@sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea
SCRATCH_DIR=$(mktemp -d /tmp/recalld-unit-tests-XXXXXX)
mkdir -p "$SCRATCH_DIR"/{source,cargo,target,logs}
printf '%s\n' "$SCRATCH_DIR" > "$SCRATCH_DIR/logs/scratch-path.txt"
printf '%s\n' "$SOURCE_COMMIT" > "$SCRATCH_DIR/logs/source-commit.txt"
git -C "$REPO_ROOT" archive "$SOURCE_COMMIT" > "$SCRATCH_DIR/source.tar"
tar -xf "$SCRATCH_DIR/source.tar" -C "$SCRATCH_DIR/source"

# Only this image pull and dependency fetch use network access.
docker pull "$IMAGE" > "$SCRATCH_DIR/logs/pull.log" 2>&1
docker image inspect "$IMAGE" --format '{{json .RepoDigests}} {{.Id}} {{.Architecture}}' > "$SCRATCH_DIR/logs/image.log"
docker run --rm --network none "$IMAGE" sh -c 'rustc --version --verbose; cargo --version; uname -a' > "$SCRATCH_DIR/logs/environment.log" 2>&1
docker run --rm --network none "$IMAGE" sh -c 'lscpu; getconf _NPROCESSORS_ONLN' > "$SCRATCH_DIR/logs/cpu.log" 2>&1

# No host home, original source, credentials, daemon socket or Docker socket mounts.
mounts=(
  --mount "type=bind,src=$SCRATCH_DIR/source,dst=/work"
  --mount "type=bind,src=$SCRATCH_DIR/cargo,dst=/cargo"
  --mount "type=bind,src=$SCRATCH_DIR/target,dst=/target"
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target
  -e RUSTUP_TOOLCHAIN=1.100.0 -w /work
)

# Always record an exit code, and allow expected regression failures to finish.
run_logged() {
  local label=$1 code
  shift
  if "$@" > "$SCRATCH_DIR/logs/$label.log" 2>&1; then
    code=0
  else
    code=$?
  fi
  printf '%s\n' "$code" > "$SCRATCH_DIR/logs/$label-exit.txt"
  printf '%s exit=%s; log=%s\n' "$label" "$code" "$SCRATCH_DIR/logs/$label.log"
}

run_logged fetch docker run --rm "${mounts[@]}" "$IMAGE" cargo fetch --locked
[[ $(cat "$SCRATCH_DIR/logs/fetch-exit.txt") == 0 ]] || exit 1
run_logged baseline-test docker run --rm --network none "${mounts[@]}" "$IMAGE" cargo test --locked --offline
[[ $(cat "$SCRATCH_DIR/logs/baseline-test-exit.txt") == 0 ]] || exit 1

patch -d "$SCRATCH_DIR/source" -p1 < "$ARTIFACT_DIR/validation-probes.patch" > "$SCRATCH_DIR/logs/apply-patch.log"
run_logged validation-probes docker run --rm --network none "${mounts[@]}" "$IMAGE" cargo test --locked --offline mcp::tools::tests::research_ -- --nocapture
run_logged supersession-probe docker run --rm --network none "${mounts[@]}" "$IMAGE" cargo test --locked --offline research_supersedes -- --nocapture
mkdir -p "$SCRATCH_DIR/source/examples"
cp "$ARTIFACT_DIR/research_flat_vector.rs" "$SCRATCH_DIR/source/examples/"
run_logged vector-benchmark docker run --rm --network none "${mounts[@]}" "$IMAGE" cargo run --release --locked --offline --example research_flat_vector

printf '\nAll logs retained at %s/logs\n' "$SCRATCH_DIR"
printf 'Recorded-commit expectations: baseline 0; validation 101; supersession 101; benchmark 0.\n'
# Probe exit 101 is expected and must not prevent the benchmark from running.
[[ $(cat "$SCRATCH_DIR/logs/vector-benchmark-exit.txt") == 0 ]]
