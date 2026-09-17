//! The durable dataset: who opens it, who may write to it, and what the handle
//! leaving this crate costs.
//!
//! Read `docs/design/handle-model.md` before changing anything here. The shape of
//! this module is a decision made from measurement, not a convenience.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
// `Path` is named only by the durable constructors; `PathBuf` by `Backing`, always.
#[cfg(all(feature = "persistent", not(target_family = "wasm")))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use ikigai_core::{Error, Result};
use oxigraph::model::{GraphNameRef, NamedOrBlankNode};

/// The canonical store type, re-exported so a host names ONE `Store`.
///
/// Rust unifies `oxigraph::store::Store` across crates only when every crate in the
/// build resolves the same `oxigraph`. Depending on `ikigai_store::Store` rather than
/// adding a direct `oxigraph` dependency makes that alignment structural instead of
/// coincidental — the same reason `ikigai-sparql` re-exports it.
pub use oxigraph::store::Store;

/// Where a [`DurableStore`]'s bytes live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backing {
    /// In memory: gone at process exit. The default, and the only backing that
    /// exists without the `persistent` feature or on wasm.
    Memory,
    /// A RocksDB directory this process holds the write lock on.
    Durable(PathBuf),
}

impl std::fmt::Display for Backing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backing::Memory => write!(f, "memory"),
            Backing::Durable(path) => write!(f, "durable {}", path.display()),
        }
    }
}

/// **Which graphs the holder of a shared handle may write** — a promise the host makes
/// at construction, and the only thing that can make a read of a shared store cacheable.
///
/// # ★ Why a promise is admissible here at all
///
/// A scoped read (`urn:iki:store:graph-{select,ask,construct,describe}`) is confined to
/// ONE named graph by construction: `src/scope.rs` sets the prepared query's dataset
/// specification to `[G]` before evaluation, so `graph=G` is exactly
/// `FROM <G> FROM NAMED <G>` and the query's whole universe is that graph. If the
/// invisible writer cannot write `G`, it cannot change that read's answer, and the read
/// is as covered as it would be on an owned store — under this crate's own three write
/// threads, which are the only remaining way `G` can change.
///
/// The **default graph needs no mention and cannot be named**: it has no IRI, so no
/// `urn:cap:store:read:graph:` token names it and no scoped query reaches it. A sharer
/// writing there is invisible to every scoped read by construction, which is why
/// [`only_the_default_graph`](SharerWrites::only_the_default_graph) is the promise that
/// buys the most and costs the least to keep.
///
/// # ⚠⚠ What a FALSE promise costs
///
/// Everything. `src/endpoints.rs` states the rule this crate is built on: *a thread that
/// is right on some writes and wrong on others is worse than no thread — "always fresh"
/// becomes "fresh until someone writes the other way, then stale with no bound and no
/// signal"*. Declare a graph the sharer does write, and reads of it are cached against
/// three threads that write never cuts: the kernel serves the pre-write bytes, for as
/// long as the process lives, with no error, no log line and no expiry to wait out. It
/// is the worst failure mode this crate has, and it is silent.
///
/// So the promise is deliberately awkward to give: every graph is named, there is no
/// "trust me" form, and a host that does not know what its sharer writes keeps the
/// blanket behaviour of `open_shared` — which is still the
/// right default and costs only speed.
///
/// # The tripwire
///
/// A promise nobody can check is documentation.
/// [`reserved_graphs_fingerprint`](DurableStore::reserved_graphs_fingerprint) is the
/// check: take one before driving the sharer and one after, and assert nothing moved.
///
/// ```
/// use ikigai_store::{DurableStore, SharerWrites};
///
/// # fn demo() -> ikigai_core::Result<()> {
/// let (store, handle) =
///     DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph())?;
/// let before = store.reserved_graphs_fingerprint()?;
///
/// // …drive the sharer exactly as the host does: here, straight through the handle.
/// handle
///     .load_from_slice(
///         oxigraph::io::RdfFormat::NTriples,
///         b"<http://ex/a> <http://ex/p> <http://ex/b> .",
///     )
///     .unwrap();
///
/// let changed = store.reserved_graphs_fingerprint()?.changed_since(&before);
/// assert!(changed.is_empty(), "the sharer wrote {changed:?}, which it promised not to");
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharerWrites {
    /// The named graphs the sharer may write. The default graph is always permitted and
    /// is never in here — it has no IRI to put there.
    named: BTreeSet<String>,
}

