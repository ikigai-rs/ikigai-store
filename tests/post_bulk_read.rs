//! A read right after a bulk write answers as fast as the same read on a fresh copy of the
//! same data (ledger #1127).
//!
//! # What happened
//!
//! On `ikigai-gonk`'s CI, a page read right after 750 ledger appends ran past the store's 5 s
//! SPARQL budget; locally the same read took 4.5 s in a debug build against about 0.3 s on a
//! fresh process over the same data. The time was all in RocksDB: every term a query decodes
//! is a `Get` on the `id2str` column family, and each of those walked a MEMTABLE skiplist that
//! had degenerated towards a linked list.
//!
//! **Why it degenerated.** RocksDB draws each skiplist node's height from a THREAD-LOCAL
//! random generator seeded from a hash of the thread's id (`Random::GetTLSInstance`). Until
//! this fix every write evaluation ran on a thread spawned for it (`src/budget.rs`), and a
//! thread spawned after the previous one exited usually gets the SAME id back — the C library
//! reuses the thread's memory — so every transaction drew the same short run of heights. The
//! k-th node of every commit got the same height, the list stopped growing upper levels, and
//! a lookup became a walk of a long top level. The cost grows with the number of writes since
//! the memtable last flushed, and how bad it gets is a matter of which seed the run happened
//! to reuse, which is why the reports varied: replaying gonk's exact 750 transactions on raw
//! oxigraph measured 2.3x to 20x across runs (debug), against 1.0x when the same transactions
//! committed on one thread, and 1.0x on fresh threads that were never reused. Reopening the
//! store flushes the memtable, which is why a fresh hub was fast. The in-memory backing has
//! no skiplist and never showed it.
//!
//! **The fix** (`budget::Writer`): every write evaluation on a store runs on that store's one
//! long-lived writer thread, so every commit draws from one generator that keeps advancing.
//! Writes were already serialized by the store's write lock, so nothing that could run in
//! parallel before is serialized now.
//!
//! # Why the timing test is not flaky, and what it cannot promise
//!
//! The bound is generous on purpose — the post-bulk read may take [`MAX_RATIO`] times the
//! fresh copy's, where the fixed code measures about 1x — and each side is the BEST of
//! several reads in the same process, so a noisy CI runner has to be slow on every one of
//! them to fail it. What it cannot promise is to catch the regression every time: the
//! degeneration depends on which seed a run reuses, and a lucky one stays under the bound.
//! The deterministic half is `budget`'s unit test that every write on a store runs on the
//! same thread (`every_write_on_a_store_runs_on_one_thread`), which fails on every run if the
//! writer is removed. This file measures the symptom; that one pins the mechanism.
//!
//! The measurement, kept so it can be re-run when oxigraph or RocksDB moves:
//!
//! ```text
//! cargo test --features persistent --test post_bulk_read -- --ignored --nocapture
//! WRITES=750 DOOR=one-graph cargo test --release --features persistent \
//!     --test post_bulk_read -- --ignored --nocapture
//! ```
//!
//! `DOOR` is `own-graph` (each write to its own graph through `urn:iki:store:graph-update`,
//! the default), `one-graph` (every write to ONE graph through the same door, as a ledger does;
//! that door copies the graph per write, so this is quadratic), `update` (the broad door) or
//! `load` (each write a document through `urn:iki:store:load` into its own graph, asked from a
//! thread spawned for it, as a socket door asks: ledger #1209).

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore, Store};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The most a read right after a bulk write may take, as a multiple of the same read on a
/// fresh copy of the same data. The fixed code measures about 1x.
const MAX_RATIO: f64 = 3.0;

/// A query whose cost is decoding terms: every IRI and long literal it touches is a lookup in
/// the store's string table, which is exactly where the degenerated skiplist cost the time.
const DECODE: &str = "SELECT (MAX(STRLEN(STR(?s)) + STRLEN(STR(?p)) + STRLEN(STR(?o))) AS ?m) \
                      WHERE { GRAPH ?g { ?s ?p ?o } }";

fn kernel(store: DurableStore) -> Kernel {
    Kernel::with_meta_renderer(
        Arc::new(space(store)),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

fn issue(kernel: &Kernel, verb: Verb, iri: &str, args: &[(&str, &str)]) -> String {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec())),
    );
    let answer = block_on(kernel.issue(request, &Capability::root()))
        .unwrap_or_else(|e| panic!("{iri}: {e}"));
    String::from_utf8(answer.bytes).unwrap()
}

