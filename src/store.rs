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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use ikigai_core::{Capability, Error, Result};
use oxigraph::model::{
    GraphNameRef, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, QuadRef, TermRef,
};
use oxigraph::store::QuadIter;

use crate::budget::{AnswerBudget, Deadline, TimeBudget};

/// The canonical store type, re-exported so a host names ONE `Store`.
///
/// Rust unifies `oxigraph::store::Store` across crates only when every crate in the
/// build resolves the same `oxigraph`. Depending on `ikigai_store::Store` rather than
/// adding a direct `oxigraph` dependency makes that alignment structural instead of
/// coincidental — the same reason `ikigai-sparql` re-exports it.
pub use oxigraph::store::Store;

/// Oxigraph's transaction and storage error, re-exported for the same reason as [`Store`]:
/// a [`SharedWriter::write`] closure names them, and one `oxigraph` in the build makes
/// them the types the host already has.
pub use oxigraph::store::{StorageError, Transaction};

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

    /// The graphs both declarations permit: the default graph, and the named graphs each one
    /// names. What a [`SharedWriter::over`] that joins a store's writer enforces (ledger #1276).
    fn within(&self, ceiling: &SharerWrites) -> SharerWrites {
        SharerWrites {
            named: self.named.intersection(&ceiling.named).cloned().collect(),
        }
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
    /// How many spaces are bound over this dataset right now, shared by every clone. Each
    /// space is a kernel's door, and a kernel cannot see another kernel's writes — see
    /// [`spaces_bound`](Self::spaces_bound).
    spaces: Arc<AtomicUsize>,
    /// Whether a bound space has committed a read to some kernel's cache since the count
    /// above was last zero, shared by every clone — what decides whether a LATER space can
    /// be bound safely. See [`spaces_bound`](Self::spaces_bound).
    cached: Arc<AtomicBool>,
    /// Held only by the copy `crate::space` moves into its endpoints: dropping that space
    /// drops this, and the count above goes back down.
    binding: Option<Arc<SpaceBinding>>,
    /// Set only on the copy a space owns, when that space was bound too late to be safe:
    /// every request through it is refused. See [`spaces_bound`](Self::spaces_bound).
    refused: bool,
    /// How long a SPARQL evaluation may run — see [`with_time_budget`](Self::with_time_budget).
    budget: TimeBudget,
    /// How large a SPARQL answer may be — see [`with_answer_budget`](Self::with_answer_budget).
    answer: AnswerBudget,
    /// How many evaluations are still running after their callers gave up, shared by every
    /// clone — see [`overdue_evaluations`](Self::overdue_evaluations).
    overdue: Arc<AtomicUsize>,
    /// The one thread every update on this store runs on, shared by every clone (ledger
    /// #1127): a write committed on a freshly spawned thread degrades RocksDB's memtable. See
    /// `budget::Writer`.
    writer: Arc<crate::budget::Writer>,
}

/// One bound space's place in [`DurableStore::spaces`]. Decrements on drop, so the count
/// is of LIVE spaces: a kernel that is gone writes nothing — and when the last one goes,
/// every cache that could have been stale went with it.
#[derive(Debug)]
struct SpaceBinding {
    spaces: Arc<AtomicUsize>,
    cached: Arc<AtomicBool>,
}

