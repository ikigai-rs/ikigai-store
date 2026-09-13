//! The RocksDB facts the handle model rests on, pinned so an upstream change fails a
//! test instead of silently changing what this crate is.
//!
//! `docs/design/handle-model.md` is the argument; this file is its evidence. Everything
//! here needs a real RocksDB directory, so the whole file is behind `persistent` — which
//! is why `ci.yml` must pass `features: persistent`.
#![cfg(feature = "persistent")]

use ikigai_store::{Backing, DurableStore, Store};
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ikigai-store-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// ★ Measurement M2, as an assertion. A second `Store::open` in the SAME process is
/// refused — POSIX advisory locks do not conflict within a process, so this is RocksDB's
/// own in-process registry catching it.
///
/// This is the fact that made "open per request" require a concurrency-1 bulkhead rather
/// than merely benefit from one, and so the fact that decided model A. If it ever stops
/// being true, the design doc's reasoning changes and this test is where that surfaces.
#[test]
fn a_second_open_in_the_same_process_is_refused_and_the_refusal_is_legible() {
    let path = scratch("m2");
    let first = DurableStore::open(&path).unwrap();
    assert_eq!(first.backing(), &Backing::Durable(path.clone()));

    let err = DurableStore::open(&path).unwrap_err();
    assert!(
        matches!(err, ikigai_core::Error::Unavailable(_)),
        "a held directory is a TRANSIENT refusal — the holder may exit: {err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains(&path.display().to_string()),
        "the refusal must name the path an operator has to go look at: {text}"
    );
    assert!(
        text.contains("one writer per directory"),
        "the refusal must say WHY, not just that it failed: {text}"
    );

    drop(first);
    // And dropping really does release it: the lock is held by the handle, not the
    // process, which is the only reason open-per-request was worth costing at all.
    DurableStore::open(&path).unwrap();
    let _ = std::fs::remove_dir_all(&path);
}

/// The whole point of the crate: bytes written through one handle are there after it is
/// gone.
#[test]
fn what_was_written_survives_the_handle() {
    let path = scratch("durable");
    {
        // Written through the raw handle, because this test is about the BACKING, not
        // the module face — tests/endpoints.rs covers the kernel door.
        let (_store, handle) = DurableStore::open_shared(&path).unwrap();
        handle
            .load_from_slice(
                oxigraph::io::RdfFormat::NTriples,
                b"<http://ex/a> <http://ex/p> <http://ex/b> .",
            )
            .unwrap();
        assert_eq!(handle.len().unwrap(), 1);
    }
    let (_store, handle) = DurableStore::open_shared(&path).unwrap();
    assert_eq!(
        handle.len().unwrap(),
        1,
        "a durable store that forgets is an in-memory store with extra steps"
    );
    let _ = std::fs::remove_dir_all(&path);
}

/// A read-only opener beside a live writer succeeds — and is a FROZEN SNAPSHOT. This
/// crate offers no read-only face precisely because of this, and the test exists so the
/// reason is reproducible rather than remembered.
///
/// ⚠ If a future arc is tempted by `open_read_only`, run this first: the reader below
/// never sees the writer's later commit, and nothing anywhere says so.
#[test]
fn a_read_only_opener_is_a_frozen_snapshot() {
    let path = scratch("frozen");
    let (_writer, handle) = DurableStore::open_shared(&path).unwrap();
    handle
        .load_from_slice(
            oxigraph::io::RdfFormat::NTriples,
            b"<http://ex/a> <http://ex/p> <http://ex/b> .",
        )
        .unwrap();
    handle.flush().unwrap();

    let reader = Store::open_read_only(&path).expect("a read-only opener is permitted");
    assert_eq!(reader.len().unwrap(), 1);

    handle
        .load_from_slice(
            oxigraph::io::RdfFormat::NTriples,
            b"<http://ex/c> <http://ex/p> <http://ex/d> .",
        )
        .unwrap();
    handle.flush().unwrap();
    assert_eq!(handle.len().unwrap(), 2);
    assert_eq!(
        reader.len().unwrap(),
        1,
        "if this ever reads 2, open_read_only stopped being a frozen snapshot and the \
         `no read-only face` decision in docs/design/handle-model.md should be revisited"
    );
    let _ = std::fs::remove_dir_all(&path);
}

/// ⚠ Measurement M1, kept runnable and kept OUT of CI: it writes a million quads, which
/// is seconds in release and minutes in the debug profile CI uses.
///
/// `cargo test --release --features persistent -- --ignored --nocapture`
///
/// The numbers it produced on 2026-09-13 (Apple M5 Max, oxigraph 0.5.11) are in
/// `docs/design/handle-model.md`: `Store::open` is ~5 ms and FLAT in population, while
/// `Store::len()` is a full scan (72 ms at 1M quads).
#[test]
#[ignore = "benchmark: writes 1M quads; run explicitly in release"]
fn m1_open_cost_by_population() {
    use std::time::Instant;
    for n in [0usize, 10_000, 100_000, 1_000_000] {
        let path = scratch(&format!("m1-{n}"));
        {
            let (_s, handle) = DurableStore::open_shared(&path).unwrap();
            if n > 0 {
                let mut nt = String::with_capacity(n * 60);
                for i in 0..n {
                    nt.push_str(&format!(
                        "<http://ex/s{i}> <http://ex/p{}> \"value {i}\" .\n",
                        i % 20
                    ));
                }
                handle
                    .load_from_slice(oxigraph::io::RdfFormat::NTriples, nt.as_bytes())
                    .unwrap();
            }
            handle.flush().unwrap();
        }
        for round in 0..3 {
            let t = Instant::now();
            let store = Store::open(&path).unwrap();
            let open = t.elapsed();
            let t2 = Instant::now();
            let len = store.len().unwrap();
            println!(
                "n={n} round={round}: Store::open = {open:?}, len() = {:?} ({len} quads)",
                t2.elapsed()
            );
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}
