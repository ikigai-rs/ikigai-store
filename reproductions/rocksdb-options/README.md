# RocksDB options experiment

This runs Oxigraph 0.5.13 with the released RocksDB settings, with LZ4 at every level, with
`max_subcompactions = 4`, and with both changes. It modifies an isolated copy of the released
Oxigraph source under `target/`; it does not configure the store used by this crate. The copy
reads two experiment-only environment variables at open. `prepare.py` checks that its patch applies
exactly once. The C API calls run after Oxigraph's normal options are set and before column-family
options are copied.

From the repository root, with Python 3, Rust, libclang, a C++ toolchain and enough disk space:

```sh
ulimit -n 65536
python3 reproductions/rocksdb-options/prepare.py
CARGO_TARGET_DIR="$PWD/target" cargo build --locked --release --manifest-path reproductions/rocksdb-options/Cargo.toml
python3 -u reproductions/rocksdb-options/run.py 10000000
```

The 10-million-quad atomic load creates thousands of files; the first attempt with a soft limit of
1,024 failed during `optimize()` with too many open files. Set the limit above before running.

`run.py` refuses to overwrite existing run directories; remove `target/rocksdb-options/runs` to
start again. To prepare a new Oxigraph copy, remove `target/rocksdb-options/oxigraph` first. The
`OPTIONS-*` files in each run record the effective per-level compression and subcompaction setting.
The program counts the quads after optimizing. The runner samples the sum of file lengths in each
store directory once per second; a short-lived peak can be missed. This is apparent file size, not
allocated blocks, and includes logs and metadata. The reported return and optimized sizes are
snapshots while the store is open. Each mode uses a fresh store and the same generated data.

The input is 10 million unique default-graph quads with scattered subjects, one predicate and an
integer literal. The one-thread atomic bulk loader uses 10 MB chunks. This creates many overlapping
files for a test-sized run, but is not a substitute for measuring a particular production dataset,
loader configuration, or machine. Each result is a single run, with no statistical confidence claim.
See [the measurement](../../docs/design/rocksdb-options.md) for the recorded results and limitations.
