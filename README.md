# ikigai-store

**The persistent RDF store for [ikigai](https://github.com/ikigai-rs)**: a dataset that
survives a process restart, opened from a path instead of rebuilt from its sources on
every boot.

> I think the original purpose was to have a store that was backed by a persistent
> mechanism like the rocksdb implementation. Keep it and we'll migrate it to that.
> — Brian, 2026-09-12

Nothing else in the ecosystem does that, and the gap is load-bearing: every host that
materializes an expensive graph recomputes it at startup. The reading room's books graph
derives from a 4.3 MB Zotero export; materializing a whole relational database is on the
roadmap. A store that survives a restart is the difference between those being viable and
being a warm-up cost.

```rust
use ikigai_store::{space, DurableStore, StoreConfig};

let config = StoreConfig::load(Some("my-host"))?;   // ~/.config/ikigai/store.toml
let store = DurableStore::open(&config.path)?;      // takes the write lock, for good
let kernel = Kernel::new(Arc::new(space(store)));
```

## What it binds

| resource | verb | what it does | capability |
| --- | --- | --- | --- |
| `urn:iki:store:select` | `Source` | SPARQL SELECT | `urn:cap:store:read` |
| `urn:iki:store:ask` | `Source` | SPARQL ASK | `urn:cap:store:read` |
| `urn:iki:store:construct` | `Source` | SPARQL CONSTRUCT | `urn:cap:store:read` |
| `urn:iki:store:describe` | `Source` | SPARQL DESCRIBE | `urn:cap:store:read` |
| `urn:iki:store:info` | `Source` | backing, quad count, coverage | `urn:cap:store:read` |
| `urn:iki:store:update` | `Sink` | SPARQL UPDATE | `urn:cap:store:write` |
| `urn:iki:store:load` | `Sink` | bulk-load an RDF document | `urn:cap:store:write` |

One IRI per query form, following `ikigai-sparql`: the form fixes the result family, so
it fixes the declared outputs and the default `as` too — and a query of another form is
**refused**, not served under an IRI that promised something else.

**A read is not free here.** That is the difference from a query module: `ikigai-sparql`
assembles its dataset per call from sources the caller already named, so gating it would
gate nothing. This store holds standing state the caller did not name — whatever the host
ever put in it.

The namespace is `urn:iki:store:`, **born migrated**. The ecosystem is moving 77
namespaces under `urn:iki:` one at a time, and a brand-new namespace is the only kind
whose migration is free.

## ⚠ One writer per directory — the constraint that shapes everything

RocksDB permits one writer per directory and enforces it with a `LOCK` file. That is a
property of the storage engine, not a detail, and it is why this module is
**service-shaped**:

- `DurableStore::open` takes the lock **for the life of the process** and does not give
  it back. A second host trying the same directory gets a legible `Error::Unavailable`
  naming the path — not a panic out of RocksDB's guts.
- So a second process reaches this data **over the wire**: ikigai's IPC and QUIC
  transports and mount-over-wire federation. The runbook for that is the owning host's.
- A read-only opener is **not** the way around it. `Store::open_read_only` succeeds
  beside a live writer and is a **frozen snapshot** — it never sees a later commit, and
  nothing says so. This crate offers no read-only face, and
  `a_read_only_opener_is_a_frozen_snapshot` keeps the reason reproducible.

The alternative — open per request, so the lock is held only during a call — was costed
against measurement and rejected. [`docs/design/handle-model.md`](docs/design/handle-model.md)
has the numbers (`Store::open` is ~5 ms and **flat** in population; a second open in the
*same* process is refused), the decision, and what would reopen it.

## Cacheable reads, and the one thing that forfeits them

One process owning every write is exactly the **coverage** `ikigai-sparql`'s
`UPDATE_THREAD` documents as the missing precondition for caching a shared store. The
kernel cuts the thread named after a mutating request's target, so a read depending on
`urn:iki:store:update` and `urn:iki:store:load` is invalidated by every write the kernel
can see — and under `DurableStore::open` there are no others.

⚠ **Handing out the `Arc<Store>` hands out a writer the kernel cannot see.** That is not
held by prose: it is held by the constructors.

```rust
let owned = DurableStore::open(&path)?;           // reads .cacheable() under the
                                                  // write threads
let (shared, handle) = DurableStore::open_shared(&path)?;  // reads Expiry::Always,
                                                           // permanently
```

There is no accessor that turns the first into the second. A host that wants
`ikigai_sparql::space_with_store` over this dataset asks for the `_shared` constructor
and pays for it in cacheability, at the call site, on the line where the choice is made.
`urn:iki:store:info` reports `covered: true|false` so an operator can see which it got.

## Features

| feature | what it adds | cost |
| --- | --- | --- |
| *(default)* | in-memory only, wasm-clean | — |
| `persistent` | `DurableStore::open`, the RocksDB backend | `oxrocksdb-sys` (~494 s of CPU on a cache miss) and `libclang` in the toolchain |

The same trade `ikigai-cli` makes for `quic` and `web`: in-memory is the default so the
wasm face and cheap tests survive, and the heavy backend is opted into.

⚠ **Cargo feature unification is global and additive**, and `rocksdb` is a *default*
feature of `oxigraph` that this crate turns off. The moment any crate in a host's graph
enables `persistent`, **every** `oxigraph` consumer in that build gets the RocksDB
backend, `ikigai-sparql`'s copy included. That is exactly what makes the
`space_with_store` composition work — one `Store` type either way — and it means "off by
default" is a property of the whole build, not of this crate.

`ci.yml` passes `features: persistent`, and must keep doing so: the durable backend lives
behind a `cfg(feature)` inside a *library*, which is not a target, so `--all-targets`
never reaches it and `ci-drift`'s `required-features` check cannot see the omission.

## Configuration

`~/.config/ikigai/store.toml` (or `$XDG_CONFIG_HOME/ikigai/store.toml`), layered the way
every ikigai config is — `<app>.store.toml` beside it overrides the keys one host differs
on:

```toml
path = "/Volumes/fast/ikigai-store"   # optional; default is ~/.ikigai/store
```

Config home for the setting, data home for the bytes. A relative `path` is resolved
against the data home, never against the working directory. **No environment variable
names the path**, ever — an env var is invisible to `ikigai config`, is not inherited by
a launchd agent, and two processes that disagree about it never meet. An unknown key, an
empty `path` and an unwritable directory are all loud.

## Status: `publish = false`, and the seven conditions are met

`ikigai-core` #110 set seven conditions for lifting the guard, and this version meets all
seven — durable backend, a `Sink` that writes through the kernel, declared = enforced
capabilities in both directions, a golden thread the writer cuts, typed inputs and an `as`
selector that refuses rather than substitutes, an `ikigai-conformance` walk, and a
namespace this crate owns.

The line stays on anyway, deliberately. **The last time it was absent, this crate was
published by accident** — a lockstep sweep shipped the in-memory placeholder as 0.1.70 on
2026-09-12, and that version is still on crates.io, unyanked, wearing the name of a
persistent store. Lifting `publish = false` is one line, and it is Brian's, together with
the yank.

## Where this lives, and why

Its own repo since 2026-09-13 (`ikigai-core` #110/#111), with the crate's 13 commits of
history carried across by `git subtree split`.

**Not because of wasm** — that claim is false and should stop being repeated. Core's
`wasm-check` runs with *default* features, and Oxigraph target-gates `oxrocksdb-sys` off
wasm regardless: measured 2026-09-12, a crate with RocksDB on `cargo check`s clean for
`wasm32-unknown-unknown` in 10 s, pulling no sys crate at all.

The reasons that survive that check: **native compile cost charged to the foundation**
(`oxrocksdb-sys` at 494 s of CPU on a cache miss, paid even under `cargo check` because
it is a build script, on the most-frequently-built workspace in the ecosystem, plus
`libclang` in its toolchain expectations and an edge runbook that already records an
OOM-killed compiler); **lockstep versioning**, which is how this crate got published by
accident; and **precedent twice in that same workspace** — `ikigai-fs` (#21) and
`ikigai-shacl` (#52), the latter a near-exact analogy.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option.