impl Drop for SpaceBinding {
    fn drop(&mut self) {
        if self.spaces.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.cached.store(false, Ordering::SeqCst);
        }
    }
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
            spaces: Arc::new(AtomicUsize::new(0)),
            cached: Arc::new(AtomicBool::new(false)),
            binding: None,
            refused: false,
            budget: TimeBudget::default(),
            answer: AnswerBudget::default(),
            overdue: Arc::new(AtomicUsize::new(0)),
            writer: Arc::default(),
        })
    }

    /// An in-memory dataset **and a clone of its handle**. See the type docs: taking
    /// the handle makes every read `Expiry::Always` for the life of this store.
    pub fn in_memory_shared() -> Result<(Self, Arc<Store>)> {
        let mut this = Self::in_memory()?;
        this.coverage = Coverage::Shared;
        Ok(this.give_out_the_handle())
    }

    /// [`in_memory_shared`](Self::in_memory_shared), with the host declaring which
    /// graphs the handle's holder may write.
    ///
    /// ⚠ Read [`SharerWrites`] first. A promise that is wrong makes reads of a graph the
    /// sharer does write stale with no bound and no signal.
    pub fn in_memory_shared_declaring(writes: SharerWrites) -> Result<(Self, Arc<Store>)> {
        let mut this = Self::in_memory()?;
        this.coverage = Coverage::SharedDeclaring(writes);
        Ok(this.give_out_the_handle())
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
            spaces: Arc::new(AtomicUsize::new(0)),
            cached: Arc::new(AtomicBool::new(false)),
            binding: None,
            refused: false,
            budget: TimeBudget::default(),
            answer: AnswerBudget::default(),
            overdue: Arc::new(AtomicUsize::new(0)),
            writer: Arc::default(),
        })
    }

    /// `open` **and a clone of the handle**. See the type docs:
    /// taking the handle makes every read `Expiry::Always` for the life of this store.
    #[cfg(all(feature = "persistent", not(target_family = "wasm")))]
    pub fn open_shared(path: impl AsRef<Path>) -> Result<(Self, Arc<Store>)> {
        let mut this = Self::open(path)?;
        this.coverage = Coverage::Shared;
        Ok(this.give_out_the_handle())
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
        Ok(this.give_out_the_handle())
    }

    /// Every shared constructor's last step: a clone of the handle, recorded as this store's
    /// so that [`SharedWriter::over`] on it joins this store's writer instead of starting a
    /// second one beside it (ledger #1276).
    fn give_out_the_handle(self) -> (Self, Arc<Store>) {
        let handle = Arc::clone(&self.store);
        let declared = match &self.coverage {
            Coverage::SharedDeclaring(writes) => Some(writes.clone()),
            Coverage::Shared | Coverage::Owned => None,
        };
        given_out::record(given_out::GivenOut {
            handle: Arc::downgrade(&handle),
            writes: Arc::downgrade(&self.writes),
            writer: Arc::downgrade(&self.writer),
            declared,
        });
        (self, handle)
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
        matches!(self.coverage, Coverage::Owned) && self.spaces_bound() <= 1
    }

    /// How many spaces ([`crate::space`]) are bound over this dataset right now, across
    /// every clone of this handle.
    ///
    /// ★ **More than one forfeits coverage, exactly as handing out the handle does**
    /// (ledger #751). Each space is some kernel's door, each kernel keeps its own cache, and
    /// a kernel cuts a golden thread only for a write IT served — so a write through one
    /// space never invalidates a read another kernel cached, and that kernel would serve the
    /// stale answer with no bound. So while a second space is bound,
    /// [`is_sole_writer`](Self::is_sole_writer) is `false`,
    /// [`read_is_covered`](Self::read_is_covered) is `false` for every read, and
    /// `urn:iki:store:info` says `covered: false` through every one of them. The count is
    /// of LIVE spaces: drop the second and caching resumes.
    ///
    /// ★ **…and a space bound TOO LATE refuses instead.** Coverage is decided per read, so
    /// an answer the first kernel cached while it was the only space would stay in its
    /// cache after a second space is bound — and a write through the newcomer could never
    /// cut it, because nothing here can reach into a kernel's cache. So if any space over
    /// this dataset has already cached a read, a new space is bound REFUSING: every request
    /// through it fails with a sentence saying why, and it is not counted, so the first
    /// kernel stays covered and correct. The rule is therefore simply **bind every space
    /// before the first read** (a host builds its kernels at startup, and a test binds both
    /// before it asks anything); once every space is dropped the slate is clean again.
    ///
    /// ⚠ **One space shared by several kernels is the same hazard, and this cannot see
    /// it.** `Arc<EndpointSpace>` handed to two `Kernel`s is one binding here and two
    /// caches there. The only shape this crate can vouch for is one space per kernel; a
    /// host with several kernels over one dataset binds a space for each, up front.
    /// ★ Why it cannot be closed from here (ledger #761): an endpoint is not told which
    /// kernel invoked it (`Invocation`'s issuer is private and carries no identity), and
    /// nothing in this crate holds a kernel, so it can neither count kernels nor cut their
    /// caches. A cached probe resource would tell kernels apart only until the cache
    /// evicted it or a second capability keyed a second entry, so a refusal built on it
    /// would refuse legitimate callers. Closing it needs core to expose an invocation's
    /// kernel identity; `tests/two_spaces.rs` pins the hazard so that change is noticed.
    pub fn spaces_bound(&self) -> usize {
        self.spaces.load(Ordering::SeqCst)
    }

    /// The copy of this handle a space owns.
    ///
    /// ⚠ The order of the two atomic steps here and in
    /// [`commit_to_caching`](Self::commit_to_caching) is the whole argument, Dekker-style
    /// and `SeqCst` throughout: a bind announces itself (the count) and then looks for a
    /// cached read; a read announces itself (the flag) and then looks for a second space.
    /// In every interleaving at least one of them sees the other, so either the read is
    /// not cached or the bind refuses — never a cached read beside a writer it cannot see.
    pub(crate) fn bound_into_a_space(mut self) -> Self {
        let before = self.spaces.fetch_add(1, Ordering::SeqCst);
        if before >= 1 && self.cached.load(Ordering::SeqCst) {
            self.spaces.fetch_sub(1, Ordering::SeqCst);
            self.refused = true;
            return self;
        }
        self.binding = Some(Arc::new(SpaceBinding {
            spaces: Arc::clone(&self.spaces),
            cached: Arc::clone(&self.cached),
        }));
        self
    }

    /// Refuse every request through a space bound too late — see
    /// [`spaces_bound`](Self::spaces_bound).
    pub(crate) fn serving(&self) -> Result<()> {
        if !self.refused {
            return Ok(());
        }
        Err(Error::Endpoint(
            "this space was bound over a dataset that another space had already served a \
             cached read from, so a write through this one could leave that kernel serving a \
             stale answer with nothing able to evict it. It refuses every request instead. \
             Bind every space over one dataset before the first read (or use one space)"
                .to_string(),
        ))
    }

    /// The last word on caching one read whose coverage was `covered`: `true` only if it is
    /// covered AND no second space is bound — see
    /// [`bound_into_a_space`](Self::bound_into_a_space) for why it records the read first.
    pub(crate) fn commit_to_caching(&self, covered: bool) -> bool {
        if !covered {
            return false;
        }
        self.cached.store(true, Ordering::SeqCst);
        self.spaces_bound() <= 1
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
    /// | any, with a second space bound | **bare** | **bare** |
    ///
    /// ★ The `None` column is bare for BOTH shared modes and that is not conservatism
    /// for its own sake: a broad read sees the default graph, the sharer may always
    /// write the default graph (that is the point of sharing), and no declaration this
    /// type can express says otherwise. A host that wants its broad reads cached wants
    /// `open`.
    pub fn read_is_covered(&self, graph: Option<&str>) -> bool {
        // A second live space is a writer this kernel cannot see, whatever the mode —
        // see [`spaces_bound`](Self::spaces_bound).
        if self.spaces_bound() > 1 {
            return false;
        }
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

    /// Set how long this store's SPARQL evaluations may run (builder). Every clone made
    /// AFTER this carries it, so call it before [`space`](crate::space) — which a host does
    /// at startup anyway. The default is [`TimeBudget::default`]: 5 s for every caller,
    /// 120 s for root and as the most a `urn:cap:store:budget:<ms>` grant can lift a caller
    /// to. [`crate::budget`] has the whole story, including what it cannot stop.
    ///
    /// ```
    /// use ikigai_store::{budget::TimeBudget, DurableStore};
    /// use std::time::Duration;
    ///
    /// // An anonymous door gets half a second; the host's own doors hold a grant or root.
    /// let store = DurableStore::in_memory()?
    ///     .with_time_budget(TimeBudget::new(Duration::from_millis(500)));
    /// assert_eq!(store.time_budget().base(), Duration::from_millis(500));
    /// # Ok::<(), ikigai_core::Error>(())
    /// ```
    pub fn with_time_budget(mut self, budget: TimeBudget) -> Self {
        self.budget = budget;
        self
    }

    /// The time budget this store's evaluations run under.
    pub fn time_budget(&self) -> TimeBudget {
        self.budget
    }

    /// Set how large this store's SPARQL answers may be (builder). Every clone made AFTER
    /// this carries it, so call it before [`space`](crate::space). The default is
    /// [`AnswerBudget::default`]: 100,000 rows and 16 MiB for every caller; 10,000,000 rows
    /// and 1 GiB for root and as the most a `urn:cap:store:answer:*` grant can lift a caller
    /// to. [`crate::budget`] (section 4) has the contract (ledger #970).
    ///
    /// ```
    /// use ikigai_store::budget::{AnswerBound, AnswerBudget};
    /// use ikigai_store::DurableStore;
    ///
    /// // An anonymous door gets 1,000 rows and 1 MiB; the host's own doors hold a grant or root.
    /// let store = DurableStore::in_memory()?
    ///     .with_answer_budget(AnswerBudget::new(AnswerBound::new(1_000, 1 << 20)?));
    /// assert_eq!(store.answer_budget().base().rows(), 1_000);
    /// # Ok::<(), ikigai_core::Error>(())
    /// ```
    pub fn with_answer_budget(mut self, answer: AnswerBudget) -> Self {
        self.answer = answer;
        self
    }

    /// How large this store's SPARQL answers may be.
    pub fn answer_budget(&self) -> AnswerBudget {
        self.answer
    }

    /// How many SPARQL evaluations over this dataset are still running after their callers
    /// were answered with a timeout, right now.
    ///
    /// Usually 0, and back to 0 within milliseconds of a timeout: oxigraph stops when it
    /// next reads a quad. A number that STAYS up is a query shape oxigraph cannot stop
    /// partway (see [`crate::budget`]), holding a core each; at
    /// [`TimeBudget::max_overdue`] the store refuses new evaluations until one ends.
    pub fn overdue_evaluations(&self) -> usize {
        self.overdue.load(Ordering::SeqCst)
    }

    /// Run one evaluation of `text` (an update when `write`) for a caller holding
    /// `capability`, within the budget that capability gets — or the request's own `budget=`
    /// milliseconds (`requested`) when that is smaller. See [`crate::budget::run`].
    pub(crate) fn evaluate<T, F>(
        &self,
        text: &str,
        write: bool,
        capability: &Capability,
        requested: Option<&str>,
        work: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Deadline) -> Result<T> + Send + 'static,
    {
        let budget =
            crate::budget::effective_budget(requested, self.budget.for_capability(capability))?;
        crate::budget::run(
            text,
            write,
            budget,
            &self.overdue,
            self.budget.max_overdue(),
            write.then_some(&*self.writer),
            work,
        )
    }

    /// The dataset, for this crate's endpoints. Deliberately **not** `pub`: see the
    /// type docs for why handing this out is a constructor-level decision.
    pub(crate) fn dataset(&self) -> &Store {
        &self.store
    }

    /// The writer a host that holds the raw handle commits its OWN writes through: on this
    /// store's one long-lived writer thread, one transaction per call (ledger #1207). `None`
    /// for an owned store. Read [`SharedWriter`] for why both halves of that matter.
    ///
    /// ★ **`None` on an owned store is the point, not a gap.** An owned store's every write
    /// goes through a kernel door, where the golden thread sees it; a writer handed out
    /// here would be a write the kernel cannot see, which is exactly what the type docs say
    /// no accessor may turn an owned store into. The handle a shared store's constructor
    /// already gave away can write anyway, so this adds no authority: it only decides which
    /// THREAD those writes commit on.
    ///
    /// ★ **On a store opened with `open_shared_declaring` the writer carries the
    /// declaration and enforces it** (ledger #1209): a write through it to a named graph the
    /// [`SharerWrites`] did not name is refused, and the whole transaction with it. On a store
    /// opened with `open_shared` nothing was declared, so nothing is refused.
    ///
    /// ★ This writer SHARES the store's own: its writer thread and its write lock, so a host's
    /// write is ordered with every write door. [`SharedWriter::over`] on the handle this store
    /// gave out shares them too (ledger #1276), narrowed to the declaration passed to it: the
    /// way for a module handed only the raw handle.
    pub fn shared_writer(&self) -> Option<SharedWriter> {
        let declared = match &self.coverage {
            Coverage::Owned => return None,
            Coverage::Shared => None,
            Coverage::SharedDeclaring(writes) => Some(Arc::new(writes.clone())),
        };
        Some(SharedWriter {
            store: Arc::clone(&self.store),
            writes: Arc::clone(&self.writes),
            writer: Arc::clone(&self.writer),
            declared,
        })
    }

    /// Run `work` on this store's writer thread under its write lock, and wait for it: how
    /// `urn:iki:store:load` commits (ledger #1209), on the thread every other write commits
    /// on. Refused at once when asked from that thread already, which would wait for itself.
    pub(crate) fn on_writer<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&DurableStore) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let store = Arc::clone(self);
        self.writer
            .call(crate::limits::STACK_BASE, move || {
                // Taken ON the writer thread, as `SharedWriter::write` takes it: see there.
                let _writes = store.write_lock();
                work(&store)
            })
            .and_then(|outcome| outcome)
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

/// How a host holding a shared store's raw handle commits its own writes: on one long-lived
/// writer thread, a whole record per transaction (ledger #1207), and only into the graphs it
/// declared (ledger #1209). Clones share the thread.
///
/// # Two ways to get one
///
/// | | [`DurableStore::shared_writer`] | [`SharedWriter::over`] |
/// | --- | --- | --- |
/// | for a host holding | a `DurableStore` opened shared | an `Arc<Store>`: a handle a shared constructor gave out, or a store it opened itself |
/// | writer thread | **the store's own**, shared with its update, graph-update and load doors | the store's own on a handle a `DurableStore` gave out; on a store the host opened itself, **the handle's own**, started by the first `over` on it and shared by every later one |
/// | write lock | the store's own, so a write is ordered with every door | the same split: the store's own, or the handle's own, ordering every `over` writer on that handle |
/// | declaration | the store's: a [`SharerWrites`] from `open_shared_declaring`, none from `open_shared` | the [`SharerWrites`] passed in, narrowed to the store's own declaration when it joined a store that made one, or to the first `over`'s on a store the host opened |
///
/// ★ **`over` on a handle a `DurableStore` gave out joins that store's writer** (ledger
/// #1276). Until 0.2.16 it started a second writer thread and a second lock beside the
/// store's, and a read-modify-write through a door could then lose half of a record rewritten
/// here (on RocksDB, a 15-quad record left as 1 quad; `tests/provenance.rs`). The shared
/// constructors now record the handle they return, so `over` on it shares the store's thread
/// and lock, and enforces BOTH declarations: the store's, which its cached reads rest on, and
/// the one passed in, which is the caller's own fence. Neither widens the other. A module
/// handed only the raw handle can therefore take `over` with its own narrow declaration and
/// be both ordered with the store and fenced to its own graphs.
///
/// ★ **And every `over` on one handle a host opened itself shares one writer** (ledger
/// #1324). Until 0.2.17 each `over` there was a thread and a lock of its own, so a host's
/// writer and a module's writer made with `over` on one handle could lose half of a record
/// exactly as above (`ikigai-browse`'s race test; on RocksDB, 1 quad of 15). The first `over`
/// on a handle now records it, and a later one joins that writer, narrowed to the first one's
/// declaration as a joined writer is narrowed to a store's: a later `over` declaring a graph
/// the first did not has that graph **refused**, so make the widest writer first (the host's,
/// before it hands the handle to a module). Once every writer on the handle is dropped, the
/// next `over` starts a fresh writer and its declaration is the new ceiling.
///
/// ⚠ The record is of the `Arc` (the one the constructor returned, or the one `over` was
/// first given) and its clones, not of the dataset: a `Store` cloned and re-wrapped in a new
/// `Arc` is not recognized, and `over` on it makes a second writer again. Pass the handle on as
/// it came.
///
/// # ★ Why the thread matters: the RocksDB seed trap (ledger #1127)
///
/// RocksDB draws the height of every memtable skiplist node from a THREAD-LOCAL generator
/// seeded from a hash of the thread's id. A thread spawned after the previous one exited
/// usually gets the same id back, so a host that commits each write on a fresh thread (a
/// socket door that spawns one per connection, `tokio::task::spawn_blocking`, the stack
/// [`crate::limits::on_sparql_stack`] spawns) draws the same few heights for every
/// transaction. The skiplists stop growing upper levels, and every read walks a long top
/// level until the memtable flushes: 2x to 20x slower reads after gonk's 750-append fill,
/// and 6x to 68x after 750 browse annotations, against 1.0x when the same writes came from
/// one thread. This crate's own doors commit on one writer thread (the load door since
/// 0.2.15); a writer from [`DurableStore::shared_writer`], or from [`over`](Self::over) on the
/// handle a shared constructor gave out, commits on that SAME thread, so every write to the
/// dataset advances one generator, and the `over` writers on a store the host opened itself on
/// one thread of the handle's own, which lives as long as one of them does.
///
/// # ★ Declared = enforced (ledger #1209)
///
/// A [`SharerWrites`] declaration is what lets a shared store cache scoped reads of every
/// graph the sharer does NOT write, and a false one is the worst failure this crate has:
/// silent, unbounded staleness. A writer that carries one keeps it:
///
/// - [`write_declared`](Self::write_declared) hands the closure a [`DeclaredTransaction`],
///   whose every insert and remove is checked against the declaration. One outside it is
///   refused ([`UndeclaredGraph`]) **and the whole transaction is rolled back**, even if the
///   closure ignored the refusal and returned `Ok`.
/// - [`write`](Self::write) hands the closure oxigraph's own [`Transaction`], which can write
///   any graph, so on a writer carrying a declaration it is **refused before the closure
///   runs**. It is for a writer with nothing declared (a store opened with `open_shared`).
///
/// The default graph is always writable, as [`SharerWrites`] has always said: it has no IRI,
/// so no scoped read reaches it. A blank-node graph name never is, since no declaration can
/// name one. A writer with no declaration refuses nothing.
///
/// ⚠ It covers writes made THROUGH this type. The raw handle a shared constructor returned
/// can still write anything, and [`DurableStore::reserved_graphs_fingerprint`] stays the
/// tripwire for that.
///
/// # What a call does
///
/// - **One transaction.** A write opens a transaction on the writer thread, hands it to the
///   closure, and commits it only if the closure returns `Ok` (and, for `write_declared`,
///   nothing it asked for was refused): a record of fifteen quads is one commit, and a reader
///   sees all of it or none of it. On `Err` nothing is written.
/// - **Ordered with the store's own writes** when it came from
///   [`DurableStore::shared_writer`], or from [`over`](Self::over) on the handle a shared
///   constructor gave out: it takes the store's write lock, as every write door
///   does, and runs in the same queue as `urn:iki:store:update`, `urn:iki:store:graph-update`
///   and `urn:iki:store:load`. A scoped write can no longer lose an update made through this
///   type; one made through the raw handle directly still can. From `over` on a store the
///   host opened itself, it is ordered the same way with every other `over` writer on that
///   handle.
/// - **It waits, with no deadline.** The caller blocks until the transaction commits or
///   fails, including any time spent behind a store update that is still running.
/// - **A panic in the closure is the caller's**: caught on the writer thread, which lives
///   on, and resumed on the thread that asked.
/// - **Never from inside a write.** Make a record's writes through the transaction you were
///   given. A closure that asks for another write through this type is refused rather than
///   left waiting for itself forever, and so is one through the `load` door, and through the
///   `update` and `graph-update` doors when the writer's stack fits them (when it does not,
///   the update moves to a larger writer thread and waits there for the lock this write
///   holds, until its budget runs out).
/// - **A stack sized for inserts and removes**, at least [`crate::limits::STACK_BASE`]. A
///   SPARQL update over caller text belongs at the store's update door, which sizes its
///   thread to the text and bounds it first; run through `Transaction::update` in a `write`
///   closure it gets neither.
/// - On wasm there are no threads, and the closure runs inline.
///
/// ⚠ It is still a write the kernel cannot see: reads stay `Expiry::Always` on a store
/// opened with `open_shared`, and a store opened with `open_shared_declaring` stays exactly
/// as honest as its declaration, which writes through this type can no longer break.
///
/// ```
/// use ikigai_store::{DurableStore, SharerWrites, StorageError, UndeclaredGraph};
/// use oxigraph::model::{GraphNameRef, LiteralRef, NamedNodeRef, QuadRef};
///
/// let (store, handle) =
///     DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph())?;
/// let writer = store.shared_writer().expect("a shared store has a writer");
/// let record = NamedNodeRef::new_unchecked("urn:example:record:1");
/// let quad = move |p: &str, o: &'static str, g: GraphNameRef<'static>| {
///     QuadRef::new(record, NamedNodeRef::new_unchecked(p), LiteralRef::new_simple_literal(o), g)
///         .into_owned()
/// };
/// let title = quad("urn:example:title", "a title", GraphNameRef::DefaultGraph);
/// let body = quad("urn:example:body", "a body", GraphNameRef::DefaultGraph);
///
/// // Asked for from a thread spawned for it, the shape that trips the seed trap if the
/// // write commits there. It does not: it commits on the store's writer thread.
/// let asking = writer.clone();
/// let committed_on = std::thread::spawn(move || {
///     asking.write_declared(move |txn| {
///         txn.insert(&title)?;
///         txn.insert(&body)?;
///         Ok::<_, StorageError>(std::thread::current().name().map(str::to_owned))
///     })
/// })
/// .join()
/// .unwrap()?;
/// assert_eq!(committed_on.as_deref(), Some("ikigai-store-writer"));
/// assert_eq!(handle.len()?, 2);
///
/// // A record that reaches a graph the declaration reserved writes none of itself.
/// let elsewhere = quad(
///     "urn:example:title",
///     "another title",
///     GraphNameRef::NamedNode(NamedNodeRef::new_unchecked("urn:example:reserved")),
/// );
/// let another = quad("urn:example:title", "another title", GraphNameRef::DefaultGraph);
/// let refused = writer
///     .write_declared(move |txn| {
///         txn.insert(&another)?;
///         txn.insert(&elsewhere)
///     })
///     .unwrap_err();
/// assert_eq!(
///     UndeclaredGraph::of(&refused).map(UndeclaredGraph::graph),
///     Some("<urn:example:reserved>")
/// );
/// assert_eq!(handle.len()?, 2);
///
/// // An owned store hands out no writer: its every write goes through a kernel door.
/// assert!(DurableStore::in_memory()?.shared_writer().is_none());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct SharedWriter {
    store: Arc<Store>,
    writes: Arc<Mutex<()>>,
    writer: Arc<crate::budget::Writer>,
    /// The graphs this writer may write, or `None` when nothing was declared.
    declared: Option<Arc<SharerWrites>>,
}

