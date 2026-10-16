//! `DurableStore::reader`: every quad, from a consistent snapshot, for a host that needs to
//! READ the whole dataset (a backup) without taking the shared handle (ledger #1732).
//!
//! What it must not cost is the store's caching. A shared constructor hands out a handle that
//! can write, so the store assumes a writer no kernel sees and stops caching; gonk PR 178 had
//! to take that handle for its backup, and a gonk with no browse face paid for a writer it
//! never had. A reader cannot write, so an owned store stays owned. Each claim is pinned from
//! both sides, as `tests/shared_coverage.rs` does: a read IS cacheable beside a reader and is
//! NOT beside the shared handle.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Expiry, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore, SnapshotReader};
use std::sync::Arc;

const DATA: &str = r#"
<urn:x:a> <urn:x:p> "in the default graph" .
GRAPH <urn:x:g1> { <urn:x:a> <urn:x:p> "in g1" . }
GRAPH <urn:x:g2> { <urn:x:b> <urn:x:p> "in g2" . }
"#;

fn issue(
    kernel: &Kernel,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
) -> ikigai_core::Representation {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec())),
    );
    block_on(kernel.issue(request, &Capability::root())).unwrap()
}

fn seeded(store: DurableStore) -> (Kernel, SnapshotReader) {
    let reader = store.reader();
    let kernel = Kernel::new(Arc::new(space(store)));
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", DATA), ("format", "application/trig")],
    );
    (kernel, reader)
}

/// A broad read, over the whole dataset: the read a shared handle makes bare.
fn broad_select(kernel: &Kernel) -> ikigai_core::Representation {
    issue(
        kernel,
        Verb::Source,
        "urn:iki:store:select",
        &[("query", "SELECT * WHERE { ?s ?p ?o }")],
    )
}

/// ★ The point of the reader: taking it leaves an owned store owned, every read covered and a
/// broad read cacheable. The pair is the shared handle, which makes the same read bare.
#[test]
fn a_reader_costs_an_owned_store_none_of_its_caching() {
    let store = DurableStore::in_memory().unwrap();
    let kept = store.clone();
    let (kernel, reader) = seeded(store);
    assert!(kept.is_sole_writer());
    assert!(kept.read_is_covered(None));
    assert!(matches!(broad_select(&kernel).expiry, Expiry::Never));
    assert_eq!(reader.snapshot_iter().count(), 3);

    let (shared, _handle) = DurableStore::in_memory_shared().unwrap();
    let (kernel, _) = seeded(shared);
    assert!(matches!(broad_select(&kernel).expiry, Expiry::Always));
}

/// Every graph and the default graph: a reader is the HOST's authority, never a tenant's,
/// which is why it is a Rust value and never served.
#[test]
fn a_reader_sees_every_graph_and_the_default_graph() {
    let (_kernel, reader) = seeded(DurableStore::in_memory().unwrap());
    let mut graphs: Vec<String> = reader
        .snapshot_iter()
        .map(|quad| quad.unwrap().graph_name.to_string())
        .collect();
    graphs.sort_unstable();
    assert_eq!(graphs, ["<urn:x:g1>", "<urn:x:g2>", "DEFAULT"]);
}

/// Live, not frozen: an iterator reads the snapshot taken when it was asked for and nothing
/// written after it, and the next one sees that write. (A read-only OPENER is frozen at its
/// open, which is why this crate offers none.)
#[test]
fn each_snapshot_is_consistent_and_the_next_one_sees_later_writes() {
    let store = DurableStore::in_memory().unwrap();
    let kept = store.clone();
    let (kernel, reader) = seeded(store);
    let before = reader.snapshot_iter();
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:update",
        &[("content", "INSERT DATA { <urn:x:c> <urn:x:p> \"later\" }")],
    );
    assert_eq!(before.count(), 3, "a snapshot saw a write made after it");
    assert_eq!(reader.snapshot_iter().count(), 4);
    // The store's own shorthand reads the same dataset.
    assert_eq!(kept.snapshot_iter().count(), 4);
}

/// The same on RocksDB, whose snapshots are the backing's own.
#[cfg(feature = "persistent")]
#[test]
fn on_rocksdb_too_a_reader_keeps_the_store_owned_and_reads_a_snapshot() {
    let path = std::env::temp_dir().join(format!(
        "ikigai-store-reader-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let store = DurableStore::open(&path).unwrap();
    let kept = store.clone();
    let (kernel, reader) = seeded(store);
    assert!(kept.is_sole_writer());
    assert!(matches!(broad_select(&kernel).expiry, Expiry::Never));
    let before = reader.snapshot_iter();
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:update",
        &[("content", "INSERT DATA { <urn:x:c> <urn:x:p> \"later\" }")],
    );
    assert_eq!(before.count(), 3, "a snapshot saw a write made after it");
    assert_eq!(kept.snapshot_iter().count(), 4);
    drop((kernel, reader, kept));
    let _ = std::fs::remove_dir_all(&path);
}
