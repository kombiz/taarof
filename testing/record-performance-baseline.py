#!/usr/bin/env python3
"""Run and validate the deterministic release performance workloads."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import platform
import statistics
import subprocess
import sys
from pathlib import Path


SCHEMA = "taarof.performance-baseline.v1"
U64_MODULUS = 1 << 64
CONTRACTS = {
    "runtime-probe": {
        "fixture": "runtime-probe-v1-8x4x12",
        "iterations": 200,
        "checksum_per_iteration": 12_978_782_536_162_598_354,
        "source_reads_per_iteration": 1_569,
        "work_units_per_iteration": 384,
    },
    "session-restore": {
        "fixture": "session-restore-v1-8x16x4",
        "iterations": 200,
        "checksum_per_iteration": 7_725_976_349_535_212_874,
        "source_reads_per_iteration": 0,
        "work_units_per_iteration": 648,
    },
}


def command_output(*command: str, cwd: Path | None = None) -> str:
    return subprocess.run(
        command,
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def cpu_model() -> str:
    try:
        for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or "unknown"


def expected_sample(workload: str, iterations: int) -> dict[str, int | str]:
    contract = CONTRACTS[workload]
    return {
        "workload": workload,
        "fixture": contract["fixture"],
        "iterations": iterations,
        "checksum": contract["checksum_per_iteration"] * iterations % U64_MODULUS,
        "source_reads": contract["source_reads_per_iteration"] * iterations,
        "work_units": contract["work_units_per_iteration"] * iterations,
    }


def run_sample(binary: Path, workload: str, iterations: int) -> dict[str, int | str]:
    raw = command_output(str(binary), workload, str(iterations))
    try:
        sample = json.loads(raw)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"{workload} emitted invalid JSON: {error}") from error

    expected = expected_sample(workload, iterations)
    actual_contract = {key: sample.get(key) for key in expected}
    if actual_contract != expected:
        raise RuntimeError(
            f"{workload} correctness contract changed:\n"
            f"expected {json.dumps(expected, sort_keys=True)}\n"
            f"actual   {json.dumps(actual_contract, sort_keys=True)}"
        )
    elapsed_ns = sample.get("elapsed_ns")
    if not isinstance(elapsed_ns, int) or elapsed_ns <= 0:
        raise RuntimeError(f"{workload} emitted invalid elapsed_ns: {elapsed_ns!r}")
    return sample


def summarize(samples: list[dict[str, int | str]]) -> dict[str, int | float]:
    elapsed = [int(sample["elapsed_ns"]) for sample in samples]
    median_ns = int(statistics.median(elapsed))
    return {
        "min_ns": min(elapsed),
        "median_ns": median_ns,
        "max_ns": max(elapsed),
        "range_percent_of_median": round((max(elapsed) - min(elapsed)) * 100 / median_ns, 2),
    }


def record(binary: Path, warmups: int, repetitions: int) -> dict[str, object]:
    repo = Path(__file__).resolve().parents[1]
    source_sha = command_output("git", "rev-parse", "HEAD", cwd=repo)
    source_dirty = bool(command_output("git", "status", "--porcelain", cwd=repo))
    workloads = []
    for workload, contract in CONTRACTS.items():
        iterations = int(contract["iterations"])
        warmup_samples = [run_sample(binary, workload, iterations) for _ in range(warmups)]
        samples = [run_sample(binary, workload, iterations) for _ in range(repetitions)]
        workloads.append(
            {
                "workload": workload,
                "fixture": contract["fixture"],
                "iterations": iterations,
                "expected": expected_sample(workload, iterations),
                "warmup_samples": warmup_samples,
                "samples": samples,
                "summary": summarize(samples),
            }
        )

    return {
        "schema": SCHEMA,
        "captured_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "source_sha": source_sha,
        "source_dirty": source_dirty,
        "build_profile": "release",
        "toolchain": {
            "rustc": command_output("rustc", "--version"),
            "cargo": command_output("cargo", "--version"),
        },
        "host": {
            "os": platform.system(),
            "kernel": platform.release(),
            "architecture": platform.machine(),
            "logical_cpus": os.cpu_count(),
            "cpu_model": cpu_model(),
        },
        "method": {
            "warmups": warmups,
            "repetitions": repetitions,
            "timing_gate": "none; elapsed times are observational until runner variance is reviewed",
        },
        "workloads": workloads,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--repetitions", type=int, default=5)
    args = parser.parse_args()
    if args.warmups < 1 or args.repetitions < 1:
        parser.error("--warmups and --repetitions must be positive")
    return args


def main() -> int:
    args = parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"performance harness not found: {binary}")

    try:
        baseline = record(binary, args.warmups, args.repetitions)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"performance baseline failed: {error}", file=sys.stderr)
        return 1

    encoded = json.dumps(baseline, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
        print(f"performance baseline: {args.output}")
    else:
        print(encoded, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