impl std::fmt::Debug for SharedWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedWriter")
            .field("declared", &self.declared)
            .finish_non_exhaustive()
    }
}

impl SharedWriter {
    /// A writer over a bare `Store` handle, for a host or module that holds no
    /// [`DurableStore`], writing only the graphs `writes` declares (ledger #1207, #1209).
    ///
    /// - On a handle a shared constructor gave out, while that store or a writer it gave out
    ///   lives, it **joins the store's writer**: its thread and its write lock, so every write
    ///   is ordered with the store's doors, and it writes only graphs that both `writes` and
    ///   the store's own declaration permit (ledger #1276).
    /// - On any other handle, a store the host opened itself, the first `over` starts the
    ///   handle's writer: a long-lived thread and a write lock, recorded as the handle's.
    ///   Nothing starts until the first write, and the thread ends when the last writer on the
    ///   handle is dropped. **Every later `over` on the same `Arc` joins it** (ledger #1324),
    ///   writing only graphs that both `writes` and the first `over`'s declaration permit: a
    ///   graph the first did not declare is refused, so make the widest writer first. Once
    ///   every writer on the handle is gone, the next `over` starts afresh.
    ///
    /// It is infallible either way: a join never fails, it narrows.
    ///
    /// See the type docs for how it differs from [`DurableStore::shared_writer`].
    ///
    /// ```
    /// use ikigai_store::{SharedWriter, SharerWrites, StorageError, Store};
    /// use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};
    /// use std::sync::Arc;
    ///
    /// // A host that opened the store itself (`Store::open` on disk; in memory here).
    /// let store = Arc::new(Store::new()?);
    /// let writer = SharedWriter::over(
    ///     Arc::clone(&store),
    ///     SharerWrites::only_the_default_graph().and_named_graph("urn:example:notes"),
    /// );
    /// let note = NamedNodeRef::new_unchecked("urn:example:note:1");
    /// let notes = GraphNameRef::NamedNode(NamedNodeRef::new_unchecked("urn:example:notes"));
    /// writer.write_declared(move |txn| txn.insert(QuadRef::new(note, note, note, notes)))?;
    /// assert_eq!(store.len()?, 1);
    ///
    /// // A module handed the same handle joins that writer, held to what the first declared.
    /// let module = SharedWriter::over(
    ///     Arc::clone(&store),
    ///     SharerWrites::only_the_default_graph()
    ///         .and_named_graph("urn:example:notes")
    ///         .and_named_graph("urn:example:elsewhere"),
    /// );
    /// let declared = module.declared().unwrap().named_graphs().collect::<Vec<_>>();
    /// assert_eq!(declared, ["urn:example:notes"]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn over(store: Arc<Store>, writes: SharerWrites) -> SharedWriter {
        let joined = given_out::join_or_start(&store, &writes);
        SharedWriter {
            store,
            writes: joined.writes,
            writer: joined.writer,
            declared: Some(Arc::new(match &joined.declared {
                Some(ceiling) => writes.within(ceiling),
                None => writes,
            })),
        }
    }

    /// The raw handle, for reads: the same `Arc<Store>` the shared constructor returned (or
    /// [`over`](Self::over) was given), so a host can hand a module this one value instead of
    /// two.
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The graphs this writer may write, or `None` when nothing was declared (a store opened
    /// with `open_shared`), in which case it refuses nothing.
    pub fn declared(&self) -> Option<&SharerWrites> {
        self.declared.as_deref()
    }

    /// Run `record` against one [`DeclaredTransaction`] on the writer thread, and commit it if
    /// `record` returns `Ok` and nothing it asked for was refused. See the type docs.
    ///
    /// The error type is the caller's, so long as a [`StorageError`] converts into it: a
    /// failure to start, commit or reach the writer thread arrives as one, and so does a
    /// refusal, which [`UndeclaredGraph::of`] recognizes.
    pub fn write_declared<T, E, F>(&self, record: F) -> std::result::Result<T, E>
    where
        F: FnOnce(&mut DeclaredTransaction<'_>) -> std::result::Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: From<StorageError> + Send + 'static,
    {
        let (store, declared) = (Arc::clone(&self.store), self.declared.clone());
        self.on_writer(move || {
            let mut transaction = DeclaredTransaction {
                inner: store.start_transaction()?,
                declared,
                refused: None,
            };
            let value = record(&mut transaction)?;
            match transaction.refused {
                // The closure went on after a refusal, or swallowed it: the transaction is
                // dropped uncommitted all the same.
                Some(refused) => Err(E::from(refused.into_storage_error())),
                None => {
                    transaction.inner.commit()?;
                    Ok(value)
                }
            }
        })
    }

    /// Run `record` against one raw oxigraph [`Transaction`] on the writer thread, and commit
    /// it if `record` returns `Ok`.
    ///
    /// ⚠ **Refused, before `record` runs, on a writer carrying a declaration** (ledger #1209):
    /// a raw transaction can write any graph, so the declaration could not be kept. Use
    /// [`write_declared`](Self::write_declared), which serves every writer. This one is for a
    /// writer with nothing declared that needs what a [`DeclaredTransaction`] does not offer
    /// (`Transaction::update`, `load_from_reader`, `clear`).
    pub fn write<T, E, F>(&self, record: F) -> std::result::Result<T, E>
    where
        F: FnOnce(&mut Transaction<'_>) -> std::result::Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: From<StorageError> + Send + 'static,
    {
        if let Some(declared) = &self.declared {
            return Err(E::from(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "this writer carries a `SharerWrites` declaration ({}), and a raw \
                     `Transaction` could write any graph, so it would not be kept. Nothing was \
                     written. Write through `SharedWriter::write_declared`, whose transaction \
                     refuses a graph outside the declaration (ledger #1209)",
                    describe(declared)
                ),
            ))));
        }
        let store = Arc::clone(&self.store);
        self.on_writer(move || {
            let mut transaction = store.start_transaction()?;
            let value = record(&mut transaction)?;
            transaction.commit()?;
            Ok(value)
        })
    }

    /// Run `job` on the writer thread under the write lock, and wait for its answer.
    fn on_writer<T, E>(
        &self,
        job: impl FnOnce() -> std::result::Result<T, E> + Send + 'static,
    ) -> std::result::Result<T, E>
    where
        T: Send + 'static,
        E: From<StorageError> + Send + 'static,
    {
        let writes = Arc::clone(&self.writes);
        let job = move || -> std::result::Result<T, E> {
            // Taken ON the writer thread, as the store's update doors take it: taken on the
            // caller's thread it would wait for a door's job queued ahead of this one, which
            // waits for the lock.
            let _writes = writes.lock().unwrap_or_else(|e| e.into_inner());
            job()
        };
        match self.writer.call(crate::limits::STACK_BASE, job) {
            Ok(outcome) => outcome,
            Err(e) => Err(E::from(StorageError::Io(std::io::Error::other(
                e.to_string(),
            )))),
        }
    }
}

