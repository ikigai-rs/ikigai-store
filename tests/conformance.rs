//! The module recipe as one test: `ikigai-conformance` walks the fifteen resources
//! [`ikigai_store::space`] binds and reports every violation at once, and holds that
//! space to the naming rule for an instance-built constructor.
//!
//! # The fixture is a store, and the walk WRITES to it
//!
//! The suite fires the actions it checks, `urn:iki:store:update` and
//! `urn:iki:store:load` included, so the kernel under test is built over a fresh
//! **in-memory** store. In-memory rather than durable on purpose: every property the
//! suite can see — ArgSpecs, enforcement, declared outputs, pipeline citizenship,
//! cacheability — is above the backing, and a durable fixture would make the walk need a
//! scratch directory and the `persistent` feature for nothing. The facts that really are
//! about RocksDB are pinned in `tests/handle_model.rs`.
//!
//! # What the suite cannot say, and this file says instead
//!
//! - **Every query form needs a real query of ITS form.** The ArgSpec says
//!   `xsd:string`, and a synthesized `"x"` is a parse error — so each of the four gets a
//!   [`Fixture`]. They are not interchangeable: `urn:iki:store:select` refuses a
//!   CONSTRUCT, on purpose, because the IRI is what fixes the declared outputs.
//! - **The three Sinks need parseable bodies** for the same reason: valid N-Triples, a
//!   valid `INSERT DATA`, and — for `store-graph-update` — one inside a `GRAPH` block
//!   naming the graph its fixture also passes, because a bare `INSERT DATA` writes the
//!   default graph and that endpoint refuses it on purpose.
//! - **The four scoped query forms need a `graph=` as well**, and the graph they name is
//!   seeded by [`kernel`] so their RDF faces have something to serialize. ⚠ They are also
//!   the endpoints with TWO required by-value inputs, so a BARE pipe into one is
//!   ambiguous — naming `graph=` leaves `query` as the single unnamed required input and
//!   the pipe works. See `src/endpoints.rs`.
//! - **`store-graphs` needs no input fixture.** Its caller fixture preserves the broad
//!   read grant during BODIES' bounded call. It declares no
//!   inputs at all — the caller's capability IS its input — so the suite drives it with
//!   nothing and gets a real answer. ⚠ The consequence worth knowing is upstream in
//!   `ikigai-core`: `select_action` skips an action with no required inputs when a query
//!   names present classes, so this resource (like `store-info`) is invisible to
//!   class-driven selection and reachable only by name or by an unconstrained query.
//! - ⚠ **`AUTHORITY` (0.3.0) is silent here, and that is the correct result rather than
//!   a gap.** It catches the fourth cell of the enforcement square — a `Sink` or
//!   `Delete` that declares no `requires` and mutates anyway under a capability holding
//!   no grants. All three Sinks here declare a scope, so `ENFORCED` holds them instead,
//!   in both directions. What neither check can see is the *parameterized* half of
//!   `urn:cap:store:write:graph:*`: the suite probes with no grants and with root, and
//!   the interesting case is a caller holding a grant for **one** graph reaching for
//!   another. `tests/graph_scope.rs` is that ablation, and it is where a regression in
//!   the boundary would actually show.
//! - **The reads are cacheable because the store is covered**, which is this crate's
//!   central claim, and is declared here so `CACHEABLE` holds them to it; all but
//!   `store-dump`, which is never cached, by choice (ledger #1732). None of them
//!   is `pure` — they read standing state — so the check also requires a non-empty
//!   thread set, which is exactly [`UPDATE_THREAD`] and [`LOAD_THREAD`].
//! - **The RDF faces are probed over a SEEDED store** (see [`kernel`]). Left empty they
//!   serialize nothing and the report says `0 triple(s) — nothing was checked`, which is
//!   a clean line meaning the check did not happen.
//! - ⚠ **A clean `SKOLEM-RDF` / `VOCABULARY` here is a weaker result than it looks.**
//!   `store-construct` and `store-describe` echo back whatever the host put in the
//!   store, so those two checks are really about the seed this file chose. A
//!   pass-through query face cannot be *held* to skolemization or to a vocabulary — the
//!   graph is not its own, and a store containing a user's blank nodes would otherwise
//!   fail its module's conformance for holding its user's data. The checks are load-
//!   bearing for a module that AUTHORS a graph; here they confirm the face parses and is
//!   labelled correctly, and no more.
//! - **`REQUIRED` (0.8.0) drops each required by-value input, one at a time, and
//!   expects a typed `MissingArgument` or `InvalidArgument`.** On the reads that is
//!   `query` on all eight query forms and `graph` on the four scoped ones. The walk also
//!   declares [`Suite::required_on_mutating`], so the three Sinks are fired without
//!   `content` (and `store-graph-update` without `graph`) too. That is safe only because
//!   the fixture store is in-memory and belongs to this test: each drop is a real
//!   firing, and a Sink that wrongly accepted one would have written. `budget`,
//!   `bindings`, `format`, `as` and `store-load`'s `graph` are optional and not dropped.
//!   `store-graph-info` drops its `graph`. `store-info`, `store-graphs` and `store-dump`
//!   declare nothing required, so REQUIRED has nothing to drop there.
//! - **`SPACE-NAME` (0.6.0) holds [`ikigai_store::space`] to claiming NO name**, because it
//!   is instance-built: its doors serve whichever [`DurableStore`] it was handed, so two
//!   calls are two different datasets behind identical-looking doors, and a self-claimed
//!   `urn:iki:space:store` would let the cache and every corridor treat them as one
//!   (ledger #987). Only the host knows which store it passed in, so the host names it.
//!   The suite is given the SAME `Arc` the kernel runs on, never a second
//!   `space(store.clone())`: a second live space over one dataset forfeits coverage here
//!   ([`DurableStore::spaces_bound`]), which would change the very reads being walked.