impl SharerWrites {
    /// The sharer writes the **default graph and nothing else**.
    ///
    /// This is the case worth having: `ikigai-browse` writes every quad it stores —
    /// annotations, the explanation archive, review passes — into the default graph,
    /// hard-coded, with no configuration knob. A host sharing its store with browse can
    /// say so, and every scoped read in that host goes back to being a cache hit.
    pub fn only_the_default_graph() -> Self {
        SharerWrites {
            named: BTreeSet::new(),
        }
    }

    /// …and also this named graph.
    ///
    /// One call per graph, on purpose: there is no prefix or wildcard form, for the same
    /// reason `urn:cap:store:write:graph:` grants have none — a promise about
    /// `urn:iki:x:*` would be a promise about graphs that do not exist yet, made by
    /// someone who cannot have checked them. This is what a host spells after
    /// `ikigai-browse` 0.4.0's `Mount::graph(G)` moves browse's writes into `G`.
    pub fn and_named_graph(mut self, iri: impl Into<String>) -> Self {
        self.named.insert(iri.into());
        self
    }

    /// [`and_named_graph`](Self::and_named_graph) over an iterator, for a host that
    /// already holds its mounts in a list.
    pub fn and_named_graphs<I, S>(self, iris: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        iris.into_iter().fold(self, Self::and_named_graph)
    }

    /// Whether the sharer may write the named graph `iri` — the question
    /// [`DurableStore::read_is_covered`] asks, exactly once per read.
    pub fn may_write(&self, iri: &str) -> bool {
        self.named.contains(iri)
    }

    /// The named graphs promised to the sharer, sorted.
    pub fn named_graphs(&self) -> impl ExactSizeIterator<Item = &str> {
        self.named.iter().map(String::as_str)
    }
}

/// What this crate can see of the writes to its dataset. Private: the three cases are
/// reached through the constructors and read through
/// [`is_sole_writer`](DurableStore::is_sole_writer),
/// [`sharer_writes`](DurableStore::sharer_writes) and
/// [`read_is_covered`](DurableStore::read_is_covered).
#[derive(Clone, Debug)]
enum Coverage {
    /// The handle never left: every write is a write the kernel saw.
    Owned,
    /// The handle left and nothing was said about where it writes. Every read is
    /// `Expiry::Always`, which is what `open_shared` has always meant.
    Shared,
    /// The handle left and the host named the graphs it may write.
    SharedDeclaring(SharerWrites),
}

/// An RDF dataset this crate owns the bytes of.
///
/// # The central API decision: `open` versus `open_shared`
///
/// The kernel can invalidate a cached read only when it can SEE every write. A raw
/// `Arc<Store>` in someone else's hands is a writer the kernel cannot see, so handing
/// one out forfeits the golden-thread coverage this crate exists to provide
/// (`ikigai-sparql`'s `UPDATE_THREAD` docs work the argument through).
///
/// Prose cannot hold that line — the handout would be one accessor call in a host,
/// months later, with the caching still declared. So the modes are constructors, and
/// **there is no accessor that turns the first into the others**:
///
/// - `open` (feature `persistent`) / [`in_memory`](DurableStore::in_memory) — *owned*.
///   Nothing else holds the handle and (durably) no other process can even open the
///   directory, so reads are `.cacheable()` under the write endpoints' threads.
/// - `open_shared` / [`in_memory_shared`](DurableStore::in_memory_shared) — the handle leaves at
///   construction, for a host that wants `ikigai_sparql::space_with_store` over the
///   same dataset. An invisible writer may exist for the whole life of the store, so
///   reads are `Expiry::Always`, permanently and by construction.
/// - `open_shared_declaring` (feature `persistent`) /
///   [`in_memory_shared_declaring`](DurableStore::in_memory_shared_declaring) — the
///   handle leaves, **and the host names the graphs it may write** ([`SharerWrites`]).
///   A scoped read of a graph outside that set is cacheable again; every other read is
///   `Expiry::Always` exactly as above. Read [`SharerWrites`] before using it: the
///   promise is load-bearing and a false one is silent, unbounded staleness.
///
/// The choice is therefore made at the call site, visibly, on the line where the cost
/// is taken. [`is_sole_writer`](DurableStore::is_sole_writer) reports whether the handle
/// stayed; [`read_is_covered`](DurableStore::read_is_covered) answers the per-read
/// question the endpoints actually ask.
#[derive(Clone)]
pub struct DurableStore {
    store: Arc<Store>,
    backing: Backing,
    coverage: Coverage,
    /// Serializes every write this crate performs — see [`write_lock`](Self::write_lock).
    writes: Arc<Mutex<()>>,
}

