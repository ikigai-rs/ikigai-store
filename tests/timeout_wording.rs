//! A timeout's advice names only an argument the caller can actually pass (ledger #1383).
//!
//! The claim (from ikigai-rs/ikigai-markdown PR 12): an [`EvaluationBudget`] consumer's
//! `Error::Timeout` tells its caller that "a request's `budget=` can only lower it", but
//! `budget=` is an argument only this store's own doors accept. No consumer passes a
//! `requested` bound (intray, shacl, layers and markdown all pass `None`), so the sentence
//! names an argument their callers cannot send, and markdown rewrote it in its own words.
//!
//! These pin both halves: the doors' sentence is exactly what it was, and a consumer's names
//! its own argument, or none.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::budget::{EvaluationBudget, TimeBudget};
use ikigai_store::{space, DurableStore, CAP_READ};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

mod common;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Run work that waits until released (so it can only time out) through `budget`, and
/// return the timeout's sentence. The work is released afterwards, so nothing stays overdue.
fn timeout_sentence(budget: &EvaluationBudget) -> String {
    let (release, held) = mpsc::channel::<()>();
    let result = budget.run(
        "ASK {}",
        &Capability::scoped(["urn:cap:space:read"]),
        None,
        move |_| {
            let _ = held.recv();
            Ok(())
        },
    );
    let _ = release.send(());
    // Wait (bounded) for the released worker to leave the overdue count, so the next call
    // through the same budget is not refused as Unavailable at a small `max_overdue`.
    let start = std::time::Instant::now();
    while budget.overdue_evaluations() != 0 {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "worker never ended"
        );
        std::thread::sleep(ms(5));
    }
    match result {
        Err(Error::Timeout(message)) => message,
        other => panic!("expected a Timeout, got {other:?}"),
    }
}

#[test]
fn a_consumers_timeout_names_no_budget_argument_by_default() {
    let message = timeout_sentence(&EvaluationBudget::new(TimeBudget::new(ms(50))));
    assert!(message.contains("50 ms"), "{message}");
    assert!(
        !message.contains("budget="),
        "a consumer's caller cannot pass the store's `budget=`: {message}"
    );
    assert_eq!(
        message,
        "this SPARQL query ran past its time budget of 50 ms and was stopped (ledger #964). \
         The budget is set by the host. Narrow the query — fewer joins, a bound subject, a \
         LIMIT — or ask the host for a larger budget"
    );
}

#[test]
fn a_consumer_names_its_own_argument() {
    let budget = EvaluationBudget::new(TimeBudget::new(ms(50))).with_argument(Some("match_budget"));
    let message = timeout_sentence(&budget);
    assert_eq!(
        message,
        "this SPARQL query ran past its time budget of 50 ms and was stopped (ledger #964). \
         The budget is set by the host, and a request's `match_budget=` can only lower it. \
         Narrow the query — fewer joins, a bound subject, a LIMIT — or ask the host for a \
         larger budget"
    );
    // And a clone keeps it, as it keeps the overdue count.
    assert_eq!(timeout_sentence(&budget.clone()), message);
    // `None` takes the advice back out.
    let message = timeout_sentence(&budget.with_argument(None));
    assert!(!message.contains("match_budget="), "{message}");
}

#[test]
fn the_doors_keep_their_sentence() {
    // 60 nodes, two edges each, and a read only the clock can stop: a cross product of four
    // patterns is refused for its SIZE first on a fast machine (ledger #1685).
    let triples = common::seed_triples();
    let store = DurableStore::in_memory()
        .unwrap()
        .with_time_budget(TimeBudget::new(common::slow_budget(300)));
    let kernel = Kernel::new(Arc::new(space(store.clone())));
    block_on(kernel.issue(
        Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap()).with_arg(
            "content",
            ArgRef::Inline(format!("INSERT DATA {{ {triples} }}").into_bytes()),
        ),
        &Capability::root(),
    ))
    .expect("seeding");
    let query = common::slow("SELECT ?n");
    let result = block_on(
        kernel.issue(
            Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
                .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec())),
            &Capability::scoped([CAP_READ]),
        ),
    );
    match result {
        Err(Error::Timeout(message)) => assert_eq!(
            message,
            "this SPARQL query ran past its time budget of 300 ms and was stopped (ledger \
             #964). The budget is set by the host, and a request's `budget=` can only lower \
             it. Narrow the query — fewer joins, a bound subject, a LIMIT — or ask the host \
             for a larger budget"
        ),
        Ok(rep) => panic!(
            "expected a Timeout, got {}",
            common::not_slow_enough(rep.bytes.len())
        ),
        other => panic!("expected a Timeout, got {other:?}"),
    }
    // Let the stopped worker end before the store is dropped.
    let start = std::time::Instant::now();
    while store.overdue_evaluations() != 0 && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(ms(5));
    }
}