/// The handles the shared constructors gave out, and the handles [`SharedWriter::over`] was
/// first called on, with the writer each belongs to: the provenance a bare `Arc<Store>` cannot
/// carry itself, since `Store` is oxigraph's type (ledger #1276, #1324). Read by
/// [`SharedWriter::over`] alone.
///
/// ★ **Keyed on the `Arc`'s address, and that is sound only because each entry holds a
/// `Weak` to it.** A `Weak` keeps the allocation (never the dataset: the `Store` inside is
/// dropped with its last strong handle, so RocksDB's lock is released on time), so no other
/// `Arc<Store>` can take that address while the entry exists. An entry is pruned once its
/// handle or its writer is gone, and only then can the address be reused.
///
/// ⚠ It knows the `Arc`, not the dataset: a `Store` is itself a cheap clone of one dataset,
/// and a clone re-wrapped in a new `Arc` is a handle nobody recorded.
/// `tests/provenance.rs` pins that limit.
mod given_out {
    use super::{SharerWrites, Store};
    use std::sync::{Arc, Mutex, Weak};

    /// One recorded handle: one a shared constructor returned, or one `over` was first
    /// called on.
    pub(super) struct GivenOut {
        pub(super) handle: Weak<Store>,
        pub(super) writes: Weak<Mutex<()>>,
        pub(super) writer: Weak<crate::budget::Writer>,
        /// The ceiling a joining `over` is narrowed to: the store's own declaration, or
        /// `None` when it declared nothing (`open_shared`); for a handle `over` recorded, the
        /// first `over`'s declaration.
        pub(super) declared: Option<SharerWrites>,
    }