/// Hand-written because `oxigraph::store::Store` has no `Debug` — and because the two
/// fields that matter to a reader are the ones a derive could not have chosen to print.
impl std::fmt::Debug for DurableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableStore")
            .field("backing", &self.backing)
            .field("coverage", &self.coverage)
            .finish_non_exhaustive()
    }
}

impl DurableStore {
    /// An empty in-memory dataset, owned. Nothing survives process exit.
    pub fn in_memory() -> Result<Self> {
        Ok(DurableStore {
            store: Arc::new(Store::new().map_err(endpoint_err)?),
            backing: Backing::Memory,
            coverage: Coverage::Owned,
            writes: Arc::new(Mutex::new(())),
        })
    }

    /// An in-memory dataset **and a clone of its handle**. See the type docs: taking
    /// the handle makes every read `Expiry::Always` for the life of this store.
    pub fn in_memory_shared() -> Result<(Self, Arc<Store>)> {
        let mut this = Self::in_memory()?;
        this.coverage = Coverage::Shared;
        let handle = Arc::clone(&this.store);
        Ok((this, handle))
    }

    /// [`in_memory_shared`](Self::in_memory_shared), with the host declaring which
    /// graphs the handle's holder may write.
    ///
    /// ⚠ Read [`SharerWrites`] first. A promise that is wrong makes reads of a graph the
    /// sharer does write stale with no bound and no signal.
    pub fn in_memory_shared_declaring(writes: SharerWrites) -> Result<(Self, Arc<Store>)> {
        let mut this = Self::in_memory()?;
        this.coverage = Coverage::SharedDeclaring(writes);
        let handle = Arc::clone(&this.store);
        Ok((this, handle))
    }

