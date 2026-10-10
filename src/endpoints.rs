//! The thirteen resources this crate binds, under the namespace it owns.
//!
//! ```text
//! urn:iki:store:select           Source  SPARQL SELECT            urn:cap:store:read
//! urn:iki:store:ask              Source  SPARQL ASK               urn:cap:store:read
//! urn:iki:store:construct        Source  SPARQL CONSTRUCT         urn:cap:store:read
//! urn:iki:store:describe         Source  SPARQL DESCRIBE          urn:cap:store:read
//! urn:iki:store:graph-select     Source  SELECT, named graph set  urn:cap:store:read:graph:<iri> each
//! urn:iki:store:graph-ask        Source  ASK, named graph set     urn:cap:store:read:graph:<iri> each
//! urn:iki:store:graph-construct  Source  CONSTRUCT, graph set     urn:cap:store:read:graph:<iri> each
//! urn:iki:store:graph-describe   Source  DESCRIBE, graph set      urn:cap:store:read:graph:<iri> each
//! urn:iki:store:info             Source  backing, size, coverage  urn:cap:store:read
//! urn:iki:store:graphs           Source  readable graph names     urn:cap:store:read* (either)
//! urn:iki:store:update           Sink    SPARQL UPDATE, all of it urn:cap:store:write
//! urn:iki:store:graph-update     Sink    SPARQL UPDATE, one graph urn:cap:store:write:graph:<iri>
//! urn:iki:store:load             Sink    bulk-load an RDF doc     urn:cap:store:write
//! ```
//!
//! **Two doors in each direction, wide and narrow.** [`CAP_WRITE`] is `DROP ALL`, which
//! makes a module layered over this store hand that authority to everyone who may append
//! one triple; [`CAP_WRITE_GRAPH`] is the boundary that fixes it, enforced on effects
//! rather than syntax so that an update naming its graph with a *variable* — or naming
//! none at all — cannot slip through (`src/confine.rs`). [`CAP_READ`] is the same problem
//! read-side, and until [`CAP_READ_GRAPH`] existed the boundary had a **documented
//! bypass**: a caller holding the broad read grant could query another tenant's graph
//! directly and go around the module that was enforcing access to it. The read half is
//! confined **by construction** — the prepared query's dataset specification is set to
//! the named graphs before evaluation, so `graph=G` is exactly `FROM <G> FROM NAMED <G>`,
//! `graph="G1 G2"` is `FROM <G1> FROM <G2> FROM NAMED <G1> FROM NAMED <G2>`, and nothing is
//! copied (`src/scope.rs`).
//!
//! # ★ Which dataset each query face reads — stated because it is the bug class
//!
//! Two faces, two datasets, and neither is "everything":
//!
//! - **The broad forms** (`urn:iki:store:{select,ask,construct,describe}`) read SPARQL's
//!   default dataset: the store's **default graph** for bare patterns, with every named
//!   graph available to `GRAPH`. A bare `{ ?s ?p ?o }` therefore does NOT see a quad in a
//!   named graph — ledger #373. The whole-dataset read is
//!   `{ GRAPH ?g { ?s ?p ?o } } UNION { ?s ?p ?o }`.
//! - **The scoped forms** read exactly the named graphs in `graph=`: their merge is the
//!   default graph and they are the only named graphs, and the store's own default graph
//!   is unreachable.
//!
//! ★ **Several graphs, and every one of them must be granted.** A caller naming a graph it
//! holds no token for is REFUSED, with every missing token named — never answered over the
//! subset it does hold. Quietly shrinking the dataset is the failure #380, #373 and #378
//! share: a query that looks right, returns rows, and is wrong.
//!
//! ⚠ **A scoped query endpoint declares TWO required by-value inputs** (`graph` and
//! `query`), so a bare pipe into one is ambiguous and the engine says so rather than
//! guessing. Naming the graph — which a caller must do anyway — leaves `query` as the one
//! unnamed required input, so `… | urn:iki:store:graph-select graph=<G>` pipes normally.
//!
//! **A value gets into a query through `bindings=`**, never through the parser. For an
//! update there is no such door — oxigraph binds into a prepared query and offers nothing
//! for a prepared update — so [`crate::sparql`]'s term constructors are the supported
//! path, and both Sinks refuse a `bindings` argument rather than accepting one they would
//! have to honour by rewriting text.
//!
//! **One IRI per query form, following `ikigai-sparql`.** The form fixes the result
//! family, so it fixes the declared `outputs` and the default `as` too — a single
//! `query` IRI would have to declare all six serializations and let an agent guess which
//! three it can actually get. The conformance walk found that concretely: with both
//! families on one endpoint, the RDF face could not be probed at all, because probing it
//! means asking a SELECT to answer in N-Triples. Each form also **refuses** a query of
//! the wrong FAMILY rather than serving it under the wrong IRI.
//!
//! ⚠ **The check is on the result family, not the form, and that is intentional.** The
//! IRI's promise is its declared `outputs` and its default `as`, and those are fixed by the
//! family: an ASK served by `urn:iki:store:select` comes back in a result-set syntax that
//! IRI declares, and a DESCRIBE served by `urn:iki:store:construct` in a graph syntax.
//! What is refused is the crossing — a CONSTRUCT under `select`, a SELECT under
//! `construct` — which would answer in a syntax the IRI never offered. (Ledger #751 found
//! the docs claiming more than this; the docs were wrong, not the code.)
//!
//! # Why there is a query face here at all
//!
//! This crate's README warns, correctly, that a persistent store must not grow a second
//! query surface: `ikigai-sparql` already has four typed forms, an `as` selector and a
//! conformance walk. Two things force one anyway, and they are worth stating because the
//! warning is otherwise the right instinct:
//!
//! 1. **`ikigai-sparql`'s query endpoints declare no capability.** A `SELECT * { ?s ?p
//!    ?o }` is answered for any attenuated caller. Over a per-query federated dataset
//!    that is defensible; over a host's durable store — which holds whatever anyone ever
//!    put in it — it is not. A read gate is the whole point of `urn:cap:store:read`, and
//!    it can only be declared by the endpoint that serves the read.
//! 2. **Reaching `space_with_store` means handing out the `Arc<Store>`**, which forfeits
//!    golden-thread coverage for the life of the store (see [`crate::DurableStore`]).
//!    A read through *this* face is covered and therefore cacheable; a read through that
//!    one never can be. ★ And when the handle does leave, this face is the only one
//!    that can take the host's [`SharerWrites`](crate::SharerWrites) declaration into
//!    account — a scoped read of a graph the sharer cannot write is cacheable again,
//!    which the other face has no way to know or to say.
//!
//! So the duplication is one `evaluate` and one `serialize` — and it is the price of a
//! gated, cacheable read. The composition the README describes is still available and
//! still supported: ask for `DurableStore::open_shared`
//! and bind `ikigai_sparql::space_with_store` yourself, knowing what it costs.
//!
//! ⚠ **Do not bind both faces over two different stores in one host.** An agent reading
//! the manifold would see two query actions it cannot tell apart, and the one declaring
//! no capability is the one that answers an unrestricted query.

use std::sync::Arc;

use async_trait::async_trait;
use ikigai_core::{
    ArgRef, ArgSpec, Description, Endpoint, EndpointSpace, Error, Exact, Invocation, ReprType,
    Representation, Result, Verb,
};
use oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use oxigraph::model::{
    GraphName, GraphNameRef, NamedNode, NamedNodeRef, NamedOrBlankNode, Term, Variable,
};
use oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};
use oxigraph::sparql::QueryResults;

use crate::budget::{too_large, AnswerBound, AnswerBudget, CappedWriter, Deadline, Measure};
use crate::scope::GraphSet;
use crate::store::DurableStore;

/// The capability a read requires. Declared, therefore enforced by the kernel before
/// `invoke` and before any cache lookup.
///
/// **A read is not free here, and that is the difference from a query module.** This
/// store holds whatever the host put in it — an explanation archive, an annotation
/// graph, a materialized relational database. `ikigai-sparql`'s query endpoints declare
/// nothing because their dataset is assembled per query from sources the caller already
/// named; this one's dataset is standing state the caller did not name.
pub const CAP_READ: &str = "urn:cap:store:read";

/// The capability a write requires — coarse on purpose, and it is the keys to the store.
///
/// SPARQL UPDATE is not a family of small permissions: `DROP ALL`, `CLEAR ALL` and a
/// bare `DELETE WHERE { ?s ?p ?o }` each empty the dataset. `urn:iki:store:load` is the
/// same authority by another door. One unqualified name, so nothing about it suggests a
/// narrower grant than it is. (`ikigai-sparql`'s `CAP_UPDATE` reasons the same way about
/// the same act, at length.)
pub const CAP_WRITE: &str = "urn:cap:store:write";

/// The **per-graph** write scope, as declared: the wildcard-ACL form the ecosystem
/// already uses for `urn:cap:net:*` and fs path ACLs, meaning "holds SOME grant under
/// this prefix". A held grant names one graph — [`cap_write_graph`].
///
/// ★ This is the boundary a module layered over the store needs in order not to hand
/// [`CAP_WRITE`] to everyone who may append a triple. `urn:iki:store:graph-update`
/// enforces it on **effects rather than syntax**, which is the only way it can be exact;
/// `src/confine.rs` has the mechanism and the table of shapes it closes.
///
/// ⚠ **A grant names exactly one graph and nothing is a prefix of anything.** There is
/// no `urn:cap:store:write:graph:urn:iki:ledger:*` form, because `Capability::allows` is
/// an exact-match set membership and inventing prefix semantics for one token would make
/// this crate's grants mean something different from every other grant in the system. A
/// host that wants a caller to write three graphs grants three scopes — which is the
/// intended use, not a workaround.
pub const CAP_WRITE_GRAPH: &str = "urn:cap:store:write:graph:*";

