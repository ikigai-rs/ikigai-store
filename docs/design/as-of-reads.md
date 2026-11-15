# As-of reads of a named graph

Ledger #3731. Design for the "view as of" verb from the hub's dynamic-contexts spike
(2026-11-14): a read pinned to a past time is a pure function, so it can be cached forever
and cited. Before this change the store had no history: a read answered the graph as it is.

## What a caller gets

`urn:iki:store:graph-{select,ask,construct,describe}` take an optional `as-of=T`, where `T`
is a time in milliseconds since the Unix epoch. The answer is the query evaluated over the
named graphs in `graph=` as they were at `T`.

- **Same grants as a live read.** `as-of` is checked after the per-graph read grants, so a
  past read never needs more or less authority than a live one. A context never widens
  authority.
- **Scoped forms only.** The broad forms (`urn:iki:store:select`, …) refuse `as-of`, naming
  the scoped form. History is kept per named graph; the default graph and "every graph" have
  none.
- **`T` must be in the past**, strictly before the store's clock. A read at "now" could still
  change when the next write lands, so it is refused instead of being answered under a
  cache promise it cannot keep.
- **Refused when the history does not reach `T`.** That covers a time before the graph's
  first recorded write, a time after a broad write (see below) and before the next scoped
  write, a time before a compaction, and any time after a purge. The refusal names why. A
  past answer is never computed from a partial history, because that would be a wrong answer
  that looked right.
- **Refused for a graph a writer outside this store may write** (`open_shared`, or a
  `SharerWrites` declaration naming the graph). Writes made through the raw handle are not
  logged.

## Cacheability

A past answer is `cacheable()` and depends on exactly one thread per graph:
`history_thread(G)` (`urn:iki:store:graph-history:<encoded G>`). No write cuts it. A live
scoped read still depends on `urn:iki:store:update`, `urn:iki:store:load` and
`graph_thread(G)`, as before. The tests check both.

## Mechanism (first cut)

An append-only log per named graph (`src/history.rs`), shared by every clone of the
`DurableStore`.

| Write | What the log records |
|---|---|
| `graph-update` to `G`, first one, or first since a gap | **base**: `G`'s whole contents after the write |
| `graph-update` to `G`, otherwise | **diff**: the quads added and removed, already computed by `confine::scoped_update` |
| `urn:iki:store:update`, `urn:iki:store:load` | **gap** in every graph's log: these doors can write any graph and do not say which |

Entries are recorded under the write lock, after the commit, so the log's order is the commit
order. Each entry is stamped with the wall clock in milliseconds, held monotonic: two writes in
one millisecond share a stamp, and a clock stepped back never orders a later write first.

To read as of `T`, the store folds the log's entries stamped at or before `T`: a base replaces
the state, a diff applies to it, a gap clears it. If the state is clear at the end, the read is
refused. Otherwise the folded quads go into a private in-memory store, and the query runs there
under the same confinement as a live scoped read.

## Valid time, then knowledge time

**Valid time first.** In this cut a write takes effect when it is recorded: no write can
backdate itself. So "what was true at `T`" and "what the store held at `T`" are the same
answer, and `as-of=T` is that answer.

**Knowledge time is the second step**, and needs writes that can state a valid time
different from their record time (a correction: "since last Tuesday, X"). Each log entry
would then carry two stamps, `valid_from` (given by the writer, defaulting to the record
time) and `recorded_at` (the clock). A read would take two coordinates: `as-of=T` (valid) and
`known-at=K` (knowledge), folding only the entries recorded at or before `K` and asking what
they say about `T`. Today's entries already store `recorded_at`, so this extends the log rather
than replacing it. The same rule holds: `K` must be in the past for the answer to be
cacheable.

## Retention and cost

- **Memory.** One base per graph per history segment (the graph's size when the segment
  started), plus every scoped write's diff since then. With no compaction it grows with the
  number of writes, without bound. `DurableStore::history_entries(G)` reports the count.
- **Not durable in this cut.** The log is held in memory. When a persistent store reopens, it
  starts with no history, and each graph's history restarts at its next scoped write. Moving
  the log into the store, so it shares the dataset's durability, is the next step. It needs a
  reserved key space that no SPARQL read can reach.
- **A broad write costs every graph its history**, until that graph's next scoped write takes
  a base, which costs one full copy of the graph. A host that wants long histories writes
  through `graph-update`.
- **Each as-of read folds and copies.** The cost is linear in the log up to `T` plus the
  graph's size at `T`. Past answers are cacheable, so the kernel pays this once per distinct
  query and time.

### Compaction

`DurableStore::compact_history(G, before)` folds every entry stamped at or before `before`
into one base at `before`. Reads as of `before` or later answer exactly as they did. Reads as
of an earlier time are refused afterward. This is the retention bound a host applies ("keep
30 days").

### Purge

`DurableStore::purge_history(G)` forgets `G`'s whole history. Every as-of read of `G` is
refused until a scoped write starts a new history from `G`'s contents at that time. A purge
does not touch the live graph: erasing the data is a write (`DROP GRAPH`), and a write is
recorded, so to erase both, drop the graph first, then purge.

**Cached past answers.** Purge and compact are store methods, not kernel requests, so they
cannot cut a thread themselves. A host that purges or compacts `G` calls
`kernel.cut(history_thread(G))` afterward. That clears every cached past answer folded from
the history it removed. The purge test does exactly this and then checks the refusal.

## Not in this cut

- A durable log (above).
- Knowledge time (above).
- `as-of` on the broad forms, on `graph-info`, or on `dump`.
- A kernel-facing door for purge and compaction, which would cut `history_thread(G)` itself
  under the write grant for `G`.
- Times written as `xsd:dateTime`. Milliseconds avoid a parser and a time-zone question in the
  first cut.
