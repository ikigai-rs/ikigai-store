//! What a small write through the broad doors costs, as the store grows (ledger #3270).
//!
//! `urn:iki:store:update` and `urn:iki:store:load` once counted every quad in the dataset twice
//! per call, under the write lock, for nothing but a `before -> after` receipt: oxigraph keeps
//! no size, so `len()` is a full scan. A one-triple update cost 1.4 s at 10M quads and was
//! refused at 1B, and a load in pieces was quadratic in the store (the Oxigraph load round,
//! ledger #2684). Now the update door reports no counts, and the load door reports what the
//! document added, found by a point lookup per statement it loads.
//!
//! The pins are generous on purpose (the fixed code measures about 1x): each side is the BEST
//! of several writes, so a slow runner has to be slow on every write of the large store to
//! fail them.
//!
//! The measurement, kept so it can be re-run:
//!
//! ```text
//! QUADS=1000,100000,1000000 cargo test --release --test write_cost -- --ignored --nocapture
//! ```

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore, Store};
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use std::sync::Arc;
use std::time::{Duration, Instant};

const NS: &str = "urn:x:write-cost:";

/// The most the best write to the large store may take, as a multiple of the best to the
/// small one.
const MAX_RATIO: f64 = 3.0;

/// The two stores the pins compare. The counting code measured far past `MAX_RATIO` here.
const SMALL: usize = 1_000;
const LARGE: usize = 200_000;

fn kernel(store: DurableStore) -> Kernel {
    Kernel::with_meta_renderer(
        Arc::new(space(store)),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

fn issue(kernel: &Kernel, iri: &str, args: &[(&str, &str)]) -> String {
    let request = args.iter().fold(
        Request::new(Verb::Sink, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec())),
    );
    let answer = block_on(kernel.issue(request, &Capability::root()))
        .unwrap_or_else(|e| panic!("{iri}: {e}"));
    String::from_utf8(answer.bytes).unwrap()
}

/// `quads` quads in the default graph, written straight to the store in one transaction.
fn preload(handle: &Store, quads: usize) {
    let p = NamedNode::new(format!("{NS}p")).unwrap();
    handle
        .extend((0..quads).map(|i| {
            Quad::new(
                NamedNode::new(format!("{NS}s:{i:020}")).unwrap(),
                p.clone(),
                Literal::from(i as i64),
                GraphName::DefaultGraph,
            )
        }))
        .unwrap();
}

/// How long each of `writes` one-triple writes through `door` to a store preloaded with
/// `quads` quads took, fastest first, checking each answer.
fn writes(door: &str, quads: usize, writes: usize) -> Vec<Duration> {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    preload(&handle, quads);
    let kernel = kernel(store);
    let mut took: Vec<Duration> = (0..writes)
        .map(|n| {
            let triple = format!("<{NS}new:{n}> <{NS}p> \"{n}\" .");
            let started = Instant::now();
            let answer = match door {
                "update" => issue(
                    &kernel,
                    "urn:iki:store:update",
                    &[("content", &format!("INSERT DATA {{ {triple} }}"))],
                ),
                "load" => issue(&kernel, "urn:iki:store:load", &[("content", &triple)]),
                other => unreachable!("{other}"),
            };
            let elapsed = started.elapsed();
            let expected = match door {
                "update" => "updated\n",
                _ => "loaded text/turtle: +1 quads\n",
            };
            assert_eq!(answer, expected, "{door}");
            elapsed
        })
        .collect();
    assert_eq!(handle.len().unwrap(), quads + writes);
    took.sort();
    took
}

fn assert_flat(door: &str) {
    let small = writes(door, SMALL, 9)[0];
    let large = writes(door, LARGE, 9)[0];
    let ratio = large.as_secs_f64() / small.as_secs_f64();
    assert!(
        ratio <= MAX_RATIO,
        "a one-triple write through `urn:iki:store:{door}` to {LARGE} quads took {large:?}, \
         {ratio:.1}x the {small:?} it takes to {SMALL} (the bound is {MAX_RATIO}x). This is the \
         ledger #3270 shape: something on the write path scans the whole store"
    );
}

/// ★ THE REGRESSION (ledger #3270): a one-triple broad update costs the same at 200,000 quads
/// as at 1,000.
#[test]
fn a_small_update_costs_the_same_in_a_large_store() {
    assert_flat("update");
}

/// ★ THE REGRESSION (ledger #3270): so does a one-triple load.
#[test]
fn a_small_load_costs_the_same_in_a_large_store() {
    assert_flat("load");
}

/// The measurement behind ledger #3270. `QUADS` (default `1000,100000`) and `WRITES` (default
/// 9), in memory.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn measure_a_small_write_as_the_store_grows() {
    let var = |name: &str, default: &str| std::env::var(name).unwrap_or(default.to_string());
    let count: usize = var("WRITES", "9").parse().unwrap();
    for quads in var("QUADS", "1000,100000").split(',') {
        let quads: usize = quads.trim().parse().unwrap();
        for door in ["update", "load"] {
            let took = writes(door, quads, count);
            println!(
                "{} build, memory, {quads} quads, {door}: median {:?}, best {:?}, over {count}",
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                },
                took[took.len() / 2],
                took[0],
            );
        }
    }
}