/// One write the shape of a ledger append: several quads, most of whose terms are long enough
/// to live in the string table.
fn write(kernel: &Kernel, door: &str, i: usize) {
    let graph = match door {
        "one-graph" | "update" => "urn:x:post-bulk:graph".to_string(),
        _ => format!("urn:x:post-bulk:graph:{i}"),
    };
    let quads = format!(
        "GRAPH <{graph}> {{ <urn:x:post-bulk:item:{i:020}> \
         <urn:x:post-bulk:predicate:title> \"the title of item number {i:08}\" ; \
         <urn:x:post-bulk:predicate:status> <urn:x:post-bulk:status:open> ; \
         <urn:x:post-bulk:predicate:number> {i} ; \
         <urn:x:post-bulk:predicate:body> \"a body long enough for the string table {i}\" . }}"
    );
    let update = format!("INSERT DATA {{ {quads} }}");
    if door == "load" {
        // The same four statements as a document, loaded into the write's own graph from a
        // thread spawned for it, the way a socket door asks (ledger #1209).
        let document = format!(
            "<urn:x:post-bulk:item:{i:020}> <urn:x:post-bulk:predicate:title> \
             \"the title of item number {i:08}\" .\n\
             <urn:x:post-bulk:item:{i:020}> <urn:x:post-bulk:predicate:status> \
             <urn:x:post-bulk:status:open> .\n\
             <urn:x:post-bulk:item:{i:020}> <urn:x:post-bulk:predicate:number> \
             \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n\
             <urn:x:post-bulk:item:{i:020}> <urn:x:post-bulk:predicate:body> \
             \"a body long enough for the string table {i}\" .\n"
        );
        std::thread::scope(|scope| {
            scope.spawn(|| {
                issue(
                    kernel,
                    Verb::Sink,
                    "urn:iki:store:load",
                    &[
                        ("content", &document),
                        ("format", "application/n-triples"),
                        ("graph", &graph),
                    ],
                )
            });
        });
        return;
    }
    match door {
        "update" => issue(
            kernel,
            Verb::Sink,
            "urn:iki:store:update",
            &[("content", &update)],
        ),
        _ => issue(
            kernel,
            Verb::Sink,
            "urn:iki:store:graph-update",
            &[("content", &update), ("graph", &graph)],
        ),
    };
}

/// The best of `rounds` reads through the store's own query door. A shared store's reads
/// are never cached, so each one evaluates.
fn best_read(kernel: &Kernel, rounds: usize) -> Duration {
    (0..rounds)
        .map(|_| {
            let started = Instant::now();
            let answer = issue(
                kernel,
                Verb::Source,
                "urn:iki:store:select",
                &[("query", DECODE)],
            );
            assert!(answer.contains("\"m\""), "{answer}");
            started.elapsed()
        })
        .min()
        .unwrap()
}

