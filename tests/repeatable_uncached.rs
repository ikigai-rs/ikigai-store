//! A SPARQL read the shared classifier flags as not repeatable is never cached (ledger
//! #4418, follow-up 2 of `docs/design/deterministic-sparql.md`).
//!
//! The claim, from the design spike: on main at 64a4672, `SELECT (RAND() AS ?v) WHERE {}`
//! and the same for `UUID()`, `STRUUID()` and `BNODE()` all entered the kernel cache through
//! `urn:iki:store:select`, and a repeat of a UUID CONSTRUCT was served the first run's IRI.
//! These tests go through the real doors only, broad and graph-scoped, and ask the kernel
//! whether the exact request is cached. That avoids inferring a hit from a random value.

use futures::executor::block_on;
use ikigai_core::{
    ArgRef, Capability, Description, Endpoint, EndpointSpace, Invocation, Iri, Kernel,
    Representation, Request, Resolution, Result, Scope, Space, Threaded, TraceEvent, TraceState,
    Tracer, Verb, UNCACHED_NOTE,
};
use ikigai_store::endpoints::{NOT_REPEATABLE_NOTE, NOW_NOTE};
use ikigai_store::{space, DurableStore};
use std::sync::{Arc, Mutex, RwLock};

const G: &str = "urn:test:g";

fn kernel() -> Kernel {
    Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())))
}

fn request(door: &str, query: &str) -> Request {
    let mut request = Request::new(Verb::Source, Iri::parse(door).unwrap())
        .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()));
    if door.contains(":graph-") {
        request = request.with_arg("graph", ArgRef::Inline(G.as_bytes().to_vec()));
    }
    request
}

fn issue(kernel: &Kernel, request: &Request) -> Vec<u8> {
    block_on(kernel.issue(request.clone(), &Capability::root()))
        .unwrap()
        .bytes
}

/// The door each query form answers at, broad and scoped.
fn doors(query: &str) -> [&'static str; 2] {
    if query.starts_with("CONSTRUCT") {
        ["urn:iki:store:construct", "urn:iki:store:graph-construct"]
    } else {
        ["urn:iki:store:select", "urn:iki:store:graph-select"]
    }
}

/// Every hazard the classifier names, one query each, and the words its note must carry.
const FLAGGED: [(&str, &str); 10] = [
    ("SELECT (RAND() AS ?v) WHERE {}", "RAND()"),
    ("SELECT (UUID() AS ?v) WHERE {}", "UUID()"),
    ("SELECT (STRUUID() AS ?v) WHERE {}", "STRUUID()"),
    ("SELECT (BNODE() AS ?v) WHERE {}", "BNODE()"),
    (
        "SELECT ?v WHERE { VALUES ?v { 1 2 } } ORDER BY ?v LIMIT 1",
        "LIMIT or OFFSET",
    ),
    (
        "SELECT (SAMPLE(?v) AS ?s) WHERE { VALUES ?v { 1 2 } }",
        "SAMPLE",
    ),
    (
        "SELECT (GROUP_CONCAT(?v) AS ?s) WHERE { VALUES ?v { 1 2 } }",
        "GROUP_CONCAT",
    ),
    ("SELECT REDUCED ?v WHERE { VALUES ?v { 1 1 } }", "REDUCED"),
    (
        "CONSTRUCT { <urn:s> <urn:p> ?id } WHERE { BIND(UUID() AS ?id) }",
        "UUID()",
    ),
    ("CONSTRUCT { _:b <urn:p> 1 } WHERE {}", "CONSTRUCT template"),
];

#[test]
fn every_flagged_read_is_answered_uncached_by_both_doors() {
    let kernel = kernel();
    let mut cached = Vec::new();
    for (query, _) in FLAGGED {
        for door in doors(query) {
            let request = request(door, query);
            issue(&kernel, &request);
            issue(&kernel, &request);
            if kernel.is_cached(&request, &Capability::root()) {
                cached.push(format!("{door}: {query}"));
            }
        }
    }
    assert!(
        cached.is_empty(),
        "flagged reads entered the cache: {cached:#?}"
    );
}

