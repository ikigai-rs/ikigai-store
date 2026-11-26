# Measuring RocksDB compression and subcompactions

The reported large-store cost and compaction peak motivated a 10-million-quad comparison. This
experiment changes Oxigraph itself in an isolated local copy; the store library does not expose
or apply these changes. The inputs and runner are in
[`reproductions/rocksdb-options`](../../reproductions/rocksdb-options/README.md).

## The configuration claim

The claim about the settings holds for Oxigraph 0.5.13 with its bundled RocksDB. Oxigraph calls
`rocksdb_options_optimize_level_style_compaction` before setting its general compression to LZ4.
RocksDB's optimization fills `compression_per_level` with no compression at indexes 0 and 1 and
LZ4 at indexes 2–6. That per-level vector remains in effect despite the later general compression
setting. The default `max_subcompactions` is 1. A fresh store's `OPTIONS-*` file confirms both.

There is a distinction between a file's level and its actual encoding. RocksDB's external SST writer
uses the last entry in `compression_per_level`, so the files made by the bulk loader can already be
LZ4-compressed when ingested. Later compactions may rewrite them according to the destination level.
The effective option alone does not establish how many bytes changing it will save.

Oxigraph's public `StoreOptions` in 0.5.13 only covers open-file limits and the descriptor reserve;
it exposes neither per-level compression nor subcompactions. A request to expose these controls is
filed as [oxigraph/oxigraph#2050](https://github.com/oxigraph/oxigraph/issues/2050). The same is true of the upstream
`main` source inspected at commit `9b0e27b2e8043b3eaa5664f40786d4fa5d6569ff`. These are separate
tradeoffs, so the comparison changes each one alone and then both. Raising a subcompaction limit
allows parallel work; it does not guarantee RocksDB schedules four subcompactions.

## Method

Each mode starts with an empty on-disk store. The same one-thread atomic bulk loader uses 10 MB
chunks and 10 million unique default-graph quads, with scattered subject IDs, a shared predicate,
and integer objects. It commits, records the elapsed load time and directory bytes, calls
`Store::optimize()`, records its time and directory bytes, and counts the quads. A separate process
samples apparent directory bytes once per second during both phases. These are sums of file lengths,
including logs and metadata; they are not allocated disk blocks, and sampling can miss a brief peak.
The run keeps the store open for the reported snapshots.

On 2026-11-26: Linux x86-64, Intel Core i7-11800H, 16 CPUs in the affinity mask, rustc 1.99.0,
release build, Oxigraph and oxrocksdb-sys 0.5.13, with the open-file soft limit set to 65,536. The
first baseline attempt with the default limit of 1,024 loaded the quads but `optimize()` failed
with too many open files; all comparisons below were rerun with the higher limit. The input's
small chunks generate many files and overlapping ranges, so its timings and byte ratios should not
be treated as measurements of the original 100M/1B workload. Each result is one run on this host,
without an idle-machine guarantee or statistical confidence interval.

## Results

| Setting | Load, s | Bytes at return | Optimize, s | Bytes after optimize | Sampled peak bytes |
|---|---:|---:|---:|---:|---:|
| Baseline | 136.124 | 1,176,028,180 | 5.648 | 1,690,491,455 | 2,322,380,385 |
| LZ4 at all levels | 137.549 | 1,169,748,336 | 6.011 | 1,167,845,702 | 2,313,669,214 |
| Four allowed subcompactions | 136.130 | 1,170,809,336 | 3.864 | 1,690,517,796 | 2,322,404,421 |
| Both | 135.772 | 1,165,526,022 | 3.907 | 1,167,825,450 | 2,315,489,043 |

Each store contained 10,000,000 quads after optimize. The `OPTIONS-*` files confirmed the intended
settings in each mode. With compression, the snapshot after optimize was about 31% smaller than the
baseline, but the sampled peak during the whole run changed by less than 1%. With the baseline
settings the snapshot grew after optimize, consistent with compaction rewriting compressed external
SSTs at an uncompressed level; this experiment did not inspect each file's encoding or level.
The baseline's optimized snapshot was about 169 bytes per quad, not the reported 318–328 bytes:
the fixture and conditions differ, so this does not reproduce the original size ratio.

The runs allowing four subcompactions had shorter optimize times on this host. These are single,
ordered runs; the experiment did not record the number RocksDB actually scheduled, isolate other
host activity, or measure query latency, CPU cost, or sustained write throughput. They do not prove
that increasing the setting will improve a production workload. The result supports offering the
settings for measurement, not imposing a new default. In particular, this does not establish the
reported 100M or 1B behavior, the time needed to settle those stores, or their peak disk use.
