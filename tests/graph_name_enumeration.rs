//! A scoped read cannot enumerate another tenant's graph NAMES — the oxigraph floor's
//! regression test (ledger #751).
//!
//! Under oxigraph 0.5.0 (spareval 0.2.0) the evaluator ignored the confined dataset for a
//! `GRAPH ?g {}` with an empty pattern and listed every registered named graph, so a
//! tenant reading its own graph through `urn:iki:store:graph-select` learned the name of
//! every other tenant's. 0.5.1 fixed it upstream, and `Cargo.toml` pins that floor — but
//! a pin only guards the low end. This test goes through the endpoint, on whatever oxigraph
//! the build resolved, so a regression in a LATER release fails here too.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, space, DurableStore};
use std::sync::Arc;

const TA: &str = "urn:tenant:a";
const TB: &str = "urn:tenant:b";
const EMPTY: &str = "urn:tenant:registered-but-empty";

fn issue(
    kernel: &Kernel,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
    cap: &Capability,
) -> String {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    String::from_utf8(block_on(kernel.issue(request, cap)).expect(iri).bytes).unwrap()
}

#[test]
fn a_scoped_read_cannot_enumerate_another_tenants_graph_names() {
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:update",
        &[(
            "content",
            &format!(
                "INSERT DATA {{ GRAPH <{TA}> {{ <urn:a:1> <urn:p> \"a\" }} \
                 GRAPH <{TB}> {{ <urn:b:1> <urn:p> \"b\" }} }} ; CREATE GRAPH <{EMPTY}>"
            ),
        )],
        &Capability::root(),
    );
    let tenant = Capability::scoped([cap_read_graph(TA)]);
    for query in [
        "SELECT ?g WHERE { GRAPH ?g {} }",
        "SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } }",
    ] {
        let rows = issue(
            &kernel,
            Verb::Source,
            "urn:iki:store:graph-select",
            &[("graph", TA), ("query", query), ("as", "text/csv")],
            &tenant,
        );
        assert!(
            rows.contains(TA),
            "`{query}` lost the tenant's own graph: {rows}"
        );
        assert!(
            !rows.contains(TB) && !rows.contains(EMPTY),
            "`{query}` enumerated another tenant's graph names through the scoped door: {rows}"
        );
    }
    // The control: unconfined, the same query really does list all three, so the
    // assertion above is about confinement and not an empty store.
    let all = issue(
        &kernel,
        Verb::Source,
        "urn:iki:store:select",
        &[
            ("query", "SELECT ?g WHERE { GRAPH ?g {} }"),
            ("as", "text/csv"),
        ],
        &Capability::root(),
    );
    assert!(
        all.contains(TA) && all.contains(TB) && all.contains(EMPTY),
        "{all}"
    );
}
