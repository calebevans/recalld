#!/usr/bin/env python3
"""Frozen corrected-baseline namespace-batch study; first invocation stops after the 1k/seed-7 pilot."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
REVISIONS = {"baseline": "1a634006023cfab6e277a1ebe8503b5e3de4e40a",
             "candidate": "0e58f3b3dd36e2d74dfd5f97bfb522ba96c6efe8"}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def source_at(revision, destination):
    listing = subprocess.check_output(["git", "ls-tree", "-rz", revision, "--", "src",
                                       "Cargo.toml", "Cargo.lock", "rust-toolchain.toml"], cwd=ROOT)
    hashes = {}
    for item in listing.split(b"\0"):
        if not item:
            continue
        metadata, raw_name = item.split(b"\t", 1)
        name = raw_name.decode()
        if name.startswith("src/") and not name.endswith(".rs"):
            continue
        if not metadata.startswith((b"100644 blob ", b"100755 blob ")):
            raise RuntimeError(f"nonregular input {name}")
        path = destination / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(subprocess.check_output(["git", "show", f"{revision}:{name}"], cwd=ROOT))
        hashes[name] = sha(path)
    path = destination / "examples/offline_eval.rs"
    path.parent.mkdir(parents=True)
    shutil.copyfile(ROOT / "examples/offline_eval.rs", path)
    hashes["examples/offline_eval.rs"] = sha(path)
    return hashes


def docker(arm, image):
    return ["docker", "run", "--rm", "--network", "none", "--cpus", "2", "--memory", "4g",
            "--memory-swap", "4g", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
            "--read-only", "--tmpfs", "/tmp:rw,nosuid,nodev", "--mount", f"type=bind,src={arm},dst=/work",
            "--workdir", "/work/source", "--env", "HOME=/tmp/home", "--env", "CARGO_HOME=/work/cargo",
            "--env", "CARGO_TARGET_DIR=/work/target", "--env", "CARGO_BUILD_JOBS=2",
            "--env", "RUSTUP_TOOLCHAIN=1.94.1", image]


def pair(study, manifest, records, seed):
    name = f"n{records}-s{seed}"
    # Alternate order within each scale, reverse the starting arm at 10k.
    order = ["baseline", "candidate"]
    if (seed - 7 + (records == 10000)) % 2:
        order.reverse()
    if name in manifest["pairs"]:
        return
    for arm in order:
        directory = study / arm
        cmd = docker(directory, manifest["imageId"]) + ["/work/target/release/examples/offline_eval",
            "--out", f"/work/output/{name}", "--records", str(records), "--seed", str(seed),
            "--dimensions", "384", "--k", "20", "--warmup", "20", "--samples", "100"]
        with (directory / "output" / f"{name}.log").open("w") as log:
            subprocess.run(cmd, check=True, stdout=log, stderr=subprocess.STDOUT)
    result = {"records": records, "seed": seed, "order": order, "arms": {}}
    for artifact in ["fixture.json", "turn-ledger.jsonl", "state-before.json", "state-after.json", "replay.json"]:
        if sha(study / "baseline/output" / name / artifact) != sha(study / "candidate/output" / name / artifact):
            raise RuntimeError(f"paired state/result mismatch: {name}/{artifact}")
    for arm in order:
        directory = study / arm / "output" / name
        summary = json.loads((directory / "summary.json").read_text())
        result["arms"][arm] = {"summary": summary, "artifactsSha256": {
            p.name: sha(p) for p in sorted(directory.iterdir()) if p.is_file()}}
    result["fixtureAndResultsIdentical"] = True
    manifest["pairs"][name] = result
    save(study / "manifest.json", manifest)
    costs = {}
    for cell in ["internal-engine-vector", "internal-engine-hybrid"]:
        p95 = {arm: next(c for c in result["arms"][arm]["summary"]["cells"] if c["cell"] == cell)
               ["stageUs"]["engineCallUs"]["p95"] for arm in order}
        costs[cell] = {**p95, "candidateOverBaseline": p95["candidate"] / p95["baseline"]}
    print(json.dumps({"pair": name, "engineP95Us": costs}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path)
    parser.add_argument("--build-cache", type=Path)
    parser.add_argument("--image", default="recalld-ci:evaluation-recipe")
    parser.add_argument("--continue-study", type=Path)
    args = parser.parse_args()
    if args.continue_study:
        study = args.continue_study.resolve()
        manifest = json.loads((study / "manifest.json").read_text())
        if manifest["driverSha256"] != sha(Path(__file__)) or manifest["harnessSha256"] != sha(ROOT / "examples/offline_eval.rs"):
            raise RuntimeError("study code changed after freezing the pilot")
        if manifest["status"] != "pilot_complete":
            raise RuntimeError("continuation requires a completed pilot")
        for arm, inputs in manifest["sourceFilesSha256"].items():
            for name, digest in inputs.items():
                if sha(study / arm / "source" / name) != digest:
                    raise RuntimeError(f"frozen input changed: {arm}/{name}")
    else:
        if not args.cache or not args.build_cache:
            parser.error("pilot requires --cache and --build-cache from prior isolated validation")
        study = Path(tempfile.mkdtemp(prefix="recalld-namespace-read-study-", dir="/tmp"))
        image = json.loads(subprocess.check_output(["docker", "image", "inspect", args.image]))[0]
        manifest = {"studyVersion": 1, "status": "preparing", "revisions": REVISIONS,
                    "driverSha256": sha(Path(__file__)), "harnessSha256": sha(ROOT / "examples/offline_eval.rs"),
                    "imageId": image["Id"], "imageDigests": image.get("RepoDigests", []),
                    "sourceFilesSha256": {}, "pairs": {},
                    "plan": {"records": [1000, 10000], "seeds": [7, 8, 9, 10, 11], "k": 20,
                             "dimensions": 384, "warmup": 20, "samples": 100,
                             "order": "alternating arms within scale, reverse first arm at 10k",
                             "pilot": "1k seed7; document before continuing",
                             "mode": "unchanged internal diagnostic access adapter, tmpfs, no providers",
                             "limits": "network none, 2 CPUs, 4 GiB, 2 build jobs",
                             "releaseGate": False, "absoluteBudgets": None,
                             "purpose": "paired overhead characterization, not promotion"}}
        save(study / "manifest.json", manifest)
        print(f"Study: {study}", flush=True)
        for arm, revision in REVISIONS.items():
            directory = study / arm
            (directory / "output").mkdir(parents=True)
            manifest["sourceFilesSha256"][arm] = source_at(revision, directory / "source")
            shutil.copytree(args.cache / "registry", directory / "cargo/registry")
            shutil.copytree(args.build_cache, directory / "target")
            save(study / "manifest.json", manifest)
            shell = """set -eu
