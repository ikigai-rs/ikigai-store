# Who holds the write handle — decided, and what it was decided on

**Status:** decided 2026-09-13, from measurement. This is the decision the rest of the
crate is built on, written before the backend rather than after it, because it is the
one choice that is expensive to reverse: it fixes what a second process can do, whether
a read can ever be cached, and whether the module is library-shaped or service-shaped.

---

## The question

RocksDB permits **one writer per directory**, enforced by a `LOCK` file, and that is a
property of the storage engine rather than a detail of this crate. So before any
endpoint exists, someone has to say which process holds the handle and for how long.
Two models were live:

- **A — long-lived exclusive handle.** Open once at construction, hold for the life of
  the module. The directory belongs to that process until it exits.
- **B — open per request.** Open, serve, drop. The lock is held only for the duration of
  a request, so another process can take the directory in between.

`ikigai-core` #110's probe had measured the cross-process half (a second read-write
`Store::open` on a live store is refused; `open_read_only` succeeds and is a **frozen
snapshot** that never sees a later commit). It had not measured the two things that
decide between A and B, and this arc's brief said not to carry either forward as
settled. So they were measured.

## What was measured

`tests/handle_model.rs`, `cargo test --release --features persistent`, on an
Apple M5 Max (18 cores), macOS 26.6.2, rustc 1.97.1, `oxigraph` 0.5.11. Release, not
debug: the numbers are about RocksDB's C++, and a debug build would be measuring the
wrong program.

### M1 — what `Store::open` costs on a populated store

The brief's framing was "tens of milliseconds is fine for a write Sink and ruinous on a
read path". The answer is smaller than that, and — the part that matters — **flat**:

| quads in the store | `Store::open` (3 rounds) | `open` + one-quad scan + drop |
| --- | --- | --- |
| 0 | 5.28 / 5.04 / 5.18 ms | 5.10 ms |
| 10,000 | 5.09 / 4.78 / 5.27 ms | 5.34 ms |
| 100,000 | 6.01 / 5.26 / 5.32 ms | 6.12 ms |
| 1,000,000 | 8.76 / 5.64 / 5.44 ms | 5.76 ms |

**Open is ~5 ms regardless of population** — a 100× growth in the dataset moved it by
less than the round-to-round noise. Opening does not read the data; it opens files and
replays whatever the WAL holds.

⚠ **The number that *is* population-dependent is not `open`, and it is worth recording
so nobody re-measures the wrong thing.** `Store::len()` is a full scan: 2.4 µs empty,
733 µs at 10k, 7.3 ms at 100k, **72 ms at 1M**. Any "how big is the store" face must
treat that as a scan, not as metadata.

So B was **not** ruled out on cost. 5 ms is affordable for a write Sink and a real but
survivable floor on a read.

### M2 — is a second open in the SAME process refused?

POSIX advisory locks do not conflict within a process, so the only thing that could
catch this is RocksDB's own registry. **It has one, and it refuses:**

```text
IO error: lock hold by current process, acquire time 1789326565
acquiring thread 6171930624: …/LOCK: No locks available
```

And, alongside a live writer in the same process, `Store::open_read_only` **succeeds** —
handing back the frozen snapshot #110 measured across processes. Both facts matter:
the first says B needs serialization, the second says the obvious workaround for B's
serialization is a stale-forever reader.

For completeness (M3): a re-open after the previous handle is **dropped** succeeds in
~7.5 ms and sees the data. Dropping the handle really does release the lock, which is
what made B worth costing at all.

## The decision: model A, the long-lived exclusive handle

`DurableStore` opens its directory once and holds the handle for its own lifetime. The
reasoning, in the order the evidence sits:

1. **M2 makes B's concurrency cap mandatory, not prudent.** Two requests arriving at once
   in one process would collide on the `LOCK` — a hard error out of RocksDB's guts on an
   ordinary concurrent read. B is therefore "open-per-request **plus** a
   concurrency-1 bulkhead", never open-per-request alone.
2. **That cap costs what RocksDB is built to give away.** RocksDB serves concurrent
   readers and one writer inside a process natively. Model A gets that for free; model B
   pays 5 ms of `open` per request *and* serializes every request behind every other one.
3. **B cannot cache, ever.** The whole point of releasing the lock between requests is
   that another process may take it — which means another process may write between two
   of our reads, and nothing cuts our golden thread when it does. Under B every read is
   `Expiry::Always` permanently, and work list item 4 is unreachable by construction.
4. **B's benefit was already delivered another way.** The motivating story for letting a
   second process in is "another instance benefits from work already done" — the cache
   ejection case. Brian settled that on 2026-09-13: the bundle is a **file** the second
   instance imports into its own store, not a shared store and not a read-only opener.
   With that settled, B buys intermittent, failure-prone access to a directory that no
   longer needs to be shared.
