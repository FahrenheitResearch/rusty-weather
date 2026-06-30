#!/usr/bin/env python3
"""Local load test for the Rusty Fire Weather API.

This intentionally uses only Python's standard library so it can run on a
plain Windows box. It submits draw-a-box render jobs to rw_fire_api, polls each
job until completion, and writes machine-readable latency summaries.
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import random
import statistics
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Any


CALIFORNIA_BOUNDS = (-126.0, -113.8, 31.9, 42.5)

SINGLE_PRODUCTS = [
    "fire_weather_composite",
    "hdw",
    "vpd_2m",
    "10m_wind_1h_max",
    "10m_wind_run_max",
]


@dataclass
class Sample:
    index: int
    state: str
    job_id: str
    products: str
    output_format: str
    output_width: int | None
    bounds: list[float]
    submit_ms: int
    client_ms: int
    api_wall_ms: int | None
    render_wall_ms: int | None
    renderer_total_ms: int | None
    files: int
    total_bytes: int
    error: str


def main() -> int:
    args = parse_args()
    rng = random.Random(args.seed)
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    requests = [build_request(args, rng, index) for index in range(args.requests)]

    print(
        f"fire-api-load scenario={args.scenario} requests={args.requests} "
        f"concurrency={args.concurrency} api={args.api}"
    )
    started = time.perf_counter()
    samples: list[Sample] = []
    with ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [
            pool.submit(run_one, args.api, request, index, args.poll_ms / 1000.0)
            for index, request in enumerate(requests)
        ]
        completed = 0
        for future in as_completed(futures):
            sample = future.result()
            samples.append(sample)
            completed += 1
            print(
                f"{completed:>3}/{args.requests:<3} "
                f"{sample.state:<9} client={sample.client_ms:>5}ms "
                f"api={sample.api_wall_ms or -1:>5}ms "
                f"render={sample.renderer_total_ms or -1:>5}ms "
                f"files={sample.files:<2} mb={sample.total_bytes / 1048576:>5.2f} "
                f"{sample.products}"
            )

    elapsed_ms = int((time.perf_counter() - started) * 1000)
    samples.sort(key=lambda item: item.index)
    summary = summarize(samples, args, elapsed_ms)

    csv_path = out_dir / f"{args.label}_samples.csv"
    json_path = out_dir / f"{args.label}_summary.json"
    write_samples(csv_path, samples)
    json_path.write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(json.dumps(summary, indent=2))
    print(f"samples_csv={csv_path}")
    print(f"summary_json={json_path}")
    return 1 if summary["failed"] else 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Load-test local rw_fire_api jobs")
    parser.add_argument("--api", default="http://127.0.0.1:8788")
    parser.add_argument("--label", default="fire_api_load")
    parser.add_argument("--model", default="hrrr")
    parser.add_argument("--run", default="20260629_03z")
    parser.add_argument("--hour", type=int, default=3)
    parser.add_argument(
        "--scenario",
        choices=["preview-mixed", "preview-core", "preview-single", "full-png-core"],
        default="preview-mixed",
    )
    parser.add_argument("--requests", type=int, default=20)
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--seed", type=int, default=20260630)
    parser.add_argument("--poll-ms", type=int, default=200)
    parser.add_argument(
        "--out-dir",
        default=r"C:\Users\drew\Documents\Codex\2026-06-28\so\outputs\fire_api_load_tests",
    )
    return parser.parse_args()


def build_request(args: argparse.Namespace, rng: random.Random, index: int) -> dict[str, Any]:
    products = choose_products(args.scenario, rng)
    output_format = "png" if args.scenario == "full-png-core" else "webp"
    output_width = None if args.scenario == "full-png-core" else 800
    bounds = random_bounds(rng, regional=(rng.random() < 0.18))
    payload: dict[str, Any] = {
        "model": args.model,
        "run": args.run,
        "hour": args.hour,
        "products": products,
        "output_format": output_format,
        "domain_slug": f"{args.label}_{index:03d}",
        "bounds": bounds,
    }
    if output_width is not None:
        payload["output_width"] = output_width
    return payload


def choose_products(scenario: str, rng: random.Random) -> str:
    if scenario in {"preview-core", "full-png-core"}:
        return "cafire-core"
    if scenario == "preview-single":
        return rng.choice(SINGLE_PRODUCTS)
    roll = rng.random()
    if roll < 0.45:
        return rng.choice(SINGLE_PRODUCTS)
    if roll < 0.90:
        return "cafire-core"
    return rng.choice(["hdw", "fire_weather_composite", "vpd_2m"])


def random_bounds(rng: random.Random, regional: bool) -> list[float]:
    west_limit, east_limit, south_limit, north_limit = CALIFORNIA_BOUNDS
    if regional:
        lon_span = rng.uniform(5.0, 9.0)
        lat_span = rng.uniform(4.0, 8.0)
    else:
        lon_span = rng.uniform(1.0, 4.2)
        lat_span = rng.uniform(1.0, 4.5)
    center_lon = rng.uniform(west_limit + lon_span / 2, east_limit - lon_span / 2)
    center_lat = rng.uniform(south_limit + lat_span / 2, north_limit - lat_span / 2)
    west = clamp(center_lon - lon_span / 2, west_limit, east_limit - 0.05)
    east = clamp(center_lon + lon_span / 2, west + 0.05, east_limit)
    south = clamp(center_lat - lat_span / 2, south_limit, north_limit - 0.05)
    north = clamp(center_lat + lat_span / 2, south + 0.05, north_limit)
    return [round(west, 4), round(east, 4), round(south, 4), round(north, 4)]


def clamp(value: float, low: float, high: float) -> float:
    return max(low, min(high, value))


def run_one(api: str, payload: dict[str, Any], index: int, poll_seconds: float) -> Sample:
    started = time.perf_counter()
    submit_started = time.perf_counter()
    try:
        started_job = post_json(f"{api}/api/render", payload)
        submit_ms = int((time.perf_counter() - submit_started) * 1000)
        status_url = started_job["status_url"]
        job_id = started_job["id"]
        while True:
            time.sleep(poll_seconds)
            job = get_json(f"{api}{status_url}")
            if job.get("state") not in {"queued", "running"}:
                break
        client_ms = int((time.perf_counter() - started) * 1000)
        files = job.get("files") or []
        stdout_tail = job.get("stdout_tail") or ""
        return Sample(
            index=index,
            state=job.get("state", "unknown"),
            job_id=job_id,
            products=payload["products"],
            output_format=payload["output_format"],
            output_width=payload.get("output_width"),
            bounds=payload["bounds"],
            submit_ms=submit_ms,
            client_ms=client_ms,
            api_wall_ms=job.get("wall_ms"),
            render_wall_ms=parse_domain_wall_ms(stdout_tail),
            renderer_total_ms=parse_renderer_total_ms(stdout_tail),
            files=len(files),
            total_bytes=sum(int(file.get("bytes", 0)) for file in files),
            error="" if job.get("state") == "succeeded" else job.get("message", ""),
        )
    except Exception as err:  # noqa: BLE001 - load harness should report all failures.
        client_ms = int((time.perf_counter() - started) * 1000)
        return Sample(
            index=index,
            state="client-error",
            job_id="",
            products=payload["products"],
            output_format=payload["output_format"],
            output_width=payload.get("output_width"),
            bounds=payload["bounds"],
            submit_ms=int((time.perf_counter() - submit_started) * 1000),
            client_ms=client_ms,
            api_wall_ms=None,
            render_wall_ms=None,
            renderer_total_ms=None,
            files=0,
            total_bytes=0,
            error=str(err),
        )


def post_json(url: str, payload: dict[str, Any]) -> dict[str, Any]:
    body = json.dumps(payload).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={"content-type": "application/json"},
    )
    return read_json(request)


def get_json(url: str) -> dict[str, Any]:
    return read_json(urllib.request.Request(url, method="GET"))


def read_json(request: urllib.request.Request) -> dict[str, Any]:
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as err:
        body = err.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"HTTP {err.code}: {body}") from err


def parse_renderer_total_ms(stdout_tail: str) -> int | None:
    marker = "total wall "
    for line in reversed(stdout_tail.splitlines()):
        if "rendered " in line and marker in line:
            tail = line.split(marker, 1)[1].split(" ms", 1)[0].strip()
            if tail.isdigit():
                return int(tail)
    return None


def parse_domain_wall_ms(stdout_tail: str) -> int | None:
    marker = "| wall "
    for line in reversed(stdout_tail.splitlines()):
        if line.startswith("domain ") and marker in line:
            tail = line.split(marker, 1)[1].split(" ms", 1)[0].strip()
            if tail.isdigit():
                return int(tail)
    return None


def summarize(samples: list[Sample], args: argparse.Namespace, elapsed_ms: int) -> dict[str, Any]:
    succeeded = [sample for sample in samples if sample.state == "succeeded"]
    failed = [sample for sample in samples if sample.state != "succeeded"]
    client = [sample.client_ms for sample in succeeded]
    api_wall = [sample.api_wall_ms for sample in succeeded if sample.api_wall_ms is not None]
    renderer_total = [
        sample.renderer_total_ms for sample in succeeded if sample.renderer_total_ms is not None
    ]
    bytes_total = sum(sample.total_bytes for sample in succeeded)
    return {
        "label": args.label,
        "scenario": args.scenario,
        "requests": args.requests,
        "concurrency": args.concurrency,
        "succeeded": len(succeeded),
        "failed": len(failed),
        "elapsed_ms": elapsed_ms,
        "throughput_jobs_per_sec": round(len(succeeded) / (elapsed_ms / 1000), 3)
        if elapsed_ms
        else 0,
        "total_mb": round(bytes_total / 1048576, 2),
        "client_ms": stats(client),
        "api_wall_ms": stats(api_wall),
        "renderer_total_ms": stats(renderer_total),
        "failures": [asdict(sample) for sample in failed[:10]],
    }


def stats(values: list[int]) -> dict[str, int | None]:
    if not values:
        return {"min": None, "p50": None, "p90": None, "p95": None, "max": None, "mean": None}
    sorted_values = sorted(values)
    return {
        "min": sorted_values[0],
        "p50": percentile(sorted_values, 50),
        "p90": percentile(sorted_values, 90),
        "p95": percentile(sorted_values, 95),
        "max": sorted_values[-1],
        "mean": int(statistics.mean(sorted_values)),
    }


def percentile(sorted_values: list[int], pct: int) -> int:
    if len(sorted_values) == 1:
        return sorted_values[0]
    rank = (pct / 100) * (len(sorted_values) - 1)
    low = math.floor(rank)
    high = math.ceil(rank)
    if low == high:
        return sorted_values[low]
    weight = rank - low
    return int(sorted_values[low] * (1 - weight) + sorted_values[high] * weight)


def write_samples(path: Path, samples: list[Sample]) -> None:
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(asdict(samples[0]).keys()))
        writer.writeheader()
        for sample in samples:
            row = asdict(sample)
            row["bounds"] = json.dumps(row["bounds"])
            writer.writerow(row)


if __name__ == "__main__":
    raise SystemExit(main())
