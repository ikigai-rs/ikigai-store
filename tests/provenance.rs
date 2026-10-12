//! A raw handle a `DurableStore` gave out knows where it came from (ledger #1276).
//!
//! The claim, reproduced here before it was fixed: `SharedWriter::over` on the `Arc<Store>`
//! a shared constructor returned could not tell that handle from a standalone store, so it
//! started a second writer thread and a second write lock beside the store's own. A
//! read-modify-write held open on the store's writer then lost half of a record rewritten
//! through the `over` writer, on RocksDB (ikigai-rs/ikigai-browse PR 94 found it from the
//! browse side: a 15-quad record left as 1 quad). On 0.2.15 five tests below failed: the
//! two "joins" tests (two writer threads), the declaration test (an `over` writer wrote a
//! graph the store promised its sharer would not), the nested-write test (it deadlocked), and
//! the race at the bottom (1 quad left).
//!
//! The fix: the shared constructors record the handle they give out, and `over` on it JOINS
//! the store's writer (its thread, its lock) and enforces both declarations: the store's,
//! which is the promise its cached reads rest on, and the caller's, which is the module's own
//! fence. Neither can widen the other.

use ikigai_store::{DurableStore, SharedWriter, SharerWrites, StorageError, Store};
use oxigraph::model::{GraphNameRef, NamedNodeRef, Quad, QuadRef};
use std::sync::Arc;
use std::thread::ThreadId;

const DECLARED: &str = "urn:example:declared";
const OTHER: &str = "urn:example:other";

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

/// The thread a write through `writer` commits on.
fn commits_on(writer: &SharedWriter) -> ThreadId {
    writer
        .write_declared(|_| Ok::<_, StorageError>(std::thread::current().id()))
        .unwrap()
}

fn refused(writer: &SharedWriter, graph: &str) -> bool {
    let graph = graph.to_owned();
    writer
        .write_declared(move |txn| txn.insert(&quad_in(named(&graph), 0)))
        .is_err()
}

/// ★ THE CLAIM: `over` on the handle `in_memory_shared_declaring` returned made a writer of
/// its own. It joins the store's now.
#[test]
fn over_on_a_declaring_stores_handle_joins_the_stores_writer() {
    let (store, handle) = DurableStore::in_memory_shared_declaring(
        SharerWrites::only_the_default_graph().and_named_graph(DECLARED),
    )
    .unwrap();
    let ours = store.shared_writer().unwrap();
    let theirs = SharedWriter::over(
        Arc::clone(&handle),
        SharerWrites::only_the_default_graph().and_named_graph(DECLARED),
    );
    assert!(Arc::ptr_eq(theirs.store(), &handle));
    assert_eq!(
        commits_on(&theirs),
        commits_on(&ours),
        "`over` on a handle a DurableStore gave out started a second writer thread"
    );
}

/// The same for a store that declared nothing (`in_memory_shared`, as `open_shared`).
#[test]
fn over_on_an_undeclared_shared_stores_handle_joins_the_stores_writer() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let ours = store.shared_writer().unwrap();
    let theirs = SharedWriter::over(handle, SharerWrites::only_the_default_graph());
    assert_eq!(commits_on(&theirs), commits_on(&ours));
}

/// A write from inside the store's write, through a joined `over` writer, would wait for
/// itself: refused, exactly as a nested write through the store's own writer is.
///
/// ★ Before the fix this one DEADLOCKED, in memory: the nested write went to the second
/// writer thread, whose transaction then waited for the store-wide lock the outer write's
/// in-memory transaction held, while the outer write waited for it. On 0.2.15 the test failed
/// on its 10 s timeout, with the two threads still parked behind it.
#[test]
fn a_joined_over_write_from_inside_the_stores_write_is_refused_not_deadlocked() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let ours = store.shared_writer().unwrap();
    let theirs = SharedWriter::over(handle, SharerWrites::only_the_default_graph());
    let (done, outcome) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(ours.write_declared(move |_| {
            Ok::<_, StorageError>(
                theirs
                    .write_declared(|_| Ok::<_, StorageError>(()))
                    .err()
                    .map(|e| e.to_string()),
            )
        }));
    });
    let nested = outcome
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("a nested write deadlocked")
        .unwrap();
    assert!(
        nested
            .as_deref()
            .is_some_and(|e| e.contains("wait for itself")),
        "{nested:?}"
    );
}