    /// The writer a handle belongs to.
    pub(super) struct Joined {
        pub(super) writes: Arc<Mutex<()>>,
        pub(super) writer: Arc<crate::budget::Writer>,
        pub(super) declared: Option<SharerWrites>,
    }

    static GIVEN_OUT: Mutex<Vec<GivenOut>> = Mutex::new(Vec::new());

    fn alive(entry: &GivenOut) -> bool {
        entry.handle.strong_count() > 0 && entry.writer.strong_count() > 0
    }

    fn given() -> std::sync::MutexGuard<'static, Vec<GivenOut>> {
        GIVEN_OUT.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn record(entry: GivenOut) {
        let mut given = given();
        given.retain(alive);
        given.push(entry);
    }

    /// The writer `handle` belongs to, while some `DurableStore` clone or `SharedWriter`
    /// still holds it; otherwise a new writer, recorded as the handle's with `writes` as the
    /// ceiling every later `over` on it is narrowed to (ledger #1324).
    ///
    /// The lookup and the record are one step under one lock, so two `over` calls racing on
    /// a handle nobody recorded yet still end up on one writer.
    pub(super) fn join_or_start(handle: &Arc<Store>, writes: &SharerWrites) -> Joined {
        let mut given = given();
        given.retain(alive);
        if let Some(at) = given
            .iter()
            .position(|entry| std::ptr::eq(entry.handle.as_ptr(), Arc::as_ptr(handle)))
        {
            let entry = &given[at];
            match (entry.writes.upgrade(), entry.writer.upgrade()) {
                (Some(writes), Some(writer)) => {
                    return Joined {
                        writes,
                        writer,
                        declared: entry.declared.clone(),
                    }
                }
                // The last writer went between the prune and here: nothing is left to join.
                _ => {
                    given.swap_remove(at);
                }
            }
        }
        let joined = Joined {
            writes: Arc::new(Mutex::new(())),
            writer: Arc::default(),
            declared: Some(writes.clone()),
        };
        given.push(GivenOut {
            handle: Arc::downgrade(handle),
            writes: Arc::downgrade(&joined.writes),
            writer: Arc::downgrade(&joined.writer),
            declared: joined.declared.clone(),
        });
        joined
    }
}