/// The **per-graph** read scope, as declared — the same wildcard-ACL form as
/// [`CAP_WRITE_GRAPH`], meaning "holds SOME grant under this prefix". A held grant names
/// one graph: [`cap_read_graph`].
///
/// ★ **Without this, the write boundary has a documented bypass.** 0.2.1 segmented writes
/// and left [`CAP_READ`] as the whole dataset, so a module enforcing its own read
/// capability over a graph it owns could be gone around entirely by querying the store
/// directly under the broad grant. A boundary that holds in one direction is not a
/// boundary.
///
/// ⚠ **A grant names exactly one graph and nothing is a prefix of anything**, for the
/// reason set out on [`CAP_WRITE_GRAPH`]: `Capability::allows` is exact-match set
/// membership, and inventing prefix semantics for one token would make this crate's grants
/// mean something different from every other grant in the system. Three graphs is three
/// grants.
pub const CAP_READ_GRAPH: &str = "urn:cap:store:read:graph:*";

/// **Some** read authority over this store, broad or per-graph — the declared scope of
/// `urn:iki:store:graphs`, and the only resource that needs it.
///
/// ★ **It is one token because `requires` is ALL-of and the honest requirement here is
/// ANY-of.** `graphs` answers the same question — *which named graphs may I read* —
/// for two kinds of caller, and they hold different tokens for it: a tenant holds
/// [`cap_read_graph`] grants, a broad reader (and root) holds [`CAP_READ`]. Declaring
/// both would demand both and deny each of them; declaring one would deny the other at
/// the kernel's pre-check, *before* this endpoint can say why. So the declaration names
/// the family that both tokens belong to, and the endpoint enforces which half the
/// caller actually holds — the same declared-wildcard / enforced-exactly shape as
/// [`CAP_READ_GRAPH`], one segment further up. (`ikigai-core` PENDING §57's any-of
/// problem: with an any-of form in `requires`, this constant would be
/// `any_of([CAP_READ, CAP_READ_GRAPH])` and nothing else here would change.)
///
/// ⚠ **The `*` is NOT at a segment boundary, and that is deliberate.** The predicate is
/// `crate::select::cap_satisfies`, which is a plain `starts_with` over the held scopes
/// with the `*` stripped, so `urn:cap:store:read*` is satisfied by exactly
/// `urn:cap:store:read` and `urn:cap:store:read:graph:<iri>` — the whole read family and
/// nothing in the write one. `urn:cap:store:read:*` would miss the broad token (no
/// trailing colon on it) and `urn:cap:store:*` would admit a write-only caller, who would
/// then pass the pre-check and be told it may read no graphs instead of being denied.
/// The cost of the spelling is that a token nobody mints — `urn:cap:store:readable`, say
/// — would also satisfy it; this crate owns the grammar and mints its tokens through
/// [`cap_read_graph`], so no such token exists.
pub const CAP_READ_ANY: &str = "urn:cap:store:read*";

/// The scope a caller must hold to write `graph` through `urn:iki:store:graph-update`.
///
/// ```
/// assert_eq!(
///     ikigai_store::cap_write_graph("urn:iki:ledger:acme"),
///     "urn:cap:store:write:graph:urn:iki:ledger:acme"
/// );
/// ```
///
/// Concatenation is injective for a fixed prefix, so two graph IRIs never collide on one
/// token; and because the grant is matched exactly, a token for a *shorter* IRI grants
/// nothing over a longer one.
pub fn cap_write_graph(graph: &str) -> String {
    format!("urn:cap:store:write:graph:{graph}")
}

/// The scope a caller must hold to read `graph` through `urn:iki:store:graph-{select,
/// ask,construct,describe}`.
///
/// ```
/// assert_eq!(
///     ikigai_store::cap_read_graph("urn:iki:ledger:acme"),
///     "urn:cap:store:read:graph:urn:iki:ledger:acme"
/// );
/// ```
///
/// ⚠ **It is a sibling of [`cap_write_graph`], not a weaker form of it.** Holding the
/// write scope over a graph does not imply the read scope over it and vice versa — the
/// two are separate grants for the same reason the broad pair are, and a host that means
/// a module to do both grants both.
///
/// ★ **An update with a `WHERE` needs this as well as the write scope** (ledger #751),
/// because a `WHERE` reads the graph: `urn:iki:store:graph-update` refuses one from a
/// write-only caller on the grant, before evaluating anything. A write-only grant is
/// enough for `INSERT DATA`, `DELETE DATA`, `CLEAR`, `DROP` and `CREATE`.
pub fn cap_read_graph(graph: &str) -> String {
    format!("urn:cap:store:read:graph:{graph}")
}

/// The golden thread `urn:iki:store:update` cuts on success.
///
/// The kernel cuts the thread named after a mutating request's target, so this needs no
/// code — it is the target IRI. A cacheable read declares both this and [`LOAD_THREAD`].
pub const UPDATE_THREAD: &str = "urn:iki:store:update";

/// The golden thread `urn:iki:store:load` cuts on success.
///
/// ⚠ **Two writing IRIs means two threads, and a reader must depend on both.** The
/// kernel's auto-cut is per target and an endpoint cannot cut an arbitrary thread except
/// by resolving `urn:kernel:cut` (which needs `urn:cap:kernel:cut`), so collapsing these
/// into one name would cost a capability this module has no business holding. Depending
/// on both is cheaper and exact.
pub const LOAD_THREAD: &str = "urn:iki:store:load";

/// The golden thread `urn:iki:store:graph-update` cuts on success.
///
/// ★ **A third writing IRI means a third thread, and a reader must depend on all
/// three.** Adding a write door without adding its thread would have left every
/// cacheable read serving stale bytes after a scoped write — silently, on the branch
/// that looks like success. `a_scoped_write_invalidates_a_cached_read` is the test that
/// keeps it true; the reason an endpoint cannot simply cut [`UPDATE_THREAD`] instead is
/// on [`LOAD_THREAD`].
pub const GRAPH_UPDATE_THREAD: &str = "urn:iki:store:graph-update";

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_ANY_URI: &str = "http://www.w3.org/2001/XMLSchema#anyURI";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_POSITIVE_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#positiveInteger";

/// Result serializations, SELECT/ASK first — index 0 is the default and the value a
/// conformance walk synthesizes.
const RESULT_OUTPUTS: [&str; 4] = [
    "application/sparql-results+json",
    "application/sparql-results+xml",
    "text/csv",
    "text/tab-separated-values",
];
/// Graph serializations for CONSTRUCT/DESCRIBE, default first.
const GRAPH_OUTPUTS: [&str; 2] = ["application/n-triples", "text/turtle"];
/// Input syntaxes `urn:iki:store:load` parses, default first.
const LOAD_FORMATS: [&str; 5] = [
    "text/turtle",
    "application/n-triples",
    "application/n-quads",
    "application/trig",
    "application/rdf+xml",
];

/// Bind this store's thirteen resources into a space.
///
/// The store is moved in: this space and the endpoints in it are the only holders of the
/// dataset unless the caller took a handle at construction (see
/// [`DurableStore`]).
///
/// ⚠ **One kernel per space.** Several kernels over one dataset each get their own
/// `space(store.clone())`, which this crate counts ([`DurableStore::spaces_bound`]). One
/// space — or one composite containing it — handed to several kernels is invisible here,
/// because an endpoint is not told which kernel invoked it, and a write through one kernel
/// then leaves the others serving stale cached reads (ledger #761).
pub fn space(store: DurableStore) -> EndpointSpace {
    // ★ Counted for as long as this space lives, so a SECOND live space over the same
    // dataset forfeits coverage for both — and one bound after a cached read refuses
    // (ledger #751; `DurableStore::spaces_bound` has the argument).
    let store = Arc::new(store.bound_into_a_space());
    let mut space = EndpointSpace::new();
    for (form, id, scoped_id, graph_shaped) in FORMS {
        space = space.bind(
            Exact::new(format!("urn:iki:store:{form}")),
            QueryEndpoint {
                store: Arc::clone(&store),
                form,
                id,
                graph_shaped,
                scoped: false,
            },
        );
        space = space.bind(
            Exact::new(format!("urn:iki:store:graph-{form}")),
            QueryEndpoint {
                store: Arc::clone(&store),
                form,
                id: scoped_id,
                graph_shaped,
                scoped: true,
            },
        );
    }
    space
        .bind(
            Exact::new("urn:iki:store:info"),
            InfoEndpoint {
                store: Arc::clone(&store),
            },
        )
        .bind(
            Exact::new("urn:iki:store:graphs"),
            GraphsEndpoint {
                store: Arc::clone(&store),
            },
        )
        .bind(
            Exact::new("urn:iki:store:update"),
            UpdateEndpoint {
                store: Arc::clone(&store),
            },
        )
        .bind(
            Exact::new("urn:iki:store:graph-update"),
            GraphUpdateEndpoint {
                store: Arc::clone(&store),
            },
        )
        .bind(Exact::new("urn:iki:store:load"), LoadEndpoint { store })
}

/// Cacheable under all three write threads when this read is covered; `Expiry::Always`
/// when an invisible writer may have changed what it read.
///
/// ★ This is the one place the coverage decision has teeth. `ikigai-sparql` documents
/// why a thread that is right on some writes and wrong on others is worse than no
/// thread — "always fresh" becomes "fresh until someone writes the other way, then stale
/// with no bound and no signal". A covered store has no other way to write.
///
/// ⚠ **`graphs` is the whole difference between a right answer and a wrong one**, which
/// is why it is threaded from the call sites rather than read off the store: on a store
/// built with `open_shared_declaring` the answer is per-read, not per-store. `Some(set)`
/// is a scoped read — its universe is exactly those named graphs, by construction
/// (`src/scope.rs`) — and `None` is every read that can see the whole dataset, the
/// default graph included.
///
/// ★ **A scoped read over several graphs is covered only if EVERY member is**, the same
/// `.all` rule `urn:iki:store:graphs` applies: an invisible writer that can reach any one
/// member can change the answer. The consequence worth knowing before someone wonders why
/// a join is slow — in a host where one graph is uncovered (gonk's browse graph, which
/// its sharer writes), **a join across that graph and a covered one is never cached**,
/// while a read of the covered graph alone still is. Passing `None` where a graph was known is merely slow;
/// passing `Some` where the read was NOT confined would cache a read of the whole
/// dataset under a promise about one graph, which is the silent staleness above.
/// [`DurableStore::read_is_covered`] holds the table.
fn with_freshness(
    rep: Representation,
    store: &DurableStore,
    graphs: Option<&[NamedNode]>,
) -> Representation {
    let covered = match graphs {
        None => store.read_is_covered(None),
        Some(graphs) => graphs
            .iter()
            .all(|graph| store.read_is_covered(Some(graph.as_str()))),
    };
    covered_by(rep, store, covered)
}

