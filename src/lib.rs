//! **The persistent RDF store**: a dataset that survives a process restart, opened from
//! a path instead of rebuilt from its sources on every boot.
//!
//! > I think the original purpose was to have a store that was backed by a persistent
//! > mechanism like the rocksdb implementation. Keep it and we'll migrate it to that.
//! >   — Brian, 2026-09-12
//!
//! Nothing else in the ecosystem does that, and the gap is load-bearing: every host that
//! materializes an expensive graph recomputes it at startup. [`ikigai-sparql`] is a
//! *query* module and is explicit about not owning storage — its `space()` builds a
//! dataset per query and drops it, and `space_with_store(Arc<Store>)` takes a store the
//! **caller** owns without saying where its bytes live. This crate is the answer to
//! *where they live*.
//!
//! ```text
//! urn:iki:store:select           Source  SPARQL SELECT            urn:cap:store:read
//! urn:iki:store:ask              Source  SPARQL ASK               urn:cap:store:read
//! urn:iki:store:construct        Source  SPARQL CONSTRUCT         urn:cap:store:read
//! urn:iki:store:describe         Source  SPARQL DESCRIBE          urn:cap:store:read
//! urn:iki:store:graph-select     Source  SELECT, one graph        urn:cap:store:read:graph:<iri>
//! urn:iki:store:graph-ask        Source  ASK, one graph           urn:cap:store:read:graph:<iri>
//! urn:iki:store:graph-construct  Source  CONSTRUCT, one graph     urn:cap:store:read:graph:<iri>
//! urn:iki:store:graph-describe   Source  DESCRIBE, one graph      urn:cap:store:read:graph:<iri>
//! urn:iki:store:info             Source  backing, size, coverage  urn:cap:store:read
//! urn:iki:store:update           Sink    SPARQL UPDATE, all of it urn:cap:store:write
//! urn:iki:store:graph-update     Sink    SPARQL UPDATE, one graph urn:cap:store:write:graph:<iri>
//! urn:iki:store:load             Sink    bulk-load an RDF doc     urn:cap:store:write
//! ```
//!
//! One IRI per query form, following `ikigai-sparql`: the form fixes the result family,
//! so it fixes the declared outputs and the default `as` too, and a query of another
//! form is refused rather than served under an IRI that promised something else.
//!
//! **The namespace is `urn:iki:store:`, born migrated.** The ecosystem is moving 77
//! namespaces under `urn:iki:` one at a time and a brand-new one is the only kind whose
//! migration is free. It is deliberately NOT `urn:sparql:` — that belongs to
//! `ikigai-sparql`, and a host binding both would offer an agent two indistinguishable
//! query actions over two different stores, with the uncapped one answering unrestricted
//! queries.
//!
//! ```no_run
//! use ikigai_store::{space, DurableStore, StoreConfig};
//!
//! # fn demo() -> ikigai_core::Result<()> {
//! # #[cfg(feature = "persistent")] {
//! let config = StoreConfig::load(Some("my-host"))?;
//! let store = DurableStore::open(&config.path)?;   // takes the write lock, for good
//! let one = space(store.clone());                  // bind into a kernel
//! let two = space(store.clone());                  // …and into a second one
//! # }
//! # Ok(()) }
//! ```
//!
//! ⚠ **`open` ONCE per process, then clone the handle** — that is what the `.clone()` is
//! for, and it is the first thing a host gets wrong. RocksDB refuses a second open of the
//! same directory from the *same* process through its own in-process registry, so a host
//! with several kernels (several CLI modes, a test binary with a dozen) written the
//! obvious way gets [`Error::Unavailable`](ikigai_core::Error::Unavailable) on the
//! second. `DurableStore` is `Clone` over an `Arc<Store>`: clones share the dataset, the
//! write lock and the coverage flag. Each kernel still keeps its own cache, though, so a
//! write through one does not cut the other's golden threads — prefer one kernel per
//! process where you can.
//!
//! # Getting a value in without it becoming syntax
//!
//! A query and an update are strings, so a consumer interpolating user text into one is
//! standing on a security boundary whether it means to be or not. [`sparql`] is the one
//! answer: `bindings=` for queries, where the value never reaches the parser, and term
//! constructors for updates, where oxigraph offers no binding and this crate will not
//! fake one by rewriting text. Nothing there escapes anything — it builds RDF terms and
//! lets oxigraph serialize them, because the only correct escaper for a grammar is the
//! one that owns the grammar.
//!
//! # A tenancy boundary: scopes narrower than the whole dataset
//!
//! [`CAP_WRITE`] is all-or-nothing, so a module layered over this store makes its callers
//! hold `DROP ALL` to append one triple. `urn:iki:store:graph-update` is the narrow door:
//! an arbitrary UPDATE confined to one named graph under
//! [`cap_write_graph(graph)`](cap_write_graph). The scope is enforced on **effects, not
//! syntax** — the update runs against a private copy of that graph and is refused in full
//! if anything lands elsewhere — which is what makes it exact against
//! `DELETE WHERE { GRAPH ?g { … } }` and against a bare `INSERT DATA` that names no graph
//! at all. The mechanism and its costs are in `src/confine.rs`.
//!
//! [`CAP_READ`] is the matching problem in the other direction, and leaving it unsolved
//! left the boundary with a **documented bypass**: a module enforcing its own read
//! capability over the graph it owns could be gone around by a caller who holds the broad
//! grant and queries this store directly. `urn:iki:store:graph-{select,ask,construct,
//! describe}` close it under [`cap_read_graph(graph)`](cap_read_graph). That half is
//! confined **by construction** rather than by inspection: a query's dataset is a
//! first-class thing in SPARQL, so `graph=G` is set on the prepared query as exactly
//! `FROM <G> FROM NAMED <G>` and nothing is copied or diffed. `src/scope.rs` has the
//! mechanism, the per-shape probes, and the one surprise — `DESCRIBE` reads the dataset's
//! default graph and nothing else, upstream, in both doors.
//!
//! # One writer per directory — the constraint that shapes everything here
//!
//! RocksDB permits one writer per directory and enforces it with a `LOCK` file. That is
//! a property of the storage engine, not a detail, and it decides what this module is:
//!
//! - `DurableStore::open` (feature `persistent`) takes the lock **for the life of the
//!   process** and does not
//!   give it back. A second host trying the same directory gets a legible
//!   [`Error::Unavailable`](ikigai_core::Error::Unavailable) naming the path, not a
//!   panic out of RocksDB's guts.
//! - So a second process reaches this data **over the wire** — ikigai's IPC and QUIC
//!   transports and mount-over-wire federation — which makes a host binding this module
//!   service-shaped rather than library-shaped.
//! - A read-only opener is **not** the way around that. `Store::open_read_only` succeeds
//!   beside a live writer and is a **frozen snapshot**: it never sees a later commit and
//!   nothing says so. This crate does not offer one.
//!
//! The alternative — open per request, so the lock is held only during a call — was
//! costed against measurement and rejected. `docs/design/handle-model.md` has the
//! numbers, the decision, and what would reopen it. Read it before changing
//! [`DurableStore`].
//!
//! # Cacheable reads, and the one thing that forfeits them
//!
//! One process owning every write is exactly the **coverage** `ikigai-sparql`'s
//! `UPDATE_THREAD` docs name as the missing precondition for caching a shared store. The
//! kernel cuts the thread named after a mutating request's target, so a read that
//! depends on [`UPDATE_THREAD`], [`LOAD_THREAD`] and [`GRAPH_UPDATE_THREAD`] is
//! invalidated by every write the kernel can see — and under `DurableStore::open` there
//! are no others. ⚠ **Three writing IRIs means three threads**: adding a write door
//! without adding its thread leaves every cacheable read serving stale bytes after a
//! write through it, silently, on the branch that looks like success.
//!
//! ⚠ **Handing out the `Arc<Store>` hands out a writer the kernel cannot see, and the
//! coverage is gone.** That is not held by prose here: it is held by the constructors.
//! `DurableStore::open_shared` returns the handle *at construction* and makes every
//! read `Expiry::Always` for the life of the store; there is no accessor that turns an
//! owned store into a shared one later. The choice is made at the call site, on the line
//! where it is paid for.
//!
//! # Cache ejection
//!
//! `ikigai-core`'s `cache-ejection.md` stops because there is nowhere durable to put an
//! ejected cache. The bundle is a **file** a second instance imports into its own store
//! — not a shared store, which the one-writer rule forbids, and not a read-only opener,
//! which is frozen. `docs/design/cache-bundle.md` records what that asks of this crate;
//! nothing here exports or imports yet, and deliberately so.
//!
//! # Features
//!
//! | feature | what it adds | cost |
//! | --- | --- | --- |
//! | *(default)* | in-memory only, wasm-clean | — |
//! | `persistent` | `DurableStore::open`, the RocksDB backend | `oxrocksdb-sys` (~494 s of CPU on a cache miss) and `libclang` in the toolchain |
//!
//! ⚠ **`persistent` is a no-op on wasm, by construction.** RocksDB is native-linked, so
//! the durable constructors are gated off wasm entirely and a call there is a compile
//! error rather than a runtime fallback. That is not tidiness: a wasm-only fallback
//! branch would be code nothing lints, which is the failure family constitution 9c is
//! about. The wasm face is `DurableStore::in_memory`, or a durable store over the wire.
//!
//! The same trade `ikigai-cli` makes for `quic` and `web`: in-memory is the default so
//! the wasm face and cheap tests survive, and the heavy backend is opted into.
//!
//! ⚠ **Cargo feature unification is global and additive**, and `rocksdb` is a *default*
//! feature of `oxigraph` that this crate turns off. The moment any crate in a host's
//! graph enables `persistent` (or `oxigraph/rocksdb` directly), every other `oxigraph`
//! consumer in that build gets the RocksDB backend too, `ikigai-sparql`'s copy included.
//! That is exactly what makes the `space_with_store` composition work — one `Store` type
//! either way — and it means "off by default" is a property of the whole build, not of
//! this crate.
//!
//! [`ikigai-sparql`]: https://crates.io/crates/ikigai-sparql

pub mod config;
pub(crate) mod confine;
pub mod endpoints;
pub(crate) mod scope;
pub mod sparql;
pub mod store;

pub use config::StoreConfig;
pub use endpoints::{
    cap_read_graph, cap_write_graph, space, CAP_READ, CAP_READ_GRAPH, CAP_WRITE, CAP_WRITE_GRAPH,
    GRAPH_UPDATE_THREAD, LOAD_THREAD, UPDATE_THREAD,
};
pub use store::{Backing, DurableStore, Store};