/// A declaration in a sentence, for a refusal.
fn describe(declared: &SharerWrites) -> String {
    if declared.named_graphs().len() == 0 {
        "the default graph only".to_string()
    } else {
        format!(
            "the default graph and {}",
            declared
                .named_graphs()
                .map(|g| format!("<{g}>"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// The transaction a [`SharedWriter::write_declared`] closure writes through: oxigraph's own,
/// with every write checked against the writer's [`SharerWrites`] declaration (ledger #1209).
///
/// A write to a graph outside the declaration returns an [`UndeclaredGraph`] refusal, writes
/// nothing, and marks the transaction so it is **not committed**, whatever the closure does
/// next. Reads see the transaction's own writes, as oxigraph's do, and any graph: the raw
/// handle could read them anyway, so a read is no new authority.
pub struct DeclaredTransaction<'a> {
    inner: Transaction<'a>,
    declared: Option<Arc<SharerWrites>>,
    refused: Option<UndeclaredGraph>,
}

impl std::fmt::Debug for DeclaredTransaction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclaredTransaction")
            .field("declared", &self.declared)
            .field("refused", &self.refused)
            .finish_non_exhaustive()
    }
}

impl DeclaredTransaction<'_> {
    /// Add a quad, if its graph is declared.
    pub fn insert<'b>(
        &mut self,
        quad: impl Into<QuadRef<'b>>,
    ) -> std::result::Result<(), StorageError> {
        let quad = quad.into();
        self.permit(quad.graph_name)?;
        self.inner.insert(quad);
        Ok(())
    }

    /// Add quads, each of whose graphs must be declared. The first one that is not stops the
    /// insert there and refuses the transaction.
    pub fn extend<'b>(
        &mut self,
        quads: impl IntoIterator<Item = impl Into<QuadRef<'b>>>,
    ) -> std::result::Result<(), StorageError> {
        quads.into_iter().try_for_each(|quad| self.insert(quad))
    }

    /// Remove a quad, if its graph is declared: removing changes a graph as surely as adding.
    pub fn remove<'b>(
        &mut self,
        quad: impl Into<QuadRef<'b>>,
    ) -> std::result::Result<(), StorageError> {
        let quad = quad.into();
        self.permit(quad.graph_name)?;
        self.inner.remove(quad);
        Ok(())
    }

    /// Whether the dataset, as this transaction sees it, holds `quad`.
    pub fn contains<'b>(
        &self,
        quad: impl Into<QuadRef<'b>>,
    ) -> std::result::Result<bool, StorageError> {
        self.inner.contains(quad)
    }

    /// The quads matching a pattern, as this transaction sees them (its own writes included).
    pub fn quads_for_pattern(
        &self,
        subject: Option<NamedOrBlankNodeRef<'_>>,
        predicate: Option<NamedNodeRef<'_>>,
        object: Option<TermRef<'_>>,
        graph_name: Option<GraphNameRef<'_>>,
    ) -> QuadIter<'_> {
        self.inner
            .quads_for_pattern(subject, predicate, object, graph_name)
    }

    /// How many quads the dataset holds, as this transaction sees it. A full scan.
    pub fn len(&self) -> std::result::Result<usize, StorageError> {
        self.inner.len()
    }

    /// Whether the dataset, as this transaction sees it, is empty.
    pub fn is_empty(&self) -> std::result::Result<bool, StorageError> {
        self.inner.is_empty()
    }

    /// Refuse a write to `graph` unless the declaration names it, and remember the refusal so
    /// the transaction is not committed.
    fn permit(&mut self, graph: GraphNameRef<'_>) -> std::result::Result<(), StorageError> {
        let Some(declared) = self.declared.as_deref() else {
            return Ok(());
        };
        let permitted = match graph {
            GraphNameRef::DefaultGraph => true,
            GraphNameRef::NamedNode(iri) => declared.may_write(iri.as_str()),
            // No declaration can name a blank node, so none permits one.
            GraphNameRef::BlankNode(_) => false,
        };
        if permitted {
            return Ok(());
        }
        let refused = UndeclaredGraph {
            graph: graph.to_string(),
            declared: describe(declared),
        };
        self.refused.get_or_insert_with(|| refused.clone());
        Err(refused.into_storage_error())
    }
}