    /// Open the durable dataset at `path`, taking the write lock for the life of this
    /// process (model A — see `docs/design/handle-model.md`).
    ///
    /// The directory is created if it does not exist. **The refusal when it is already
    /// held is loud and legible**: [`Error::Unavailable`] naming the path and what to
    /// look for, rather than RocksDB's internal lock string surfacing raw. The error is
    /// typed `Unavailable` rather than `Endpoint` because it is exactly the transient,
    /// retryable shape the overlays gate on — the holder may exit.
    ///
    /// ⚠ **This constructor does not exist on wasm**, even with `persistent` enabled:
    /// RocksDB is native-linked and Oxigraph target-gates it out. A call is then a
    /// compile error naming the missing item — which is both louder and earlier than a
    /// runtime refusal, and leaves no wasm-only code path that no CI job can reach. (A
    /// store that quietly was not durable is the failure this crate is named after; a
    /// store whose *fallback* is only exercised on a target nothing lints is the next
    /// one.) The wasm face is [`in_memory`](DurableStore::in_memory), or a durable store
    /// reached over the wire.
    #[cfg(all(feature = "persistent", not(target_family = "wasm")))]
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(DurableStore {
            store: Arc::new(open_rocksdb(path.as_ref())?),
            backing: Backing::Durable(path.as_ref().to_path_buf()),
            coverage: Coverage::Owned,
            writes: Arc::new(Mutex::new(())),
        })
    }

    /// `open` **and a clone of the handle**. See the type docs:
    /// taking the handle makes every read `Expiry::Always` for the life of this store.
    #[cfg(all(feature = "persistent", not(target_family = "wasm")))]
    pub fn open_shared(path: impl AsRef<Path>) -> Result<(Self, Arc<Store>)> {
        let mut this = Self::open(path)?;
        this.coverage = Coverage::Shared;
        let handle = Arc::clone(&this.store);
        Ok((this, handle))
    }

    /// [`open_shared`](Self::open_shared), with the host declaring which graphs the
    /// handle's holder may write.
    ///
    /// ```no_run
    /// use ikigai_store::{DurableStore, SharerWrites};
    ///
    /// # fn demo() -> ikigai_core::Result<()> {
    /// # #[cfg(all(feature = "persistent", not(target_family = "wasm")))] {
    /// // The sharer is `ikigai-browse`, which writes the default graph and nothing else.
    /// let (store, handle) = DurableStore::open_shared_declaring(
    ///     "/var/lib/host/store",
    ///     SharerWrites::only_the_default_graph(),
    /// )?;
    ///
    /// // …or a sharer that also owns one named graph of its own.
    /// let (store, handle) = DurableStore::open_shared_declaring(
    ///     "/var/lib/host/store",
    ///     SharerWrites::only_the_default_graph().and_named_graph("urn:iki:browse:notes"),
    /// )?;
    /// # }
    /// # Ok(()) }
    /// ```
    ///
    /// ⚠ Read [`SharerWrites`] first, and pin the promise with
    /// [`reserved_graphs_fingerprint`](Self::reserved_graphs_fingerprint) in the host's
    /// own tests. A promise that is wrong makes reads of a graph the sharer does write
    /// stale with no bound and no signal.
    #[cfg(all(feature = "persistent", not(target_family = "wasm")))]
    pub fn open_shared_declaring(
        path: impl AsRef<Path>,
        writes: SharerWrites,
    ) -> Result<(Self, Arc<Store>)> {
        let mut this = Self::open(path)?;
        this.coverage = Coverage::SharedDeclaring(writes);
        let handle = Arc::clone(&this.store);
        Ok((this, handle))
    }

    /// Where this store's bytes live.
    pub fn backing(&self) -> &Backing {
        &self.backing
    }

    /// **Whole-dataset provenance**: `true` when this crate is the only writer of this
    /// dataset, because the handle never left it — so every write to it is a write the
    /// kernel saw. `false` after any of the `*_shared*` constructors, declaring or not.
    ///
    /// It answers *did the handle escape*, and nothing else. In particular it is **not
    /// the caching question**: [`read_is_covered`](Self::read_is_covered) is, and on a
    /// store built with `open_shared_declaring` a scoped read of a graph the sharer
    /// cannot write is cached while this is still `false`.
    ///
    /// ⚠ **The two were one bit until 0.2.4**, and separating them is why this method is
    /// named for the handle rather than for coverage. `is_covered` was the old spelling
    /// and is [deprecated](Self::is_covered): it kept its name, its signature and its
    /// return type across a release that inverted what it means for one store mode, so a
    /// caller reading it the obvious way — *may I cache, or do I need a freshness
    /// wrapper* — gets the answer backwards on `open_shared_declaring` with no build
    /// error, no lint and no failing test, because the wrong branch is slower rather than
    /// wrong. `ikigai-gonk`'s `compose_with` asked exactly that and would have wrapped
    /// precisely the store that needs no wrapper (~1076× pessimization dressed as a
    /// correctness guard); it was caught by a human having written a warning down, which
    /// is not a mechanism. The deprecation is that warning escalated to something the
    /// compiler says.
    pub fn is_sole_writer(&self) -> bool {
        matches!(self.coverage, Coverage::Owned)
    }

    /// The 0.2.0–0.2.4 spelling of [`is_sole_writer`](Self::is_sole_writer).
    ///
    /// Kept as an exact delegate — the value is unchanged for every store mode, so this
    /// is a rename and not a behaviour change. See [`is_sole_writer`](Self::is_sole_writer)
    /// for why the name moved.
    #[deprecated(
        since = "0.2.5",
        note = "renamed to `is_sole_writer`. It answers WHOLE-DATASET PROVENANCE — did the \
                handle leave this crate — and it is NOT the caching question. Its meaning \
                inverted for `open_shared_declaring` stores in 0.2.4 while its name, \
                signature and return type stayed identical: such a store is `false` here \
                while its scoped reads ARE cached. For \"may I cache this read / do I need \
                a freshness wrapper\", call `read_is_covered(Option<&str>)` with the graph \
                the read is confined to (`None` for a read that sees the whole dataset)."
    )]
    pub fn is_covered(&self) -> bool {
        self.is_sole_writer()
    }

    /// The graphs the host declared its sharer may write, or `None` — either because the
    /// handle never left (nothing to declare) or because it left undeclared.
    ///
    /// [`is_sole_writer`](Self::is_sole_writer) tells those two apart.
    pub fn sharer_writes(&self) -> Option<&SharerWrites> {
        match &self.coverage {
            Coverage::SharedDeclaring(writes) => Some(writes),
            Coverage::Owned | Coverage::Shared => None,
        }
    }

    /// **The per-read question**: may a read whose universe is `graph` be cached under
    /// this crate's write threads?
    ///
    /// `graph` is `Some(iri)` for a scoped read — confined to that named graph by
    /// `src/scope.rs`, by construction — and `None` for every read that sees the whole
    /// dataset: the four broad query faces and `urn:iki:store:info`. A scoped read over
    /// SEVERAL graphs asks once per graph and is covered only if every answer is yes.
    ///
    /// | coverage | `None` (whole dataset) | `Some(G)` |
    /// | --- | --- | --- |
    /// | owned | covered | covered |
    /// | shared, undeclared | bare | bare |
    /// | shared, declaring `W` | **bare** | covered iff `G ∉ W` |
    ///
    /// ★ The `None` column is bare for BOTH shared modes and that is not conservatism
    /// for its own sake: a broad read sees the default graph, the sharer may always
    /// write the default graph (that is the point of sharing), and no declaration this
    /// type can express says otherwise. A host that wants its broad reads cached wants
    /// `open`.
    pub fn read_is_covered(&self, graph: Option<&str>) -> bool {
        match (&self.coverage, graph) {
            (Coverage::Owned, _) => true,
            (Coverage::Shared, _) => false,
            (Coverage::SharedDeclaring(_), None) => false,
            (Coverage::SharedDeclaring(writes), Some(graph)) => !writes.may_write(graph),
        }
    }

    /// **The tripwire for a [`SharerWrites`] promise**: a fingerprint of every named
    /// graph the declaration keeps away from the sharer.
    ///
    /// Take one before driving the sharer, one after, and ask
    /// [`changed_since`](GraphFingerprint::changed_since) what moved. Nothing should
    /// have. `ikigai-gonk` wrote this assertion by hand
    /// (`a_browse_write_touches_no_named_graph`) against the set of named graph NAMES,
    /// which catches a sharer that creates a graph and misses one that writes into a
    /// graph that already exists; this covers both, because it fingerprints the quads.
    ///
    /// ⚠ **Take both fingerprints around the SHARER's write and nothing else.** A write
    /// through this crate's own endpoints changes a reserved graph legitimately — it cut
    /// a thread when it did — and would show up here as a broken promise.
    ///
    /// ⚠ **This is a test-time check, not a request-time one.** It scans every quad of
    /// every reserved graph (`Store::len` itself is a full scan — 72 ms at 1M quads, see
    /// `docs/design/handle-model.md`), so it belongs beside the host's other assertions
    /// about its own composition, not on a read path.
    ///
    /// # Errors
    ///
    /// [`Error::Endpoint`] when this store carries no declaration. ★ Deliberately an
    /// error rather than an empty fingerprint: an empty one would make a host's tripwire
    /// pass vacuously on a store where nothing was promised, which is exactly the gate
    /// that silently covers less than it appears to.
    pub fn reserved_graphs_fingerprint(&self) -> Result<GraphFingerprint> {
        let Some(writes) = self.sharer_writes() else {
            return Err(Error::Endpoint(format!(
                "this store declares no `SharerWrites`, so there is nothing for a \
                 fingerprint to guard ({}). Build it with `open_shared_declaring` / \
                 `in_memory_shared_declaring` — an empty fingerprint here would let the \
                 host's tripwire pass without checking anything",
                if self.is_sole_writer() {
                    "the handle never left this crate"
                } else {
                    "the handle left undeclared, so no graph is reserved from it"
                }
            )));
        };
        let mut graphs = BTreeMap::new();
        for name in self.store.named_graphs() {
            let name = name.map_err(endpoint_err)?;
            if let NamedOrBlankNode::NamedNode(iri) = &name {
                if writes.may_write(iri.as_str()) {
                    continue;
                }
            }
            // A blank-node graph name can never be promised — a declaration names IRIs —
            // so it is always reserved, and always fingerprinted.
            let mut quads = 0usize;
            let mut checksum = 0u64;
            for quad in self.store.quads_for_pattern(
                None,
                None,
                None,
                Some(GraphNameRef::from(name.as_ref())),
            ) {
                let quad = quad.map_err(endpoint_err)?;
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                // The N-Quads text of the quad, so the fingerprint is over RDF and not
                // over oxigraph's internal encoding. XOR-folded because a store is a SET
                // — no quad appears twice, so nothing can cancel — and because the
                // iteration order of a scan is not a thing to depend on.
                quad.to_string().hash(&mut hasher);
                checksum ^= hasher.finish();
                quads += 1;
            }
            graphs.insert(name.to_string(), GraphDigest { quads, checksum });
        }
        Ok(GraphFingerprint { graphs })
    }

    /// The dataset, for this crate's endpoints. Deliberately **not** `pub`: see the
    /// type docs for why handing this out is a constructor-level decision.
    pub(crate) fn dataset(&self) -> &Store {
        &self.store
    }

    /// Serialize this crate's writes against each other.
    ///
    /// ★ **Needed because the graph-scoped write is a read-modify-write.**
    /// `urn:iki:store:graph-update` runs the caller's UPDATE against a private copy of
    /// one graph and then applies the delta (the mechanism is in `src/confine.rs`), so two
    /// concurrent scoped writes to the same graph could otherwise lose one of them — and this
    /// kernel really can run requests concurrently (`Kernel::into_scheduled`, and
    /// `Invocation::fan_out`). Every write door in this crate takes the lock, including
    /// the two that would be atomic without it, because a lock that only *some* writers
    /// take orders nothing.
    ///
    /// ⚠ It covers the writes **this crate** performs, which is exactly the coverage
    /// [`is_sole_writer`](Self::is_sole_writer) already describes: after `open_shared`
    /// the raw handle is a writer that takes no lock, and a scoped write can then lose an
    /// update to it. That is one more cost of handing out the handle, and the same one
    /// the golden thread pays.
    ///
    /// A poisoned lock is treated as held-and-released rather than fatal: the data is an
    /// `Arc<Store>` that no panic here can leave half-written, since every mutation goes
    /// through an oxigraph transaction that either commits or does not.
    pub(crate) fn write_lock(&self) -> MutexGuard<'_, ()> {
        self.writes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What one reserved graph held when it was fingerprinted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GraphDigest {
    quads: usize,
    checksum: u64,
}