#[test]
fn an_ordinary_read_still_hits_the_cache_through_both_doors() {
    let kernel = kernel();
    for query in [
        "SELECT ?v WHERE { VALUES ?v { 1 2 } }",
        "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
        "SELECT (\"UUID() RAND()\" AS ?v) WHERE {} # LIMIT 1",
        "CONSTRUCT { <urn:s> <urn:p> ?o } WHERE { VALUES ?o { 1 } }",
    ] {
        for door in doors(query) {
            let request = request(door, query);
            issue(&kernel, &request);
            assert!(
                kernel.is_cached(&request, &Capability::root()),
                "{door}: {query}"
            );
            let collect = Arc::new(Collect::default());
            block_on(kernel.issue_traced(request, &Capability::root(), collect.clone())).unwrap();
            let event = collect.event(door);
            assert_eq!(event.state(), TraceState::Served, "{door}: {query}");
        }
    }
}

#[test]
fn two_runs_of_a_uuid_construct_mint_two_iris() {
    let kernel = kernel();
    let query = "CONSTRUCT { <urn:s> <urn:p> ?id } WHERE { BIND(UUID() AS ?id) }";
    for door in doors(query) {
        let request = request(door, query);
        let first = issue(&kernel, &request);
        let second = issue(&kernel, &request);
        assert_ne!(first, second, "{door} served the first run's IRI again");
    }
}

#[derive(Default)]
struct Collect(Mutex<Vec<TraceEvent>>);

