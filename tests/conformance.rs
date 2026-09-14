//! The module recipe as one test: `ikigai-conformance` walks the twelve resources
//! [`ikigai_store::space`] binds and reports every violation at once.
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
//!   central claim, and is declared here so `CACHEABLE` holds them to it. None of them
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

use futures::executor::block_on;
use ikigai_conformance::{Fixture, Suite};
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
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
fn kernel(store: DurableStore) -> Kernel {
    let kernel = Kernel::with_meta_renderer(
        Arc::new(space(store)),
        Arc::new(ikigai_vocab::TurtleRenderer),
    );
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
    kernel
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

/// The fixtures both walks share. The cacheable/live DECLARATION is deliberately not
/// here: it is the one thing the two modes disagree about, so each test states its own
/// rather than the suite carrying both and contradicting itself.
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
                .arg("graph", SCOPED_GRAPH),
        )
    });
    suite
        .fixture(Fixture::new("store-load", Verb::Sink).arg("content", INSERT_TURTLE))
        .fixture(Fixture::new("store-update", Verb::Sink).arg("content", INSERT_UPDATE))
        .fixture(
            Fixture::new("store-graph-update", Verb::Sink)
                .arg("content", SCOPED_UPDATE)
                .arg("graph", SCOPED_GRAPH),
        )
}

/// `store-select` → `store-graph-select`: the scoped twin's description id.
fn scoped_id(id: &str) -> String {
    id.replace("store-", "store-graph-")
}

/// Every read endpoint, for the declaration both walks make about all of them. ★ A scoped
/// read is cacheable exactly like a broad one — it depends on the same three write
/// threads — and saying so here is what would catch a scoped face that quietly stopped
/// being cached.
const READS: [&str; 9] = [
    "store-select",
    "store-ask",
    "store-construct",
    "store-describe",
    "store-graph-select",
    "store-graph-ask",
    "store-graph-construct",
    "store-graph-describe",
    "store-info",
];

#[test]
fn conforms() {
    let report = READS
        .iter()
        .fold(fixtures(), |suite, id| suite.cacheable(*id))
        .run_blocking(&kernel(DurableStore::in_memory().unwrap()));
    println!("{report}");
    assert!(report.is_clean(), "{report}");
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
    let report = READS
        .iter()
        .fold(fixtures(), |suite, id| suite.live(*id))
        .run_blocking(&kernel(store));
    println!("{report}");
    assert!(report.is_clean(), "{report}");
}