use futures::executor::block_on;
use ikigai_conformance::{Check, Fixture, Suite};
use ikigai_core::{ArgRef, Capability, EndpointSpace, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, space, DurableStore, SharerWrites, CAP_READ};
use std::sync::Arc;

/// A kernel over `store`, **seeded through its own Sink** with the one triple the RDF
/// faces echo back.
///
/// ★ Seeded, and not left empty, for a reason worth stating: an empty store makes
/// `store-construct` and `store-describe` serialize nothing, and the report then says
/// `0 triple(s) — nothing was checked` — a clean line that is indistinguishable from a
/// face that was never exercised. The seed is skolemized (`urn:example:conformance`,
/// RFC 6963's reserved namespace) and uses a defined term (`dcterms:title`), so
/// `SKOLEM-RDF` and `VOCABULARY` have something they can actually accept or reject.
///
/// It goes in through `urn:iki:store:load` under root rather than through the raw
/// handle, because a raw-handle write is exactly the invisible write this crate exists
/// to not have.
///
/// The space comes back beside the kernel so the walk can declare that same instance
/// host-named (see the module docs).
fn kernel(store: DurableStore) -> (Kernel, Arc<EndpointSpace>) {
    let space = Arc::new(space(store));
    let kernel = Kernel::with_meta_renderer(space.clone(), Arc::new(ikigai_vocab::TurtleRenderer));
    block_on(
        kernel.issue(
            Request::new(Verb::Sink, Iri::parse("urn:iki:store:load").unwrap())
                .with_arg("content", ArgRef::Inline(INSERT_TURTLE.as_bytes().to_vec())),
            &Capability::root(),
        ),
    )
    .expect("seeding the conformance fixture");
    // …and the same triple inside the scoped graph, so the scoped RDF faces serialize
    // something rather than reporting `0 triple(s) — nothing was checked`.
    block_on(
        kernel.issue(
            Request::new(Verb::Sink, Iri::parse("urn:iki:store:load").unwrap())
                .with_arg("content", ArgRef::Inline(INSERT_TURTLE.as_bytes().to_vec()))
                .with_arg("graph", ArgRef::Inline(SCOPED_GRAPH.as_bytes().to_vec())),
            &Capability::root(),
        ),
    )
    .expect("seeding the scoped graph");
    (kernel, space)
}

/// The label SPACE-NAME findings name the space by: the constructor's call.
const SPACE_LABEL: &str = "ikigai_store::space(store)";