/// A point-in-time fingerprint of the named graphs a [`SharerWrites`] declaration keeps
/// away from the sharer — see
/// [`DurableStore::reserved_graphs_fingerprint`](DurableStore::reserved_graphs_fingerprint).
///
/// Compare two with [`changed_since`](Self::changed_since) rather than `assert_eq!`: the
/// list of graph IRIs that moved is what a failing host test needs to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphFingerprint {
    graphs: BTreeMap<String, GraphDigest>,
}

impl GraphFingerprint {
    /// The reserved graphs that differ between `before` and `self`, sorted: one that
    /// gained or lost quads, one that appeared, one that vanished.
    ///
    /// Each name is in N-Quads graph-name form (`<urn:…>`, or `_:b0` for the blank-node
    /// graph name a declaration can never permit), so a failure message names the graph
    /// the way the data does.
    pub fn changed_since(&self, before: &Self) -> Vec<String> {
        let mut changed: Vec<String> = self
            .graphs
            .iter()
            .filter(|(name, digest)| before.graphs.get(*name) != Some(digest))
            .map(|(name, _)| name.clone())
            .collect();
        changed.extend(
            before
                .graphs
                .keys()
                .filter(|name| !self.graphs.contains_key(*name))
                .cloned(),
        );
        changed.sort();
        changed.dedup();
        changed
    }