/// The refusal of a write through a [`SharedWriter`] to a graph its [`SharerWrites`]
/// declaration does not name (ledger #1209). It arrives inside a [`StorageError`] (kind
/// `PermissionDenied`), or inside the caller's own error converted from one; recognize it with
/// [`UndeclaredGraph::of`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndeclaredGraph {
    graph: String,
    declared: String,
}

impl UndeclaredGraph {
    /// The graph the refused write named, in N-Quads graph-name form (`<urn:…>`, or `_:b0`).
    pub fn graph(&self) -> &str {
        &self.graph
    }

    /// The refusal inside `error`, if that is what it is: a [`StorageError`], or any error
    /// whose source chain reaches one.
    pub fn of<'e>(error: &'e (dyn std::error::Error + 'static)) -> Option<&'e UndeclaredGraph> {
        let mut next = Some(error);
        while let Some(error) = next {
            if let Some(refused) = error.downcast_ref::<UndeclaredGraph>() {
                return Some(refused);
            }
            if let Some(StorageError::Io(io)) = error.downcast_ref::<StorageError>() {
                if let Some(refused) = io
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<UndeclaredGraph>())
                {
                    return Some(refused);
                }
            }
            if let Some(io) = error.downcast_ref::<std::io::Error>() {
                if let Some(refused) = io
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<UndeclaredGraph>())
                {
                    return Some(refused);
                }
            }
            next = error.source();
        }
        None
    }

    fn into_storage_error(self) -> StorageError {
        StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            self,
        ))
    }
}

impl std::fmt::Display for UndeclaredGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a write to graph {} was refused: this writer's `SharerWrites` declaration names {}, \
             and a write outside it would make cached reads of that graph stale with no signal. \
             The whole transaction was rolled back (ledger #1209)",
            self.graph, self.declared
        )
    }
}

