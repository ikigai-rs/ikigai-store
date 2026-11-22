//! A query that calls SPARQL `NOW()` is never cached (ledger #3989).
//!
//! The claim, found by the `NOW()` design spike (ledger #2203): through `urn:iki:store:select`,
//! `SELECT (NOW() AS ?t)` repeated 20 ms later returned the FIRST answer from the kernel cache,
//! because the store declared it cacheable like any other covered read. Its answer depends on
//! the current time, which no write thread can cut. These tests go through the real doors
//! only. On main at 64a4672 (0.6.0), `a_repeated_now_query_reads_the_clock_again` fails:
//! both reads return the same instant.

use futures::executor::block_on;
use ikigai_core::{
    ArgRef, Capability, Iri, Kernel, Request, TraceEvent, Tracer, Verb, UNCACHED_NOTE,
};
use ikigai_store::endpoints::NOW_NOTE;
use ikigai_store::{space, DurableStore};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn kernel() -> Kernel {
    Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())))
}

fn select_request(query: &str) -> Request {
    Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
        .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
        .with_arg("as", ArgRef::Inline(b"text/csv".to_vec()))
}

/// The first row of the CSV answer, without the header.
fn select(kernel: &Kernel, query: &str) -> String {
    let answer = block_on(kernel.issue(select_request(query), &Capability::root())).unwrap();
    let csv = String::from_utf8(answer.bytes).unwrap();
    csv.lines().nth(1).unwrap_or_default().trim().to_string()
}

fn is_cached(kernel: &Kernel, query: &str) -> bool {
    kernel.is_cached(&select_request(query), &Capability::root())
}

/// What `urn:kernel:uncached` says about the store's select door.
fn uncached_readout(kernel: &Kernel) -> String {
    let request = Request::new(Verb::Source, Iri::parse("urn:kernel:uncached").unwrap());
    let answer = block_on(kernel.issue(request, &Capability::root())).unwrap();
    String::from_utf8(answer.bytes).unwrap()
}

#[test]
fn a_repeated_now_query_reads_the_clock_again() {
    let kernel = kernel();
    let query = "SELECT (NOW() AS ?t) WHERE {}";
    let first = select(&kernel, query);
    std::thread::sleep(Duration::from_millis(20));
    let second = select(&kernel, query);
    assert_ne!(
        first, second,
        "the repeat, 20 ms of wall clock later, was served the first instant"
    );
    assert!(!is_cached(&kernel, query));
}

#[test]
fn a_now_query_is_explained_by_the_uncached_readout() {
    let kernel = kernel();
    select(&kernel, "SELECT (NOW() AS ?t) WHERE {}");
    let readout = uncached_readout(&kernel);
    let row = readout
        .lines()
        .find(|line| line.contains("urn:iki:store:select"))
        .unwrap_or_else(|| panic!("no row for the select door in:\n{readout}"));
    assert!(row.contains("declared"), "{row}");
}

struct Collect(Mutex<Vec<TraceEvent>>);

impl Tracer for Collect {
    fn record(&self, event: TraceEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[test]
fn the_trace_names_now_as_the_reason() {
    let kernel = kernel();
    let collect = Arc::new(Collect(Mutex::new(Vec::new())));
    block_on(kernel.issue_traced(
        select_request("SELECT (NOW() AS ?t) WHERE {}"),
        &Capability::root(),
        collect.clone(),
    ))
    .unwrap();
    let events = collect.0.lock().unwrap();
    let event = events
        .iter()
        .find(|e| e.target == "urn:iki:store:select")
        .expect("the select door was traced");
    let note = |key: &str| {
        event
            .notes
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(note(UNCACHED_NOTE).as_deref(), Some("declared"));
    let why = note(NOW_NOTE).expect("the NOW() note is on the event");
    assert!(why.contains("NOW()"), "{why}");
}

#[test]
fn a_query_without_now_is_still_cached() {
    let kernel = kernel();
    let query = "SELECT (STR(1) AS ?t) WHERE {}";
    select(&kernel, query);
    assert!(is_cached(&kernel, query));
}

#[test]
fn now_inside_a_filter_is_detected() {
    let kernel = kernel();
    let query = "SELECT ?x WHERE { VALUES ?x { 1 } FILTER(NOW() > \"2000-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>) }";
    assert_eq!(select(&kernel, query), "1");
    assert!(!is_cached(&kernel, query));
}

#[test]
fn now_inside_a_subquery_is_detected() {
    let kernel = kernel();
    let query = "SELECT ?t WHERE { { SELECT (NOW() AS ?t) WHERE {} } }";
    select(&kernel, query);
    assert!(!is_cached(&kernel, query));
}

#[test]
fn now_inside_a_bind_is_detected() {
    let kernel = kernel();
    let query = "SELECT ?t WHERE { BIND(NOW() AS ?t) }";
    select(&kernel, query);
    assert!(!is_cached(&kernel, query));
}

#[test]
fn the_word_now_in_a_string_is_not_now() {
    let kernel = kernel();
    let query = "SELECT (\"NOW()\" AS ?t) WHERE {}";
    assert_eq!(select(&kernel, query), "NOW()");
    assert!(is_cached(&kernel, query));
}
