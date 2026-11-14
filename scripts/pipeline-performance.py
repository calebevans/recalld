#!/usr/bin/env python3
"""Run the preregistered isolated A/D/E protocol; no downloads or shared targets."""
import argparse
import hashlib
import datetime
import json
import math
import os
from pathlib import Path
import random
import re
import shutil
import statistics
import subprocess
import threading
import time

SEEDS = [7, 19, 41, 73, 101]
ORDERS = ["ADE", "DEA", "EAD", "AED", "DAE"]
IMAGE = "sha256:daefdc9cbe9b0f39b1fc29d975148b3b10adc1bd4309f9ca1be8abd36ed811ef"

def write(path, data):
    path.write_text(json.dumps(data, indent=2) + "\n")

def capture():
    snapshot = {"time_ns": time.time_ns(), "loadavg": os.getloadavg()}
    try:
        # Container IDs are transient join keys only. Never persist names/IDs.
        rows = subprocess.run(["docker", "ps", "--format", "{{.ID}}\t{{.Image}}"],
                              text=True, capture_output=True, timeout=5, check=True).stdout.splitlines()
        usage = subprocess.run(["docker", "stats", "--no-stream", "--format",
                                "{{.ID}}\t{{.CPUPerc}}\t{{.MemUsage}}"],
                               text=True, capture_output=True, timeout=5, check=True).stdout.splitlines()
        stats = {}
        for row in usage:
            key, cpu, memory = row.split("\t")
            stats[key] = {"cpu_percent": float(cpu.rstrip("%")), "memory_usage": memory}
        groups = {}
        for row in rows:
            key, image = row.split("\t", 1)
            group = groups.setdefault(image, {"count": 0, "cpu_percent_sum": 0.0,
                                              "memory_usage_samples": [], "missing_stats": 0})
            group["count"] += 1
            if key in stats:
                group["cpu_percent_sum"] += stats[key]["cpu_percent"]
                group["memory_usage_samples"].append(stats[key]["memory_usage"])
            else:
                group["missing_stats"] += 1
        snapshot.update(container_count=len(rows), images=groups)
        # Two samples: the final sample measures an interval, not since boot.
        top = subprocess.run(["top", "-l", "2", "-s", "1", "-n", "0"],
                             text=True, capture_output=True, timeout=5, check=True).stdout
        cpu = re.findall(r"CPU usage: ([0-9.]+)% user, ([0-9.]+)% sys, ([0-9.]+)% idle", top)[-1]
        snapshot["host_cpu_percent"] = dict(zip(["user", "system", "idle"], map(float, cpu)))
    except (subprocess.SubprocessError, OSError, ValueError, IndexError) as error:
        # Do not include raw command output that could identify other projects.
        snapshot["capture_error"] = type(error).__name__
    return snapshot

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def main():
    p = argparse.ArgumentParser()
    p.add_argument("--anchor", type=Path, required=True)
    p.add_argument("--candidate", type=Path, required=True)
    p.add_argument("--root", type=Path, required=True)
    p.add_argument("--source", required=True, help="frozen candidate source SHA")
    p.add_argument("--anchor-source", required=True, help="probe-only frozen anchor commit")
    p.add_argument("--deadline-utc", help="coordinator-approved UTC deadline, ISO 8601")
    p.add_argument("--prepare", action="store_true")
    p.add_argument("--mode", choices=["quiet", "characterization"], required=True,
                   help="quiet requires an explicitly arranged exclusive interval")
    args = p.parse_args()
    root = args.root.resolve()
    root.mkdir(parents=True, exist_ok=True)
    anchor, candidate = args.anchor.resolve(), args.candidate.resolve()
    manifest = {"candidate_source": args.source, "probe_sha256": digest(Path(__file__).resolve().parent.parent / "examples/performance_probe.rs"), "anchor_source": args.anchor_source, "runtime_parent": "3b590fb28c84bb2aa2d8afaac66e34556d7da6a6",
                "anchor_binary_sha256": digest(anchor), "candidate_binary_sha256": digest(candidate),
                "deadline_utc": args.deadline_utc, "mode": args.mode, "orders": dict(zip(SEEDS, ORDERS)), "seeds": SEEDS,
                "runtime": {"image": IMAGE, "network": "none", "cpus": 2, "memory": "4g"},
                "warmup": 200, "samples": 1000, "throughput_seconds": 60,
                "sink": "full JSON file, synchronous per-request flush", "sampling": 1,
                "host_before": capture()}
    manifest_path = root / ("setup-manifest.json" if args.prepare else "manifest.json")
    if manifest_path.exists():
        raise RuntimeError("fresh manifest required; preserve previous attempts")
    write(manifest_path, manifest)

    deadline = None

    def run(seed, arm, initialize=False):
        name = f"a5-probe-{seed}-{arm.lower()}"
        destination = root / f"seed-{seed}-{arm}"
        destination.mkdir(exist_ok=False)
        write(destination / "host-before.json", capture())
        store = root / f"store-{seed}"
        if not initialize:
            store = destination / "store"
            shutil.copytree(root / f"store-{seed}", store)
        binary = anchor if arm == "A" or initialize else candidate
        command = ["docker", "run", "--rm", "--name", name, "--network", "none", "--cpus", "2", "--memory", "4g",
                   "-e", "RUSTUP_TOOLCHAIN=1.94.1", "-e", "CARGO_BUILD_JOBS=2",
                   "-v", f"{binary}:/probe:ro", "-v", f"{root}:/study",
                   IMAGE, "/probe", "--store", f"/study/{store.relative_to(root)}",
                   "--out", f"/study/seed-{seed}-{arm}/raw", "--seed", str(seed)]
        if initialize:
            command.append("--initialize")
        if arm == "E":
            command.append("--trace")
        print(f"starting seed={seed} arm={arm}", flush=True)
        with (destination / "process.log").open("w") as log:
            try:
                timeout = None if initialize else max(0.01, min(90, deadline - time.monotonic()))
                code = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, timeout=timeout).returncode
            except subprocess.TimeoutExpired:
                code = 124
                subprocess.run(["docker", "rm", "-f", name], stdout=log, stderr=subprocess.STDOUT, timeout=5)
        write(destination / "exit.json", {"returncode": code, "command": command})
        write(destination / "host-after.json", capture())
        print(f"completed seed={seed} arm={arm} exit={code}", flush=True)
        return code

    if args.prepare:
        for seed in SEEDS:
            if run(seed, "SETUP", True):
                raise RuntimeError("setup failed; raw attempt retained")
        return
    if not args.deadline_utc:
        raise RuntimeError("measurement requires the coordinated UTC deadline")
    cutoff = datetime.datetime.fromisoformat(args.deadline_utc.replace("Z", "+00:00"))
    if cutoff.utcoffset() != datetime.timedelta(0):
        raise RuntimeError("deadline must explicitly use UTC")
    deadline = time.monotonic() + min(1185, cutoff.timestamp() - time.time() - 15)
    preflight = [capture() for _ in range(3)]
    write(root / "preflight.json", preflight)
    idle = [s.get("host_cpu_percent", {}).get("idle", 0) for s in preflight]
    steady = all("capture_error" not in s for s in preflight) and min(idle) >= 90 and max(idle) - min(idle) <= 5
    if args.mode == "quiet" and not steady:
        write(root / "analysis.json", {"promotion": "not assessed", "reason": "quiet CPU preflight failed", "preflight": preflight})
        raise SystemExit(2)
    stop = threading.Event()
    def observer():
        with (root / "host-observer.jsonl").open("x") as out:
            while not stop.is_set():
                out.write(json.dumps(capture()) + "\n")
                out.flush()
                stop.wait(10)
    thread = threading.Thread(target=observer)
    thread.start()
    failures = []
    try:
        for seed, order in zip(SEEDS, ORDERS):
            for arm in order:
                if time.monotonic() >= deadline:
                    failures.append([seed, arm, "window_expired_not_started"])
                    continue
                if run(seed, arm):
                    failures.append([seed, arm])
    finally:
        stop.set()
        thread.join()
    write(root / "host-after.json", capture())
    pairs = []
    for seed in SEEDS:
        summaries = {}
        for arm in "ADE":
            path = root / f"seed-{seed}-{arm}/raw/summary.json"
            if path.exists():
                summaries[arm] = json.loads(path.read_text())
        if len(summaries) == 3:
            same_results = len({s["result_sha256"] for s in summaries.values()}) == 1
            same_state = len({s["canonical_state_sha256"] for s in summaries.values()}) == 1
            pairs.append({"seed": seed, "arms": summaries, "same_results": same_results, "same_state": same_state})
    def interval(values):
        rng = random.Random(20261114)
        means = sorted(math.exp(statistics.mean(math.log(v) for v in rng.choices(values, k=len(values)))) for _ in range(10000))
        return {"geometric_mean": math.exp(statistics.mean(map(math.log, values))), "ci95": [means[249], means[9749]], "pairs": values}
    ratios = {}
    if pairs:
        for arm in "DE":
            ratios[arm] = {"p95": interval([p["arms"][arm]["p95_ns"] / p["arms"]["A"]["p95_ns"] for p in pairs]),
                           "throughput": interval([p["arms"][arm]["requests_per_second"] / p["arms"]["A"]["requests_per_second"] for p in pairs])}
    # Human review of host evidence and preregistered absolute budgets remains
    # mandatory. Never auto-promote from noisy host samples or five point estimates.
    write(root / "analysis.json", {"mode": args.mode, "failed_attempts": failures, "pairs": pairs,
                                    "ratios": ratios, "promotion": "not assessed; host/budget/interval review required"})
    if failures:
        raise SystemExit(1)

if __name__ == "__main__":
    main()
