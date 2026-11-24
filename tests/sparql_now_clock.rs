//! The query and update doors use the invocation instant for SPARQL `NOW()`.

use futures::executor::block_on;
use ikigai_core::{
    ArgRef, Capability, EndpointSpace, FixedClock, Iri, Kernel, Request, Scope, Space, Verb,
};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;

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