/// The three threads, or nothing — factored out because `urn:iki:store:graphs` reaches
/// the same decision a different way: its universe is a SET of graphs, so it asks
/// [`DurableStore::read_is_covered`] once per graph and is covered only if every answer
/// was yes. One place still holds which threads a cacheable read depends on, so a fourth
/// write door cannot be added to one path and forgotten on the other.
///
/// ⚠ The final say goes through [`DurableStore::commit_to_caching`], which records that
/// this dataset now has a cached read before re-checking for a second space — the half
/// of the handshake that lets a late `space()` refuse instead of going stale under it.
fn covered_by(rep: Representation, store: &DurableStore, covered: bool) -> Representation {
    if store.commit_to_caching(covered) {
        rep.cacheable()
            .depends_on(UPDATE_THREAD)
            .depends_on(LOAD_THREAD)
            .depends_on(GRAPH_UPDATE_THREAD)
    } else {
        rep
    }
}

// ---------------------------------------------------------------------------- query

/// The four query forms, as `(IRI suffix, description id, scoped description id,
/// graph-shaped?)`.
///
/// Each carries a UNIQUE description id, so their catalog subjects and any id-keyed
/// projection (an MCP tool name) do not collide — with each other, with the scoped twin,
/// or with `ikigai-sparql`'s `sparql-{form}`.
///
/// ⚠ **Eight IRIs rather than a `graph=` argument on four, and that is a real surface
/// cost paid deliberately.** The declared `requires` differs between the broad and the
/// scoped form ([`CAP_READ`] vs [`CAP_READ_GRAPH`]), and the kernel's capability
/// pre-check runs *before* `invoke` can see an argument — so one IRI taking an optional
/// `graph=` would have to declare the weaker of the two and let the endpoint decide,
/// which is exactly the over-offer the module recipe forbids. Same argument the write
/// door made in 0.2.1.
const FORMS: [(&str, &str, &str, bool); 4] = [
    ("select", "store-select", "store-graph-select", false),
    ("ask", "store-ask", "store-graph-ask", false),
    (
        "construct",
        "store-construct",
        "store-graph-construct",
        true,
    ),
    ("describe", "store-describe", "store-graph-describe", true),
];

#[derive(Clone)]
struct QueryEndpoint {
    store: Arc<DurableStore>,
    /// The SPARQL form this IRI answers.
    form: &'static str,
    id: &'static str,
    /// Whether this form answers with a graph (CONSTRUCT/DESCRIBE) or a result set.
    graph_shaped: bool,
    /// Whether this is the graph-scoped twin: takes `graph=` (one IRI or several),
    /// requires a grant for every graph named, and sees nothing else. See `src/scope.rs`.
    scoped: bool,
}

#[async_trait]
impl Endpoint for QueryEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.store.serving()?;
        match inv.request.verb {
            Verb::Source => {
                let query = inv.inline_str("query")?;
                // ★ The parameterized half of the capability, checked before anything is
                // parsed or evaluated: the kernel's pre-check can only see the wildcard
                // (this caller holds SOME grant under `urn:cap:store:read:graph:`), and
                // this is where it is checked that a grant names EVERY graph asked for.
                let target = self.scope(inv)?;
                if let Some(set) = target.as_ref().filter(|set| !set.is_canonical()) {
                    // ★ Only after the refusal above: a caller is told what it lacks under
                    // the spelling it sent, not under one it never wrote.
                    return self.reissue_canonical(inv, set).await;
                }
                let target = target.map(|set| set.graphs().to_vec());
                // ★ Bounded before the parser sees a byte (ledger #915): a stack overflow
                // in the recursive parser or evaluator aborts the whole host.
                crate::limits::check_sparql(query, "query")?;
                // ⚠ Absent and UNREADABLE are different answers (ledger #751): a `bindings`
                // that is present but not inline UTF-8 is refused, never read as "no
                // bindings" — which would run the query unfiltered and return every row.
                let bound = match optional_inline_str(inv, "bindings")? {
                    Some(json) => crate::sparql::parse_bindings(json)?,
                    None => Vec::new(),
                };
                // Read here, before evaluation, so an unreadable `as` is refused rather
                // than substituted with the default after the query has already run.
                let as_type = optional_inline_str(inv, "as")?.map(str::to_string);
                // ★ Parse, evaluate and serialize on a stack sized for the query: brackets
                // are bounded above, but a long `FILTER` list or `||` chain recurses in the
                // evaluator too, and does so legitimately. `src/limits.rs` has the numbers.
                // ★ …and within this caller's TIME budget (ledger #964): the caller is
                // answered at the budget, the evaluation is cancelled, and the serializers
                // stop between rows. `src/budget.rs` has the numbers and the limits.
                let this = self.clone();
                let text = query.to_string();
                let confined = target.clone();
                let requested = crate::budget::inline_bound(inv, "budget")?;
                // ★ …and within this caller's answer SIZE (ledger #970): rows (triples for a
                // graph) and serialized bytes, counted while serializing and refused past the
                // bound, never truncated. A grant raises it; `max_rows=`/`max_bytes=` only
                // lower it, and either one present but not inline is refused, never ignored.
                let answer = crate::budget::effective_answer(
                    crate::budget::inline_bound(inv, "max_rows")?,
                    crate::budget::inline_bound(inv, "max_bytes")?,
                    self.store.answer_budget().for_capability(inv.capability),
                )?;
                let (media, bytes) = self.store.evaluate(
                    query,
                    false,
                    inv.capability,
                    requested,
                    move |deadline| {
                        let query = text.as_str();
                        let as_type = as_type.as_deref();
                        // ★ Parsed by the parser oxigraph uses, measured, and only then
                        // handed to oxigraph: its planner cannot be cancelled, so the
                        // algebra is bounded before it plans (ledger #964, `src/budget.rs`).
                        let parsed =
                            spargebra::SparqlParser::new()
                                .parse_query(query)
                                .map_err(|e| Error::InvalidArgument {
                                    name: "query".to_string(),
                                    detail: format!("not a SPARQL query: {e}"),
                                })?;
                        crate::budget::check_query(&parsed, "query")?;
                        // ★ No `SERVICE`, refused by name before evaluation, and an evaluator
                        // that refuses one itself in every build (ledger #1083,
                        // `src/service.rs`): in a host with `oxigraph/http-client` on, a plain
                        // evaluator turns `SERVICE <http://…>` into an ungated request.
                        crate::service::refuse_service(&parsed, "query")?;
                        let mut prepared = crate::service::evaluator()
                            .with_cancellation_token(deadline.token())
                            .for_query(parsed);
                        if let Some(target) = &confined {
                            // ⚠ `FROM` / `FROM NAMED` is a SECOND way to name a dataset. It could
                            // not widen the scope — `confine` overwrites the specification the
                            // parser built from it — but answering `FROM <other>` with this
                            // graph's rows would label one tenant's data with another's graph
                            // name, so it is refused rather than silently overridden.
                            if crate::scope::names_its_own_dataset(&prepared) {
                                let from: String = target
                                    .iter()
                                    .map(|g| format!("FROM <{}> ", g.as_str()))
                                    .collect();
                                let named: Vec<String> = target
                                    .iter()
                                    .map(|g| format!("FROM NAMED <{}>", g.as_str()))
                                    .collect();
                                return Err(Error::InvalidArgument {
                                    name: "query".to_string(),
                                    detail: format!(
                                    "this query carries its own `FROM` / `FROM NAMED` clauses, \
                                     and `urn:iki:store:graph-{}` already fixes the dataset: \
                                     `graph=` IS the dataset, exactly `{from}{}`. The clauses are \
                                     refused rather than overridden, because answering a `FROM` \
                                     naming another graph with this graph's rows would be a wrong \
                                     answer that looked right. Drop them, and name every graph \
                                     you mean in `graph=`",
                                    this.form,
                                    named.join(" "),
                                ),
                                });
                            }
                            crate::scope::confine(&mut prepared, target);
                        }
                        for (name, term) in bound.iter().cloned() {
                            // `Variable::new` cannot fail here: `parse_bindings` already held the
                            // name to the same character set, and named the offending key when it
                            // did — which the evaluator's own error does not.
                            let variable =
                                Variable::new(&name).map_err(|e| Error::InvalidArgument {
                                    name: "bindings".to_string(),
                                    detail: format!("`{name}` is not a variable name: {e}"),
                                })?;
                            prepared = prepared.substitute_variable(variable, term);
                        }
                        let results = prepared
                            .on_store(this.store.dataset())
                            .execute()
                            .map_err(|e| this.query_error(e, &bound))?;

                        // ★ Refuse a query of the wrong shape rather than serving it here. The
                        // IRI is a promise about what comes back — it is what fixes this
                        // action's declared `outputs` — and answering a CONSTRUCT under
                        // `urn:iki:store:select` would make that promise true only by accident.
                        let is_graph = matches!(results, QueryResults::Graph(_));
                        if is_graph != this.graph_shaped {
                            return Err(Error::InvalidArgument {
                                name: "query".to_string(),
                                detail: format!(
                                    "this is `{}`, which answers with {}; that query answers with \
                                 {}. Resolve the IRI for its form instead",
                                    this.iri(),
                                    shape(this.graph_shaped),
                                    shape(is_graph)
                                ),
                            });
                        }

                        if this.graph_shaped {
                            serialize_graph(results, as_type, deadline, answer)
                        } else {
                            serialize_solutions(results, as_type, deadline, answer)
                        }
                    },
                )?;
                Ok(with_freshness(
                    Representation::new(
                        ReprType::new(&media).with_param("charset", "utf-8"),
                        bytes,
                    ),
                    &self.store,
                    // `target` is `Some` exactly when `confine` ran, so this is the
                    // read's real universe and not a restatement of the IRI.
                    target.as_deref(),
                ))
            }
            other => Err(unsupported(self.id, other)),
        }
    }

    fn name(&self) -> &str {
        self.id
    }

    fn describe(&self) -> Description {
        let outputs: &[&str] = if self.graph_shaped {
            &GRAPH_OUTPUTS
        } else {
            &RESULT_OUTPUTS
        };
        let form = self.form.to_uppercase();
        let desc = if self.scoped {
            Description::new(self.id)
                .title(format!("SPARQL {form} confined to named graphs"))
                .summary(format!(
                    "Evaluate a SPARQL {form} against exactly the named graphs given by \
                     `graph` — one, or several separated by ASCII whitespace — under a grant for \
                     every one of them. The dataset is theirs alone: `graph=\"G1 G2\"` means \
                     `FROM <G1> FROM <G2> FROM NAMED <G1> FROM NAMED <G2>`, so a bare pattern \
                     reads (and joins across) the merge of the graphs, `GRAPH ?g` binds only \
                     them, `GRAPH <other>` matches nothing, and the store's own default graph \
                     is unreachable. Naming a graph the caller holds no grant for is refused \
                     with the missing grant named, never answered over the rest. Covered — \
                     and so cached — only when every graph named is. A query whose answer is \
                     of the other family (a graph where this form answers with a result set, \
                     or the reverse) is refused, and so is one carrying its own `FROM` \
                     clauses; SELECT and ASK share a family, as CONSTRUCT and DESCRIBE do, \
                     and either of a pair is answered under either IRI.",
                ))
                .verb(Verb::Source)
                .verb(Verb::Meta)
                .requires(CAP_READ_GRAPH)
                .input(
                    ArgSpec::new("graph")
                        .summary(
                            "The named graphs this query may read: one IRI, or several \
                             separated by ASCII whitespace — space, tab, newline (not commas, and \
                             not non-ASCII spaces: an IRI may contain either). \
                             Order and repetition do not matter. The caller must hold \
                             `urn:cap:store:read:graph:<IRI>` for EVERY graph named; one \
                             missing grant refuses the whole read.",
                        )
                        // A LIST of IRIs, and an ArgSpec has no way to say "many" — so the
                        // wire's own class, as `ikigai-sparql` declares for its graph list.
                        // `xsd:anyURI` would tell a validator `G1 G2` is malformed.
                        .class(XSD_STRING),
                )
        } else {
            Description::new(self.id)
                .title(format!("SPARQL {form} over the durable store"))
                .summary(format!(
                    "Evaluate a SPARQL {form} against the store this host owns, under \
                     SPARQL's default dataset: a bare pattern reads the store's DEFAULT \
                     graph only, and a quad in a named graph is reached through `GRAPH`. \
                     To read every quad write `{{ GRAPH ?g {{ ?s ?p ?o }} }} UNION {{ ?s ?p ?o \
                     }}`. A query whose answer is of the other family (a graph where this \
                     form answers with a result set, or the reverse) is refused; SELECT and \
                     ASK share a family, as CONSTRUCT and DESCRIBE do, and either of a pair \
                     is answered under either IRI.",
                ))
                .verb(Verb::Source)
                .verb(Verb::Meta)
                .requires(CAP_READ)
        };
        let desc = desc
            .input(
                ArgSpec::new("query")
                    .summary(format!("A SPARQL {form} query."))
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("bindings")
                    .summary(
                        "Values for variables in the query, as a JSON object of name → \
                         value, so a value never passes through the SPARQL parser as \
                         syntax. A bare JSON string is a plain literal; a number, an \
                         `xsd:integer` or `xsd:double`; `true`/`false`, an `xsd:boolean`; \
                         and the SPARQL-results term shape \
                         (`{\"type\":\"uri\",\"value\":…}`) names an IRI or a typed or \
                         language-tagged literal. Every bound variable must appear in the \
                         query's projection — `SELECT *` if in doubt — and a binding the \
                         query does not mention is refused, not ignored.",
                    )
                    .class(XSD_STRING)
                    .optional(),
            )
            .input(
                ArgSpec::new("as")
                    .summary(format!(
                        "Result serialization; one of {}. An `as` this form cannot answer \
                         in is refused, never substituted.",
                        outputs.join(", ")
                    ))
                    .class(XSD_STRING)
                    .one_of(outputs.iter().copied())
                    .default_value(outputs[0])
                    .optional(),
            )
            .input(budget_arg())
            .input(answer_rows_arg(
                self.store.answer_budget(),
                self.graph_shaped,
            ))
            .input(answer_bytes_arg(self.store.answer_budget()));
        outputs.iter().fold(desc, |desc, media| desc.output(*media))
    }
}