5. **B's failure mode is unbounded and per-request.** If a second process holds the lock
   when a request arrives, the request fails. Making that tolerable needs retry with
   backoff — real machinery (`ikigai-throttle`'s `Retry`) bolted on to restore a property
   model A has by construction.

**So: one process owns the directory until it exits, and the refusal is loud.** When the
path is already held, `DurableStore::open` returns a typed error naming the path and
saying what holds it, rather than letting RocksDB's internal string surface through a
panic. A second host starting up finds out in its first second, in a sentence.

### What this costs, stated plainly

The directory is unavailable to every other process for as long as the owning process
lives. A CLI invocation cannot read the daemon's store. That is a real operational
constraint and the honest answer to it is the wire: ikigai already has IPC and QUIC
transports and mount-over-wire federation, so the second process reaches the data by
resolving through the first. **That makes this module service-shaped**, and the
runbook for it is the owning host's, not this crate's.

## Where a `Throttle` overlay belongs — Brian's question, answered

> *"Would it be useful to put a throttle in front of the endpoint that is
> single-threaded, to keep the handling outside of the module and easily removed if the
> situation changes?"*

The instinct is right about **where** policy belongs. `ikigai-throttle`'s `Throttle` is
Nygard's Bulkhead — `limit(prefix, max)` caps concurrency per URI prefix with a
semaphore, parks the excess rather than erroring, longest prefix first, `Meta` passing
through uncapped. It is a `Space` overlay bound in the host, transparent to identity, so
the manifold is unchanged and removing it is one line of host wiring. Keeping a
serialization policy there rather than inside the module is exactly right when
serialization is what is wanted.

**But it cannot make the store safe to open twice, and it is not what model A needs.**

- The exclusion is **between processes** (RocksDB's `LOCK`); a semaphore is one
  process's memory. Two hosts each capping the prefix at 1 still collide on the second
  `Store::open`. No overlay can fix that, and none substitutes for the open failing
  loudly.
- Under model A a cap of **1 would be actively harmful**: it would serialize concurrent
  reads RocksDB handles natively, and because `Throttle` parks rather than rejects, one
  slow query would queue everything behind it with no bound. A cap used that way wants a
  `Timeout` inside it — machinery to contain a problem the cap created.

So this module declares no throttle and asks for none. The overlay stays available to a
host for the thing it is good at — capping a genuinely expensive prefix (a SPARQL query
face over a large store is a fair candidate) at a number greater than one, chosen by the
operator. Had model B won, the concurrency-1 cap would have been required, and putting
it in the overlay rather than in the module would have been the right way to hold it:
single-writer as a tunable rather than as an architecture. It did not win, so that stays
recorded here rather than built.

## What model A buys: reads that can be cached, and the seam where that ends

One process owning every write is exactly the **coverage** `ikigai-sparql`'s
`UPDATE_THREAD` documents as missing: the kernel cuts the thread named after a mutating
request's target on success, so a read that depends on the write endpoints' threads is
invalidated by every write **the kernel can see**. Under model A, with writes going
through `urn:iki:store:update` and `urn:iki:store:load` and no other process able to
open the directory, the kernel can see all of them — and the reads become cacheable for
real rather than aspirationally.

That guarantee has exactly one hole, and it is the crate's central API decision: **if
`ikigai-store` hands its `Arc<Store>` to anyone, it has handed out a writer the kernel
cannot see, and the coverage is gone.** Prose cannot hold that line — the handout would
be one accessor call in a host, months later, with the caching still declared. So it is
held by the constructors instead:

```rust
DurableStore::open(path)?         // owned:  nothing else can write. Reads are cacheable
                                  //         under the write threads.
DurableStore::open_shared(path)?  // -> (DurableStore, Arc<Store>)
                                  // shared: the handle leaves at construction, so an
                                  //         invisible writer may exist for the whole
                                  //         life of the store. Reads are Expiry::Always,
                                  //         permanently and by construction.
```

There is no accessor that turns the first into the second. A host that wants
`ikigai_sparql::space_with_store` over this store — the composition this crate's README
advertises — asks for `open_shared` and pays for it in cacheability, at the call site,
visibly, on the line where the choice is made.

## What would reopen this

- A host genuinely needing two processes read-write against one directory. The answer is
  still not B; it is the wire, or a different storage engine.
- A `Thread` that can produce a **witness** (a content digest or store revision rather
  than a per-process generation counter). That is what `cache-ejection.md` §1 wants for
  cross-process imports, and it would also let a read-only opener be refreshed honestly
  instead of frozen. It is a change to the thread model, not a store feature.
- RocksDB gaining a multi-process mode, or a swap to an engine that has one.
