//! Review-only memory-backend completion probe.

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
fn review_memory_write_eventually_finishes_without_abandoned_commit() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    handle
        .load_from_slice(
            oxigraph::io::RdfFormat::Turtle,
            common::seed_triples().as_bytes(),
        )
        .expect("seeding");
    let store = store.with_time_budget(
        TimeBudget::new(Duration::from_secs(60))
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
    eprintln!("memory follower completed in {took:?}");
    let count = handle
        .quads_for_pattern(
            None,
            None,
            None,
            Some(oxigraph::model::NamedNodeRef::new(G).unwrap().into()),
        )
        .count();
    assert_eq!(count, 1, "the scoped write landed");
    let until = Instant::now() + Duration::from_secs(60);
    while store.overdue_evaluations() != 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(store.overdue_evaluations(), 0, "abandoned evaluation ended");
    assert_eq!(
        handle
            .quads_for_pattern(
                None,
                Some(oxigraph::model::NamedNodeRef::new("urn:copied").unwrap()),
                None,
                None
            )
            .count(),
        0,
        "abandoned transaction did not commit"
    );
    drop((kernel, store, handle));
}