impl QueryEndpoint {
    /// This endpoint's IRI, for a refusal that has to name it.
    fn iri(&self) -> String {
        if self.scoped {
            format!("urn:iki:store:graph-{}", self.form)
        } else {
            format!("urn:iki:store:{}", self.form)
        }
    }

    /// The graphs this invocation may read, or `None` on the broad form.
    ///
    /// ★ Declared and enforced are the same scope, which is the whole contract. The
    /// kernel checks the declared [`CAP_READ_GRAPH`] wildcard before `invoke`; this
    /// checks a grant against EVERY graph actually named, because an argument is not
    /// visible to a pre-check.
    ///
    /// ⚠ **Refuse, never drop.** A caller naming `G1 G2` while holding only `G1`'s grant
    /// is denied, and the denial names `G2`'s token. The alternative — confining to the
    /// graphs it does hold — returns rows, looks right, and silently answers a different
    /// question from the one asked; that is the bug class this door exists to close.
    ///
    /// ⚠ **[`CAP_READ`] does NOT satisfy this door and is not meant to.** A broad holder
    /// uses `urn:iki:store:select` and sees the whole dataset. Accepting both here would
    /// mean the declared scope was not the enforced one — and the ablation runs the other
    /// way too: a graph grant does not open the broad door, which is what makes it a
    /// boundary rather than a hint.
    fn scope(&self, inv: &Invocation<'_>) -> Result<Option<GraphSet>> {
        if !self.scoped {
            return Ok(None);
        }
        let set = GraphSet::parse(inv.inline_str("graph")?)?;
        let missing: Vec<&NamedNode> = set
            .graphs()
            .iter()
            .filter(|graph| !inv.capability.allows(&cap_read_graph(graph.as_str())))
            .collect();
        if missing.is_empty() {
            return Ok(Some(set));
        }
        let graphs: Vec<String> = missing
            .iter()
            .map(|g| format!("<{}>", g.as_str()))
            .collect();
        let tokens: Vec<String> = missing
            .iter()
            .map(|g| format!("`{}`", cap_read_graph(g.as_str())))
            .collect();
        // An IRI may contain a comma, so `graph=A,B` parses as ONE graph and demands a
        // token nobody holds. Say what was probably meant rather than only that it failed.
        let comma = if missing.iter().any(|g| g.as_str().contains(',')) {
            " If a comma was meant to separate several graphs, separate them with whitespace \
             instead: an IRI may contain a comma, so this was read as one graph."
        } else {
            ""
        };
        Err(Error::Denied(format!(
            "reading {} through `{}` needs {} {}, which this capability does not hold. \
             Every graph a scoped read names must be granted: the read is refused rather \
             than answered over the graphs that are, because a narrower dataset than the one \
             asked for returns rows that look right and are not. A grant names exactly one \
             graph; holding `{CAP_READ}` does not imply it, and is instead the authority for \
             `urn:iki:store:{}` over the whole dataset.{comma}",
            graphs.join(", "),
            self.iri(),
            if tokens.len() == 1 {
                "the grant"
            } else {
                "the grants"
            },
            tokens.join(", "),
            self.form,
        )))
    }

    /// Answer a non-canonical `graph=` spelling by resolving the canonical one.
    ///
    /// ★ **This is what makes the cache order-independent**, and it has to be done this
    /// way because the kernel keys an entry on the request's raw argument bytes — which an
    /// endpoint cannot rewrite. So `graph="B A"`, `graph="A B"` and `graph="A B A"` each
    /// become a sub-request for `graph="A B"`: one evaluation, one computed entry, and the
    /// outer spelling inherits its expiry and golden threads through `Invocation::issue`,
    /// so a write still invalidates every spelling.
    ///
    /// ⚠ The canonical spelling never reaches here, so a single-graph read — the ledger's
    /// hot path, spelled canonically by construction — pays nothing for any of this. A
    /// trailing newline from a pipe (field guide 9h) is non-canonical and is normalized.
    async fn reissue_canonical(
        &self,
        inv: &Invocation<'_>,
        set: &GraphSet,
    ) -> Result<Representation> {
        let mut request = inv.request.clone();
        request.args.insert(
            "graph".to_string(),
            ArgRef::Inline(set.canonical().into_bytes()),
        );
        inv.issue(request).await
    }

    /// Make oxigraph's refusal of an unusable binding say what to do about it.
    ///
    /// ★ **The refuse-or-ignore question is settled upstream, in the right direction.**
    /// oxigraph rejects a substitution for a variable the query does not project rather
    /// than dropping it, so a binding that would have done nothing is an error instead of
    /// a query that silently ran unconstrained — which is exactly the failure this
    /// argument exists to prevent, arriving by the back door. This crate does not have to
    /// choose; it only has to explain, because the upstream sentence ("does not contains
    /// variable ?o in its SELECT projection") is true and tells a caller nothing about
    /// the fix.
    ///
    /// ⚠ Matched on text, like [`crate::store`]'s lock matcher and for the same reason:
    /// the evaluator reports it as an untyped evaluation error.
    /// `an_unprojected_binding_is_refused_with_the_fix` pins it against a real query, so
    /// an upstream rewording fails a test rather than quietly degrading the message.
    fn query_error(&self, e: impl std::fmt::Display, bound: &[(String, Term)]) -> Error {
        let text = e.to_string();
        if bound.is_empty() || !text.contains("projection") {
            return Error::Endpoint(format!("query: {text}"));
        }
        let names: Vec<String> = bound.iter().map(|(n, _)| format!("?{n}")).collect();
        Error::InvalidArgument {
            name: "bindings".to_string(),
            detail: format!(
                "{text}. Every bound variable must appear in the query's projection, and this \
                 query projects fewer than the {} bound here ({}). Either name it in the \
                 SELECT clause or write `SELECT *`; ASK, CONSTRUCT and DESCRIBE have no \
                 projection to widen and accept any variable in the pattern. A binding the \
                 query does not mention is refused rather than ignored, so that a filter you \
                 thought was applied can never silently not be",
                names.len(),
                names.join(", "),
            ),
        }
    }
}

