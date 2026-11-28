//! The query and update doors use the invocation instant for SPARQL `NOW()`, and a `NOW()`
//! answer computed from a temporal corridor's pinned instant is cached under that corridor
//! (ledger #4286) while one computed from a live clock is not.

use futures::executor::block_on;
use ikigai_core::{
    ArgRef, BodyIssue, Capability, Clock, EndpointSpace, FixedClock, Iri, Kernel, Request, Scope,
    Space, Time, TraceEvent, TraceState, Tracer, Verb, UNCACHED_NOTE,
};
use ikigai_store::endpoints::{NOT_REPEATABLE_NOTE, NOW_NOTE};
use ikigai_store::{space, DurableStore};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 2001-09-09T01:46:40Z, far enough from any wall clock this test runs under that the two
/// cannot be confused.
const PINNED_MILLIS: u64 = 1_000_000_000_000;
const PINNED_LEXICAL: &str = "2001-09-09T01:46:40Z";

const NOW_QUERY: &str = "SELECT (NOW() AS ?t) WHERE {}";

fn select(kernel: &Kernel, query: &str, scope: Option<Scope>) -> String {
    let request = select_request(query);
    let answer = match scope {
        Some(scope) => block_on(kernel.issue_in(request, &Capability::root(), scope)),
        None => block_on(kernel.issue(request, &Capability::root())),
    }
    .unwrap();
    let csv = String::from_utf8(answer.bytes).unwrap();
    csv.lines().nth(1).unwrap_or_default().trim().to_string()
}

fn select_request(query: &str) -> Request {
    Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
        .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
        .with_arg("as", ArgRef::Inline(b"text/csv".to_vec()))
}

fn pinned_kernel() -> Kernel {
    Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())))
        .with_clock(Arc::new(FixedClock::at(PINNED_MILLIS)))
}

#[test]
fn now_reads_the_kernel_clock() {
    let kernel = pinned_kernel();
    let t = select(&kernel, NOW_QUERY, None);
    assert_eq!(t, PINNED_LEXICAL);
    assert!(!kernel.is_cached(&select_request(NOW_QUERY), &Capability::root()));
}

#[test]
fn now_reads_a_temporal_corridors_instant() {
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    let corridor: Arc<dyn Space> = Arc::new(EndpointSpace::new());
    let scope = Scope::empty().with_named_at(
        Iri::parse("urn:example:corridor:pinned").unwrap(),
        corridor,
        Arc::new(FixedClock::at(PINNED_MILLIS)),
    );
    let t = select(&kernel, NOW_QUERY, Some(scope));
    assert_eq!(t, PINNED_LEXICAL);
}

#[test]
fn all_now_calls_in_one_query_use_the_same_instant() {
    let row = select(
        &pinned_kernel(),
        "SELECT ?t ?same WHERE {
            BIND(NOW() AS ?t)
            FILTER(NOW() <= NOW())
            BIND(sameTerm(?t, NOW()) AS ?same)
        }",
        None,
    );
    assert_eq!(row, format!("{PINNED_LEXICAL},true"));
}

#[test]
fn an_update_writing_now_replays_the_same_quads_under_the_same_clock() {
    const GRAPH: &str = "urn:example:clock";
    let update = format!(
        "INSERT DATA {{ GRAPH <{GRAPH}> {{ <urn:event> <urn:time> 0 }} }};
         DELETE {{ GRAPH <{GRAPH}> {{ <urn:event> <urn:time> ?old }} }}
         INSERT {{ GRAPH <{GRAPH}> {{ <urn:event> <urn:time> ?t }} }}
         WHERE {{
            GRAPH <{GRAPH}> {{ <urn:event> <urn:time> ?old }}
            BIND(NOW() AS ?t)
            FILTER(sameTerm(?t, NOW()))
         }}"
    );
    let expected = format!(
        "<urn:event> <urn:time> \"{PINNED_LEXICAL}\"^^<http://www.w3.org/2001/XMLSchema#dateTime> <{GRAPH}> ."
    );

    // Replay the same request for each door in two fresh stores.
    for door in ["urn:iki:store:update", "urn:iki:store:graph-update"] {
        let replay = || {
            let kernel = pinned_kernel();
            let mut request = Request::new(Verb::Sink, Iri::parse(door).unwrap())
                .with_arg("content", ArgRef::Inline(update.as_bytes().to_vec()));
            if door == "urn:iki:store:graph-update" {
                request = request.with_arg("graph", ArgRef::Inline(GRAPH.as_bytes().to_vec()));
            }
            block_on(kernel.issue(request, &Capability::root())).unwrap();
            let dump = Request::new(Verb::Source, Iri::parse("urn:iki:store:dump").unwrap());
            let quads = block_on(kernel.issue(dump, &Capability::root())).unwrap();
            String::from_utf8(quads.bytes).unwrap()
        };
        let first = replay();
        assert_eq!(first.trim_end(), expected, "{door}");
        assert_eq!(replay(), first, "{door}");
    }
}

