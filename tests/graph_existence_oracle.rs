//! A caller who may WRITE a graph but not READ it learns nothing about whether it exists
//! (ledger #761).
//!
//! The ledger #751 arc closed every read channel it found through the write doors except one,
//! reproduced then and left open: a non-`SILENT` `DROP GRAPH <G>` or `CLEAR GRAPH <G>`
//! fails when `G` does not exist and succeeds when it does, so a write-only caller reads one
//! bit of `G`'s state per call. `CREATE GRAPH <G>` is the same bit read the other way round
//! (it fails when `G` DOES exist). The scoped door's grant covers destroying `G`, never
//! knowing what was there.
//!
//! ★ The uniform answer is the `SILENT` one: for a caller without the read grant, those
//! three operations run as if the update had said `SILENT`. The answer is then the same
//! success line whatever `G`'s state, and the EFFECT is still exactly what the caller asked
//! for (after `DROP`, `G` is gone; after `CREATE`, it exists), which no uniform refusal
//! could say. A caller who may read `G` keeps standard SPARQL semantics, because the error is
//! useful to someone who could have asked `graph-ask` instead.
//!
//! Each test compares one call across every state `G` can be in and asserts the answers are
//! byte-identical, which is the only form of "learns nothing" a test can check.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, cap_write_graph, space, DurableStore, CAP_READ, CAP_WRITE};
use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};
use oxigraph::store::Store;
use std::sync::Arc;

const SCOPED: &str = "urn:iki:store:graph-update";
const BROAD: &str = "urn:iki:store:update";
const G: &str = "urn:tenant:g";

/// Every state a named graph can be in, as far as SPARQL graph management can tell.
#[derive(Debug, Clone, Copy)]
enum State {
    /// Never created, holds nothing.
    Absent,
    /// Registered by `CREATE GRAPH`, holds nothing.
    Empty,
    /// Holds a quad.
    Populated,
}

const STATES: [State; 3] = [State::Absent, State::Empty, State::Populated];

/// A kernel over a fresh store with `G` in `state`, and the raw handle to inspect it by.
///
/// The `_shared` constructor only so the test can look at the dataset directly; it makes
/// reads uncached, which nothing here depends on.
fn kernel_with(state: State) -> (Kernel, Arc<Store>) {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let g = NamedNodeRef::new(G).unwrap();
    match state {
        State::Absent => {}
        State::Empty => handle.insert_named_graph(g).unwrap(),
        State::Populated => {
            handle
                .insert(QuadRef::new(
                    NamedNodeRef::new("urn:s").unwrap(),
                    NamedNodeRef::new("urn:p").unwrap(),
                    NamedNodeRef::new("urn:o").unwrap(),
                    GraphNameRef::NamedNode(g),
                ))
                .unwrap();
        }
    }
    (Kernel::new(Arc::new(space(store))), handle)
}