    /// How many named graphs this fingerprint covers.
    pub fn len(&self) -> usize {
        self.graphs.len()
    }

    /// Whether it covers none. ⚠ True on a dataset whose reserved graphs do not exist
    /// **yet** — which is the normal state at the start of a host's test, and the reason
    /// [`changed_since`](Self::changed_since) reports an APPEARING graph as a change.
    pub fn is_empty(&self) -> bool {
        self.graphs.is_empty()
    }
}

/// Open (creating if needed) a RocksDB-backed store, mapping the one failure an
/// operator will actually hit into a sentence.
#[cfg(all(feature = "persistent", not(target_family = "wasm")))]
fn open_rocksdb(path: &Path) -> Result<Store> {
    // Fail loud on an unwritable path, and say which path. Without this the first
    // symptom is RocksDB's own errno string with no ikigai context around it.
    if let Err(e) = std::fs::create_dir_all(path) {
        return Err(Error::Endpoint(format!(
            "cannot create the store directory {}: {e}",
            path.display()
        )));
    }
    Store::open(path).map_err(|e| {
        let text = e.to_string();
        if is_lock_error(&text) {
            // ⚠ Matched on text because RocksDB reports the lock as a generic IO error
            // and oxigraph passes it through untyped.
            // `a_second_open_in_the_same_process_is_refused_and_the_refusal_is_legible`
            // in tests/handle_model.rs pins this against a real second open, so an
            // upstream rewording fails a test instead of silently degrading the message
            // to the raw string.
            Error::Unavailable(format!(
                "the store at {} is already open read-write and RocksDB permits one \
                 writer per directory. Another ikigai host (or another handle in this \
                 process) holds it; that process must exit, or this one must reach the \
                 data over the wire. Underlying error: {text}",
                path.display()
            ))
        } else {
            Error::Endpoint(format!("opening the store at {}: {text}", path.display()))
        }
    })
}

