//! What a [`SharedWriter`] lets a host write, and what its writes may wait on (ledger #1209).
//!
//! Two claims, each reproduced here before it was fixed:
//!
//! 1. **A declaration was only a promise.** `open_shared_declaring` takes the graphs the
//!    sharer may write ([`SharerWrites`]) and caches scoped reads of every other graph on
//!    the strength of it, but a write through the store's `SharedWriter` could land in any
//!    graph. On 0.2.14 the reproduction below committed a quad into a graph the declaration
//!    reserved, and a cached read of that graph would have gone stale with no signal.
//! 2. **`urn:iki:store:load` committed on the calling thread**, under the write lock, so a
//!    `SharedWriter` closure (which runs on the writer thread, holding that lock) that asked
//!    the load door for anything waited for itself forever.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore, SharerWrites, StorageError};
use oxigraph::model::{GraphNameRef, NamedNodeRef, Quad, QuadRef};
use std::sync::Arc;
use std::time::Duration;

const DECLARED: &str = "urn:example:declared";
const RESERVED: &str = "urn:example:reserved";

fn quad_in(graph: GraphNameRef<'_>, n: usize) -> Quad {
    QuadRef::new(
        NamedNodeRef::new_unchecked("urn:example:s"),
        NamedNodeRef::new_unchecked("urn:example:p"),
        NamedNodeRef::new_unchecked(&format!("urn:example:o:{n}")),
        graph,
    )
    .into_owned()
}

fn named(iri: &str) -> GraphNameRef<'_> {
    GraphNameRef::NamedNode(NamedNodeRef::new_unchecked(iri))
}

fn declaring() -> (DurableStore, Arc<oxigraph::store::Store>) {
    DurableStore::in_memory_shared_declaring(
        SharerWrites::only_the_default_graph().and_named_graph(DECLARED),
    )
    .unwrap()
}

/// ★ CLAIM 1, as 0.2.14 shipped it: the raw `write` on a declaring store's writer put a
/// quad in a graph the declaration reserved.
#[test]
fn a_raw_write_on_a_declaring_writer_cannot_reach_a_reserved_graph() {
    let (store, handle) = declaring();
    let writer = store.shared_writer().unwrap();
    let before = store.reserved_graphs_fingerprint().unwrap();
    let outcome = writer.write(|txn| {
        txn.insert(&quad_in(named(RESERVED), 0));
        Ok::<_, StorageError>(())
    });
    assert!(
        outcome.is_err(),
        "a raw write through a declaring store's writer was allowed"
    );
    assert_eq!(
        store
            .reserved_graphs_fingerprint()
            .unwrap()
            .changed_since(&before),
        Vec::<String>::new(),
        "the reserved graph moved"
    );
    assert_eq!(handle.len().unwrap(), 0);
}

/// ★ CLAIM 2: a `SharedWriter` closure that asks the load door for a load is answered,
/// not left waiting for itself. Bounded, so the 0.2.14 deadlock fails instead of hanging.
#[test]
fn a_load_asked_from_inside_a_shared_write_is_refused_not_deadlocked() {
    let (store, _handle) = DurableStore::in_memory_shared().unwrap();
    let writer = store.shared_writer().unwrap();
    let kernel = Arc::new(Kernel::new(Arc::new(space(store))));
    let (done, outcome) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let answered = writer.write(move |_| {
            let request = Request::new(Verb::Sink, Iri::parse("urn:iki:store:load").unwrap())
                .with_arg(
                    "content",
                    ArgRef::Inline(b"<urn:example:a> <urn:example:b> <urn:example:c> .".to_vec()),
                )
                .with_arg("format", ArgRef::Inline(b"application/n-triples".to_vec()));
            Ok::<_, StorageError>(block_on(kernel.issue(request, &Capability::root())).map(|_| ()))
        });
        let _ = done.send(answered);
    });
    let load = outcome
        .recv_timeout(Duration::from_secs(10))
        .expect("a load asked from inside a shared write deadlocked")
        .unwrap();
    let refusal = load.expect_err("a load from inside a write was run, not refused");
    assert!(refusal.to_string().contains("wait for itself"), "{refusal}");
}

// ------------------------------------------------------------ declared = enforced

