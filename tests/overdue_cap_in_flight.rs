//! The overdue cap bounds the evaluations IN FLIGHT, not only those already overdue
//! (ledger #1427).
//!
//! The claim, reproduced here before it was fixed: `budget` promised that "while
//! `TimeBudget::max_overdue` evaluations are overdue, every new evaluation is refused … That
//! bounds how many cores one store's callers can pin." But the check read the overdue count
//! BEFORE starting and reserved nothing, and the count rose only when a caller timed out. So
//! every request that arrived before the first timeout passed the check, started, and became
//! overdue with the rest: eight concurrent requests at `max_overdue = 1` left eight
//! evaluations overdue at once (release build, 300 ms budget), bounded by request
//! concurrency, not by the cap.
//!
//! Now a slot is reserved when an evaluation starts and released when its worker ends, so at
//! most `max_overdue` evaluations are in flight, and so at most that many can outlive their
//! callers. A request that finds every slot taken waits for one within its own budget, and is
//! refused at once, as before, while the cap is filled by overdue evaluations.
//!
//! ⚠ The one evaluation that does start is a shape oxigraph cannot stop, and it is left to
//! the end of this test binary, holding one core: that is the cap working.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::budget::TimeBudget;
use ikigai_store::{space, DurableStore, CAP_READ};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A small dense graph: 60 nodes, two `<urn:p>` edges each, so a cross product of a few
/// unconstrained patterns is 120^n rows (as `tests/sparql_time_budget.rs` seeds it).
fn seed(kernel: &Kernel) {
    let mut triples = String::new();
    for i in 0..60usize {
        for j in [i + 1, i * 7 + 3] {
            triples.push_str(&format!("<urn:n{i}> <urn:p> <urn:n{}> . ", j % 60));
        }
    }
    block_on(kernel.issue(
        Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap()).with_arg(
            "content",
            ArgRef::Inline(format!("INSERT DATA {{ {triples} }}").into_bytes()),
        ),
        &Capability::root(),
    ))
    .expect("seeding");
}

/// `COUNT(*)` over a six-way cross product: inside every algebra bound, and oxigraph's
/// aggregate loop never looks at the cancellation token (still running after 30 s, release).
fn unstoppable() -> String {
    let patterns: String = (0..6).map(|i| format!("?s{i} ?p{i} ?o{i} . ")).collect();
    format!("SELECT (COUNT(*) AS ?n) WHERE {{ {patterns}}}")
}

fn select(kernel: &Kernel, query: &str) -> ikigai_core::Result<String> {
    block_on(
        kernel.issue(
            Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
                .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec())),
            &Capability::scoped([CAP_READ]),
        ),
    )
    .map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

/// ★ The auditor's case: eight concurrent unstoppable queries at `max_overdue = 1`. Before
/// the fix all eight started and the overdue count peaked at 8.
#[test]
fn concurrent_requests_cannot_overshoot_the_cap() {
    let store = DurableStore::in_memory()
        .unwrap()
        .with_time_budget(TimeBudget::new(Duration::from_millis(300)).with_max_overdue(1));
    let kernel = Arc::new(Kernel::new(Arc::new(space(store.clone()))));
    seed(&kernel);

    let watched = store.clone();
    let watch = std::thread::spawn(move || {
        let (mut peak, start) = (0, Instant::now());
        while start.elapsed() < Duration::from_secs(3) {
            peak = peak.max(watched.overdue_evaluations());
            std::thread::sleep(Duration::from_millis(2));
        }
        peak
    });
    let n = 8;
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let kernel = Arc::clone(&kernel);
            std::thread::spawn(move || select(&kernel, &unstoppable()))
        })
        .collect();
    let (mut timeouts, mut unavailable) = (0, 0);
    for handle in handles {
        match handle.join().unwrap() {
            Err(Error::Timeout(_)) => timeouts += 1,
            Err(Error::Unavailable(message)) => {
                assert!(
                    message.contains("the most it allows is 1")
                        || message.contains("the most it runs at once is 1"),
                    "the refusal says why: {message}"
                );
                unavailable += 1
            }
            other => panic!("expected a Timeout or Unavailable, got {other:?}"),
        }
    }
    let peak = watch.join().unwrap();
    eprintln!(
        "{n} concurrent: {timeouts} Timeout, {unavailable} Unavailable; peak overdue {peak} \
         (max_overdue 1)"
    );
    assert!(peak <= 1, "the cap was overshot: {peak} overdue at once");
    assert_eq!(timeouts, 1, "only one evaluation started");
    assert_eq!(unavailable, n - 1);
    // A request arriving now is refused at once, with the message it always had.
    let start = Instant::now();
    match select(&kernel, "ASK {}") {
        Err(Error::Unavailable(message)) => {
            assert!(message.contains("still running"), "{message}")
        }
        other => panic!("expected the cap to refuse, got {other:?}"),
    }
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "refused at once"
    );
}

/// The other half of the bargain: requests that are merely CONCURRENT, not overdue, are not
/// refused at the cap. They wait for a slot within their own budgets and are answered.
#[test]
fn concurrent_fast_requests_wait_for_a_slot_and_are_answered() {
    let store = DurableStore::in_memory()
        .unwrap()
        .with_time_budget(TimeBudget::new(Duration::from_secs(20)).with_max_overdue(1));
    let kernel = Arc::new(Kernel::new(Arc::new(space(store.clone()))));
    seed(&kernel);
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let kernel = Arc::clone(&kernel);
            std::thread::spawn(move || {
                select(&kernel, "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }")
            })
        })
        .collect();
    for handle in handles {
        let answer = handle.join().unwrap().expect("a fast query is answered");
        assert!(answer.contains("120"), "{answer}");
    }
    assert_eq!(store.overdue_evaluations(), 0);
}
