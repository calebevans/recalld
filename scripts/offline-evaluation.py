#!/usr/bin/env python3
"""Snapshot this checkout, build/test offline, and compare three fresh processes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib

IMAGE = "recalld-ci:1.94"
ROOT = Path(__file__).resolve().parents[1]
FIXED_INPUTS = (
    "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "examples/offline_eval.rs",
    "scripts/offline-evaluation.py", "scripts/offline-evaluation.Dockerfile",
    "scripts/test_offline_evaluation.py",
)


def command(args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def snapshot_source(root, destination):
    # Tracked target inputs only; never copy scratch, ambient Cargo config or secrets.
    tracked = {os.fsdecode(p) for p in subprocess.check_output(
        ["git", "ls-files", "-z", "--cached"], cwd=root).split(b"\0") if p}
    roots = ("src/", "tests/", "examples/", "benches/")
    legacy_data = {f"src/bench/data/{name}.json" for name in
                   ("locomo1", "locomo10", "locomo_conv30", "locomo_conv41", "locomo_first")}
    names = set(FIXED_INPUTS)
    for name in tracked:
        if name.startswith(roots):
            if name.endswith(".rs"):
                names.add(name)
            elif name not in legacy_data:
                raise RuntimeError(f"undeclared target asset requires explicit allowlist: {name}")
    cargo = tomllib.loads((root / "Cargo.toml").read_text())
    if "workspace" in cargo:
        raise RuntimeError("workspace snapshot requires explicit policy")
    for kind in ("lib", "bin", "test", "example", "bench"):
        targets = [cargo[kind]] if kind == "lib" and kind in cargo else cargo.get(kind, [])
        for target in targets:
            if target.get("path") and target["path"] not in names:
                raise RuntimeError(f"undeclared Cargo target: {target['path']}")
    build = cargo.get("package", {}).get("build", "build.rs" if "build.rs" in tracked else False)
    if build and build not in names:
        raise RuntimeError(f"undeclared build script: {build}")
    if not names <= tracked:
        raise RuntimeError("required source inputs must be tracked")
    hashes = {}
    for name in sorted(names):
        source = root / name
        if source.is_symlink() or not source.is_file() or not source.resolve().is_relative_to(root.resolve()):
            raise RuntimeError(f"missing, nonregular or escaping source input: {name}")
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        hashes[name] = sha(target)
    return hashes


def source_patch(root, names):
    # Unrelated tracked changes can also contain credentials or bulky output.
    return subprocess.check_output(["git", "diff", "HEAD", "--binary", "--", *sorted(names)], cwd=root)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", required=True, type=Path, help="Prior scratch Cargo home; only registry/ is copied")
    parser.add_argument("--build-cache", type=Path, help="Optional prior scratch target to copy, never share")
    parser.add_argument("--image", default=IMAGE)
    parser.add_argument("--toolchain", default="1.94.1")
    parser.add_argument("--records", type=int, default=1000)
    parser.add_argument("--dimensions", type=int, default=384)
    parser.add_argument("--k", type=int, default=20)
    parser.add_argument("--warmup", type=int, default=20)
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--seed", type=int, default=7)
    args = parser.parse_args()
    if args.runs < 3:
        parser.error("at least three processes are required for the replay check")
    cache = args.cache.resolve()
    if not str(cache).startswith(("/tmp/recalld-", "/private/tmp/recalld-")) or not (cache / "registry").is_dir():
        parser.error("cache must be a previous recalld scratch directory with registry/")

    # No home/config/credentials/socket mounts. Git is read only on the host.
    scratch = Path(tempfile.mkdtemp(prefix="recalld-evaluation-"))
    source = scratch / "source"
    source.mkdir()
    output = scratch / "output"
    output.mkdir()
    print(f"Artifacts: {output}", flush=True)
    hashes = snapshot_source(ROOT, source)
    shutil.copytree(cache / "registry", scratch / "cargo" / "registry")
    if args.build_cache:
        build_cache = args.build_cache.resolve()
        if build_cache.name != "target" or not build_cache.parent.name.startswith("recalld-evaluation-"):
            parser.error("build cache must be target/ in a prior recalld-evaluation scratch")
        shutil.copytree(build_cache, scratch / "target")
    else:
        (scratch / "target").mkdir()
    patch = source_patch(ROOT, hashes)
    (output / "source.patch").write_bytes(patch)
    image = json.loads(command(["docker", "image", "inspect", args.image]))[0]
    manifest = {
        "diagnosticManifestVersion": 1,
        "scope": "A5/A6 internal foundation; not the integrated A6 manifest/trace contract",
        "status": "running",
        "sourceCommit": command(["git", "rev-parse", "HEAD"], cwd=ROOT),
        "dirtyPatchSha256": hashlib.sha256(patch).hexdigest(),
        "sourceFilesSha256": hashes,
        "sourceInputPolicy": "tracked Rust under src/tests/examples/benches plus FIXED_INPUTS; unknown assets/targets rejected; legacy datasets excluded",
        "dirtyPatchScope": "allowlisted source inputs only; per-file hashes are authoritative",
        "sourceTreeSha256": hashlib.sha256(json.dumps(hashes, sort_keys=True).encode()).hexdigest(),
        "lockfileSha256": hashes["Cargo.lock"],
        "containerImageId": image["Id"], "containerRepoDigests": image.get("RepoDigests", []),
        "architecture": image["Architecture"],
        "command": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        "limits": {"cpus": 2, "memoryBytes": 4294967296, "cargoJobs": 2, "tokioWorkers": 2},
        "isolation": {"network": "none", "home": "fresh container tmpfs", "mount": "scratch only", "externalCalls": 0},
        "fixture": {"generator": "splitmix64-f32-l2-v1", "source": "synthetic, generated by examples/offline_eval.rs",
                    "intendedUse": "offline contract/measurement diagnostics; no semantic quality claims",
                    "externalDataset": None, "license": "no third-party dataset", "sourceMappingVersion": 1},
        "versions": {"publicContract": None, "retrievalSemantics": "legacy source-pinned internal path",
                     "lifecycle": "legacy_access_v1 materialized strength; no sweeps", "rankingProfile": "baseline-v1",
                     "canonicalSchema": "source-pinned; no v2 claim", "receipts": 1, "readiness": "production startup completion; not whole-store barrier"},
        "embedding": {"profile": "splitmix64-f32-l2-v1", "model": None, "dimensions": args.dimensions,
                      "input": "exact fixture namespace and query text; misses fail closed", "cache": "off"},
        "behavior": {"clock": "ManualClock for lifecycle; random store/operation IDs remain", "rif": "off", "graphDepth": 0,
                     "storageFilesystem": "container /tmp tmpfs; CPU/logic control, not persistent-disk ingestion measurement",
                     "recordAccess": "false through production recorder/admission",
                     "ingestion": "canonical InsertMemory receipts then production startup/completion; internal Rust, not public transport",
                     "engine": "real QueryEngine and production storage/vector/FTS/entity/cache/graph/RIF adapters",
                     "store": "fresh temporary root per process; no snapshot/restore claim"},
        "performanceGate": {"established": False, "absoluteBudgets": None,
                            "reason": "baseline characterization; no paired candidate, five-seed run or promotion gate"},
    }
    save(output / "manifest.json", manifest)
    # Execute the exact snapshotted script. No shell interpolation of host input.
    inner = r'''set -eu
mkdir -p /tmp/home
cd /work/source
rustc -Vv > /work/output/rustc.txt
uname -a > /work/output/kernel.txt
cat /proc/cpuinfo > /work/output/cpu.txt
cat /proc/meminfo > /work/output/memory.txt
df -T /work > /work/output/filesystem.txt
df -T /tmp > /work/output/storage-filesystem.txt
cat /sys/fs/cgroup/memory.max > /work/output/cgroup-memory.txt
cat /sys/fs/cgroup/cpu.max > /work/output/cgroup-cpu.txt
python3 scripts/test_offline_evaluation.py > /work/output/runner-tests.log 2>&1
cargo fmt --all --check > /work/output/format.log 2>&1
cargo test --locked --offline --all-features --lib --bins --tests --examples -j1 > /work/output/library-tests.log 2>&1
cargo test --locked --offline --example offline_eval > /work/output/harness-tests.log 2>&1
cargo clippy --locked --offline --all-features --all-targets > /work/output/clippy.log 2>&1
cargo clean -p recalld > /work/output/package-clean.log 2>&1
cargo build --locked --offline --release --example offline_eval -j1 > /work/output/build.log 2>&1
'''
    (scratch / "build.sh").write_text(inner)
    docker = ["docker", "run", "--rm", "--network", "none", "--cpus", "2", "--memory", "4g",
              "--memory-swap", "4g", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
              "--read-only", "--tmpfs", "/tmp:rw,nosuid,nodev", "--mount", f"type=bind,src={scratch},dst=/work",
              "--workdir", "/work/source", "--env", "HOME=/tmp/home", "--env", "CARGO_HOME=/work/cargo",
              "--env", "CARGO_TARGET_DIR=/work/target", "--env", "CARGO_BUILD_JOBS=2",
              "--env", f"RUSTUP_TOOLCHAIN={args.toolchain}", image["Id"]]
    try:
        subprocess.run(docker + ["bash", "/work/build.sh"], check=True)
        manifest["executableSha256"] = sha(scratch / "target/release/examples/offline_eval")
        for run in range(args.runs):
            cmd = docker + ["/work/target/release/examples/offline_eval", "--out", f"/work/output/run-{run}"]
            for key in ["records", "dimensions", "k", "warmup", "samples", "seed"]:
                cmd += [f"--{key}", str(getattr(args, key))]
            with (output / f"run-{run}.log").open("w") as log:
                subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, check=True)
        stable = ["fixture.json", "turn-ledger.jsonl", "state-before.json", "state-after.json", "replay.json"]
        for name in stable:
            if len({sha(output / f"run-{run}" / name) for run in range(args.runs)}) != 1:
                raise RuntimeError(f"replay mismatch: {name}")
        manifest["status"] = "passed"
        manifest["replay"] = {"processes": args.runs, "identicalArtifacts": stable}
    except BaseException as exc:
        manifest["status"] = "failed"
        manifest["error"] = str(exc)
        raise
    finally:
        manifest["artifactsSha256"] = {str(p.relative_to(output)): sha(p) for p in sorted(output.rglob("*"))
                                       if p.is_file() and p.name != "manifest.json"}
        save(output / "manifest.json", manifest)
        print(f"{manifest['status']}: {output}", flush=True)


if __name__ == "__main__":
    main()