/// ★ Both declarations hold. The STORE's is the promise its cached reads rest on, so a
/// caller declaring more than it cannot write the difference; the CALLER's is the module's
/// own fence, so a store declaring more than the module cannot widen it either.
#[test]
fn a_joined_writer_keeps_the_stores_promise_and_the_callers_fence() {
    let (store, handle) = DurableStore::in_memory_shared_declaring(
        SharerWrites::only_the_default_graph().and_named_graph(DECLARED),
    )
    .unwrap();
    let before = store.reserved_graphs_fingerprint().unwrap();

    // The caller asks for more than the store promised: the store's promise wins.
    let wider = SharedWriter::over(
        Arc::clone(&handle),
        SharerWrites::only_the_default_graph()
            .and_named_graph(DECLARED)
            .and_named_graph(OTHER),
    );
    assert!(
        refused(&wider, OTHER),
        "an `over` writer wrote a graph the store promised its sharer would not"
    );
    assert!(!refused(&wider, DECLARED));
    assert_eq!(
        wider.declared().unwrap().named_graphs().collect::<Vec<_>>(),
        vec![DECLARED],
        "a joined writer reports the declaration it enforces"
    );
    assert!(store
        .reserved_graphs_fingerprint()
        .unwrap()
        .changed_since(&before)
        .is_empty());

    // The caller asks for less: the caller's fence wins (the module's release-build backstop).
    let narrower = SharedWriter::over(Arc::clone(&handle), SharerWrites::only_the_default_graph());
    assert!(
        refused(&narrower, DECLARED),
        "a joined writer let a module write past its own declaration"
    );
    assert!(narrower
        .write_declared(|txn| txn.insert(&quad_in(GraphNameRef::DefaultGraph, 1)))
        .is_ok());
}

/// On a store that declared nothing, the caller's declaration is the only one, and is kept.
#[test]
fn a_joined_writer_on_an_undeclared_store_keeps_the_callers_fence() {
    let (_store, handle) = DurableStore::in_memory_shared().unwrap();
    let writer = SharedWriter::over(handle, SharerWrites::only_the_default_graph());
    assert!(refused(&writer, DECLARED));
    assert!(writer.write(|_| Ok::<_, StorageError>(())).is_err());
}

/// A genuinely standalone store still gets a writer of its own, and so does a handle whose
/// `DurableStore` (and every writer it gave out) is gone: nothing is left to share.
#[test]
fn over_on_a_standalone_store_or_an_orphaned_handle_makes_its_own_writer() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let ours = store.shared_writer().unwrap();
    let stores_thread = commits_on(&ours);

    let standalone = SharedWriter::over(
        Arc::new(Store::new().unwrap()),
        SharerWrites::only_the_default_graph(),
    );
    assert_ne!(commits_on(&standalone), stores_thread);

    drop(ours);
    drop(store);
    let orphan = SharedWriter::over(Arc::clone(&handle), SharerWrites::only_the_default_graph());
    orphan
        .write_declared(|txn| txn.insert(&quad_in(GraphNameRef::DefaultGraph, 0)))
        .unwrap();
    assert_eq!(handle.len().unwrap(), 1);
}

