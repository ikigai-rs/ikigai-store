//! A write refused at its time budget releases the store's writer (ledger #3271).
//!
//! The claim, from the Oxigraph load round (ledger #2684): at 1B quads a broad update refused
//! at its budget kept evaluating after the refusal, on the store's one writer thread and under
//! its write lock, so a small scoped write queued behind it waited out its own budget and was
//! refused too. Here at a size a test can afford: a 4-way cross product under `INSERT`, which
//! oxigraph's join loops do not stop at the cancellation token (`src/budget.rs` section 3),
//! refused at 100 ms, then a one-triple `INSERT DATA` through the scoped door. Against the
//! code before the fix the scoped write waited 5.8 s (debug build) for the refused write.
//!
//! On RocksDB, the backend the claim was measured on. ⚠ The in-memory backend admits one
//! transaction at a time inside oxigraph itself, and the refused write's transaction stays
//! open until its evaluation ends, so there the write behind it still waits.
#![cfg(feature = "persistent")]

#[allow(dead_code)]
mod common;

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::budget::TimeBudget;
use ikigai_store::{space, DurableStore, CAP_READ, CAP_WRITE};
use std::sync::Arc;
use std::time::{Duration, Instant};

const G: &str = "urn:example:g";

fn issue(
    kernel: &Kernel,
    cap: &Capability,
    iri: &str,
    args: &[(&str, &str)],
) -> ikigai_core::Result<String> {
    let mut req = Request::new(Verb::Sink, Iri::parse(iri).unwrap());
    for (name, value) in args {
        req = req.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(req, cap)).map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

fn cross(n: usize, head: &str) -> String {
    let patterns: String = (0..n).map(|i| format!("?s{i} ?p{i} ?o{i} . ")).collect();
    format!("{head} WHERE {{ {patterns} }}")
}

#[test]
fn a_scoped_write_behind_a_refused_one_is_not_refused() {
    let path =
        std::env::temp_dir().join(format!("ikigai-store-refused-write-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    let (store, handle) = DurableStore::open_shared(&path).unwrap();
    handle
        .load_from_slice(
            oxigraph::io::RdfFormat::Turtle,
            common::seed_triples().as_bytes(),
        )
        .expect("seeding");
    let store = store.with_time_budget(
        TimeBudget::new(Duration::from_secs(10))
            .with_max_overdue(4)
            .with_caller_share(3),
    );
    let kernel = Kernel::new(Arc::new(space(store.clone())));

    // The broad write, refused at a lowered budget. Oxigraph cannot stop it there.
    let broad = cross(4, "INSERT { ?s0 <urn:copied> ?o0 }");
    let refused = issue(
        &kernel,
        &Capability::scoped([CAP_READ, CAP_WRITE]),
        "urn:iki:store:update",
        &[("content", &broad), ("budget", "100")],
    );
    assert!(matches!(refused, Err(Error::Timeout(_))), "{refused:?}");
    assert_eq!(store.overdue_evaluations(), 1, "the refused write runs on");

    // A small scoped write right behind it, with the store's 10 s budget.
    let scoped = Capability::scoped([ikigai_store::cap_write_graph(G)]);
    let started = Instant::now();
    let answer = issue(
        &kernel,
        &scoped,
        "urn:iki:store:graph-update",
        &[
            (
                "content",
                &format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:a> <urn:b> <urn:c> }} }}"),
            ),
            ("graph", G),
        ],
    );
    let took = started.elapsed();
    assert!(
        answer.is_ok(),
        "the scoped write was refused after {took:?}: {answer:?}"
    );
    assert!(
        took < Duration::from_secs(5),
        "the scoped write waited {took:?} behind a write already refused"
    );
    let count = handle
        .quads_for_pattern(
            None,
            None,
            None,
            Some(oxigraph::model::NamedNodeRef::new(G).unwrap().into()),
        )
        .count();
    assert_eq!(count, 1, "the scoped write landed");
    drop((kernel, store, handle));
    let _ = std::fs::remove_dir_all(&path);
}