#[cfg(feature = "persistent")]
fn scratch(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ikigai-store-post-bulk-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

struct Measured {
    writes: Duration,
    post_bulk: Duration,
    fresh: Duration,
}

impl Measured {
    fn ratio(&self) -> f64 {
        self.post_bulk.as_secs_f64() / self.fresh.as_secs_f64()
    }
}

/// `writes` writes through `door`, then the best of `rounds` reads, against the best of
/// `rounds` on a fresh store holding the same quads, loaded in one transaction.
///
/// `open` gives a store and its raw handle: the handle is what makes the copy (and, being
/// shared, what keeps every read uncached).
fn measure(
    open: &dyn Fn(&str) -> (DurableStore, Arc<Store>),
    door: &str,
    writes: usize,
    rounds: usize,
) -> Measured {
    let (store, handle) = open("bulk");
    let bulk = kernel(store);
    let started = Instant::now();
    for i in 0..writes {
        write(&bulk, door, i);
    }
    let writes_took = started.elapsed();
    let post_bulk = best_read(&bulk, rounds);

    let mut quads = Vec::new();
    handle
        .dump_to_writer(oxigraph::io::RdfFormat::NQuads, &mut quads)
        .unwrap();
    let (copy, copy_handle) = open("fresh");
    copy_handle
        .load_from_slice(oxigraph::io::RdfFormat::NQuads, &quads)
        .unwrap();
    let fresh = kernel(copy);
    assert_eq!(copy_handle.len().unwrap(), handle.len().unwrap());
    Measured {
        writes: writes_took,
        post_bulk,
        fresh: best_read(&fresh, rounds),
    }
}

/// ★ THE REGRESSION (ledger #1127): after 4,000 writes, a read answers within [`MAX_RATIO`]
/// times what it takes on a fresh copy of the same data.
#[cfg(feature = "persistent")]
#[test]
fn a_read_after_a_bulk_write_answers_like_a_read_on_a_fresh_copy() {
    let measured = measure(
        &|name| DurableStore::open_shared(scratch(&format!("pin-{name}"))).unwrap(),
        "own-graph",
        4_000,
        3,
    );
    for name in ["bulk", "fresh"] {
        let _ = std::fs::remove_dir_all(scratch(&format!("pin-{name}")));
    }
    assert!(
        measured.ratio() <= MAX_RATIO,
        "a read right after 4,000 writes took {:?}, {:.1}x the {:?} it takes on a fresh copy \
         of the same data (the bound is {MAX_RATIO}x). This is the ledger #1127 shape: see this \
         file's header, and check that every write still runs on the store's writer thread",
        measured.post_bulk,
        measured.ratio(),
        measured.fresh,
    );
}

/// ★ The same bound for `urn:iki:store:load`, each load asked from a thread spawned for it
/// (ledger #1209). Before the load door moved onto the writer thread this measured 4.8x,
/// 11.0x and 14.8x in three debug runs; after, 0.88x to 1.21x in five. 750 loads, not 4,000:
/// each load counts the dataset twice, so the fill is quadratic. The deterministic half is
/// `a_load_asked_from_inside_a_shared_write_is_refused_not_deadlocked` in
/// `tests/shared_writer.rs`, whose refusal only the writer's queue can give.
#[cfg(feature = "persistent")]
#[test]
fn a_read_after_bulk_loads_from_fresh_threads_answers_like_a_read_on_a_fresh_copy() {
    let measured = measure(
        &|name| DurableStore::open_shared(scratch(&format!("load-{name}"))).unwrap(),
        "load",
        750,
        3,
    );
    for name in ["bulk", "fresh"] {
        let _ = std::fs::remove_dir_all(scratch(&format!("load-{name}")));
    }
    assert!(
        measured.ratio() <= MAX_RATIO,
        "a read right after 750 loads took {:?}, {:.1}x the {:?} it takes on a fresh copy of \
         the same data (the bound is {MAX_RATIO}x). Check that `urn:iki:store:load` still \
         commits on the store's writer thread (ledger #1209)",
        measured.post_bulk,
        measured.ratio(),
        measured.fresh,
    );
}

/// The in-memory backing, held to the same bound: it has no skiplist, so it never showed
/// the problem, and this keeps it that way.
#[test]
fn an_in_memory_store_reads_after_a_bulk_write_like_a_fresh_copy() {
    let measured = measure(
        &|_| DurableStore::in_memory_shared().unwrap(),
        "own-graph",
        1_500,
        3,
    );
    assert!(
        measured.ratio() <= MAX_RATIO,
        "in memory: {:?} after the writes against {:?} fresh ({:.1}x)",
        measured.post_bulk,
        measured.fresh,
        measured.ratio(),
    );
}

/// The measurement behind this file's header. `BACKING` is `disk` (the default when the
/// `persistent` feature is on) or `memory`; `WRITES` (default 750), `DOOR` (see the header)
/// and `ROUNDS` (default 3) set the rest.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn measure_a_read_after_a_bulk_write() {
    let var = |name: &str, default: &str| std::env::var(name).unwrap_or(default.to_string());
    let writes: usize = var("WRITES", "750").parse().unwrap();
    let rounds: usize = var("ROUNDS", "3").parse().unwrap();
    let door = var("DOOR", "own-graph");
    let backing = var(
        "BACKING",
        if cfg!(feature = "persistent") {
            "disk"
        } else {
            "memory"
        },
    );
    let measured = match backing.as_str() {
        #[cfg(feature = "persistent")]
        "disk" => {
            let measured = measure(
                &|name| DurableStore::open_shared(scratch(&format!("measure-{name}"))).unwrap(),
                &door,
                writes,
                rounds,
            );
            for name in ["bulk", "fresh"] {
                let _ = std::fs::remove_dir_all(scratch(&format!("measure-{name}")));
            }
            measured
        }
        "memory" => measure(
            &|_| DurableStore::in_memory_shared().unwrap(),
            &door,
            writes,
            rounds,
        ),
        other => panic!("BACKING={other}: `disk` needs --features persistent"),
    };
    println!(
        "{} build, {backing}, {writes} writes through {door} in {:?}: read after them {:?}, on a \
         fresh copy {:?} ({:.2}x)",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        measured.writes,
        measured.post_bulk,
        measured.fresh,
        measured.ratio(),
    );
}