/// ⚠ THE LIMIT, pinned so a change is noticed: provenance rides the `Arc`, not the dataset.
/// `Store` is itself a cheap clone of one dataset, so a host that re-wraps a clone in a NEW
/// `Arc` hands out a handle no constructor recorded, and `over` on it makes a second writer
/// again. Nothing in oxigraph's public API identifies a dataset, so this cannot be closed
/// from here. Pass the handle the constructor returned (or `SharedWriter::store`), never a
/// re-wrapped clone.
#[test]
fn a_rewrapped_clone_of_the_handle_carries_no_provenance() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let ours = store.shared_writer().unwrap();
    let rewrapped = Arc::new(Store::clone(&handle));
    let theirs = SharedWriter::over(rewrapped, SharerWrites::only_the_default_graph());
    assert_ne!(commits_on(&theirs), commits_on(&ours));
}

/// ★ THE LOST UPDATE, on RocksDB, where it happens (on the in-memory backend every
/// transaction holds a store-wide lock from start to commit, so it cannot). A read-modify-write
/// is held open on the store's writer: it reads the default graph, waits, then removes what
/// it read. A record is rewritten through an `over` writer in that window.
///
/// On 0.2.15 the rewrite committed inside the window (its own thread, its own lock) and the
/// door's commit then removed the quads the rewrite had kept: 15 quads became 1, which
/// neither serial order produces. Joined, the rewrite waits for the door and lands whole.
#[cfg(feature = "persistent")]
#[test]
fn a_rewrite_through_over_cannot_land_inside_a_stores_read_modify_write() {
    use std::sync::mpsc::{channel, RecvTimeoutError};
    use std::time::Duration;

    let path = std::env::temp_dir().join(format!(
        "ikigai-store-provenance-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let (store, handle) = DurableStore::open_shared(&path).unwrap();
    let ours = store.shared_writer().unwrap();
    let theirs = SharedWriter::over(Arc::clone(&handle), SharerWrites::only_the_default_graph());

    // A 15-quad record: fourteen fields and a body.
    let record = |body: usize| -> Vec<Quad> {
        (0..14)
            .map(|n| quad_in(GraphNameRef::DefaultGraph, n))
            .chain(std::iter::once(quad_in(
                GraphNameRef::DefaultGraph,
                1000 + body,
            )))
            .collect()
    };
    let first = record(0);
    let to_write = first.clone();
    theirs
        .write_declared(move |txn| txn.extend(to_write.iter().map(Quad::as_ref)))
        .unwrap();
    assert_eq!(handle.len().unwrap(), 15);

    // The door: read the graph, signal, hold the window open, remove what it read.
    let (read, was_read) = channel();
    let (go, wait) = channel::<()>();
    let door = std::thread::spawn(move || {
        ours.write_declared(move |txn| {
            let seen = txn
                .quads_for_pattern(None, None, None, Some(GraphNameRef::DefaultGraph))
                .collect::<Result<Vec<_>, _>>()?;
            let _ = read.send(());
            let _ = wait.recv_timeout(Duration::from_secs(10));
            for quad in &seen {
                txn.remove(quad.as_ref())?;
            }
            Ok::<_, StorageError>(())
        })
    });
    was_read.recv().unwrap();

    // The rewrite: remove the old record, insert the edited one.
    let (committed, was_committed) = channel();
    let rewrite = std::thread::spawn(move || {
        let (old, new) = (first, record(1));
        let outcome = theirs.write_declared(move |txn| {
            for quad in &old {
                txn.remove(quad.as_ref())?;
            }
            txn.extend(new.iter().map(Quad::as_ref))
        });
        let _ = committed.send(());
        outcome
    });
    let landed_in_the_window = match was_committed.recv_timeout(Duration::from_millis(300)) {
        Ok(()) => true,
        Err(RecvTimeoutError::Timeout) => false,
        Err(RecvTimeoutError::Disconnected) => panic!("the rewrite thread died"),
    };
    go.send(()).unwrap();
    door.join().unwrap().unwrap();
    rewrite.join().unwrap().unwrap();
    let left = handle.len().unwrap();
    drop(handle);
    drop(store);
    let _ = std::fs::remove_dir_all(&path);

    assert!(
        !landed_in_the_window,
        "the `over` rewrite committed while the store's writer was held (left {left} quads)"
    );
    assert_eq!(left, 15, "a serial order leaves the whole edited record");
}