fn shape(graph: bool) -> &'static str {
    if graph {
        "a graph"
    } else {
        "a result set"
    }
}

// ----------------------------------------------------------------------------- info

#[derive(Clone)]
struct InfoEndpoint {
    store: Arc<DurableStore>,
}

#[async_trait]
impl Endpoint for InfoEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.store.serving()?;
        match inv.request.verb {
            Verb::Source => {
                // ⚠ `Store::len()` is a FULL SCAN, not metadata: measured 2.4 µs empty,
                // 7.3 ms at 100k quads, 72 ms at 1M (see docs/design/handle-model.md).
                // That is affordable here only because this face is cacheable under the
                // write threads on a covered store, so the scan is paid once per write
                // rather than once per read. On a SHARED store it is paid every time —
                // another cost of handing out the handle, and stated in the output.
                let quads = self
                    .store
                    .dataset()
                    .len()
                    .map_err(|e| Error::Endpoint(format!("counting quads: {e}")))?;
                // ★ This is whole-dataset PROVENANCE — did the handle leave this crate —
                // and not a caching verdict, which is why it reads `is_sole_writer` and
                // not `read_is_covered`: an operator asking this face wants to know what
                // kind of store the host built, and the per-read answer is not a property
                // of the store at all. ⚠ The wire label stays `covered:` deliberately.
                // 0.2.4 shipped it and a host or a runbook may be grepping for it, so
                // renaming the method is free while renaming these bytes is not; the
                // `describe` summary below says which question the line answers.
                let mut text = format!(
                    "backing: {}\nquads: {quads}\ncovered: {}\n",
                    self.store.backing(),
                    self.store.is_sole_writer()
                );
                // ★ A fourth line ONLY when there is a declaration, so the bytes an
                // existing host reads are unchanged — and so the line's presence is
                // itself the signal that this store's scoped reads are cached under a
                // promise, which is a thing an operator should be able to see from
                // outside the process.
                if let Some(writes) = self.store.sharer_writes() {
                    let named: Vec<String> = writes
                        .named_graphs()
                        .map(|iri| format!("<{iri}>"))
                        .collect();
                    text.push_str("sharer writes: the default graph");
                    if !named.is_empty() {
                        text.push_str(&format!(" and {}", named.join(", ")));
                    }
                    text.push('\n');
                }
                Ok(with_freshness(
                    Representation::new(
                        ReprType::new("text/plain").with_param("charset", "utf-8"),
                        text.into_bytes(),
                    ),
                    &self.store,
                    // `info` counts every quad in the dataset, the default graph
                    // included, so it is never confined and never covered on a shared
                    // store — whatever was declared.
                    None,
                ))
            }
            other => Err(unsupported("store-info", other)),
        }
    }

    fn name(&self) -> &str {
        "store-info"
    }

    fn describe(&self) -> Description {
        Description::new("store-info")
            .title("Store backing and size")
            .summary(
                "Where this store's bytes live, how many quads it holds, and whether its \
                 golden-thread coverage is intact (`covered: false` means the raw handle \
                 was handed out at construction, so this face and the broad query faces \
                 are never cached). A `sharer writes:` line, when present, names the \
                 graphs the host declared that handle's holder may write — every OTHER \
                 named graph is one whose scoped reads are cached under that promise.",
            )
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .requires(CAP_READ)
            .output("text/plain")
    }
}

// --------------------------------------------------------------------------- graphs

/// `urn:iki:store:graphs` — the named graphs that **exist** in this store and that
/// this capability **may read**, one IRI per line.
///
/// # ★ Why a resource for this at all
///
/// A scoped read is confined to a graph the caller already named, so it enumerates
/// nothing, and a module that partitions its state by graph (`ikigai-ledger` is the first,
/// and will not be the last) has no way to answer *which partitions are there*. Without
/// this, every such module writes the same two-path branch — its own grant list as the
/// candidate set, a broad store query for root — and the root half of that branch reaches
/// through a door the module does not declare.
///
/// # ★ The answer is the INTERSECTION, and the argument is that only one half is new
///
/// Three answers were available: the graphs you may read (from the capability alone), the
/// graphs that exist (from the store), or both, distinguished. This resource answers
/// **exists AND may-read**, because:
///
/// - **The may-read half is already in the caller's hands.** It *is* the caller's
///   capability — `Capability::scopes()`, the same set this endpoint reads. A resource
///   whose whole output a caller can compute without asking is not worth a round trip, and
///   `ikigai-ledger` #4 computed exactly that half for itself.
/// - **Existence is the half that has to be gated**, because it is the half that is not
///   the caller's own. That makes it the half worth serving.
/// - **"Both, distinguished" cannot be answered uniformly.** Root's may-read set is not
///   enumerable — that is what root means — so the granted-but-absent column would be
///   empty for root and populated for a tenant, and a consumer would be reading two
///   different documents under one IRI.
///
/// ⚠ **It is not an oracle**, and that is the property to re-check before changing
/// anything here. The lines a tenant gets back are a subset of the graphs its own grants
/// name: existence is disclosed only for a graph the caller may already read. A caller
/// learns nothing whatsoever about a graph it holds no grant for — not that it exists, not
/// that it does not, not how many there are.
///
/// ⚠ Precisely, because "it could already ask" is *nearly* true and the gap is worth
/// stating: `urn:iki:store:graph-ask` over its own graph already tells a tenant whether
/// that graph holds a quad. The one thing this adds is the **registered-but-empty** case —
/// a graph created by `CREATE GRAPH` with nothing in it, which `contains_named_graph`
/// reports and an `ASK` cannot see. That is still a graph the caller holds a grant for, so
/// it crosses no boundary; it is simply not literally true that every line here was already
/// reachable.
///
/// # The two paths, and why a consumer never branches on which one it got
///
/// - A caller holding [`CAP_READ`] — the broad reader, and **root**, which allows every
///   scope — may read the whole dataset, so its answer is every named graph in it.
///   `Capability::scopes()` is `None` for root and so cannot be the source of that answer;
///   the store is.
/// - Any other caller reaching this endpoint holds per-graph grants (the declared
///   [`CAP_READ_ANY`] guarantees at least one grant in the read family), so its candidate
///   set comes from the capability and each candidate costs one point lookup — never a
///   scan, and never a cross-graph query.
///
/// Both produce the same bytes in the same order for the same visible store: sorted IRIs,
/// one per line, no angle brackets, and an **empty body** when there is nothing to list.
/// A consumer reads lines; nothing in the shape says which path produced them.
///
/// ⚠ **The default graph is never listed**, on either path. It has no IRI, so no
/// `urn:cap:store:read:graph:` token can name it and no scoped read can reach it — which
/// also means an empty answer does NOT mean an empty store. Blank-node graph names are
/// skipped for the same reason: nothing can grant one, and `urn:iki:store:graph-*` takes
/// an IRI.
#[derive(Clone)]
struct GraphsEndpoint {
    store: Arc<DurableStore>,
}

#[async_trait]
impl Endpoint for GraphsEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.store.serving()?;
        match inv.request.verb {
            Verb::Source => {
                // ★ The parameterized half of the declared scope, and the whole branch:
                // `allows` is exact, and it is TRUE for root, so this one predicate sorts
                // root and the broad reader — who may read every graph — from a tenant,
                // whose grants are its universe. A caller passing the pre-check with
                // neither (a grant under the family prefix that names no graph) falls to
                // the second arm and is told it may read nothing, which is true.
                let (names, covered) = if inv.capability.allows(CAP_READ) {
                    (self.every_named_graph()?, self.store.read_is_covered(None))
                } else {
                    let granted = granted_graphs(inv);
                    // ⚠ Covered only if EVERY candidate is: this read's answer is a
                    // function of each candidate's existence, so an invisible writer that
                    // can reach any one of them can change it. Vacuously true for a caller
                    // with no candidates, whose answer is empty whatever the store holds.
                    let covered = granted
                        .iter()
                        .all(|graph| self.store.read_is_covered(Some(graph.as_str())));
                    (self.existing_among(&granted)?, covered)
                };
                let mut text = String::new();
                for name in &names {
                    text.push_str(name);
                    text.push('\n');
                }
                Ok(covered_by(plain(text), &self.store, covered))
            }
            other => Err(unsupported("store-graphs", other)),
        }
    }

    fn name(&self) -> &str {
        "store-graphs"
    }

    fn describe(&self) -> Description {
        Description::new("store-graphs")
            .title("Which named graphs this capability may read")
            .summary(
                "The named graphs that exist in this store and that this capability may \
                 read, as sorted IRIs, one per line — the enumeration a graph-scoped read \
                 cannot perform, because it is confined to a graph the caller already \
                 named. A caller holding a per-graph grant sees exactly the graphs its own \
                 grants name that something has been written to; a caller holding the \
                 broad `urn:cap:store:read` (and root, which holds every scope) may read \
                 the whole dataset and sees every named graph in it. The two answer the \
                 same question in the same shape, so nothing that reads this has to know \
                 which it is. The store's default graph is never listed: it has no IRI, so \
                 no grant names it and no scoped read reaches it — an empty answer does \
                 not mean an empty store.",
            )
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .requires(CAP_READ_ANY)
            .output("text/plain")
    }
}

