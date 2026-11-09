//! What a bulk load leaves behind: is the store servable the moment the load returns?
//!
//! The design this measures for is `docs/design/bulk-door.md`. A host that wants speed today
//! takes `DurableStore::open_shared` and calls oxigraph's `BulkLoader` itself. The loader writes
//! one set of sorted files per chunk and ingests them; files whose keys overlap land in level 0,
//! and RocksDB has to compact them after the load has already returned.
//!
//! A chunk is small here so that a load a test can afford makes the file shape of a very large
//! one: oxigraph's chunk is `megabytes * 1000 / threads` quads, so one thread and 10 MB is a
//! 10,000-quad chunk. Subjects are scattered, as they are in a real dump, so that chunks overlap.
//!
//! The measurement, kept so it can be re-run:
//!
//! ```text
//! QUADS=2000000 CHUNK_MB=10 ATOMIC=1 cargo test --release --features persistent \
//!     --test bulk_servable -- --ignored --nocapture
//! ```

#![cfg(feature = "persistent")]

use ikigai_store::{DurableStore, Store};
use oxigraph::model::{GraphName, GraphNameRef, Literal, NamedNode, Quad};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const NS: &str = "urn:x:bulk-servable:";

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// A directory of its own for one test, emptied first.
fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("ikigai-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// The subject of quad `i`, scattered over the key space so that every chunk overlaps the others.
fn subject(i: usize) -> NamedNode {
    let scattered = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    NamedNode::new(format!("{NS}s:{scattered:020}")).unwrap()
}

fn quads(count: usize) -> impl Iterator<Item = Quad> {
    let p = NamedNode::new(format!("{NS}p")).unwrap();
    (0..count).map(move |i| {
        Quad::new(
            subject(i),
            p.clone(),
            Literal::from(i as i64),
            GraphName::DefaultGraph,
        )
    })
}

/// Load through oxigraph's bulk loader, on one thread, in chunks of `chunk_mb` thousand quads.
fn bulk_load(handle: &Store, count: usize, chunk_mb: usize, atomic: bool) -> Duration {
    let started = Instant::now();
    let mut loader = handle
        .bulk_loader()
        .with_num_threads(1)
        .with_max_memory_size_in_megabytes(chunk_mb);
    if !atomic {
        loader = loader.without_atomicity();
    }
    loader.load_quads(quads(count)).unwrap();
    loader.commit().unwrap();
    started.elapsed()
}

fn sst_files(path: &Path) -> usize {
    std::fs::read_dir(path)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "sst")
        })
        .count()
}

/// How long `lookups` point reads by subject take, in all, over quads known to be present.
fn read_time(handle: &Store, count: usize, lookups: usize) -> Duration {
    let started = Instant::now();
    for k in 0..lookups {
        let s = subject(k * (count / lookups).max(1));
        let found = handle
            .quads_for_pattern(Some(s.as_ref().into()), None, None, None)
            .count();
        assert_eq!(found, 1, "a loaded quad was not found");
    }
    started.elapsed()
}

fn one_write(handle: &Store, tag: &str) -> Duration {
    let quad = Quad::new(
        NamedNode::new(format!("{NS}written:{tag}")).unwrap(),
        NamedNode::new(format!("{NS}p")).unwrap(),
        Literal::from(1),
        GraphNameRef::DefaultGraph,
    );
    let started = Instant::now();
    handle.insert(&quad).unwrap();
    started.elapsed()
}

/// The file shape the bulk door is designed around: an atomic load hands RocksDB its chunks all
/// at once, and returns with the compaction of them still to do. Should oxigraph stop doing
/// this, the reasoning in `docs/design/bulk-door.md` has to be read again.
#[test]
fn an_atomic_bulk_load_returns_with_its_files_still_uncompacted() {
    let (count, chunk_mb) = (100_000, 1);
    let chunks = count / (chunk_mb * 1_000);
    let path = directory("bulk-servable-pin");
    let (_store, handle) = DurableStore::open_shared(&path).unwrap();

    bulk_load(&handle, count, chunk_mb, true);
    let at_return = sst_files(&path);

    assert!(
        at_return >= chunks,
        "expected at least one file per chunk ({chunks}) when the load returned, found {at_return}"
    );
    assert_eq!(handle.len().unwrap(), count, "the load lost quads");
    let _ = std::fs::remove_dir_all(&path);
}

#[test]
#[ignore = "a measurement; run with --ignored --nocapture"]
fn measure_what_a_bulk_load_leaves_behind() {
    let count = env_or("QUADS", 2_000_000);
    let chunk_mb = env_or("CHUNK_MB", 10);
    let settle_seconds = env_or("SETTLE_SECONDS", 120) as u64;
    let lookups = 2_000;
    let path = directory("bulk-servable-measure");
    let (_store, handle) = DurableStore::open_shared(&path).unwrap();

    let atomic = env_or("ATOMIC", 1) != 0;
    let load = bulk_load(&handle, count, chunk_mb, atomic);
    let ssts_at_return = sst_files(&path);
    // Side by side: a stall on level-0 files is only there to see in the moments after the load
    // returns, and a write made first would hide the read penalty behind its own wait.
    let (write_at_return, read_at_return) = std::thread::scope(|scope| {
        let write = scope.spawn(|| one_write(&handle, "at-return"));
        let read = read_time(&handle, count, lookups);
        (write.join().unwrap(), read)
    });
    println!(
        "quads {count}, chunk {chunk_mb} MB, atomic {atomic}: load {load:?}, {:.0} quads/s",
        count as f64 / load.as_secs_f64()
    );
    println!("at return: {ssts_at_return} sst files, {lookups} reads {read_at_return:?}, one write {write_at_return:?}");

    // Watch the file count until it has held still for five samples, or the time is up.
    let settle_started = Instant::now();
    let (mut last, mut still) = (sst_files(&path), 0);
    while still < 5 && settle_started.elapsed() < Duration::from_secs(settle_seconds) {
        std::thread::sleep(Duration::from_millis(500));
        let now = sst_files(&path);
        still = if now == last { still + 1 } else { 0 };
        last = now;
    }
    let read_settled = read_time(&handle, count, lookups);
    println!(
        "settled after {:?}: {last} sst files, {lookups} reads {read_settled:?}",
        settle_started.elapsed()
    );

    let optimize_started = Instant::now();
    handle.optimize().unwrap();
    let optimize = optimize_started.elapsed();
    let read_optimized = read_time(&handle, count, lookups);
    let write_optimized = one_write(&handle, "optimized");
    println!(
        "optimize {optimize:?}: {} sst files, {lookups} reads {read_optimized:?}, one write {write_optimized:?}",
        sst_files(&path)
    );
    println!(
        "reads at return / reads optimized: {:.2}x",
        read_at_return.as_secs_f64() / read_optimized.as_secs_f64()
    );
    let _ = std::fs::remove_dir_all(&path);
}
