# As-of reads of a named graph

Ledger #3731. Status: **design note only; proposed, not implemented.** It covers the "view as
of" verb from the hub's dynamic-contexts spike (2026-11-14). A read pinned to a closed past
instant is a pure function, so it can be cached forever and cited. Today the store has no
history: a read answers the graph as it is now. `tests/as_of_baseline.rs` pins that fact. It
also pins a second fact this design depends on: a broad update writes a named graph without
naming it.

The first round of this PR also held a working first cut: an in-memory per-graph log, `as-of`
on the four scoped query forms, compaction and purge, and 8 tests. That cut is kept, unmerged
and with no PR, on branch `worker/item-3731-impl` (commit `7dd368f`). It makes choices that
belong to Brian, chiefly keeping history in memory. Those choices are now decisions 1 to 6
below, and the branch is only one possible answer to them.

This note follows ikigai-gonk `docs/contexts/rules-as-of.md` (gonk PR 377, "the gonk note"
below) wherever it applies. It lists every difference as a decision.

## What a caller would get

For a named graph `G` and a closed past instant `T`, the answer is the query evaluated over
`G` as the store's commits up to and including `T` left it.

- **Same grants as a live read.** Read grants are checked first, then the instant. A past read
  never needs more or less authority than a live one, and a context never widens authority.
  As in the gonk note, access checks come before existence checks and before any cache hit.
- **Scoped forms only:** `urn:iki:store:graph-{select,ask,construct,describe}`. History is kept
  per named graph. The default graph and "every graph" have none.
- **Refusals.** A future instant, a noncanonical instant, or `T` at the open edge of "now" is
  refused with `InvalidArgument`. A `T` the history does not cover is refused with `NotFound`,
  naming the reason: before coverage starts, across an unattributed write, before a
  compaction, or after a purge. These match the refusal classes in the gonk note. A partial
  history never produces an answer, because that answer would be wrong while looking right.
- **Refused for a graph that a writer outside the store may write**: `open_shared`, or a
  `SharerWrites` declaration that names the graph. The store does not see those writes.

## Time model

The gonk note recommends server **transaction time** for an unqualified "as of T": only facts
committed by `T` contribute. Each commit gets a globally ordered sequence number and a
nondecreasing UTC timestamp, and commits that share a timestamp are included together. If the
clock steps back, commits stop. It adds half-open **validity windows** `[valid_from,
valid_until)` on the records themselves. A separately named query with both `known-at` and
`valid-at` answers questions about corrected history, and it never changes what `as-of` means.

For the store:

- **Transaction time is the store's natural clock.** Every write goes through one write lock,
  so commit order is total. A sequence number and a timestamp taken under that lock give the
  gonk note's ordering for free.
- **Valid time is a property of the data, not of the store.** RDF in a graph can carry its own
  validity (for example `ex:validFrom` on a statement). The store cannot interpret it without
  a vocabulary. So "what was true at `T`" equals "what the store held at `T`" only for graphs
  whose writers never backdate anything. The item says "valid time first". In store terms the
  first step is transaction-time `as-of`, which is valid time under that no-backdating rule. A
  rule that evaluates validity windows belongs above the store, as in gonk.
- **Knowledge time later** (decision 2): `known-at=K` plus `valid-at=T`. This needs a write to
  carry a declared validity that differs from its commit time. Each commit record would then
  hold `recorded_at` (the clock) and the writer's `valid_from`.

## Mechanism options

Three places history can live. Decision 1 chooses between them.

| | A. In memory (the impl branch) | B. Versioned graphs in the RocksDB dataset | C. Append-only log on disk beside the dataset |
|---|---|---|---|
| Survives restart | No. The live graph survives and its history does not. | Yes, in the same RocksDB transaction as the write. | Only if every commit writes the log entry and the dataset atomically. Two files need a recovery rule. |
| Read cost at `T` | Fold base + diffs into a scratch store | Read a snapshot graph directly | Fold, as in A |
| Write cost | One diff in memory | One diff, or a full copy of the graph per version | One appended record |
| Reachable by SPARQL | No | Yes, unless the reserved names are confined away from every read and write door | No |
| Backup | Lost | Shares the dataset's backup | Must be backed up with the dataset, as one pair |
| Size | Bounded by RAM | Store bytes (measured below) | Smallest: raw diffs |

