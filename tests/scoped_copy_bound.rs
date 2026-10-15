//! The scoped write door's copy is bounded in quads (ledger #146).
//!
//! `urn:iki:store:graph-update` evaluates an update against a private copy of what it can
//! observe of the graph. An update that observes the graph as a whole copies all of it, so a
//! host pointing the door at a large graph got a silent memory and time cliff. Past
//! `DurableStore::max_scoped_copy_quads` the update is now refused with nothing applied, naming
//! the broad door. `tests/scoped_copy_measure.rs` has the measurement behind the default.
//!
//! The bound applies only to a caller who may READ the graph: for a write-only caller,
//! refused-or-not would say whether the graph holds more than the bound (the oracle ledger
//! #761 closed), so [`a_write_only_caller_is_never_refused_on_the_bound`] pins that it is not.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, cap_write_graph, space, DurableStore};
use std::sync::Arc;

const G: &str = "urn:x:bound:g";

fn kernel(store: DurableStore) -> Kernel {
    Kernel::with_meta_renderer(
        Arc::new(space(store)),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

fn issue(
    kernel: &Kernel,
    cap: &Capability,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
) -> ikigai_core::Result<String> {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec())),
    );
    block_on(kernel.issue(request, cap)).map(|rep| String::from_utf8(rep.bytes).unwrap())
}

/// A kernel over a store bounded at `bound` copied quads, with `quads` quads in `G` (two
/// predicates, alternating) and one in the default graph, loaded under root.
fn seeded(bound: usize, quads: usize) -> Kernel {
    let k = kernel(
        DurableStore::in_memory()
            .unwrap()
            .with_max_scoped_copy_quads(bound),
    );
    let mut doc = String::from("<urn:x:d> <urn:x:p> \"default\" .\n");
    for i in 0..quads {
        doc.push_str(&format!(
            "<urn:x:s{i}> <urn:x:p{}> \"{i}\" <{G}> .\n",
            i % 2
        ));
    }
    issue(
        &k,
        &Capability::root(),
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", &doc), ("format", "application/n-quads")],
    )
    .unwrap();
    k
}

fn reader_writer() -> Capability {
    Capability::scoped([cap_write_graph(G), cap_read_graph(G)])
}

fn quads_in_g(k: &Kernel) -> String {
    issue(
        k,
        &Capability::root(),
        Verb::Source,
        "urn:iki:store:select",
        &[(
            "query",
            &format!("SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{G}> {{ ?s ?p ?o }} }}"),
        )],
    )
    .unwrap()
}

/// An update whose only pattern has no constant: its footprint is the whole graph.
const WHOLE: &str = "DELETE WHERE { GRAPH <urn:x:bound:g> { ?s ?p ?o } }";

#[test]
fn a_whole_graph_update_past_the_bound_is_refused_and_applies_nothing() {
    let k = seeded(10, 11);
    let before = quads_in_g(&k);
    let err = issue(
        &k,
        &reader_writer(),
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[("content", WHOLE), ("graph", G)],
    )
    .unwrap_err();
    let Error::InvalidArgument { name, detail } = &err else {
        panic!("expected an InvalidArgument, got {err:?}");
    };
    assert_eq!(name, "content");
    for needle in [
        "copies at most 10 quads",
        "`urn:iki:store:update`",
        "urn:iki:store:graph-info",
        "Nothing was applied",
    ] {
        assert!(detail.contains(needle), "{needle:?} missing from: {detail}");
    }
    assert_eq!(quads_in_g(&k), before, "a refused update changed the graph");
    assert!(before.contains("\"11\""), "{before}");
}

#[test]
fn a_whole_graph_update_at_the_bound_is_applied() {
    let k = seeded(10, 10);
    let answer = issue(
        &k,
        &reader_writer(),
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[("content", WHOLE), ("graph", G)],
    )
    .unwrap();
    assert_eq!(answer, format!("updated <{G}>: +0 -10 quads\n"));
}

/// ★ The bound is on what the update can OBSERVE, not on the graph: an update naming its
/// subject and predicate copies a quad, however large the graph is (ledger #1132).
#[test]
fn an_update_that_names_its_constants_is_not_bounded_by_the_graph() {
    let k = seeded(2, 50);
    let answer = issue(
        &k,
        &reader_writer(),
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            (
                "content",
                "DELETE { GRAPH <urn:x:bound:g> { <urn:x:s4> <urn:x:p0> ?o } } \
                 INSERT { GRAPH <urn:x:bound:g> { <urn:x:s4> <urn:x:p0> \"four\" } } \
                 WHERE { GRAPH <urn:x:bound:g> { <urn:x:s4> <urn:x:p0> ?o } }",
            ),
            ("graph", G),
        ],
    )
    .unwrap();
    assert_eq!(answer, format!("updated <{G}>: +1 -1 quads\n"));
}

/// ⚠ The oracle half (ledger #761): a caller who may write `G` and may not read it gets the
/// same answer whether `G` is under the bound or over it. Its whole-graph updates are `CLEAR`
/// and `DROP`, bounded by the time budget instead.
#[test]
fn a_write_only_caller_is_never_refused_on_the_bound() {
    let write_only = Capability::scoped([cap_write_graph(G)]);
    for quads in [5, 11] {
        let k = seeded(10, quads);
        let answer = issue(
            &k,
            &write_only,
            Verb::Sink,
            "urn:iki:store:graph-update",
            &[("content", &format!("CLEAR GRAPH <{G}>")), ("graph", G)],
        )
        .unwrap();
        assert_eq!(answer, format!("updated <{G}>\n"), "at {quads} quads");
        assert!(quads_in_g(&k).contains("\"0\""), "at {quads} quads");
    }
}

#[test]
fn the_default_bound_is_the_stated_one() {
    let store = DurableStore::in_memory().unwrap();
    assert_eq!(
        store.max_scoped_copy_quads(),
        ikigai_store::limits::MAX_SCOPED_COPY_QUADS
    );
    assert_eq!(ikigai_store::limits::MAX_SCOPED_COPY_QUADS, 250_000);
    // At least 1: a host cannot configure a door that refuses everything that copies.
    assert_eq!(
        store.with_max_scoped_copy_quads(0).max_scoped_copy_quads(),
        1
    );
}
