"""Run the four modes in order, sampling apparent directory bytes once per second."""
import os
import resource
from pathlib import Path
import subprocess
import sys
import time

soft_limit, _ = resource.getrlimit(resource.RLIMIT_NOFILE)
if soft_limit != resource.RLIM_INFINITY and soft_limit < 65536:
    raise RuntimeError("raise the open-file soft limit to at least 65536 before running")

root = Path(__file__).resolve().parents[2]
binary = root / "target" / "release" / "compression-measure"
runs = root / "target" / "rocksdb-options" / "runs"
runs.mkdir(parents=True, exist_ok=True)
count = sys.argv[1] if len(sys.argv) > 1 else "10000000"
modes = [
    ("baseline", {}),
    ("compression", {"EXPERIMENT_COMPRESS": "1"}),
    ("subcompactions", {"EXPERIMENT_SUBCOMPACTIONS": "1"}),
    ("both", {"EXPERIMENT_COMPRESS": "1", "EXPERIMENT_SUBCOMPACTIONS": "1"}),
]
for name, changes in modes:
    path = runs / name
    path.mkdir()  # Refuse to overwrite an earlier run.
    env = os.environ.copy()
    env.pop("EXPERIMENT_COMPRESS", None)
    env.pop("EXPERIMENT_SUBCOMPACTIONS", None)
    env.update(changes)
    print(name, flush=True)
    proc = subprocess.Popen([str(binary), str(path), count], env=env)
    peak = 0
    while proc.poll() is None:
        total = 0
        for entry in path.iterdir():
            try:
                total += entry.stat().st_size
            except FileNotFoundError:
                pass  # RocksDB may delete a file while we sample.
        peak = max(peak, total)
        time.sleep(1)
    # Include a final sample when a small run finishes between one-second samples.
    peak = max(peak, sum(entry.stat().st_size for entry in path.iterdir()))
    if proc.returncode:
        raise RuntimeError(f"{name} exited {proc.returncode}")
    options_file = sorted(path.glob("OPTIONS-*"))[-1]
    options = options_file.read_text().splitlines()
    expected_subcompactions = "4" if "EXPERIMENT_SUBCOMPACTIONS" in changes else "1"
    assert f"  max_subcompactions={expected_subcompactions}" in options
    expected_compression = ["kLZ4Compression"] * 7
    if "EXPERIMENT_COMPRESS" not in changes:
        expected_compression[:2] = ["kNoCompression"] * 2
    expected_line = "  compression_per_level=" + ":".join(expected_compression)
    compression_lines = [line for line in options if line.startswith("  compression_per_level=")]
    assert compression_lines and all(line == expected_line for line in compression_lines)
    print(f"peak_sampled_bytes {peak}", flush=True)