Option B, done carefully, is a diff log kept *inside* RocksDB under reserved graph names
`urn:iki:store:history:{G}:…`. The dataset already gives atomic commits, durability and
backup. Hiding those names from every read and write door would be new confinement work.
Today's "reserved graphs" (`SharerWrites`) are about a sharer, not about SPARQL visibility.
Snapshots are folded only at compaction points.

## Writes that do not name their graph

`urn:iki:store:update` and `urn:iki:store:load` can write any graph without saying which one
(`tests/as_of_baseline.rs`). A per-graph log fed only by `graph-update` would miss those writes
silently. The options are in decision 3.

## Retention, compaction, purge

- **Compaction** folds every entry up to an instant `C` into one base at `C`. Reads at `C` or
  later answer byte for byte as before. Earlier reads are refused with `NotFound`, never
  answered from the base. This is how a "keep N days" policy is applied.
- **Purge** forgets history. The gonk note treats a purge as an explicit exception to
  availability, never a rewrite. It records scope, reason and approvers. It cuts the
  retention and access threads and evicts cached copies. Afterward, reads return `NotFound`
  following the access checks, and an audit view shows a tombstone. A purge does not touch the
  live graph: erasing data is a write (`DROP GRAPH`), and a write is recorded. To erase both,
  drop the graph first, then purge.
- **Who may compact or purge** is decision 4.

## Cacheability

A past answer is `.cacheable()`. It depends on one thread per graph that no ordinary write
cuts: `urn:iki:store:graph-history:{G}`. Purge and compaction cut it. A live scoped read keeps
its current threads (`urn:iki:store:update`, `urn:iki:store:load`, `graph_thread(G)`). The gonk
note adds two rules that this design adopts:

- A past answer also carries explicit `immutable: true` metadata. Immutability is not inferred
  from an empty thread set.
- Failures, refusals and incomplete-history answers are never cached as immutable successes.

Pinned answers are cached under the kernel's ordinary eviction. "Valid forever" does not mean
"resident forever".

## Measured cost

**Measured** on 2026-11-15: arm64 macOS 27.2, `--release`, oxigraph 0.5.13 with its RocksDB
backend (the one `DurableStore::open` uses), using a throwaway test that is not committed. The
input was a synthetic graph shaped like a gonk ledger graph: 4,000 items with 12 quads each.
Each item had a title, a 1,200-byte body, state, two timestamps, an author IRI, 3 labels and
3 comments of 400 bytes. It is a stand-in for the real ledger, which this worker did not read.