impl GraphsEndpoint {
    /// Every named graph in the store, for a caller that may read all of them.
    fn every_named_graph(&self) -> Result<Vec<String>> {
        let mut names = std::collections::BTreeSet::new();
        for name in self.store.dataset().named_graphs() {
            let name = name.map_err(|e| Error::Endpoint(format!("listing graphs: {e}")))?;
            // A blank-node graph name is skipped rather than rendered: no capability
            // token can name one and no `urn:iki:store:graph-*` call can take one, so
            // listing it would offer a caller a name it cannot use.
            if let NamedOrBlankNode::NamedNode(iri) = name {
                names.insert(iri.into_string());
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Which of `granted` the store actually holds — one point lookup each, never a scan.
    fn existing_among(&self, granted: &[NamedNode]) -> Result<Vec<String>> {
        let mut names = std::collections::BTreeSet::new();
        for graph in granted {
            if self
                .store
                .dataset()
                .contains_named_graph(graph.as_ref())
                .map_err(|e| Error::Endpoint(format!("looking up <{graph}>: {e}")))?
            {
                names.insert(graph.as_str().to_string());
            }
        }
        Ok(names.into_iter().collect())
    }
}

/// The graphs this capability names in its own per-graph read grants.
///
/// ⚠ Root never reaches here (it takes the [`CAP_READ`] path, which it allows), and a
/// root capability would enumerate nothing anyway — `Capability::scopes()` is `None` for
/// it, which is what root means. The empty vector is the honest answer for any capability
/// that names no graph: a *family* grant (`urn:cap:store:read:graph:*`, held rather than
/// declared) says what may be reached and not what exists, so it names nothing and is
/// skipped along with any other token under the prefix that is not an IRI.
fn granted_graphs(inv: &Invocation<'_>) -> Vec<NamedNode> {
    let Some(scopes) = inv.capability.scopes() else {
        return Vec::new();
    };
    let prefix = CAP_READ_GRAPH.trim_end_matches('*');
    scopes
        .iter()
        .filter_map(|scope| scope.strip_prefix(prefix))
        .filter_map(|iri| NamedNode::new(iri).ok())
        .collect()
}

// --------------------------------------------------------------------------- update

#[derive(Clone)]
struct UpdateEndpoint {
    store: Arc<DurableStore>,
}

#[async_trait]
impl Endpoint for UpdateEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.store.serving()?;
        match inv.request.verb {
            // `content` by name: the engine routes a pipe's (or a trailing) value into
            // `content` for every Sink, so a mutating verb that can be piped into
            // declares it.
            Verb::Sink => {
                let update = inv.inline_str("content")?;
                no_bindings(inv, "urn:iki:store:update")?;
                // ★ The same read rule as the scoped door (ledger #751): a `WHERE` reads
                // the dataset, and here it can read ALL of it, so it needs the broad read
                // grant. Decided on the text, before evaluation.
                let may_read = inv.capability.allows(CAP_READ);
                // ★ Parsed and evaluated on a stack sized for the update (ledger #915);
                // `confine::parse` refuses one past the bounds first. See `src/limits.rs`.
                // ★ Within the caller's time budget (ledger #964), and committed only while
                // the caller is still waiting: an update that runs out of time writes
                // nothing. See `src/budget.rs`.
                let store = Arc::clone(&self.store);
                let text = update.to_string();
                let requested = crate::budget::inline_bound(inv, "budget")?;
                let (before, after) = self.store.evaluate(
                    update,
                    true,
                    inv.capability,
                    requested,
                    move |deadline| {
                        // ★ A graph-management operation is an existence oracle for a
                        // caller who may not read (ledger #761): silenced. `src/confine.rs`.
                        let unreadable = if may_read {
                            crate::confine::Unreadable::None
                        } else {
                            crate::confine::Unreadable::EveryGraph
                        };
                        let prepared = crate::confine::parse(&text, deadline, unreadable)?;
                        if crate::confine::reads_the_dataset(&prepared) && !may_read {
                            return Err(Error::Denied(format!(
                                "this update has a `WHERE` clause, which reads the dataset, and \
                             reading it through `urn:iki:store:update` needs `{CAP_READ}` as well \
                             as `{CAP_WRITE}`; this capability does not hold it. Nothing was \
                             evaluated. The write grant alone covers updates that read nothing: \
                             `INSERT DATA`, `DELETE DATA`, `CLEAR`, `DROP`, `CREATE`"
                            )));
                        }
                        let _writes = store.write_lock();
                        let before = count(&store)?;
                        // ★ On a transaction this crate commits, not `on_store(…).execute()`,
                        // which commits inside oxigraph: the commit has to be the deadline's.
                        let mut transaction = store.dataset().start_transaction().map_err(|e| {
                            Error::Endpoint(format!("update: starting a transaction: {e}"))
                        })?;
                        prepared
                            .on_transaction(&mut transaction)
                            .execute()
                            .map_err(|e| Error::Endpoint(format!("update: {e}")))?;
                        deadline.settle(|| {
                            transaction
                                .commit()
                                .map_err(|e| Error::Endpoint(format!("update: committing: {e}")))
                        })?;
                        Ok((before, count(&store)?))
                    },
                )?;
                // The counts are a read of the dataset, so only for a caller that may read.
                Ok(plain(if may_read {
                    format!("updated: {before} -> {after} quads\n")
                } else {
                    "updated\n".to_string()
                }))
            }
            other => Err(unsupported("store-update", other)),
        }
    }

    fn name(&self) -> &str {
        "store-update"
    }

    fn describe(&self) -> Description {
        Description::new("store-update")
            .title("SPARQL UPDATE against the durable store")
            .summary(
                "Apply a SPARQL 1.1 UPDATE to the store. An update with a `WHERE` clause \
                 (including `DELETE WHERE`, `WITH`, `COPY`, `MOVE`, `ADD`) reads the \
                 dataset and also needs `urn:cap:store:read`, refused on the grant before \
                 evaluation; a caller without it is not told the quad counts, and its \
                 `DROP`, `CLEAR` and `CREATE` of a graph run as `SILENT`, so they do not say \
                 whether the graph exists. `LOAD` is refused: load a document through \
                 `urn:iki:store:load`. Cuts the \
                 golden thread `urn:iki:store:update`, so every cacheable read of this \
                 store recomputes.",
            )
            .verb(Verb::Sink)
            .verb(Verb::Meta)
            .requires(CAP_WRITE)
            .input(
                ArgSpec::new("content")
                    .summary("A SPARQL 1.1 UPDATE request (INSERT DATA, DELETE WHERE, …).")
                    .class(XSD_STRING),
            )
            .input(budget_arg())
            .output("text/plain")
    }
}

fn count(store: &DurableStore) -> Result<usize> {
    store
        .dataset()
        .len()
        .map_err(|e| Error::Endpoint(format!("counting quads: {e}")))
}

// --------------------------------------------------------------- graph-scoped update

/// `urn:iki:store:graph-update` — an arbitrary SPARQL UPDATE that can only affect one
/// named graph.
///
/// See [`CAP_WRITE_GRAPH`] for the capability and `src/confine.rs` for the mechanism
/// and the table of update shapes it closes.
#[derive(Clone)]
struct GraphUpdateEndpoint {
    store: Arc<DurableStore>,
}

#[async_trait]
impl Endpoint for GraphUpdateEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.store.serving()?;
        match inv.request.verb {
            Verb::Sink => {
                let update = inv.inline_str("content")?;
                no_bindings(inv, "urn:iki:store:graph-update")?;
                let graph = inv.inline_str("graph")?;
                let target = NamedNode::new(graph).map_err(|e| Error::InvalidArgument {
                    name: "graph".to_string(),
                    detail: format!("`{graph}` is not an IRI: {e}"),
                })?;

                // ★ The parameterized half of the capability, enforced here because the
                // kernel's pre-check can only see the wildcard: it knows the caller holds
                // SOME grant under `urn:cap:store:write:graph:`, and this is where it is
                // checked that the grant is for the graph actually named. Declared
                // (`CAP_WRITE_GRAPH`) and enforced (here) are the same scope, which is
                // the whole contract — the fs path ACLs are shaped identically.
                //
                // ⚠ `urn:cap:store:write` does NOT satisfy this door and is not meant to.
                // A broad holder uses `urn:iki:store:update`, which copies nothing and
                // sees the whole dataset. Accepting both here would mean the declared
                // scope was not the enforced one.
                let scope = cap_write_graph(target.as_str());
                if !inv.capability.allows(&scope) {
                    return Err(Error::Denied(format!(
                        "writing graph <{}> through `urn:iki:store:graph-update` needs the grant \
                         `{scope}`, which this capability does not hold. A grant names exactly \
                         one graph; holding `urn:cap:store:write` does not imply it, and is \
                         instead the authority for `urn:iki:store:update` over the whole dataset",
                        target.as_str()
                    )));
                }

                // ★ The READ half (ledger #751): a `WHERE` reads `G`, so an update with one
                // needs the read grant too — decided here, on the update's text and the
                // caller's grants, before anything is evaluated, so whether it is refused
                // says nothing about what `G` holds. `src/confine.rs` has the argument.
                let may_read = may_read_graph(inv, &target);
                // ★ Parsed and evaluated on a stack sized for the update (ledger #915);
                // `confine::parse` refuses one past the bounds first. See `src/limits.rs`.
                // ★ Within the caller's time budget, applied only while the caller is still
                // waiting (ledger #964). See `src/budget.rs`.
                let store = Arc::clone(&self.store);
                let text = update.to_string();
                let scope = target.clone();
                let requested = crate::budget::inline_bound(inv, "budget")?;
                let applied = self.store.evaluate(
                    update,
                    true,
                    inv.capability,
                    requested,
                    move |deadline| {
                        let target = scope;
                        // ★ `DROP`/`CLEAR`/`CREATE GRAPH <G>` would tell a write-only caller
                        // whether `G` exists (ledger #761): silenced. `src/confine.rs`.
                        let unreadable = if may_read {
                            crate::confine::Unreadable::None
                        } else {
                            crate::confine::Unreadable::Graph(&target)
                        };
                        let prepared = crate::confine::parse(&text, deadline, unreadable)?;
                        if crate::confine::reads_the_dataset(&prepared) && !may_read {
                            return Err(Error::Denied(format!(
                                "this update has a `WHERE` clause, which reads graph <{}>, and \
                             reading it needs the grant `{}` (or `{CAP_READ}`), which this \
                             capability does not hold. Nothing was evaluated. A write grant \
                             alone covers updates that read nothing: `INSERT DATA`, \
                             `DELETE DATA`, `CLEAR`, `DROP`, `CREATE`",
                                target.as_str(),
                                cap_read_graph(target.as_str()),
                            )));
                        }
                        let _writes = store.write_lock();
                        crate::confine::scoped_update(store.dataset(), &target, prepared, deadline)
                    },
                )?;
                // ⚠ The counts only for a caller that may read `G`: `+0` after an
                // `INSERT DATA` says the quad was already there, which is a read.
                Ok(plain(if may_read {
                    format!(
                        "updated <{}>: +{} -{} quads\n",
                        target.as_str(),
                        applied.added,
                        applied.removed
                    )
                } else {
                    format!("updated <{}>\n", target.as_str())
                }))
            }
            other => Err(unsupported("store-graph-update", other)),
        }
    }

    fn name(&self) -> &str {
        "store-graph-update"
    }

    fn describe(&self) -> Description {
        Description::new("store-graph-update")
            .title("SPARQL UPDATE confined to one named graph")
            .summary(
                "Apply a SPARQL 1.1 UPDATE that can only affect the named graph given by \
                 `graph`, under a grant for that graph alone. The update is evaluated \
                 against a dataset containing that graph and nothing else, and is refused \
                 in full — naming where it escaped to, never the data — if anything would \
                 land in another graph or in the default graph. It therefore cannot READ \
                 another graph either, which is the point: this is a boundary, not a \
                 filter. An update with a `WHERE` clause (including `DELETE WHERE`, `WITH`, \
                 `COPY`, `MOVE`, `ADD`) READS the graph, so it also needs \
                 `urn:cap:store:read:graph:<IRI>` (or `urn:cap:store:read`) and is refused \
                 on the grant, before evaluation, without it; `INSERT DATA`, `DELETE DATA`, \
                 `CLEAR`, `DROP` and `CREATE` need the write grant alone, and a caller who \
                 cannot read the graph is not told the quad counts, nor whether the graph \
                 exists (its `DROP`, `CLEAR` and `CREATE` run as `SILENT`). `LOAD` is \
                 refused. Cuts the golden thread `urn:iki:store:graph-update`.",
            )
            .verb(Verb::Sink)
            .verb(Verb::Meta)
            .requires(CAP_WRITE_GRAPH)
            .input(
                ArgSpec::new("content")
                    .summary(
                        "A SPARQL 1.1 UPDATE. Statements must be inside a `GRAPH <…>` \
                         block naming the scoped graph: a bare `INSERT DATA { … }` writes \
                         the default graph and is refused. One with a `WHERE` clause also \
                         needs the read grant on the graph.",
                    )
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("graph")
                    .summary(
                        "The one named graph this update may affect. The caller must hold \
                         `urn:cap:store:write:graph:<this IRI>`.",
                    )
                    .class(XSD_ANY_URI),
            )
            .input(budget_arg())
            .output("text/plain")
    }
}