/// Run `suite` over a kernel built from `store`, declaring that kernel's own space
/// host-named, and fail on any finding. Returns the rendered report so a test can pin a
/// line of it.
fn walk(suite: Suite, store: DurableStore) -> String {
    let (kernel, space) = kernel(store);
    let report = suite
        .body_space(space.clone())
        .host_named_space(SPACE_LABEL, space)
        .run_blocking(&kernel);
    println!("{report}");
    assert!(report.is_clean(), "{report}");
    // Live answers cannot be compared across independent calls. Every other
    // unprobed result is a coverage gap, including a missing body_space.
    for unprobed in report.unprobed.iter() {
        assert!(
            unprobed.check == Check::Bodies
                && report.declared.live.contains(&unprobed.endpoint)
                && unprobed
                    .reason
                    .starts_with("uncacheable, time-varying, or volatile answer:"),
            "unexpected unprobed check: {unprobed:?}\n{report}"
        );
    }
    report.to_string()
}

/// One query per form, each answering in the shape its IRI promises. The DESCRIBE
/// fixture names the subject [`kernel`] seeds, so the RDF faces have something to
/// serialize.
const FORM_FIXTURES: [(&str, &str); 4] = [
    ("store-select", "SELECT ?s WHERE { ?s ?p ?o } LIMIT 1"),
    ("store-ask", "ASK { ?s ?p ?o }"),
    (
        "store-construct",
        "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }",
    ),
    ("store-describe", "DESCRIBE <urn:example:conformance>"),
];

/// What the pipeline probe sinks into each Sink: one skolemized triple under a defined
/// term, so whatever the RDF faces echo back is something both RDF checks can accept.
const INSERT_TURTLE: &str =
    "<urn:example:conformance> <http://purl.org/dc/terms/title> \"conformance\" .";
const INSERT_UPDATE: &str = "INSERT DATA { <urn:example:conformance> \
                             <http://purl.org/dc/terms/title> \"conformance\" }";

/// `urn:iki:store:graph-update` writes into a graph, so its fixture must name one and
/// put the statement inside a `GRAPH` block — a bare `INSERT DATA` goes to the default
/// graph and that endpoint refuses it, which is the whole point of it.
const SCOPED_GRAPH: &str = "urn:example:conformance:graph";
const SCOPED_UPDATE: &str = "INSERT DATA { GRAPH <urn:example:conformance:graph> { \
                             <urn:example:conformance> \
                             <http://purl.org/dc/terms/title> \"conformance\" } }";

/// The fixtures all three walks share. The cacheable/live DECLARATION is deliberately
/// not here: it is the one thing the three modes disagree about — and the third
/// disagrees with itself, row by row — so each test states its own rather than the suite
/// carrying them all and contradicting itself.
fn fixtures() -> Suite {
    let suite = FORM_FIXTURES
        .iter()
        .fold(Suite::new(), |suite, (id, query)| {
            suite.fixture(Fixture::new(*id, Verb::Source).arg("query", *query))
        });
    let suite = FORM_FIXTURES.iter().fold(suite, |suite, (id, query)| {
        suite.fixture(
            Fixture::new(scoped_id(id), Verb::Source)
                .arg("query", *query)
                .arg("graph", SCOPED_GRAPH)
                .capability(Capability::scoped([cap_read_graph(SCOPED_GRAPH)])),
        )
    });
    suite
        // The per-graph count names the seeded scoped graph, so it answers for a graph that
        // exists rather than for an empty one.
        // BODIES bounds a scoped caller without changing its grants. Root would
        // instead be narrowed to the declared scope patterns, which neither grant
        // this graph nor preserve store-graphs' visible dataset.
        .fixture(
            Fixture::new("store-graph-info", Verb::Source)
                .arg("graph", SCOPED_GRAPH)
                .capability(Capability::scoped([cap_read_graph(SCOPED_GRAPH)])),
        )
        .fixture(
            Fixture::new("store-graphs", Verb::Source).capability(Capability::scoped([CAP_READ])),
        )
        .fixture(Fixture::new("store-load", Verb::Sink).arg("content", INSERT_TURTLE))
        .fixture(Fixture::new("store-update", Verb::Sink).arg("content", INSERT_UPDATE))
        .fixture(
            Fixture::new("store-graph-update", Verb::Sink)
                .arg("content", SCOPED_UPDATE)
                .arg("graph", SCOPED_GRAPH),
        )
        // REQUIRED on the three Sinks as well as the reads: each drop is a firing, and
        // this store is in-memory and the test's own (see the module docs).
        .required_on_mutating()
}

