//! Observations across equivalent in-memory datasets, not a cross-engine guarantee.
use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use oxigraph::model::Graph;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use std::collections::BTreeMap;
use std::sync::Arc;

fn stores() -> (Store, Store) {
    let a = Store::new().unwrap();
    let b = Store::new().unwrap();
    let triples = [
        "<urn:a> <urn:p> 1 .",
        "<urn:b> <urn:p> 2 .",
        "<urn:c> <urn:p> 3 .",
        "<urn:d> <urn:p> 2 .",
    ];
    for t in triples {
        a.update(format!("INSERT DATA {{ {t} }}").as_str()).unwrap();
    }
    for t in triples.into_iter().rev() {
        b.update(format!("INSERT DATA {{ {t} }}").as_str()).unwrap();
    }
    (a, b)
}

fn rows(store: &Store, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = SparqlEvaluator::new()
        .parse_query(query)
        .unwrap()
        .on_store(store)
        .execute()
        .unwrap()
    else {
        panic!("expected solutions")
    };
    solutions.map(|s| format!("{:?}", s.unwrap())).collect()
}

fn graph(store: &Store, query: &str) -> Graph {
    let QueryResults::Graph(triples) = SparqlEvaluator::new()
        .parse_query(query)
        .unwrap()
        .on_store(store)
        .execute()
        .unwrap()
    else {
        panic!("expected graph")
    };
    triples.map(Result::unwrap).collect()
}

#[test]
fn compare_solution_multisets_slices_and_aggregates() {
    let (a, b) = stores();
    for q in [
        "SELECT ?v WHERE { ?s <urn:p> ?v }",
        "SELECT (COUNT(*) AS ?n) (SUM(?v) AS ?sum) (AVG(?v) AS ?avg) (MIN(?v) AS ?min) (MAX(?v) AS ?max) WHERE { ?s <urn:p> ?v }",
    ] {
        let mut x = rows(&a, q); let mut y = rows(&b, q);
        x.sort(); y.sort(); assert_eq!(x, y, "{q}");
    }
    let q = "SELECT ?s ?v WHERE { ?s <urn:p> ?v } ORDER BY ?v ?s LIMIT 2 OFFSET 1";
    assert_eq!(rows(&a, q), rows(&b, q));
}

#[test]
fn construct_template_blank_nodes_are_fresh_but_graphs_are_isomorphic() {
    let (a, b) = stores();
    let q = "CONSTRUCT { _:x <urn:value> ?v; <urn:source> ?s } WHERE { ?s <urn:p> ?v }";
    let x = graph(&a, q);
    let y = graph(&b, q);
    assert_ne!(x, y);
    // Each generated blank node has exactly two outgoing edges to ground terms.
    // Sorting those edge pairs gives an isomorphism test for this fixture.
    fn signatures(g: &Graph) -> Vec<Vec<String>> {
        let mut nodes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for t in g.iter() {
            assert!(t.subject.is_blank_node());
            assert!(!t.object.is_blank_node());
            nodes
                .entry(t.subject.to_string())
                .or_default()
                .push(format!("{} {}", t.predicate, t.object));
        }
        let mut result: Vec<_> = nodes.into_values().collect();
        for edges in &mut result {
            edges.sort();
            assert_eq!(edges.len(), 2);
        }
        result.sort();
        result
    }
    assert_eq!(signatures(&x), signatures(&y));
}

fn issue(k: &Kernel, verb: Verb, iri: &str, arg: Option<(&str, &str)>) -> Vec<u8> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    if let Some((name, value)) = arg {
        request = request.with_arg(name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(k.issue(request, &Capability::root()))
        .unwrap()
        .bytes
}

#[test]
fn memory_dump_bytes_are_independent_of_insertion_order() {
    let kernels = [
        Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap()))),
        Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap()))),
    ];
    let triples = [
        "<urn:a> <urn:p> 1 .",
        "<urn:b> <urn:p> 2 .",
        "<urn:c> <urn:p> 3 .",
    ];
    for (k, ts) in [
        (&kernels[0], triples.to_vec()),
        (&kernels[1], triples.into_iter().rev().collect()),
    ] {
        for t in ts {
            issue(
                k,
                Verb::Sink,
                "urn:iki:store:update",
                Some(("content", &format!("INSERT DATA {{ {t} }}"))),
            );
        }
    }
    assert_eq!(
        issue(&kernels[0], Verb::Source, "urn:iki:store:dump", None),
        issue(&kernels[1], Verb::Source, "urn:iki:store:dump", None)
    );
}

#[test]
fn floating_sum_depends_on_input_sequence() {
    let s = Store::new().unwrap();
    let a = "SELECT (SUM(?v) AS ?sum) WHERE { VALUES ?v { 1.0e16 -1.0e16 1.0e0 } }";
    let b = "SELECT (SUM(?v) AS ?sum) WHERE { VALUES ?v { 1.0e16 1.0e0 -1.0e16 } }";
    assert_ne!(rows(&s, a), rows(&s, b));
}

#[test]
fn tied_order_keys_can_change_a_slice() {
    let s = Store::new().unwrap();
    let a =
        "SELECT ?s WHERE { VALUES (?s ?key) { (<urn:a> 1) (<urn:b> 1) } } ORDER BY ?key LIMIT 1";
    let b =
        "SELECT ?s WHERE { VALUES (?s ?key) { (<urn:b> 1) (<urn:a> 1) } } ORDER BY ?key LIMIT 1";
    assert_ne!(rows(&s, a), rows(&s, b));
}