fn refused_graph(error: &StorageError) -> Option<String> {
    ikigai_store::UndeclaredGraph::of(error).map(|r| r.graph().to_string())
}

#[test]
fn a_declared_write_reaches_the_default_graph_and_every_declared_graph() {
    let (store, handle) = declaring();
    let writer = store.shared_writer().unwrap();
    writer
        .write_declared(|txn| {
            txn.insert(&quad_in(GraphNameRef::DefaultGraph, 0))?;
            txn.insert(&quad_in(named(DECLARED), 1))
        })
        .unwrap();
    assert_eq!(handle.len().unwrap(), 2);
    writer
        .write_declared(|txn| {
            txn.remove(&quad_in(GraphNameRef::DefaultGraph, 0))?;
            txn.remove(&quad_in(named(DECLARED), 1))
        })
        .unwrap();
    assert_eq!(handle.len().unwrap(), 0);
}

/// ★ The whole transaction goes, the default-graph half included: a record is atomic, and
/// a record that broke the declaration is not a record the store keeps any of.
#[test]
fn a_write_to_an_undeclared_graph_is_refused_and_rolls_back_the_whole_record() {
    let (store, handle) = declaring();
    let writer = store.shared_writer().unwrap();
    let before = store.reserved_graphs_fingerprint().unwrap();
    let refused = writer
        .write_declared(|txn| {
            txn.insert(&quad_in(GraphNameRef::DefaultGraph, 0))?;
            txn.insert(&quad_in(named(DECLARED), 1))?;
            txn.insert(&quad_in(named(RESERVED), 2))
        })
        .unwrap_err();
    assert_eq!(
        refused_graph(&refused).as_deref(),
        Some("<urn:example:reserved>")
    );
    assert!(refused.to_string().contains("rolled back"), "{refused}");
    assert_eq!(
        handle.len().unwrap(),
        0,
        "part of a refused record was kept"
    );
    assert!(store
        .reserved_graphs_fingerprint()
        .unwrap()
        .changed_since(&before)
        .is_empty());
}

/// A closure that ignores the refusal and returns `Ok` does not get its transaction
/// committed: the refusal is the transaction's, not only the call's.
#[test]
fn a_swallowed_refusal_still_rolls_back() {
    let (store, handle) = declaring();
    let writer = store.shared_writer().unwrap();
    let outcome = writer.write_declared(|txn| {
        txn.insert(&quad_in(GraphNameRef::DefaultGraph, 0))?;
        let _ignored = txn.insert(&quad_in(named(RESERVED), 1));
        Ok::<_, StorageError>("the closure says it is fine")
    });
    assert_eq!(
        refused_graph(&outcome.unwrap_err()).as_deref(),
        Some("<urn:example:reserved>")
    );
    assert_eq!(handle.len().unwrap(), 0);
}

/// Removing a quad changes a graph as surely as adding one, so a remove from a reserved
/// graph is refused too, and the quad stays.
#[test]
fn a_remove_from_an_undeclared_graph_is_refused() {
    let (store, handle) = declaring();
    handle.insert(&quad_in(named(RESERVED), 0)).unwrap();
    let writer = store.shared_writer().unwrap();
    let refused = writer
        .write_declared(|txn| txn.remove(&quad_in(named(RESERVED), 0)))
        .unwrap_err();
    assert!(refused_graph(&refused).is_some(), "{refused}");
    assert!(handle.contains(&quad_in(named(RESERVED), 0)).unwrap());
}

/// A blank-node graph name can never be declared, so it is never writable through a
/// declaring writer; and `extend` stops at the first graph it may not write.
#[test]
fn a_blank_node_graph_is_never_declared_and_extend_stops_at_the_first_refusal() {
    let (store, handle) = declaring();
    let writer = store.shared_writer().unwrap();
    let blank = oxigraph::model::BlankNode::new_unchecked("b0");
    let refused = writer
        .write_declared(move |txn| {
            txn.extend(
                [
                    quad_in(GraphNameRef::DefaultGraph, 0),
                    quad_in(GraphNameRef::BlankNode(blank.as_ref()), 1),
                    quad_in(GraphNameRef::DefaultGraph, 2),
                ]
                .iter(),
            )
        })
        .unwrap_err();
    assert_eq!(refused_graph(&refused).as_deref(), Some("_:b0"));
    assert_eq!(handle.len().unwrap(), 0);
}