impl Tracer for Collect {
    fn record(&self, event: TraceEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Collect {
    fn event(&self, door: &str) -> TraceEvent {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.target == door)
            .unwrap_or_else(|| panic!("{door} was not traced"))
            .clone()
    }
}

fn note(event: &TraceEvent, key: &str) -> Option<String> {
    event
        .notes
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

#[test]
fn the_trace_names_the_construct_and_why() {
    let kernel = kernel();
    for (query, construct) in FLAGGED {
        for door in doors(query) {
            let collect = Arc::new(Collect::default());
            block_on(kernel.issue_traced(
                request(door, query),
                &Capability::root(),
                collect.clone(),
            ))
            .unwrap();
            let event = collect.event(door);
            assert_eq!(event.state(), TraceState::Computed, "{door}: {query}");
            assert_eq!(note(&event, UNCACHED_NOTE).as_deref(), Some("declared"));
            let why = note(&event, NOT_REPEATABLE_NOTE)
                .unwrap_or_else(|| panic!("no repeatability note for {door}: {query}"));
            assert!(why.contains(construct), "{door}: {query}: {why}");
            assert!(why.contains("never cached"), "{why}");
            assert_eq!(note(&event, NOW_NOTE), None, "{door}: {query}");
        }
    }
}

#[test]
fn an_ordinary_read_carries_no_repeatability_note() {
    let kernel = kernel();
    let collect = Arc::new(Collect::default());
    let door = "urn:iki:store:select";
    block_on(kernel.issue_traced(
        request(door, "SELECT ?v WHERE { VALUES ?v { 1 } }"),
        &Capability::root(),
        collect.clone(),
    ))
    .unwrap();
    let event = collect.event(door);
    assert_eq!(note(&event, NOT_REPEATABLE_NOTE), None);
    assert_eq!(note(&event, UNCACHED_NOTE), None);
}

#[test]
fn removing_now_does_not_hide_another_reason() {
    let kernel = kernel();
    let door = "urn:iki:store:select";
    let collect = Arc::new(Collect::default());
    block_on(kernel.issue_traced(
        request(door, "SELECT (NOW() AS ?t) (UUID() AS ?u) WHERE {}"),
        &Capability::root(),
        collect.clone(),
    ))
    .unwrap();
    let event = collect.event(door);
    assert!(note(&event, NOW_NOTE).is_some());
    let why = note(&event, NOT_REPEATABLE_NOTE).unwrap();
    assert!(why.contains("NOW()") && why.contains("UUID()"), "{why}");
}

#[test]
fn the_uncached_readout_lists_a_flagged_read() {
    let kernel = kernel();
    for door in doors("SELECT") {
        issue(&kernel, &request(door, "SELECT (UUID() AS ?v) WHERE {}"));
    }
    let readout = Request::new(Verb::Source, Iri::parse("urn:kernel:uncached").unwrap());
    let readout = String::from_utf8(issue(&kernel, &readout)).unwrap();
    for door in doors("SELECT") {
        let row = readout
            .lines()
            .find(|line| line.contains(door))
            .unwrap_or_else(|| panic!("no row for {door} in:\n{readout}"));
        assert!(row.contains("declared"), "{row}");
    }
}

// --------------------------------------------------------------------- cache rollover

/// A space whose doors a host can replace while the kernel runs, as a deployment does.
struct Swap(RwLock<Arc<dyn Space>>);

impl Space for Swap {
    fn resolve(&self, request: &Request, scope: &Scope) -> Resolution {
        self.0.read().unwrap().resolve(request, scope)
    }
}

/// The store from before the policy: the real doors, with every answer marked cacheable, as
/// main answered a `UUID()` read before ledger #4418. Its endpoints describe themselves as
/// the store's do, so a request keys the same cache entry through either.
struct BeforeThePolicy(EndpointSpace);

impl Space for BeforeThePolicy {
    fn resolve(&self, request: &Request, scope: &Scope) -> Resolution {
        self.0
            .resolve(request, scope)
            .map_endpoint(|inner| Arc::new(Cacheable(inner)))
    }
}

struct Cacheable(Arc<dyn Endpoint>);

#[async_trait::async_trait]
impl Endpoint for Cacheable {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        Ok(self.0.invoke(inv).await?.cacheable())
    }

    fn name(&self) -> &str {
        self.0.name()
    }

    fn describe(&self) -> Description {
        self.0.describe()
    }
}

/// The kernel looks a request up in its cache before it dispatches, so an entry written
/// before the policy would be served without the store ever classifying the query. A
/// fresh kernel cache has none (the cache is in memory), and on a kernel that outlives
/// the swap, cutting the thread the replaced doors hung from drops them.
#[test]
fn a_flagged_read_is_never_served_from_an_entry_written_before_the_policy() {
    const DEPLOY: &str = "urn:test:deployment";
    let old: Arc<dyn Space> = Arc::new(BeforeThePolicy(space(DurableStore::in_memory().unwrap())));
    let swap = Arc::new(Swap(RwLock::new(old)));
    let root = Threaded::new(swap.clone() as Arc<dyn Space>, DEPLOY);
    let kernel = Kernel::new(Arc::new(root));
    let query = "SELECT (UUID() AS ?v) WHERE {}";
    let root = Capability::root();
    let mut before = Vec::new();
    for door in doors(query) {
        let request = request(door, query);
        before.push(issue(&kernel, &request));
        assert!(kernel.is_cached(&request, &root), "{door}");
    }

    *swap.0.write().unwrap() = Arc::new(space(DurableStore::in_memory().unwrap()));
    // The control: until the old entries are dropped, the kernel still serves them, which
    // is why a deployment needs a fresh cache or this cut.
    for (door, before) in doors(query).into_iter().zip(&before) {
        assert_eq!(&issue(&kernel, &request(door, query)), before, "{door}");
    }
    kernel.cut(DEPLOY);

    for (door, before) in doors(query).into_iter().zip(&before) {
        let request = request(door, query);
        assert!(
            !kernel.is_cached(&request, &root),
            "{door} kept a pre-policy entry"
        );
        let first = issue(&kernel, &request);
        let second = issue(&kernel, &request);
        assert_ne!(&first, before, "{door}");
        assert_ne!(first, second, "{door}");
        assert!(!kernel.is_cached(&request, &root), "{door}");
    }

    // A fresh kernel over the new doors never saw the old answer.
    let fresh = self::kernel();
    for (door, before) in doors(query).into_iter().zip(&before) {
        let request = request(door, query);
        assert!(!fresh.is_cached(&request, &root));
        assert_ne!(&issue(&fresh, &request), before, "{door}");
        assert!(!fresh.is_cached(&request, &root));
    }
}
