//! A `SharedWriter::over` that joins an earlier, narrower writer says so (ledger #1479).
//!
//! # The claim, reproduced here before it was fixed
//!
//! Since 0.2.18 every later `over` on one handle joins the first writer and is narrowed to its
//! declaration (ledger #1324), and an `over` on a handle a declaring store gave out is narrowed
//! to the store's (ledger #1276). Both are right: a join must not widen the promise cached reads
//! rest on. But the narrowing was SILENT. `ikigai-browse` PR 101 built an archive writer over
//! the default graph first, and the mount's writer for its named graph, built second, was
//! narrowed to the default graph; every write to the named graph was refused, and the refusal
//! named only the inherited declaration ("this writer's declaration names the default graph
//! only"), which is not what that writer declared. On 0.2.21 both tests below failed on the
//! refusal text, and nothing on the writer said what the join had dropped.
//!
//! # The fix
//!
//! [`SharedWriter::narrowed_away`] names the graphs the join dropped, and the refusal of a
//! write to one of them says the join dropped it, names the declaration that won and where it
//! came from, and says how to avoid it (make the widest writer first).

use ikigai_store::{
    DurableStore, SharedWriter, SharerWrites, StorageError, Store, UndeclaredGraph,
};
use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};
use std::sync::Arc;

const NOTES: &str = "urn:example:notes";
const ELSEWHERE: &str = "urn:example:elsewhere";

/// The refusal text of a write to `graph` through `writer`, checked against the typed
/// accessor: [`UndeclaredGraph::narrowed_by_join`] agrees with the text.
fn refusal(writer: &SharedWriter, graph: &'static str) -> String {
    let error = writer
        .write_declared(move |txn| {
            let node = NamedNodeRef::new_unchecked(graph);
            txn.insert(QuadRef::new(
                node,
                node,
                node,
                GraphNameRef::NamedNode(node),
            ))
        })
        .expect_err("the write should be refused");
    let typed = UndeclaredGraph::of(&error).expect("an UndeclaredGraph refusal");
    let text = error.to_string();
    assert_eq!(
        typed.narrowed_by_join(),
        text.contains("narrowed"),
        "{text}"
    );
    text
}

/// ★ browse PR 101's shape: the narrow writer first, on a handle the host opened itself.
#[test]
fn a_later_over_narrowed_by_the_first_names_what_the_join_dropped() {
    let store = Arc::new(Store::new().unwrap());
    // The archive writer: default graph only, made first.
    let _archive = SharedWriter::over(Arc::clone(&store), SharerWrites::only_the_default_graph());
    // The mount's writer, asking for its named graph, made second.
    let mount = SharedWriter::over(
        Arc::clone(&store),
        SharerWrites::only_the_default_graph().and_named_graph(NOTES),
    );
    let text = refusal(&mount, NOTES);
    assert!(
        text.contains(&format!("<{NOTES}>")) && text.contains("narrowed"),
        "the refusal does not say the join narrowed <{NOTES}> away: {text}"
    );
    assert!(
        text.contains("the first `SharedWriter::over` on this handle"),
        "the refusal does not say which declaration won: {text}"
    );
    assert!(
        text.contains("widest writer first"),
        "the refusal does not say how to avoid it: {text}"
    );
    assert_eq!(mount.narrowed_away().collect::<Vec<_>>(), [NOTES]);
    assert_eq!(
        _archive.narrowed_away().count(),
        0,
        "the first writer lost nothing"
    );
    // Nothing was written.
    assert_eq!(store.len().unwrap(), 0);
}

/// The same narrowing by a declaring store's own declaration, named as the store's.
#[test]
fn an_over_narrowed_by_a_declaring_store_names_the_store() {
    let (_store, handle) =
        DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph()).unwrap();
    let module = SharedWriter::over(
        Arc::clone(&handle),
        SharerWrites::only_the_default_graph().and_named_graph(NOTES),
    );
    let text = refusal(&module, NOTES);
    assert!(
        text.contains(&format!("<{NOTES}>"))
            && text.contains("narrowed")
            && text.contains("the store's own `SharerWrites` declaration"),
        "the refusal does not name the store's declaration as the one that won: {text}"
    );
    assert_eq!(module.narrowed_away().collect::<Vec<_>>(), [NOTES]);
}

/// The control: a graph this writer never asked for is refused with the ordinary text, which
/// does not blame a join, and a writer the join did not narrow reports nothing dropped.
#[test]
fn a_graph_never_asked_for_is_refused_without_blaming_the_join() {
    let store = Arc::new(Store::new().unwrap());
    let host = SharedWriter::over(
        Arc::clone(&store),
        SharerWrites::only_the_default_graph().and_named_graph(NOTES),
    );
    let module = SharedWriter::over(
        Arc::clone(&store),
        SharerWrites::only_the_default_graph().and_named_graph(NOTES),
    );
    assert_eq!(module.narrowed_away().count(), 0);
    let text = refusal(&module, ELSEWHERE);
    assert!(!text.contains("narrowed"), "{text}");
    // And the writer that was made first still writes its graph.
    host.write_declared(|txn| {
        let node = NamedNodeRef::new_unchecked(NOTES);
        txn.insert(QuadRef::new(
            node,
            node,
            node,
            GraphNameRef::NamedNode(node),
        ))
    })
    .map_err(|e: StorageError| e.to_string())
    .unwrap();
    assert_eq!(store.len().unwrap(), 1);
}