fn sink(kernel: &Kernel, iri: &str, args: &[(&str, &str)], cap: &Capability) -> String {
    let mut request = Request::new(Verb::Sink, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    // Rendered whole, so `Ok` against `Err` and one error text against another both differ.
    match block_on(kernel.issue(request, cap)) {
        Ok(r) => format!("ok: {}", String::from_utf8_lossy(&r.bytes)),
        Err(e) => format!("err: {e:?}"),
    }
}

fn scoped(kernel: &Kernel, update: &str, cap: &Capability) -> String {
    sink(kernel, SCOPED, &[("graph", G), ("content", update)], cap)
}

fn broad(kernel: &Kernel, update: &str, cap: &Capability) -> String {
    sink(kernel, BROAD, &[("content", update)], cap)
}

fn scoped_write_only() -> Capability {
    Capability::scoped([cap_write_graph(G)])
}

fn broad_write_only() -> Capability {
    Capability::scoped([CAP_WRITE.to_string()])
}

fn exists(handle: &Store) -> bool {
    handle
        .contains_named_graph(NamedNodeRef::new(G).unwrap())
        .unwrap()
}

fn populated(handle: &Store) -> bool {
    handle
        .quads_for_pattern(None, None, None, Some(NamedNodeRef::new(G).unwrap().into()))
        .next()
        .is_some()
}

/// The answers to `update` from every state of `G`, through `door`, under `cap` — with the
/// effect on `G` checked by `effect` after each.
fn answers(
    door: fn(&Kernel, &str, &Capability) -> String,
    update: &str,
    cap: &Capability,
    effect: fn(&Store) -> bool,
) -> Vec<(State, String)> {
    STATES
        .iter()
        .map(|&state| {
            let (kernel, handle) = kernel_with(state);
            let answer = door(&kernel, update, cap);
            assert!(
                effect(&handle),
                "`{update}` from {state:?} did not do what it said: {answer}"
            );
            (state, answer)
        })
        .collect()
}

fn assert_uniform(update: &str, answers: &[(State, String)]) {
    let first = &answers[0].1;
    assert!(
        first.starts_with("ok: "),
        "`{update}` should succeed whatever G's state, got {first}"
    );
    for (state, answer) in answers {
        assert_eq!(
            answer, first,
            "`{update}` answered a caller who may not read G differently when G was \
             {state:?} — that is one bit of G's state:\n{answers:#?}"
        );
    }
}

// --------------------------------------------------------------- the scoped door

#[test]
fn drop_graph_tells_a_write_only_caller_nothing_about_the_graph() {
    let update = format!("DROP GRAPH <{G}>");
    let answers = answers(scoped, &update, &scoped_write_only(), |h| {
        !exists(h) && !populated(h)
    });
    assert_uniform(&update, &answers);
}

#[test]
fn clear_graph_tells_a_write_only_caller_nothing_about_the_graph() {
    let update = format!("CLEAR GRAPH <{G}>");
    let answers = answers(scoped, &update, &scoped_write_only(), |h| !populated(h));
    assert_uniform(&update, &answers);
}

#[test]
fn create_graph_tells_a_write_only_caller_nothing_about_the_graph() {
    let update = format!("CREATE GRAPH <{G}>");
    let answers = answers(scoped, &update, &scoped_write_only(), exists);
    assert_uniform(&update, &answers);
}

/// The oracle survives inside a longer request too: a `;`-separated update is all or
/// nothing, so a failing `DROP` would refuse the `INSERT DATA` beside it.
#[test]
fn a_graph_operation_inside_a_longer_update_is_uniform_too() {
    let update =
        format!("DROP GRAPH <{G}> ; INSERT DATA {{ GRAPH <{G}> {{ <urn:x> <urn:y> <urn:z> }} }}");
    let answers = answers(scoped, &update, &scoped_write_only(), populated);
    assert_uniform(&update, &answers);
}

/// A caller who may read `G` could ask `graph-ask` instead, so it keeps standard SPARQL
/// semantics and the error that tells it its `DROP` named nothing.
#[test]
fn a_caller_who_may_read_the_graph_keeps_the_standard_error() {
    let reader = Capability::scoped([cap_write_graph(G), cap_read_graph(G)]);
    let (kernel, _) = kernel_with(State::Absent);
    let answer = scoped(&kernel, &format!("DROP GRAPH <{G}>"), &reader);
    assert!(answer.starts_with("err: "), "{answer}");
    let (kernel, _) = kernel_with(State::Empty);
    assert!(scoped(&kernel, &format!("DROP GRAPH <{G}>"), &reader).starts_with("ok: "));
    // And an explicit `SILENT` is still the caller's to write.
    let (kernel, _) = kernel_with(State::Absent);
    assert!(scoped(&kernel, &format!("DROP SILENT GRAPH <{G}>"), &reader).starts_with("ok: "));
}

/// Only the scoped graph is silenced. `DROP GRAPH <other>` through the scoped door runs
/// against a private dataset holding `G` alone, so it fails the same way whatever `<other>`
/// holds — already uniform, and it stays a refusal rather than becoming a success that
/// touched nothing.
#[test]
fn another_graph_named_through_the_scoped_door_is_refused_whatever_it_holds() {
    let (kernel, handle) = kernel_with(State::Absent);
    handle
        .insert_named_graph(NamedNodeRef::new("urn:tenant:other").unwrap())
        .unwrap();
    let there = scoped(
        &kernel,
        "DROP GRAPH <urn:tenant:other>",
        &scoped_write_only(),
    );
    let (kernel, _) = kernel_with(State::Absent);
    let not_there = scoped(
        &kernel,
        "DROP GRAPH <urn:tenant:other>",
        &scoped_write_only(),
    );
    assert!(there.starts_with("err: "), "{there}");
    assert_eq!(there, not_there);
}

// ---------------------------------------------------------------- the broad door

/// The broad door has the same rule as the scoped one (ledger #751): a caller holding
/// `urn:cap:store:write` without `urn:cap:store:read` may destroy any graph and read none,
/// so the same three operations are silent for it.
#[test]
fn the_broad_door_tells_a_write_only_caller_nothing_either() {
    for (update, effect) in [
        (
            format!("DROP GRAPH <{G}>"),
            (|h: &Store| !exists(h) && !populated(h)) as fn(&Store) -> bool,
        ),
        (format!("CLEAR GRAPH <{G}>"), |h: &Store| !populated(h)),
        (format!("CREATE GRAPH <{G}>"), exists),
    ] {
        let answers = answers(broad, &update, &broad_write_only(), effect);
        assert_uniform(&update, &answers);
    }
}

#[test]
fn the_broad_reader_keeps_the_standard_error() {
    let reader = Capability::scoped([CAP_WRITE.to_string(), CAP_READ.to_string()]);
    let (kernel, _) = kernel_with(State::Absent);
    let answer = broad(&kernel, &format!("DROP GRAPH <{G}>"), &reader);
    assert!(answer.starts_with("err: "), "{answer}");
}

/// The rewrite must not reach an operation that was never an oracle: `DROP DEFAULT`,
/// `DROP NAMED` and `DROP ALL` cannot fail, and still do exactly what they say.
#[test]
fn the_whole_dataset_operations_are_unchanged() {
    let (kernel, handle) = kernel_with(State::Populated);
    assert!(broad(&kernel, "DROP ALL", &broad_write_only()).starts_with("ok: "));
    assert!(!populated(&handle) && !exists(&handle));
}