/// `store-select` → `store-graph-select`: the scoped twin's description id.
fn scoped_id(id: &str) -> String {
    id.replace("store-", "store-graph-")
}

/// Every read endpoint, for the declaration each walk makes about them. ★ A scoped
/// read is cacheable exactly like a broad one — it depends on the same three write
/// threads — and saying so here is what would catch a scoped face that quietly stopped
/// being cached.
const READS: [&str; 11] = [
    "store-select",
    "store-ask",
    "store-construct",
    "store-describe",
    "store-graph-select",
    "store-graph-ask",
    "store-graph-construct",
    "store-graph-describe",
    "store-info",
    "store-graphs",
    "store-graph-info",
];

/// ★ The one read that is `live` in EVERY mode, owned stores included: `store-dump` is never
/// cacheable, by choice (ledger #1732). The kernel copies a cacheable answer before deciding to
/// keep it, so a cacheable dump held the whole dataset twice. Declaring it `live` in all three
/// walks is what would catch it drifting back to cacheable.
const NEVER_CACHED: &str = "store-dump";

/// The reads whose universe is ONE named graph, and therefore the exact set that a
/// `SharerWrites` declaration can put back in the cache.
///
/// ⚠ **Spelled out rather than derived from a prefix**, which is what it used to be, and
/// the rename in 0.2.5 is a reason to keep it spelled out rather than to relax. Under the
/// old name `store-graph-list` the `store-graph-` prefix admitted it outright; under
/// `store-graphs` that exact prefix no longer matches, but `store-graph` (no hyphen)
/// still does, so the hazard moved one character rather than going away. Its universe
/// under a root probe is every graph name in the store, so it belongs with the broad
/// faces here, and any prefix test would have silently declared it cacheable in the one
/// walk that exists to catch exactly that mistake.
const SCOPED_READS: [&str; 5] = [
    "store-graph-select",
    "store-graph-ask",
    "store-graph-construct",
    "store-graph-describe",
    // Counts one named graph, so it is confined to it exactly as a scoped query is (ledger
    // #147). `store-dump` is NOT here: it reads every graph and the default graph.
    "store-graph-info",
];

#[test]
fn conforms() {
    let report = walk(
        READS
            .iter()
            .fold(fixtures(), |suite, id| suite.cacheable(*id))
            .live(NEVER_CACHED),
        DurableStore::in_memory().unwrap(),
    );
    // The declaration was checked, not merely made: a `space: none declared` line here
    // would mean SPACE-NAME looked at nothing and the walk above proves nothing about it.
    assert!(
        report.contains(&format!("space: {SPACE_LABEL} host-named")),
        "{report}"
    );
}

/// ★ The same walk over a SHARED store, where every read is `Expiry::Always` by
/// construction — so the declaration flips from `cacheable` to `live`, and the check
/// that would otherwise pass silently is the one that proves the two modes really differ.
///
/// If `DurableStore::open_shared` ever stopped forfeiting cacheability, `conforms` would
/// still pass and only this test would fail.
#[test]
fn a_shared_store_conforms_as_a_live_one() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    drop(handle);
    walk(
        READS
            .iter()
            .fold(fixtures(), |suite, id| suite.live(*id))
            .live(NEVER_CACHED),
        store,
    );
}

/// ★ The third mode, walked the same way: a shared store whose host declared that the
/// handle's holder writes only the default graph. The declaration splits the read list
/// in two — the four SCOPED faces go back to `cacheable` (their universe is one named
/// graph the sharer cannot write), the four broad ones and `store-info` stay `live`
/// (they see the default graph).
///
/// This is the walk that would fail if the per-graph decision ever collapsed back into
/// one bit in either direction: a blanket `bare` fails the scoped rows, a blanket
/// `cacheable` fails the broad ones.
#[test]
fn a_declared_shared_store_conforms_as_cacheable_exactly_where_it_promised() {
    let (store, handle) =
        DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph()).unwrap();
    drop(handle);
    let suite = READS.iter().fold(fixtures(), |suite, id| {
        if SCOPED_READS.contains(id) {
            suite.cacheable(*id)
        } else {
            suite.live(*id)
        }
    });
    walk(suite.live(NEVER_CACHED), store);
}
