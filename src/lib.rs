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
//! urn:iki:store:select     Source  SPARQL SELECT              urn:cap:store:read
//! urn:iki:store:ask        Source  SPARQL ASK                 urn:cap:store:read
//! urn:iki:store:construct  Source  SPARQL CONSTRUCT           urn:cap:store:read
//! urn:iki:store:describe   Source  SPARQL DESCRIBE            urn:cap:store:read
//! urn:iki:store:info       Source  backing, size, coverage    urn:cap:store:read
//! urn:iki:store:update     Sink    SPARQL UPDATE              urn:cap:store:write
//! urn:iki:store:load       Sink    bulk-load an RDF document  urn:cap:store:write
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
//! let space = space(store);                        // bind into a kernel
//! # }
//! # Ok(()) }
//! ```
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
//! depends on [`UPDATE_THREAD`] and [`LOAD_THREAD`] is invalidated by every write the
//! kernel can see — and under `DurableStore::open` there are no others.
//!
//! ⚠ **Handing out the `Arc<Store>` hands out a writer the kernel cannot see, and the
//! coverage is gone.** That is not held by prose here: it is held by the constructors.
//! `DurableStore::open_shared` returns the handle *at construction* and makes every
//! read `Expiry::Always` for the life of the store; there is no accessor that turns an
//! owned store into a shared one later. The choice is made at the call site, on the line
//! where it is paid for.
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
pub mod endpoints;
pub mod store;

pub use config::StoreConfig;
pub use endpoints::{space, CAP_READ, CAP_WRITE, LOAD_THREAD, UPDATE_THREAD};
pub use store::{Backing, DurableStore, Store};