/// A store opened with `open_shared` declared nothing, so its writer refuses nothing, and
/// the raw `write` stays open on it.
#[test]
fn an_undeclared_writer_refuses_nothing() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let writer = store.shared_writer().unwrap();
    assert!(writer.declared().is_none());
    writer
        .write_declared(|txn| txn.insert(&quad_in(named(RESERVED), 0)))
        .unwrap();
    writer
        .write(|txn| {
            txn.insert(&quad_in(named(RESERVED), 1));
            Ok::<_, StorageError>(())
        })
        .unwrap();
    assert_eq!(handle.len().unwrap(), 2);
}

/// The raw `write`'s refusal on a declaring writer says what to do instead.
#[test]
fn the_raw_write_refusal_names_the_declaration_and_the_way_out() {
    let (store, _handle) = declaring();
    let writer = store.shared_writer().unwrap();
    let refused = writer
        .write(|_| Ok::<_, StorageError>(()))
        .unwrap_err()
        .to_string();
    assert!(refused.contains("write_declared"), "{refused}");
    assert!(refused.contains("<urn:example:declared>"), "{refused}");
}

// ------------------------------------------------------------------------- over

/// ★ `over` (ledger #1207): a host holding a bare `Store` gets the same type, with a
/// long-lived writer thread of its own that is none of the threads asking, and the same
/// declaration enforcement.
#[test]
fn an_over_writer_commits_on_one_thread_of_its_own_and_keeps_its_declaration() {
    let store = Arc::new(oxigraph::store::Store::new().unwrap());
    let writer = ikigai_store::SharedWriter::over(
        Arc::clone(&store),
        SharerWrites::only_the_default_graph().and_named_graph(DECLARED),
    );
    assert!(Arc::ptr_eq(writer.store(), &store));
    assert!(writer.declared().unwrap().may_write(DECLARED));
    let from_a_fresh_thread = |n: usize| {
        let writer = writer.clone();
        std::thread::spawn(move || {
            let asked = std::thread::current().id();
            let ran = writer
                .write_declared(move |txn| {
                    txn.insert(&quad_in(named(DECLARED), n))?;
                    Ok::<_, StorageError>(std::thread::current().id())
                })
                .unwrap();
            (asked, ran)
        })
        .join()
        .unwrap()
    };
    let (asked, first) = from_a_fresh_thread(0);
    assert_ne!(first, asked, "a write committed on the thread that asked");
    for n in 1..8 {
        let (asked, ran) = from_a_fresh_thread(n);
        assert_ne!(ran, asked, "a write committed on the thread that asked");
        assert_eq!(ran, first, "a write committed on a thread of its own");
    }
    assert_eq!(store.len().unwrap(), 8);

    let refused = writer
        .write_declared(|txn| txn.insert(&quad_in(named(RESERVED), 9)))
        .unwrap_err();
    assert!(refused_graph(&refused).is_some(), "{refused}");
    assert!(
        writer.write(|_| Ok::<_, StorageError>(())).is_err(),
        "an `over` writer always carries a declaration, so its raw `write` is refused"
    );
    assert_eq!(store.len().unwrap(), 8);
}

/// A write from inside an `over` writer's own write is refused, not deadlocked, exactly as
/// on a store's writer.
#[test]
fn a_write_from_inside_an_over_write_is_refused_not_deadlocked() {
    let writer = ikigai_store::SharedWriter::over(
        Arc::new(oxigraph::store::Store::new().unwrap()),
        SharerWrites::only_the_default_graph(),
    );
    let (done, outcome) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let inner = writer.clone();
        let _ = done.send(writer.write_declared(move |_| {
            Ok::<_, StorageError>(
                inner
                    .write_declared(|_| Ok::<_, StorageError>(()))
                    .err()
                    .map(|e| e.to_string()),
            )
        }));
    });
    let nested = outcome
        .recv_timeout(Duration::from_secs(10))
        .expect("a nested write deadlocked")
        .unwrap();
    assert!(
        nested
            .as_deref()
            .is_some_and(|e| e.contains("wait for itself")),
        "{nested:?}"
    );
}
