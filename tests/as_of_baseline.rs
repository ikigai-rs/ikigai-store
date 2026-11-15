//! The two facts `docs/design/as-of-reads.md` builds on (ledger #3731), pinned before any
//! history exists so a change to either one is seen.
//!
//! 1. The store keeps no history: once a scoped write deletes a value, no read can return
//!    it. Every as-of design in the note starts from this.
//! 2. A broad update writes a named graph without naming it: `urn:iki:store:update` takes
//!    no `graph` argument, yet it changes the graph. A per-graph history fed only by
//!    `graph-update` would silently miss that write (decision 3 in the note).

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;

const G: &str = "urn:example:g";
const VALUES: &str = "SELECT ?o WHERE { ?s ?p ?o } ORDER BY ?o";

fn issue(kernel: &Kernel, verb: Verb, iri: &str, args: &[(&str, &str)]) -> String {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec())),
    );
    let answer = block_on(kernel.issue(request, &Capability::root())).unwrap();
    String::from_utf8(answer.bytes).unwrap()
}

fn values(kernel: &Kernel) -> Vec<String> {
    let csv = issue(
        kernel,
        Verb::Source,
        "urn:iki:store:graph-select",
        &[("graph", G), ("query", VALUES), ("as", "text/csv")],
    );
    csv.lines().skip(1).map(|l| l.trim().to_string()).collect()
}

#[test]
fn the_store_keeps_no_history_and_a_broad_update_writes_a_graph_it_does_not_name() {
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    let scoped = |content: &str| {
        issue(
            &kernel,
            Verb::Sink,
            "urn:iki:store:graph-update",
            &[("graph", G), ("content", content)],
        )
    };

    scoped(&format!(
        "INSERT DATA {{ GRAPH <{G}> {{ <urn:s> <urn:p> \"a\" }} }}"
    ));
    scoped(&format!(
        "DELETE DATA {{ GRAPH <{G}> {{ <urn:s> <urn:p> \"a\" }} }}"
    ));
    scoped(&format!(
        "INSERT DATA {{ GRAPH <{G}> {{ <urn:s> <urn:p> \"b\" }} }}"
    ));
    assert_eq!(
        values(&kernel),
        ["b"],
        "the deleted value is gone: there is no history"
    );

    // No `graph` argument, and still a write to G.
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:update",
        &[(
            "content",
            &format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:s> <urn:p> \"c\" }} }}"),
        )],
    );
    assert_eq!(
        values(&kernel),
        ["b", "c"],
        "a broad update changed G without naming it"
    );
}
