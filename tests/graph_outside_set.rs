//! A scoped read whose query names a graph OUTSIDE `graph=` is refused, not answered empty
//! (ledger #388).
//!
//! # The defect this pins
//!
//! Confinement was already exact: a scoped read over `{G}` that says `GRAPH <OTHER> { … }`
//! cannot reach `OTHER`, because `OTHER` is not among the dataset's available named graphs.
//! But the evaluator answers that block with an empty iterator, so a caller who typed an IRI
//! wrong, or forgot to add a graph to the set, got silence: a query that looked right and
//! answered wrong. Since 0.2.22 the endpoint walks the parsed algebra for constant `GRAPH`
//! names and refuses any outside the set, naming them.
//!
//! # ★ Still a pair
//!
//! A refusal alone could hide a door that refuses everything, so every case below sits beside
//! the in-set query that must still answer with rows, and beside the shapes that name no
//! constant graph (`GRAPH ?g`, a bare pattern) and must still be evaluated.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, space, DurableStore};
use std::sync::Arc;

const G: &str = "urn:example:acme";
const OTHER: &str = "urn:example:zenith";
const THIRD: &str = "urn:example:third";

fn kernel() -> Kernel {
    let store = DurableStore::in_memory().unwrap();
    let kernel = Kernel::new(Arc::new(space(store)));
    let seed = format!(
        "INSERT DATA {{ \
           GRAPH <{G}> {{ <urn:example:acme:1> <urn:p> \"acme\" }} \
           GRAPH <{OTHER}> {{ <urn:example:zenith:1> <urn:p> \"zenith\" }} \
           GRAPH <{THIRD}> {{ <urn:example:third:1> <urn:p> \"third\" }} \
         }}"
    );
    issue(
        &kernel,
        "urn:iki:store:update",
        Verb::Sink,
        &[("content", &seed)],
        &Capability::root(),
    )
    .expect("seeding");
    kernel
}

fn issue(
    kernel: &Kernel,
    iri: &str,
    verb: Verb,
    args: &[(&str, &str)],
    cap: &Capability,
) -> Result<String, Error> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(request, cap)).map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

/// A scoped read of `form` over `graphs`, holding a grant for every graph in the store, so a
/// refusal here is about the DATASET and never about authority.
fn scoped(kernel: &Kernel, form: &str, graphs: &str, query: &str) -> Result<String, Error> {
    let as_type = if matches!(form, "construct" | "describe") {
        "application/n-triples"
    } else {
        "text/csv"
    };
    issue(
        kernel,
        &format!("urn:iki:store:graph-{form}"),
        Verb::Source,
        &[("graph", graphs), ("query", query), ("as", as_type)],
        &Capability::scoped([
            cap_read_graph(G),
            cap_read_graph(OTHER),
            cap_read_graph(THIRD),
        ]),
    )
}

/// The refusal, checked for its shape: `InvalidArgument` on `query`, naming the outside graph
/// and saying what to do about it.
fn assert_refused_naming(result: Result<String, Error>, outside: &[&str], case: &str) {
    let err = match result {
        Ok(answer) => panic!("{case}: answered instead of refused: {answer:?}"),
        Err(err) => err,
    };
    let Error::InvalidArgument { name, detail } = &err else {
        panic!("{case}: not InvalidArgument: {err:?}");
    };
    assert_eq!(name, "query", "{case}");
    for iri in outside {
        assert!(
            detail.contains(&format!("<{iri}>")),
            "{case}: the refusal does not name <{iri}>: {detail}"
        );
    }
    assert!(
        detail.contains("graph="),
        "{case}: the refusal does not say how to widen the set: {detail}"
    );
}