/// Whether a RocksDB error string is the one-writer-per-directory refusal.
///
/// Two spellings are known and both are matched: `Resource temporarily unavailable`
/// (a second PROCESS, a POSIX advisory lock conflict) and `lock hold by current
/// process` / `No locks available` (a second handle in the SAME process, caught by
/// RocksDB's own in-process registry — measured 2026-09-13, see the design doc).
#[cfg(all(feature = "persistent", not(target_family = "wasm")))]
fn is_lock_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("lock") && (lower.contains("unavailable") || lower.contains("no locks"))
}

fn endpoint_err(e: impl std::fmt::Display) -> Error {
    Error::Endpoint(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owned_in_memory_store_is_the_sole_writer() {
        let store = DurableStore::in_memory().unwrap();
        assert!(store.is_sole_writer());
        assert_eq!(store.backing(), &Backing::Memory);
    }

    /// The deprecated 0.2.0–0.2.4 spelling still answers, and answers identically, on
    /// every mode — a rename is only free if the old name is an exact delegate, and that
    /// is a property of a pair of values, so nothing else can assert it.
    #[test]
    // The one place in this crate that may name the deprecated method: pinning that it
    // still delegates is the whole point of the test.
    #[allow(deprecated)]
    fn the_deprecated_spelling_delegates_exactly() {
        let owned = DurableStore::in_memory().unwrap();
        assert_eq!(owned.is_covered(), owned.is_sole_writer());
        assert!(owned.is_covered());

        let (shared, _h) = DurableStore::in_memory_shared().unwrap();
        assert_eq!(shared.is_covered(), shared.is_sole_writer());
        assert!(!shared.is_covered());

        let (declared, _h) = DurableStore::in_memory_shared_declaring(
            SharerWrites::only_the_default_graph().and_named_graph("urn:example:zenith"),
        )
        .unwrap();
        assert_eq!(declared.is_covered(), declared.is_sole_writer());
        assert!(
            !declared.is_covered(),
            "the mode the deprecation exists for: `false` here while its scoped reads \
             ARE cached — see `read_is_covered`"
        );
        assert!(declared.read_is_covered(Some("urn:example:acme")));
    }

    #[test]
    fn handing_out_the_handle_forfeits_coverage_at_construction() {
        let (store, handle) = DurableStore::in_memory_shared().unwrap();
        assert!(
            !store.is_sole_writer(),
            "a store that handed out its handle must never claim coverage"
        );
        // And the handle really is the same dataset — that is what was paid for.
        handle
            .load_from_slice(
                oxigraph::io::RdfFormat::NTriples,
                b"<http://ex/a> <http://ex/p> <http://ex/b> .",
            )
            .unwrap();
        assert_eq!(store.dataset().len().unwrap(), 1);
    }

    /// ★ The whole coverage table, in one place, because every other assertion in this
    /// crate rests on it. Read it as: what may be cached, on each of the three modes,
    /// for a read that sees everything (`None`) and for one confined to a graph.
    #[test]
    fn the_coverage_table_holds_on_all_three_modes() {
        const G: &str = "urn:example:acme";
        const OTHER: &str = "urn:example:zenith";

        let owned = DurableStore::in_memory().unwrap();
        assert!(owned.read_is_covered(None));
        assert!(owned.read_is_covered(Some(G)));

        let (shared, _h) = DurableStore::in_memory_shared().unwrap();
        assert!(!shared.read_is_covered(None));
        assert!(
            !shared.read_is_covered(Some(G)),
            "an undeclared shared store caches nothing — 0.2.3's behaviour, unchanged"
        );

        let (declared, _h) = DurableStore::in_memory_shared_declaring(
            SharerWrites::only_the_default_graph().and_named_graph(OTHER),
        )
        .unwrap();
        assert!(
            !declared.read_is_covered(None),
            "a read that can see the default graph is never covered on a shared store"
        );
        assert!(
            declared.read_is_covered(Some(G)),
            "reserved from the sharer"
        );
        assert!(
            !declared.read_is_covered(Some(OTHER)),
            "the sharer may write this one, so a read of it must stay bare"
        );
        assert!(
            !declared.is_sole_writer(),
            "the handle still left: `is_sole_writer` is about the dataset, not about a read"
        );
    }

    /// A promise is a set of exact IRIs. Nothing is a prefix of anything, for the reason
    /// `cap_write_graph` grants are exact — and a graph not named is reserved, which is
    /// the safe direction.
    #[test]
    fn a_declaration_is_exact_and_names_nothing_it_was_not_given() {
        let writes = SharerWrites::only_the_default_graph().and_named_graphs(["urn:a", "urn:b"]);
        assert!(writes.may_write("urn:a") && writes.may_write("urn:b"));
        assert!(!writes.may_write("urn:"), "no prefix semantics");
        assert!(!writes.may_write("urn:a:child"), "and none downward either");
        assert_eq!(
            writes.named_graphs().collect::<Vec<_>>(),
            ["urn:a", "urn:b"]
        );
        assert_eq!(
            SharerWrites::only_the_default_graph()
                .named_graphs()
                .count(),
            0,
            "the default graph has no IRI to name, so the narrowest promise is an empty set"
        );
    }

    /// The fingerprint is over QUADS, not over the set of graph names: a sharer that
    /// writes into a graph that already exists changes it. That is the case `ikigai-gonk`'s
    /// hand-written tripwire could not see.
    #[test]
    fn the_fingerprint_moves_when_a_reserved_graph_changes() {
        let (store, handle) =
            DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph())
                .unwrap();
        handle
            .load_from_slice(
                oxigraph::io::RdfFormat::NQuads,
                b"<http://ex/a> <http://ex/p> <http://ex/b> <urn:example:acme> .",
            )
            .unwrap();
        let before = store.reserved_graphs_fingerprint().unwrap();

        handle
            .load_from_slice(
                oxigraph::io::RdfFormat::NQuads,
                b"<http://ex/c> <http://ex/p> <http://ex/d> <urn:example:acme> .",
            )
            .unwrap();
        assert_eq!(
            store
                .reserved_graphs_fingerprint()
                .unwrap()
                .changed_since(&before),
            ["<urn:example:acme>"]
        );
    }

    #[cfg(all(feature = "persistent", not(target_family = "wasm")))]
    #[test]
    fn the_lock_matcher_knows_both_spellings() {
        assert!(is_lock_error(
            "IO error: While lock file: /tmp/x/LOCK: Resource temporarily unavailable"
        ));
        assert!(is_lock_error(
            "IO error: lock hold by current process, acquire time 1 acquiring thread 2: \
             /tmp/x/LOCK: No locks available"
        ));
        assert!(!is_lock_error("IO error: No space left on device"));
    }
}