// ----------------------------------------------------------------------------- load

#[derive(Clone)]
struct LoadEndpoint {
    store: Arc<DurableStore>,
}

#[async_trait]
impl Endpoint for LoadEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.store.serving()?;
        match inv.request.verb {
            Verb::Sink => {
                let bytes = inv.inline_arg("content")?;
                let media = optional_inline_str(inv, "format")?.unwrap_or(LOAD_FORMATS[0]);
                // ★ The declared `one_of` IS the contract (ledger #751): oxigraph recognizes
                // more syntaxes than this door declares — N3 among them, whose formulas
                // land in BLANK-NODE graphs that no grant, no scoped door and
                // `urn:iki:store:graphs` can ever name. A media type oxigraph maps onto
                // one of the declared five (a `;charset=` parameter, say) is that format.
                let format = RdfFormat::from_media_type(media)
                    .filter(|f| LOAD_FORMATS.contains(&bare_media(f.media_type())))
                    .ok_or_else(|| Error::InvalidArgument {
                        name: "format".to_string(),
                        detail: format!(
                            "`{media}` is not a syntax this door loads — one of {}",
                            LOAD_FORMATS.join(", ")
                        ),
                    })?;
                // ★ Bounded in triple-term depth before the parser sees a byte (ledger #992):
                // with RDF 1.2 on, ~1000 levels of `<<( … )>>` aborted the host from here, on
                // the caller's thread and under the write lock. Scanned outside the lock, in
                // the lexing mode of the parser below (strict, by format). See `src/depth.rs`.
                crate::depth::check_rdf_nesting(bytes, format, "content")?;
                let _writes = self.store.write_lock();
                let mut parser = RdfParser::from_format(format);
                // ⚠ A `graph` that is present but unreadable is refused: read as absent, it
                // would load the document into the DEFAULT graph (ledger #751).
                if let Some(graph) = optional_inline_str(inv, "graph")? {
                    let name = NamedNodeRef::new(graph).map_err(|e| Error::InvalidArgument {
                        name: "graph".to_string(),
                        detail: format!("`{graph}` is not an IRI: {e}"),
                    })?;
                    parser = parser.with_default_graph(GraphNameRef::from(name));
                }
                let before = self.len()?;
                self.store
                    .dataset()
                    .load_from_reader(parser, bytes)
                    .map_err(|e| Error::InvalidArgument {
                        name: "content".to_string(),
                        detail: format!("parsing {media}: {e}"),
                    })?;
                let after = self.len()?;
                // The counts are a read (a document whose quads were all there already
                // adds nothing), so only for a caller that may read the dataset.
                Ok(plain(if inv.capability.allows(CAP_READ) {
                    format!("loaded {media}: {before} -> {after} quads\n")
                } else {
                    format!("loaded {media}\n")
                }))
            }
            other => Err(unsupported("store-load", other)),
        }
    }

    fn name(&self) -> &str {
        "store-load"
    }

    fn describe(&self) -> Description {
        Description::new("store-load")
            .title("Bulk-load an RDF document into the durable store")
            .summary(
                "Parse an RDF document and add its statements to the store. The kernel \
                 door that replaces the old `load_turtle` side entrance: this one is \
                 capability-gated and cuts the golden thread `urn:iki:store:load`. A \
                 document nesting RDF 1.2 triple terms deeper than 64 is refused before \
                 it is parsed.",
            )
            .verb(Verb::Sink)
            .verb(Verb::Meta)
            .requires(CAP_WRITE)
            .input(
                ArgSpec::new("content")
                    .summary("The RDF document, in the syntax named by `format`.")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("format")
                    .summary(
                        "The document's syntax; one of the listed media types, and \
                         anything else is refused.",
                    )
                    .class(XSD_STRING)
                    .one_of(LOAD_FORMATS)
                    .default_value(LOAD_FORMATS[0])
                    .optional(),
            )
            .input(
                ArgSpec::new("graph")
                    .summary(
                        "Load into this named graph instead of the default graph. For a \
                         quad syntax (N-Quads, TriG) it replaces the DOCUMENT's default \
                         graph: statements with no graph of their own land here, and \
                         statements that name a graph keep it.",
                    )
                    .class(XSD_ANY_URI)
                    .optional(),
            )
            .output("text/plain")
    }
}

impl LoadEndpoint {
    fn len(&self) -> Result<usize> {
        self.store
            .dataset()
            .len()
            .map_err(|e| Error::Endpoint(format!("counting quads: {e}")))
    }
}

// ---------------------------------------------------------------------------- shared

/// An OPTIONAL by-value argument, read so that **absent and unreadable stay different
/// answers**: absent → `Ok(None)`; an inline UTF-8 value → `Ok(Some(text))`; present in any
/// other form — a reference, a content id, or bytes that are not UTF-8 — → refused, as
/// [`Error::InvalidArgument`] naming the argument.
///
/// ★ **Why this exists (ledger #751).** `Invocation::inline_str` errors both when the
/// argument is absent and when it is present but not inline UTF-8, so the obvious
/// `inline_str(name).ok()` for an optional argument turns "unreadable" into "absent". On
/// this crate that ran a query with its `bindings` silently dropped (every row instead of
/// the filtered ones), loaded a document into the DEFAULT graph when `graph=` was passed
/// by reference, and substituted the default for an unreadable `as` — each the opposite of
/// what the caller asked for, with nothing said. Required arguments do not need it:
/// `inline_str(name)?` already refuses both cases.
///
/// Matched on the request's own argument map rather than on `inline_str`'s error, so the
/// distinction does not depend on which error variant core happens to use for "absent".
fn optional_inline_str<'a>(inv: &Invocation<'a>, name: &str) -> Result<Option<&'a str>> {
    match inv.request.args.get(name) {
        None => Ok(None),
        Some(ArgRef::Inline(bytes)) => {
            std::str::from_utf8(bytes)
                .map(Some)
                .map_err(|e| Error::InvalidArgument {
                    name: name.to_string(),
                    detail: format!(
                        "is present but not valid UTF-8 ({e}); it is refused rather than \
                         treated as absent, because absent means something different here"
                    ),
                })
        }
        Some(_) => Err(Error::InvalidArgument {
            name: name.to_string(),
            detail: "is present but not an inline value (a reference or a content id); this \
                     argument is read by value only, and is refused rather than treated as \
                     absent, because absent means something different here"
                .to_string(),
        }),
    }
}

/// Whether this caller may READ `graph` — its own per-graph grant, or the broad
/// [`CAP_READ`], which reads every graph anyway.
///
/// ★ Asked by the scoped WRITE door, where accepting both is right: the question is
/// "would answering this tell the caller something it could not already read", not
/// "which door is this", so no authority is added. (The scoped READ doors are different:
/// there the declared scope is the door, and [`CAP_READ`] is refused to keep declared and
/// enforced the same.)
fn may_read_graph(inv: &Invocation<'_>, graph: &NamedNode) -> bool {
    inv.capability.allows(&cap_read_graph(graph.as_str())) || inv.capability.allows(CAP_READ)
}

/// The `budget` input every SPARQL door declares (ledger #964), worded and typed as
/// `ikigai-sparql`'s.
fn budget_arg() -> ArgSpec {
    ArgSpec::new("budget")
        .summary(
            "A time budget in milliseconds for this evaluation. It can only LOWER the budget \
             the host gives this capability, never raise it; past the budget the request is \
             refused with a timeout, never answered in part.",
        )
        .class(XSD_INTEGER)
        .optional()
}

/// The `max_rows` input every query door declares (ledger #970), worded and typed as
/// `ikigai-sparql`'s, except that the bound it lowers is the capability's, not a space's.
fn answer_rows_arg(budget: AnswerBudget, graph_shaped: bool) -> ArgSpec {
    ArgSpec::new("max_rows")
        .summary(format!(
            "optional: the most {} the answer may hold (an ASK is exempt). A larger answer is \
             refused, never truncated. It can only LOWER the bound this caller's capability \
             gets ({} {} unless it holds a `urn:cap:store:answer:<rows>` grant; at most {}), \
             never raise it",
            if graph_shaped { "triples" } else { "rows" },
            budget.base().rows(),
            if graph_shaped { "triples" } else { "rows" },
            budget.ceiling().rows(),
        ))
        .class(XSD_POSITIVE_INTEGER)
        .optional()
}