mkdir -p /tmp/home
rustc -Vv > /work/output/rustc.txt
uname -a > /work/output/kernel.txt
df -T /tmp > /work/output/storage-filesystem.txt
cat /sys/fs/cgroup/cpu.max > /work/output/cgroup-cpu.txt
cat /sys/fs/cgroup/memory.max > /work/output/cgroup-memory.txt
cargo test --offline --locked --lib > /work/output/library-tests.log 2>&1
cargo test --offline --locked --example offline_eval > /work/output/harness-tests.log 2>&1
cargo build --offline --locked --release --example offline_eval > /work/output/build.log 2>&1
"""
            subprocess.run(docker(directory, manifest["imageId"]) + ["bash", "-c", shell], check=True)
        manifest["status"] = "running_pilot"
    try:
        if args.continue_study:
            for records in manifest["plan"]["records"]:
                for seed in manifest["plan"]["seeds"]:
                    pair(study, manifest, records, seed)
            manifest["status"] = "complete"
        else:
            pair(study, manifest, 1000, 7)
            manifest["status"] = "pilot_complete"
    except BaseException as exc:
        manifest["status"] = "failed"
        manifest["error"] = str(exc)
        raise
    finally:
        save(study / "manifest.json", manifest)
        print(f"{manifest['status']}: {study}", flush=True)


if __name__ == "__main__":
    main()