/// ★ The item's own case, and one per shape a walk could miss: each names `OTHER` somewhere
/// only the full algebra reaches.
#[test]
fn a_constant_graph_outside_the_set_is_refused_wherever_it_appears() {
    let kernel = kernel();
    for (case, query) in [
        (
            "named outright",
            format!("SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}"),
        ),
        (
            "beside an in-set block",
            format!(
                "SELECT ?s WHERE {{ GRAPH <{G}> {{ ?s ?p ?o }} \
                 OPTIONAL {{ GRAPH <{OTHER}> {{ ?s ?q ?r }} }} }}"
            ),
        ),
        (
            "in a sub-select",
            format!(
                "SELECT ?s WHERE {{ {{ SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }} }} }}"
            ),
        ),
        (
            "in FILTER EXISTS",
            format!(
                "SELECT ?s WHERE {{ ?s ?p ?o FILTER EXISTS {{ GRAPH <{OTHER}> {{ ?a ?b ?c }} }} }}"
            ),
        ),
        (
            "in FILTER NOT EXISTS",
            format!(
                "SELECT ?s WHERE {{ ?s ?p ?o \
                 FILTER NOT EXISTS {{ GRAPH <{OTHER}> {{ ?s ?b ?c }} }} }}"
            ),
        ),
        (
            "in a BIND's EXISTS",
            format!(
                "SELECT ?s ?x WHERE {{ ?s ?p ?o \
                 BIND(EXISTS {{ GRAPH <{OTHER}> {{ ?s ?b ?c }} }} AS ?x) }}"
            ),
        ),
        (
            "in MINUS",
            format!("SELECT ?s WHERE {{ ?s ?p ?o MINUS {{ GRAPH <{OTHER}> {{ ?s ?b ?c }} }} }}"),
        ),
        (
            "in one branch of a UNION",
            format!(
                "SELECT ?s WHERE {{ {{ GRAPH <{G}> {{ ?s ?p ?o }} }} \
                 UNION {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }} }}"
            ),
        ),
        (
            "nested inside an in-set block",
            format!("SELECT ?s WHERE {{ GRAPH <{G}> {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }} }}"),
        ),
        (
            "with a property path",
            format!("SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s <urn:p>/<urn:q>* ?o }} }}"),
        ),
        (
            "in LATERAL",
            format!("SELECT ?s WHERE {{ ?s ?p ?o LATERAL {{ GRAPH <{OTHER}> {{ ?s ?b ?c }} }} }}"),
        ),
        (
            "under an aggregate",
            format!("SELECT (COUNT(?s) AS ?n) WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}"),
        ),
    ] {
        assert_refused_naming(scoped(&kernel, "select", G, &query), &[OTHER], case);
    }
}

/// Every outside graph is named, once each and not only the first, and an in-set graph in
/// the same query is not named as outside.
#[test]
fn every_outside_graph_is_named_once() {
    let kernel = kernel();
    let query = format!(
        "SELECT ?s WHERE {{ GRAPH <{G}> {{ ?s ?p ?o }} GRAPH <{THIRD}> {{ ?s ?p ?o }} \
         GRAPH <{OTHER}> {{ ?s ?p ?o }} GRAPH <{OTHER}> {{ ?s ?q ?o }} }}"
    );
    let result = scoped(&kernel, "select", G, &query);
    let detail = match &result {
        Err(Error::InvalidArgument { detail, .. }) => detail.clone(),
        other => panic!("not refused: {other:?}"),
    };
    assert_refused_naming(result, &[OTHER, THIRD], "two outside");
    assert_eq!(
        detail.matches(&format!("<{OTHER}>")).count(),
        1,
        "named once: {detail}"
    );
}

/// All four scoped forms refuse it, so the check is the door's and not one form's.
#[test]
fn all_four_scoped_forms_refuse_it() {
    let kernel = kernel();
    let block = format!("GRAPH <{OTHER}> {{ ?s ?p ?o }}");
    for (form, query) in [
        ("select", format!("SELECT ?s WHERE {{ {block} }}")),
        ("ask", format!("ASK {{ {block} }}")),
        (
            "construct",
            format!("CONSTRUCT {{ ?s ?p ?o }} WHERE {{ {block} }}"),
        ),
        ("describe", format!("DESCRIBE ?s WHERE {{ {block} }}")),
    ] {
        assert_refused_naming(scoped(&kernel, form, G, &query), &[OTHER], form);
    }
}

/// ★ The other half of the pair: what the walk must NOT refuse. An in-set constant, a member
/// of a multi-graph set, a variable graph and a bare pattern all still answer, with rows.
#[test]
fn in_set_and_variable_graphs_still_answer() {
    let kernel = kernel();
    let rows = |graphs: &str, query: &str| -> Vec<String> {
        scoped(&kernel, "select", graphs, query)
            .unwrap_or_else(|e| panic!("`{query}` over `{graphs}` was refused: {e}"))
            .lines()
            .skip(1)
            .map(str::to_string)
            .collect()
    };
    assert_eq!(
        rows(
            G,
            &format!("SELECT ?s WHERE {{ GRAPH <{G}> {{ ?s ?p ?o }} }}")
        ),
        ["urn:example:acme:1"]
    );
    assert_eq!(
        rows(
            &format!("{G} {OTHER}"),
            &format!("SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}")
        ),
        ["urn:example:zenith:1"],
        "a member of the set is not outside it"
    );
    assert_eq!(
        rows(G, "SELECT ?g WHERE { GRAPH ?g { ?s ?p ?o } }"),
        [G],
        "a variable graph names no constant and still enumerates only the set"
    );
    assert_eq!(
        rows(G, "SELECT ?s WHERE { ?s ?p ?o }"),
        ["urn:example:acme:1"]
    );
}

/// The broad door is untouched: it has no set, so a `GRAPH <iri>` there is an ordinary read.
#[test]
fn the_broad_door_is_untouched() {
    let kernel = kernel();
    let csv = issue(
        &kernel,
        "urn:iki:store:select",
        Verb::Source,
        &[
            (
                "query",
                &format!("SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}"),
            ),
            ("as", "text/csv"),
        ],
        &Capability::root(),
    )
    .unwrap();
    assert!(csv.contains("urn:example:zenith:1"), "{csv}");
}
