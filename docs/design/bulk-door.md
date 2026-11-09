# A bulk-load door, and how a reader knows the store is servable

**Status:** proposed 2026-11-08 (ledger #3273), from measurement. Nothing here is built yet: this
note is the design the item asks for first. `tests/bulk_servable.rs` holds the measurements and
one pin.

---

## The question

A host that needs to load a very large dataset has two choices today, and neither says when the
store is fit to answer again:

- **`urn:iki:store:load`**, one transaction per document, bounded by `max_load_bytes` (256 MiB
  unless the host raises it), parsed and committed on the writer thread.
- **`DurableStore::open_shared` and oxigraph's `BulkLoader`**, which makes every read
  uncacheable for the life of the store, and returns while the real work is still to be done.

Restores are the first consumer (architecture#2). The design has to settle the capability, the
chunking, the background priority, and a servable state that reads can observe.

## What the item claimed, and what held

The claims come from the Oxigraph load round (ledger #2684, on plasma, 2026-11-07). Its note is in
a repository this work could not read, so its numbers are quoted from the item and not re-derived.

| Claim | Found |
|---|---|
| The load door is quadratic | **No longer.** It was (two whole-store counts per call), and ledger #3270 removed them. `tests/write_cost.rs` pins a one-triple load to the same cost in a large store, and it passes. The door's limits now are the byte bound, the single transaction and the writer it holds. |
| After a bulk load the store answers nothing useful until compaction ends | **Reproduced in shape**, at 20M quads: see below. Not reproduced at 1B or for 4.5 hours. |
| 8,000 level-0 files | **Consistent, not confirmed.** An atomic load here returned with 8,003 `.sst` files. That counts files, not levels: oxigraph does not expose the level of a file. |
| RocksDB stopped writes at 1,000 | **Stall reproduced; the threshold not.** A write did block (below), but the vendored RocksDB defaults `level0_stop_writes_trigger` to 36 (`advanced_options.h`) and oxigraph does not set it, so 1,000 may be the count per column family when writes stopped rather than the trigger. |

## What was measured

`tests/bulk_servable.rs`, `--release --features persistent`, on this worker's arm64 macOS host,
rustc 1.99.1, oxigraph 0.5.13, oxrocksdb-sys 0.5.13. One thread and 10,000-quad chunks, so a load
a test can afford has the file shape of a far larger one (2,000 chunks at 20M quads); subjects are
scattered so chunks overlap. Each row is **one run**: the ratios are the shape of the effect, not
a benchmark. "At return" is 2,000 point reads with one write beside them, started the moment the
load returned.

| | atomic, 2M | atomic, 20M | `without_atomicity`, 20M |
|---|---|---|---|
| load | 1.45 s | 14.26 s | 18.74 s |
| `.sst` files at return | 803 | 8,003 | 4,247 |
| one write at return | 473 ms | **8.64 s** | 40 µs |
| 2,000 reads at return | 468 ms | **8.43 s** | 74 ms |
| 2,000 reads after `optimize` | 11.4 ms | 11.6 ms | 14.5 ms |
| reads at return / optimized | 41x | **730x** | 5.1x |
| `optimize` afterward | 0.34 s | 2.65 s | 7.60 s |

The stall is worse than a slow store: the first write waited 8.6 s, and every write door queues
behind one writer. A 1B-quad load has 50 times the data of the 20M row.

Three findings carry the design:

1. **The stall comes from the atomic commit.** An atomic load ingests every chunk at once when it
   commits; `without_atomicity` ingests chunk by chunk, so compaction runs during the load. It cost
   about 30% more load time here and removed the stall.
2. **Compaction is what makes reads fast, and it is unavoidable.** Even non-atomic reads are 5x
   off until `optimize` runs.
3. **Oxigraph exposes very little of RocksDB.** Its public surface is `bulk_loader` (with
   `with_num_threads`, `with_max_memory_size_in_megabytes`, `without_atomicity`, `on_progress`),
   `flush`, `optimize` and `backup`. It has no RocksDB property reads (`rocksdb_property_value`
   exists in the C API but the handle is private), no rate limiter, no compaction-thread control
   and no column-family options; `StoreOptions` holds only the file-descriptor settings.

## The design

### Shape

A host-level `DurableStore::bulk_load(reader, BulkOptions)` first, and a door
`urn:iki:store:bulk-load` over it once a restore has used it. The store stays sole writer (no
`open_shared`), so reads keep their caching once the state is back to serving.

### Capability: `urn:cap:store:bulk`, granted on its own

Neither `urn:cap:store:write` nor a graph-scoped write implies it. A bulk load takes the whole
store out of service for up to hours, and a dump spans every graph, so a per-graph scope cannot
describe it. Asking for the door without the grant is refused naming the grant, as the other
capability refusals do.

### Preconditions, refused before anything is written

- The store is empty. A non-atomic load has no rollback, and on an empty store a failed load is
  recoverable by deleting the directory. Merging into live data stays with `urn:iki:store:load`.
- This process is the sole writer (`is_sole_writer`).
- Input is N-Quads, what `urn:iki:store:dump` writes, read as a stream, so the 256 MiB slice-based
  `check_load` does not apply. **The triple-term depth bound must still hold**, per statement,
  because oxigraph handles nested terms recursively (`src/depth.rs`); the stream reader applies it
  before a statement reaches the loader. Blank nodes are kept, not renamed, so a restore equals its
  dump.

### Chunking: non-atomic, in oxigraph's default chunk

`without_atomicity`, with oxigraph's default of 1M quads per chunk unless the host bounds memory
through `BulkOptions`. The measurement above is why: the atomic commit is what stalls writes.
Atomicity is replaced by the empty-store precondition and the durable marker below.

### Background priority: what can be controlled, and what cannot

The door can bound its own CPU and memory with `with_num_threads` and the memory option. Its
default would be half the cores, leaving the rest for readers; oxigraph's own default is all of
them. It runs on a job thread of its own, not
the writer: `BulkLoader` writes sorted files and no memtable, so the skiplist trap (ledger #1127)
that put writes on the writer thread does not apply.

**It cannot lower the priority of compaction, or the I/O of it.** The item's SSD saturation is
RocksDB's own background pool, sized at open by oxigraph, with no rate limiter reachable from
here. That needs an upstream change: exposing RocksDB's rate limiter and the
`rocksdb.num-files-at-level0` and `rocksdb.compaction-pending` properties. Until then a host
schedules a restore in a quiet window; the note does not claim otherwise.

### The servable state

```text
Serving -> Loading { quads } -> Settling -> Serving
                 \_________________\______ Incomplete (a marker found at open)
```

- **Settling ends when `Store::optimize()` returns.** It is the only public signal that compaction
  has caught up, since the level-0 count cannot be read. Oxigraph warns it can take hours on huge
  databases: the door does not shorten that wait, it makes the wait visible and keeps writes away.
- **Reads observe it, and are not refused by it.** `DurableStore::servable()` for a host, and a
  `servable:` line in `urn:iki:store:info`, a plain `label: value` line like its others, with the
  phase named in words (for example `servable: false, settling`) so nothing depends on a code.
  Whether to serve a slow read is the caller's choice; a restored store is correct while settling,
  only slow. Reads are `Expiry::Always` until `Serving`.
- **Other writes are refused until `Serving`,** with core's typed transient `Unavailable`, instead
  of waiting behind a stall. The refusal names the phase and says to retry once `servable: true`.
- **The state survives a crash.** A marker file beside the dataset directory is written before the
  first chunk and removed after `optimize` returns. A store opened with the marker present is
  `Incomplete`: not the same as `Settling`, because it holds a partial load. It stays refused for
  writes until an operator removes the directory and runs the restore again. The marker sits beside
  the directory, not in it, so RocksDB never sees a foreign file.

## What this does not do

It does not make a 1B-quad store servable sooner than compaction allows; the 4.5 hours is compaction
time the door does not remove, though ingesting while loading should spread it through the load.
That has been measured at 20M quads only, and is the first thing to re-measure at 1B. It does not
throttle compaction I/O, which needs the upstream change above.

## Open questions

- Whether the non-atomic load's lead in reads at return holds at 1B quads and with real IRIs.
- Whether a host wants `Settling` to be skippable, serving at 5x slower reads rather than waiting.
- Whether `urn:iki:store:bulk-load` needs a time budget at all, as a job with its own state rather
  than a request.

When this is built, `ACCESSIBILITY.md` gains the `servable:` line and the new refusals, with their
wording pinned by tests.
