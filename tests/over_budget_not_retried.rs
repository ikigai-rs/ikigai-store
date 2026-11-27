//! ★ Ledger #4504: a read that runs past its time budget is answered `Error::OverBudget`, not
//! `Error::Timeout`.
//!
//! Core classes `Timeout` as Transient, so a client behind ikigai-throttle's `Retry` sent the
//! same heavy query again until its limit, and the `CircuitBreaker` counted every attempt as
//! a failure of the store and opened for every other read on it: the caller saw "gave up
//! after N attempts ... circuit open", as if the store were down when it was up and answering.
//! `OverBudget` is never retried by anything in the chain and no breaker counts it. These
//! tests go through the real overlays, so they fail if either ever treats the refusal as a
//! failure of the store.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, FailureClass, Iri, Kernel, Request, Verb};
use ikigai_store::budget::TimeBudget;
use ikigai_store::{space, DurableStore, CAP_READ};
use ikigai_throttle::{CircuitBreaker, Retry};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod common;

const BUDGET_MS: u64 = 100;

/// A seeded in-memory store whose host budget is `BUDGET_MS`.
fn store() -> DurableStore {
    let store = DurableStore::in_memory()
        .unwrap()
        .with_time_budget(TimeBudget::new(common::slow_budget(BUDGET_MS)));
    let seeding = Kernel::new(Arc::new(space(store.clone())));
    block_on(seeding.issue(
        Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap()).with_arg(
            "content",
            ArgRef::Inline(format!("INSERT DATA {{ {} }}", common::seed_triples()).into_bytes()),
        ),
        &Capability::root(),
    ))
    .expect("seeding");
    store
}

fn select(kernel: &Kernel, query: &str) -> ikigai_core::Result<usize> {
    block_on(
        kernel.issue(
            Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
                .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec())),
            &Capability::scoped([CAP_READ]),
        ),
    )
    .map(|rep| rep.bytes.len())
}

/// Wait (bounded) for stopped readers to leave the overdue count.
fn settles(store: &DurableStore) {
    let start = Instant::now();
    while store.overdue_evaluations() != 0 {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "a reader never ended"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_over_budget(result: ikigai_core::Result<usize>) -> Error {
    match result {
        Err(e @ Error::OverBudget { .. }) => {
            assert_eq!(e.class(), FailureClass::OverBudget);
            assert!(!e.is_transient(), "{e}");
            assert_eq!(e.attempts(), None, "it was tried once: {e}");
            let Error::OverBudget { detail } = &e else {
                unreachable!()
            };
            // The sentence is kept: the remedy first, then the budget in ms.
            assert!(detail.starts_with("Narrow the query"), "{detail}");
            assert!(
                detail.contains(&format!("ran past its time budget of {BUDGET_MS} ms")),
                "{detail}"
            );
            e
        }
        Ok(bytes) => panic!(
            "expected OverBudget, got {}",
            common::not_slow_enough(bytes)
        ),
        Err(other) => panic!("expected OverBudget, got {other:?} ({:?})", other.class()),
    }
}

#[test]
fn a_read_past_its_budget_is_over_budget() {
    let store = store();
    let kernel = Kernel::new(Arc::new(space(store.clone())));
    let err = assert_over_budget(select(&kernel, &common::slow("SELECT ?n")));
    assert_eq!(
        err.to_string(),
        format!(
            "over budget: Narrow the query — fewer joins, a bound subject, a LIMIT — or ask the \
             host for a larger budget. This SPARQL query ran past its time budget of \
             {BUDGET_MS} ms and was stopped. The budget is set by the host, and a request's \
             `budget=` can only lower it."
        )
    );
    settles(&store);
}

#[test]
fn retry_does_not_repeat_it_and_the_breaker_does_not_count_it() {
    let store = store();
    // A client's chain: a Retry of 5 attempts over a breaker that opens at the FIRST
    // transient failure. Had the refusal been transient, the first read would come back
    // `GaveUp`, and the cheap read after it would be refused "circuit open".
    let breaker = CircuitBreaker::new(space(store.clone()), 1, Duration::from_secs(600));
    let chain = Retry::new(breaker, 5).with_backoff(Duration::ZERO, Duration::ZERO);
    let kernel = Kernel::new(Arc::new(chain));

    for _ in 0..3 {
        assert_over_budget(select(&kernel, &common::slow("SELECT ?n")));
        // At most the one evaluation each time: the overlays sent it once.
        assert!(store.overdue_evaluations() <= 1);
        settles(&store);
    }
    // The circuit for the store's door is still closed: a cheap read is answered.
    let answer = select(&kernel, "SELECT ?s WHERE { ?s <urn:p> ?o } LIMIT 1");
    assert!(
        answer.is_ok(),
        "the breaker counted the refusal: {answer:?}"
    );
}