/// The `max_bytes` input every query door declares (ledger #970); see [`answer_rows_arg`].
fn answer_bytes_arg(budget: AnswerBudget) -> ArgSpec {
    ArgSpec::new("max_bytes")
        .summary(format!(
            "optional: the most serialized bytes the answer may hold (an ASK is exempt). A \
             larger answer is refused, never truncated. It can only LOWER the bound this \
             caller's capability gets ({} bytes unless it holds a \
             `urn:cap:store:answer:bytes:<bytes>` grant; at most {}), never raise it",
            budget.base().bytes(),
            budget.ceiling().bytes(),
        ))
        .class(XSD_POSITIVE_INTEGER)
        .optional()
}

fn plain(text: String) -> Representation {
    Representation::new(
        ReprType::new("text/plain").with_param("charset", "utf-8"),
        text.into_bytes(),
    )
}

fn unsupported(id: &str, verb: Verb) -> Error {
    Error::Endpoint(format!("{id} does not support the {verb:?} verb"))
}

/// Refuse a `bindings` argument on a write, rather than accepting one that would do
/// nothing.
///
/// ★ **The asymmetry is upstream's and it is worth saying out loud rather than letting a
/// caller discover it.** oxigraph's `PreparedSparqlQuery` has `substitute_variable`; its
/// `PreparedSparqlUpdate` has nothing, and a parsed update's AST is private, so there is
/// no way to bind a value into an update without rewriting the update's TEXT — which is
/// string interpolation with a longer name, in the one place where getting it wrong is
/// `DROP ALL`. An undeclared argument the kernel simply ignores would be the worst of
/// both: a caller who believed the value was bound, and a query that interpolated
/// nothing.
fn no_bindings(inv: &Invocation<'_>, iri: &str) -> Result<()> {
    // PRESENT in any form is refused — a by-reference or non-UTF-8 `bindings` is still a
    // caller who believes a value was bound (ledger #751).
    if !inv.request.args.contains_key("bindings") {
        return Ok(());
    }
    Err(Error::InvalidArgument {
        name: "bindings".to_string(),
        detail: format!(
            "`{iri}` takes a SPARQL UPDATE and cannot bind values into it: oxigraph offers \
             variable substitution for queries and none for updates, and this crate will not \
             fake it by rewriting the update's text. Build the terms with \
             `ikigai_store::sparql::{{literal, iri, typed_literal, integer, boolean}}`, which \
             do not escape anything — they construct an RDF term and let oxigraph serialize \
             it, so there is one escaper in the system and it is the one that owns the grammar"
        ),
    })
}

/// Serialize a SELECT/ASK result, **refusing** an `as` the form cannot answer in.
///
/// A bound must refuse, not substitute: `as=text/turtle` on a SELECT returning JSON
/// would make the declared `outputs` list true by accident, and a typo would come back
/// as a plausible answer in the wrong syntax with nothing said.
///
/// ★ **And bounds the answer's size while it is serialized** (ledger #970), as
/// `ikigai-sparql`'s `serialize_results` does: the row past `bound.rows()` is refused before
/// it is serialized, and every write goes through a [`CappedWriter`] that refuses the one
/// past `bound.bytes()`. Either way the answer is [`too_large`] and nothing of it is
/// returned. ASK is exempt: its answer is one boolean.
fn serialize_solutions(
    results: QueryResults,
    as_type: Option<&str>,
    deadline: &Deadline,
    bound: AnswerBound,
) -> Result<(String, Vec<u8>)> {
    let io = |e: std::io::Error| Error::Endpoint(format!("serialize: {e}"));
    let format = results_format(as_type)?;
    let bytes = match results {
        QueryResults::Solutions(solutions) => {
            let variables = solutions.variables().to_vec();
            let mut out = CappedWriter::new(bound.bytes());
            let written = (|| {
                let mut serializer = QueryResultsSerializer::from_format(format)
                    .serialize_solutions_to_writer(&mut out, variables)
                    .map_err(io)?;
                let mut rows: u64 = 0;
                for solution in solutions {
                    // oxigraph evaluates lazily, so the rows ARE the evaluation: a caller
                    // that gave up is not served another one.
                    deadline.check()?;
                    rows += 1;
                    if rows > bound.rows() {
                        return Err(too_large(Measure::Rows, bound.rows()));
                    }
                    let solution = solution.map_err(|e| Error::Endpoint(format!("query: {e}")))?;
                    serializer.serialize(&solution).map_err(io)?;
                }
                serializer.finish().map_err(io)?;
                Ok(())
            })();
            capped(written, out, bound)?
        }
        QueryResults::Boolean(value) => QueryResultsSerializer::from_format(format)
            .serialize_boolean_to_writer(Vec::new(), value)
            .map_err(io)?,
        // Unreachable: the caller checked the shape against the IRI first, and that
        // check is the point — this arm is here so a future refactor that drops it
        // fails loudly rather than serving a graph as a result set.
        //
        // ★ This match is exhaustive over a THIRD-PARTY enum with no catch-all, which is
        // the shape that broke 0.2.2 in `sparql.rs`. It is kept, and the difference is
        // the whole rule: **`QueryResults`'s three variants are ungated**, while
        // `oxrdf::Term`'s fourth is `#[cfg(feature = "rdf-12")]`. An ungated variant
        // added upstream appears in THIS crate's build too, so exhaustiveness turns it
        // into a compile error here — on our CI, before a consumer ever sees it, which
        // is exactly what we want. A GATED variant appears only in builds we do not
        // control, so the same exhaustiveness is a landmine that detonates downstream.
        // ⚠ Nothing checks that distinction automatically: if spareval ever puts a
        // variant behind a feature, this match joins the trap silently. Audited
        // 2026-09-13 against spareval 0.2.7, along with `serde_json::Value` in
        // `sparql::json_to_term` (no gated variants either) and `GraphName` in
        // `confine.rs` (ungated, and it has a catch-all regardless).
        QueryResults::Graph(_) => {
            return Err(Error::Endpoint(
                "internal: a graph reached the result-set serializer".to_string(),
            ))
        }
    };
    Ok((bare_media(format.media_type()).to_string(), bytes))
}

/// Serialize a CONSTRUCT/DESCRIBE result, refusing an unusable `as` the same way.
///
/// Bounded like [`serialize_solutions`], counting triples (ledger #970).
fn serialize_graph(
    results: QueryResults,
    as_type: Option<&str>,
    deadline: &Deadline,
    bound: AnswerBound,
) -> Result<(String, Vec<u8>)> {
    let io = |e: std::io::Error| Error::Endpoint(format!("serialize: {e}"));
    let format = graph_format(as_type)?;
    let QueryResults::Graph(triples) = results else {
        return Err(Error::Endpoint(
            "internal: a result set reached the graph serializer".to_string(),
        ));
    };
    let mut out = CappedWriter::new(bound.bytes());
    let written = (|| {
        let mut serializer = RdfSerializer::from_format(format).for_writer(&mut out);
        let mut rows: u64 = 0;
        for triple in triples {
            deadline.check()?;
            rows += 1;
            if rows > bound.rows() {
                return Err(too_large(Measure::Triples, bound.rows()));
            }
            let triple = triple.map_err(|e| Error::Endpoint(format!("query: {e}")))?;
            serializer
                .serialize_quad(&triple.in_graph(GraphName::DefaultGraph))
                .map_err(io)?;
        }
        serializer.finish().map_err(io)?;
        Ok(())
    })();
    Ok((
        bare_media(format.media_type()).to_string(),
        capped(written, out, bound)?,
    ))
}

/// The answer a serialization into `out` produced: its bytes, or the byte bound's refusal when
/// `out` refused a write, whatever error the serializer made of that refusal. As
/// `ikigai-sparql`'s.
fn capped(written: Result<()>, out: CappedWriter, bound: AnswerBound) -> Result<Vec<u8>> {
    if out.over() {
        return Err(too_large(Measure::Bytes, bound.bytes()));
    }
    written.map(|()| out.into_bytes())
}

/// A media type without its parameters.
///
/// ⚠ **`QueryResultsFormat::media_type()` is not always bare**: `Csv` reports
/// `text/csv; charset=utf-8` and `Tsv` likewise, while `Json` and `Xml` report no
/// parameter. Handing that string straight to `ReprType::new` and then calling
/// `.with_param("charset", "utf-8")` yields a canonical form with the parameter TWICE —
/// `ReprType` stores the media type verbatim and appends params, it does not parse them
/// out. The conformance `OUTPUTS` check strips `;charset=` before comparing, so nothing
/// catches it; `an_as_the_query_form_cannot_answer_in_is_refused` does, by asserting the
/// bare type. Same family as declaring a face you do not serve.
fn bare_media(media: &str) -> &str {
    media.split(';').next().unwrap_or(media).trim()
}

fn results_format(as_type: Option<&str>) -> Result<QueryResultsFormat> {
    let Some(spec) = as_type else {
        return Ok(QueryResultsFormat::Json);
    };
    QueryResultsFormat::from_media_type(spec)
        .filter(|f| RESULT_OUTPUTS.contains(&bare_media(f.media_type())))
        .ok_or_else(|| Error::InvalidArgument {
            name: "as".to_string(),
            detail: format!(
                "SELECT/ASK cannot answer in `{spec}` — one of {}. A graph syntax is not \
                 among them: only CONSTRUCT and DESCRIBE answer with a graph",
                RESULT_OUTPUTS.join(", ")
            ),
        })
}

fn graph_format(as_type: Option<&str>) -> Result<RdfFormat> {
    let Some(spec) = as_type else {
        return Ok(RdfFormat::NTriples);
    };
    RdfFormat::from_media_type(spec)
        .filter(|f| GRAPH_OUTPUTS.contains(&bare_media(f.media_type())))
        .ok_or_else(|| Error::InvalidArgument {
            name: "as".to_string(),
            detail: format!(
                "CONSTRUCT/DESCRIBE cannot answer in `{spec}` — one of {}",
                GRAPH_OUTPUTS.join(", ")
            ),
        })
}