impl std::error::Error for UndeclaredGraph {}

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
    use oxigraph::model::{GraphNameRef, LiteralRef, NamedNodeRef, Quad, QuadRef};

    fn quad(n: usize) -> Quad {
        QuadRef::new(
            NamedNodeRef::new_unchecked("urn:test:record"),
            NamedNodeRef::new_unchecked("urn:test:p"),
            LiteralRef::new_simple_literal(&n.to_string()),
            GraphNameRef::DefaultGraph,
        )
        .into_owned()
    }

    /// ★ THE PIN (ledger #1207, the half of #1127 a raw-handle host owns): writes asked for
    /// from many short-lived threads, the way a socket door asks, all commit on ONE thread
    /// that is none of them, and once the store's own update door has run, on the thread
    /// that door commits on. Without the writer every write commits on the thread that
    /// asked, and this fails on every run, whatever seed the run happens to draw.
    ///
    /// (An update needing more stack than the writer has moves it to a larger long-lived
    /// thread, once per size class, so the door's thread is read after the door has run.)
    #[test]
    fn a_shared_write_commits_on_the_stores_one_writer_thread_whatever_thread_asks() {
        let (store, handle) = DurableStore::in_memory_shared().unwrap();
        let writer = store.shared_writer().unwrap();
        let door = || {
            store
                .evaluate("INSERT DATA {}", true, &Capability::root(), None, |_| {
                    Ok(std::thread::current().id())
                })
                .unwrap()
        };
        let from_a_fresh_thread = |n: usize| {
            let writer = writer.clone();
            std::thread::spawn(move || {
                let asked = std::thread::current().id();
                let ran = writer
                    .write(move |txn| {
                        txn.insert(&quad(n));
                        Ok::<_, StorageError>(std::thread::current().id())
                    })
                    .unwrap();
                (asked, ran)
            })
            .join()
            .unwrap()
        };
        let (asked, first) = from_a_fresh_thread(0);
        assert_ne!(first, asked, "a write committed on the thread that asked");
        assert_ne!(first, std::thread::current().id());
        for n in 1..8 {
            let (asked, ran) = from_a_fresh_thread(n);
            assert_ne!(ran, asked, "a write committed on the thread that asked");
            assert_eq!(ran, first, "a write committed on a thread of its own");
        }
        assert_eq!(handle.len().unwrap(), 8);

        let doors = door();
        assert_ne!(doors, std::thread::current().id());
        for n in 8..16 {
            let (asked, ran) = from_a_fresh_thread(n);
            assert_ne!(ran, asked, "a write committed on the thread that asked");
            assert_eq!(
                ran, doors,
                "the store's update door and the host's writes commit on two threads"
            );
        }
        assert_eq!(door(), doors, "the update door left the shared writer");
        assert_eq!(handle.len().unwrap(), 16);
    }

    #[test]
    fn a_record_commits_whole_or_not_at_all() {
        let (store, handle) = DurableStore::in_memory_shared().unwrap();
        let writer = store.shared_writer().unwrap();
        let failed: std::result::Result<(), StorageError> = writer.write(|txn| {
            for n in 0..3 {
                txn.insert(&quad(n));
            }
            Err(StorageError::Io(std::io::Error::other("incomplete")))
        });
        assert!(failed.is_err());
        assert_eq!(
            handle.len().unwrap(),
            0,
            "a failed record left part of itself"
        );
        let written = writer
            .write(|txn| {
                for n in 0..3 {
                    txn.insert(&quad(n));
                }
                txn.len()
            })
            .unwrap();
        assert_eq!(written, 3, "the transaction sees its own writes");
        assert_eq!(handle.len().unwrap(), 3);
    }

    /// A write asked for from inside a write would wait for itself forever; it is refused
    /// instead. Run on a thread of its own and bounded, so a regression fails rather than
    /// hanging the suite. The door runs once first, so the writer's stack fits the nested
    /// update and it is asked of the same thread (see the type docs for the other case).
    #[test]
    fn a_write_from_inside_a_write_is_refused_not_deadlocked() {
        let (store, _handle) = DurableStore::in_memory_shared().unwrap();
        let writer = store.shared_writer().unwrap();
        store
            .evaluate(
                "INSERT DATA {}",
                true,
                &Capability::root(),
                None,
                |_| Ok(()),
            )
            .unwrap();
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (inner, door) = (writer.clone(), store.clone());
            let refused = writer.write(move |_| {
                let nested = inner.write(|_| Ok::<_, StorageError>(()));
                let through_a_door = door.evaluate(
                    "INSERT DATA {}",
                    true,
                    &Capability::root(),
                    None,
                    |_| Ok(()),
                );
                Ok::<_, StorageError>((
                    nested.err().map(|e| e.to_string()),
                    through_a_door.is_err(),
                ))
            });
            let _ = done.send(refused);
        });
        let (nested, through_a_door) = outcome
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("a nested write deadlocked")
            .unwrap();
        assert!(
            nested
                .as_deref()
                .is_some_and(|e| e.contains("wait for itself")),
            "{nested:?}"
        );
        assert!(
            through_a_door,
            "an update door asked from inside a write was not refused"
        );
    }

    #[test]
    fn a_panic_in_a_shared_write_reaches_its_caller_and_the_writer_lives_on() {
        let (store, handle) = DurableStore::in_memory_shared().unwrap();
        let writer = store.shared_writer().unwrap();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = writer.write(|txn| -> std::result::Result<(), StorageError> {
                txn.insert(&quad(0));
                panic!("boom")
            });
        }));
        assert!(caught.is_err());
        assert_eq!(handle.len().unwrap(), 0, "a panicking record committed");
        writer
            .write(|txn| {
                txn.insert(&quad(1));
                Ok::<_, StorageError>(())
            })
            .unwrap();
        assert_eq!(handle.len().unwrap(), 1);
    }

    #[test]
    fn only_a_shared_store_hands_out_a_writer() {
        assert!(DurableStore::in_memory().unwrap().shared_writer().is_none());
        let (store, handle) = DurableStore::in_memory_shared().unwrap();
        let writer = store.shared_writer().unwrap();
        assert!(Arc::ptr_eq(writer.store(), &handle));
        let (declaring, _) =
            DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph())
                .unwrap();
        assert!(declaring.shared_writer().is_some());
    }

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