// --------------------------------------------------------- caching under a corridor (#4286)

/// 2033-05-18T03:33:20Z: a second pinned instant.
const LATER_MILLIS: u64 = 2_000_000_000_000;
const LATER_LEXICAL: &str = "2033-05-18T03:33:20Z";

fn store_kernel() -> Kernel {
    Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())))
}

/// A temporal corridor named `name`, pinned at `clock`'s reading.
fn corridor_on(name: &str, clock: Arc<dyn Clock>) -> Scope {
    Scope::empty().with_named_at(
        Iri::parse(name).unwrap(),
        Arc::new(EndpointSpace::new()) as Arc<dyn Space>,
        clock,
    )
}

fn corridor(name: &str, millis: u64) -> Scope {
    corridor_on(name, Arc::new(FixedClock::at(millis)))
}

/// A clock that moves one second forward each time it is read: live time.
struct Ticking(AtomicU64);

impl Clock for Ticking {
    fn now(&self) -> Time {
        Time::from_millis(self.0.fetch_add(1_000, Ordering::SeqCst))
    }
}

#[derive(Default)]
struct Collect(Mutex<Vec<TraceEvent>>);

impl Tracer for Collect {
    fn record(&self, event: TraceEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// Issue the `NOW()` select in `scope`, traced: the first answer row and the door's event.
fn traced_in(kernel: &Kernel, query: &str, scope: &Scope) -> (String, TraceEvent) {
    let collect = Arc::new(Collect::default());
    let issue = BodyIssue::new()
        .in_scope(scope.clone())
        .traced(collect.clone());
    let answer =
        block_on(kernel.issue_with(select_request(query), &Capability::root(), issue)).unwrap();
    let csv = String::from_utf8(answer.bytes).unwrap();
    let row = csv.lines().nth(1).unwrap_or_default().trim().to_string();
    let event = collect
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.target == "urn:iki:store:select")
        .expect("the select door was traced")
        .clone();
    (row, event)
}

fn note(event: &TraceEvent, key: &str) -> Option<String> {
    event
        .notes
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

#[test]
fn a_now_answer_under_a_pinned_corridor_is_cached_and_served() {
    let kernel = store_kernel();
    let then = corridor("urn:example:corridor:then", PINNED_MILLIS);
    let request = select_request(NOW_QUERY);
    let (first, event) = traced_in(&kernel, NOW_QUERY, &then);
    assert_eq!(first, PINNED_LEXICAL);
    assert_eq!(event.state(), TraceState::Computed);
    assert_eq!(note(&event, NOW_NOTE), None);
    assert_eq!(note(&event, NOT_REPEATABLE_NOTE), None);
    assert_eq!(note(&event, UNCACHED_NOTE), None);
    assert!(kernel.is_cached_in(&request, &Capability::root(), &then));

    let (second, event) = traced_in(&kernel, NOW_QUERY, &then);
    assert_eq!(second, PINNED_LEXICAL);
    assert_eq!(event.state(), TraceState::Served);
    // The corridor's entry is not the bare request's.
    assert!(!kernel.is_cached(&request, &Capability::root()));
}

#[test]
fn two_corridors_at_two_instants_never_share_an_entry() {
    let kernel = store_kernel();
    let request = select_request(NOW_QUERY);
    // The same corridor name at two instants, and a second name at the first instant.
    let then = corridor("urn:example:corridor:then", PINNED_MILLIS);
    let later = corridor("urn:example:corridor:then", LATER_MILLIS);
    let other = corridor("urn:example:corridor:other", PINNED_MILLIS);

    assert_eq!(
        select(&kernel, NOW_QUERY, Some(then.clone())),
        PINNED_LEXICAL
    );
    assert!(kernel.is_cached_in(&request, &Capability::root(), &then));
    assert!(!kernel.is_cached_in(&request, &Capability::root(), &later));
    assert!(!kernel.is_cached_in(&request, &Capability::root(), &other));

    assert_eq!(
        select(&kernel, NOW_QUERY, Some(later.clone())),
        LATER_LEXICAL
    );
    assert_eq!(
        select(&kernel, NOW_QUERY, Some(other.clone())),
        PINNED_LEXICAL
    );
    for scope in [&then, &later, &other] {
        assert!(kernel.is_cached_in(&request, &Capability::root(), scope));
    }
    // Each is still its own instant after the others were cached.
    assert_eq!(select(&kernel, NOW_QUERY, Some(then)), PINNED_LEXICAL);
    assert_eq!(select(&kernel, NOW_QUERY, Some(later)), LATER_LEXICAL);
}

#[test]
fn a_store_write_cuts_a_pinned_now_answer() {
    let kernel = store_kernel();
    let query = "SELECT (NOW() AS ?t) (COUNT(*) AS ?n) WHERE { ?s ?p ?o }";
    let then = corridor("urn:example:corridor:then", PINNED_MILLIS);
    let request = select_request(query);
    assert_eq!(
        select(&kernel, query, Some(then.clone())),
        format!("{PINNED_LEXICAL},0")
    );
    assert!(kernel.is_cached_in(&request, &Capability::root(), &then));

    let update = Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap()).with_arg(
        "content",
        ArgRef::Inline(b"INSERT DATA { <urn:s> <urn:p> <urn:o> }".to_vec()),
    );
    block_on(kernel.issue(update, &Capability::root())).unwrap();
    assert!(!kernel.is_cached_in(&request, &Capability::root(), &then));
    assert_eq!(
        select(&kernel, query, Some(then)),
        format!("{PINNED_LEXICAL},1")
    );
}

#[test]
fn a_now_on_the_kernels_live_clock_reads_it_again_on_a_repeat() {
    let kernel = store_kernel().with_clock(Arc::new(Ticking(AtomicU64::new(PINNED_MILLIS))));
    let request = select_request(NOW_QUERY);
    let (first, event) = traced_in(&kernel, NOW_QUERY, &Scope::empty());
    assert!(note(&event, NOW_NOTE).is_some());
    assert!(note(&event, NOT_REPEATABLE_NOTE).is_some());
    assert!(!kernel.is_cached(&request, &Capability::root()));
    let (second, event) = traced_in(&kernel, NOW_QUERY, &Scope::empty());
    assert_eq!(event.state(), TraceState::Computed);
    assert_ne!(first, second, "the repeat was served the first instant");
}

#[test]
fn a_corridor_whose_clock_moved_since_it_was_pinned_is_not_cached() {
    // The corridor reads its clock once when injected (its pinned instant, in the cache key)
    // and again when `NOW()` asks: a second later, so the answer is not the key's instant.
    let kernel = store_kernel();
    let moving = corridor_on(
        "urn:example:corridor:moving",
        Arc::new(Ticking(AtomicU64::new(PINNED_MILLIS))),
    );
    let request = select_request(NOW_QUERY);
    let (first, event) = traced_in(&kernel, NOW_QUERY, &moving);
    assert_ne!(first, PINNED_LEXICAL);
    assert!(note(&event, NOW_NOTE).is_some());
    assert!(!kernel.is_cached_in(&request, &Capability::root(), &moving));
    let (second, _) = traced_in(&kernel, NOW_QUERY, &moving);
    assert_ne!(first, second);
}