| Quantity | Measured |
|---|---|
| Live graph | 48,000 quads; 14,898,680 bytes as N-Quads (3,724 per item) |
| RocksDB directory after `flush` and `optimize` | 53,871,917 bytes (about 3.6 times the N-Quads size, including RocksDB's own files) |
| One state change as a diff (remove 2 quads, add 2: `state` and `updated`) | 412 bytes as N-Quads |

**Estimates from those numbers** (arithmetic, not measured):

- A diff log grows by about 0.4 KB of N-Quads per state change. At 10 state changes per item,
  4,000 items add about 16 MB of N-Quads, about the live graph's own size. In the dataset
  (option B), at the measured ratio of about 3.6, that is about 60 MB. Comments and body edits
  cost their own size each time they change.
- Keeping a full copy per version instead of a diff costs the whole graph per write. That is
  about 15 MB of N-Quads for each of the 40,000 writes above, so it is ruled out for any graph
  that is written often.
- An as-of read folds the log up to `T`. Without a compaction base that is linear in the number
  of writes, so a periodic base bounds it. Past answers are cacheable, so the fold is paid once
  per distinct query and instant.

## Decisions for Brian

1. **Where history lives, and what survives a restart.**
   Options: (A) in memory, where history is lost at restart and the live graph is not; (B)
   in the RocksDB dataset, as a diff log under reserved graph names that every SPARQL door
   confines away, with periodic snapshot bases; (C) a separate append-only file beside the
   dataset.
   **Recommend B.** History commits in the same transaction as the write, survives restart,
   and shares the dataset's backup. That is what the gonk note asks of policy history ("one
   durable store transaction"). A is unacceptable beside a durable store, and C needs a
   two-file recovery rule for no gain.

2. **Time model.**
   Options: (a) server transaction time only; (b) transaction time now, with a separately
   named `known-at` plus `valid-at` later; (c) bitemporal from the start.
   **Recommend b**, as in the gonk note. Every commit gets a sequence number and a UTC
   timestamp under the write lock. Commits that share a timestamp are included together. A
   backward clock step stops commits; the impl branch instead holds the clock monotonic, which
   is the gonk note's rule relaxed, so this is a difference. No retroactive rewrite is allowed.
   Validity windows inside data are interpreted above the store.

3. **Writes that do not name their graph** (`urn:iki:store:update`, `urn:iki:store:load`).
   Options: (a) record a *gap* in every graph's history, so reads across it are refused; this
   is the impl branch's answer, and one broad write costs every graph its history until that
   graph's next scoped write takes a new base; (b) work out which graphs the write touched by
   diffing quads under the lock, and record precise per-graph diffs, at the cost of one
   changed-quad set per broad write; (c) refuse broad writes on a store with history enabled.
   **Recommend b.** The update already runs under the write lock, so history stays complete.
   I have not checked how to get the changed quads out of an oxigraph SPARQL update; it may
   need a before-and-after comparison of the graphs the update names. Fall back to (a) for a
   load that is too big to diff. The cost of (b) is not measured yet.

4. **Retention, compaction and purge, and who may do them.**
   Options for retention: indefinite (the gonk note's default for small policy records), or a
   per-graph "keep N days" with compaction. For authority: (a) the graph's write grant; (b) a
   new `urn:cap:store:history:retain:{G}` (and `…:purge:{G}`), separate from writing; (c) as
   (b), with the gonk note's proposal plus second-person approval for a purge.
   **Recommend**: retention indefinite by default, with per-graph compaction allowed, and
   (b) in the store. The approval workflow in (c) belongs to the host that issues the purge,
   because the store has no notion of people. Every purge writes a tombstone, cuts
   `graph-history:{G}`, and returns `NotFound` afterward. That is a difference from the impl
   branch, where purge is a store method with no grant and no tombstone.

5. **The as-of argument's name and format.**
   Options: (a) the argument `as-of=T` with `T` in milliseconds since the epoch (the impl
   branch); (b) the argument `as-of=T` with `T` a canonical UTC RFC 3339 instant with fixed
   nanosecond precision, as in the gonk note; (c) a keyed name, for example
   `urn:iki:store:graph-select:as-of:{instant}`, matching the gonk note's `…:as-of:{instant}`.
   **Recommend b now, with c's naming kept open.** Store reads are SPARQL forms with arguments,
   not resources keyed by subject, so an argument fits the existing doors. Using the gonk
   note's instant format lets one context value flow to both. Noncanonical spellings are
   refused before lookup, so one instant has one cache key. A host that wants a citable name
   can wrap the read in gonk's keyed composition.

6. **Cacheability of past answers.**
   Options: (a) `.cacheable()` hanging only on `graph-history:{G}` (the impl branch); (b) as
   (a), plus explicit `immutable: true` metadata, plus the closed-prefix rule: refuse, or wait
   for, a `T` whose commit prefix is still open.
   **Recommend b**, as in the gonk note. Refusals and partial answers are never cached. Caching
   per caller is not an issue here, because the store's answer does not vary by capability
   beyond the grant check, which runs before the cache.

7. **Scope of the first implementation.**
   Options: SELECT and CONSTRUCT only (the item's words), or all four scoped forms (the impl
   branch).
   **Recommend all four.** ASK and DESCRIBE share the same fold, so leaving them out saves no
   work.

## Tests the implementation must carry

These come from the item, with the gonk note's additions where they apply:

- An update after `T` is invisible as of `T`, for SELECT and CONSTRUCT, including a later
  delete.
- After a restart, the same as-of read answers byte for byte as before. This test follows
  from decision 1.
- A purge removes history as decision 4 says, and the cached past answer is cut.
- A past read is cacheable and immutable; a live read is not.
- Equal timestamps, boundary instants, future and noncanonical instants, and a broad write
  between two scoped ones, handled as decision 3 says.
- Same grants as a live read, with the grant checked before any cache hit.
