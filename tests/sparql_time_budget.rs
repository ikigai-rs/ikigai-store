//! Every SPARQL evaluation runs within a TIME budget (ledger #964).
//!
//! The claim: the evaluator is superlinear in shapes no byte or nesting bound refuses, so
//! one request could pin a core for minutes. Reproduced on 0.2.7 by
//! `tests/sparql_time_measure.rs` (the numbers are in `src/budget.rs`); here, each shape is
//! run through the real doors with a budget of a fraction of a second, and the tests assert
//! what the budget promises — and, as precisely, what it does not:
//!
//! - the caller is answered with a typed `Timeout` at the budget, never later and never with
//!   a partial answer;
//! - where oxigraph can be stopped, the worker IS stopped, and the store has no overdue
//!   evaluation left a moment later (the core is released), and a second query answers
//!   promptly;
//! - the shapes oxigraph's planner is superlinear in (which no token reaches) are refused
//!   before planning, by the algebra bounds;
//! - where the evaluation still cannot be stopped (an aggregate over a cross product), it is
//!   counted as overdue and, at the cap, new ones are refused;
//! - an update that runs out of time writes nothing — not then, and not later when its
//!   worker finally ends.
//!
//! These use only the doors' public API plus `DurableStore::{with_time_budget,
//! overdue_evaluations}`, so against 0.2.7 they do not compile; the reproduction there is
//! `tests/sparql_time_measure.rs`, which runs oxigraph beneath the doors.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::budget::{cap_budget, TimeBudget};
use ikigai_store::{space, DurableStore, CAP_READ, CAP_WRITE};
use std::sync::Arc;
use std::time::{Duration, Instant};

const G: &str = "urn:example:g";

/// A small dense graph — 60 nodes, two `<urn:p>` edges each — in the default graph and in
/// `G`, so a cross product of a few unconstrained patterns is |120|^n rows.
fn seed() -> String {
    let mut triples = String::new();
    for i in 0..60usize {
        for j in [i + 1, i * 7 + 3] {
            triples.push_str(&format!("<urn:n{i}> <urn:p> <urn:n{}> . ", j % 60));
        }
    }
    format!("INSERT DATA {{ {triples} GRAPH <{G}> {{ {triples} }} }}")
}

fn store(budget: TimeBudget) -> (DurableStore, Kernel) {
    let store = DurableStore::in_memory().unwrap().with_time_budget(budget);
    let kernel = Kernel::new(Arc::new(space(store.clone())));
    block_on(
        kernel.issue(
            Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap())
                .with_arg("content", ArgRef::Inline(seed().into_bytes())),
            &Capability::root(),
        ),
    )
    .expect("seeding");
    (store, kernel)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A reader holding the broad grants and no budget grant: it gets the base.
fn reader() -> Capability {
    Capability::scoped([CAP_READ, CAP_WRITE])
}

fn issue(
    kernel: &Kernel,
    cap: &Capability,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
) -> ikigai_core::Result<String> {
    let mut req = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        req = req.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(req, cap)).map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

fn select(kernel: &Kernel, cap: &Capability, query: &str) -> ikigai_core::Result<String> {
    issue(
        kernel,
        cap,
        Verb::Source,
        "urn:iki:store:select",
        &[("query", query)],
    )
}

/// `n` unconstrained triple patterns: |data|^n rows, in under 100 bytes.
fn cross(n: usize, head: &str) -> String {
    let patterns: String = (0..n).map(|i| format!("?s{i} ?p{i} ?o{i} . ")).collect();
    format!("{head} WHERE {{ {patterns} }}")
}

/// A property path of `n` steps — the shape ledger #964 measured cubic.
fn path(n: usize) -> String {
    format!(
        "PREFIX : <urn:> SELECT * WHERE {{ ?s :p{} ?o }}",
        "/:p".repeat(n)
    )
}

fn assert_timeout(result: ikigai_core::Result<String>, budget_ms: u64) {
    match result {
        Err(Error::Timeout(message)) => assert!(
            message.contains(&format!("{budget_ms} ms")),
            "the refusal names the budget: {message}"
        ),
        other => panic!("expected a Timeout naming {budget_ms} ms, got {other:?}"),
    }
}

/// Wait (bounded) for every overdue evaluation on `store` to end.
fn settles(store: &DurableStore, within: Duration) -> Duration {
    let start = Instant::now();
    while store.overdue_evaluations() != 0 {
        assert!(
            start.elapsed() < within,
            "{} evaluation(s) still overdue after {within:?}",
            store.overdue_evaluations()
        );
        std::thread::sleep(ms(5));
    }
    start.elapsed()
}

fn quads(kernel: &Kernel) -> String {
    let out = select(
        kernel,
        &Capability::root(),
        "SELECT (COUNT(*) AS ?n) WHERE { { GRAPH ?g { ?s ?p ?o } } UNION { ?s ?p ?o } }",
    )
    .unwrap();
    // The JSON row's value, e.g. `"value":"240"`.
    let at = out.find("\"value\":\"").expect("a count") + 9;
    out[at..].split('"').next().unwrap().to_string()
}

#[test]
fn a_query_that_would_run_for_hours_is_refused_at_the_budget_and_its_core_released() {
    let (store, kernel) = store(TimeBudget::new(ms(300)));
    // 120^4 = 207 million rows, 75 bytes of query.
    let start = Instant::now();
    assert_timeout(select(&kernel, &reader(), &cross(4, "SELECT *")), 300);
    let waited = start.elapsed();
    assert!(waited >= ms(300), "answered before the budget: {waited:?}");
    assert!(waited < ms(300) + ms(1500), "answered late: {waited:?}");
    // The worker is told to stop and does: nothing is still running a moment later.
    let stopped = settles(&store, Duration::from_secs(5));
    assert!(stopped < Duration::from_secs(2), "{stopped:?}");
    // And the store answers the next query promptly.
    let start = Instant::now();
    let out = select(&kernel, &reader(), "ASK { <urn:n0> <urn:p> <urn:n1> }").unwrap();
    assert!(out.contains("true"), "{out}");
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_construct_is_stopped_between_rows_too() {
    let (store, kernel) = store(TimeBudget::new(ms(300)));
    let construct = cross(4, "CONSTRUCT { ?s0 ?p0 ?o3 }");
    assert_timeout(
        issue(
            &kernel,
            &reader(),
            Verb::Source,
            "urn:iki:store:construct",
            &[("query", &construct)],
        ),
        300,
    );
    settles(&store, Duration::from_secs(5));
}

#[test]
fn the_scoped_doors_are_budgeted_too() {
    let (store, kernel) = store(TimeBudget::new(ms(300)));
    let cap = Capability::scoped([ikigai_store::cap_read_graph(G)]);
    assert_timeout(
        issue(
            &kernel,
            &cap,
            Verb::Source,
            "urn:iki:store:graph-select",
            &[("query", &cross(4, "SELECT *")), ("graph", G)],
        ),
        300,
    );
    settles(&store, Duration::from_secs(5));
}

#[test]
fn a_legitimate_query_answers_under_the_budget() {
    let (_store, kernel) = store(TimeBudget::new(ms(300)));
    let out = select(
        &kernel,
        &reader(),
        "SELECT ?o WHERE { <urn:n0> <urn:p>/<urn:p> ?o } ORDER BY ?o",
    )
    .unwrap();
    assert!(out.contains("urn:n"), "{out}");
}

#[test]
fn a_budget_grant_raises_the_budget_and_root_gets_the_ceiling() {
    let budget = TimeBudget::new(ms(100)).with_ceiling(ms(700));
    let (store, kernel) = store(budget);
    let granted = Capability::scoped([CAP_READ.to_string(), cap_budget(400)]);
    let start = Instant::now();
    assert_timeout(select(&kernel, &granted, &cross(4, "SELECT *")), 400);
    assert!(start.elapsed() >= ms(400));
    settles(&store, Duration::from_secs(5));
    // Attenuating the grant away cannot raise anything: it falls back to the base.
    let attenuated = granted.attenuate([CAP_READ]);
    assert_timeout(select(&kernel, &attenuated, &cross(4, "SELECT *")), 100);
    settles(&store, Duration::from_secs(5));
    assert_timeout(
        select(&kernel, &Capability::root(), &cross(4, "SELECT *")),
        700,
    );
    settles(&store, Duration::from_secs(5));
}

/// The planner shapes ledger #964 measured — a 3 KB property path (19 s of planning), a
/// 120 KB `||` chain (30 s), 250 triple patterns (13 s) — are refused BEFORE planning, by
/// name, in well under a second: oxigraph's planner never looks at the token, so a deadline
/// alone could not have stopped them.
#[test]
fn the_shapes_oxigraph_plans_too_slowly_are_refused_before_planning() {
    let (store, kernel) = store(TimeBudget::new(ms(300)));
    let bgp: String = (0..250).map(|i| format!("?s <urn:p> ?o{i} . ")).collect();
    for (query, bound) in [
        (path(1000), "MAX_JOIN_OPERANDS"),
        (format!("SELECT * WHERE {{ {bgp} }}"), "MAX_JOIN_OPERANDS"),
        (
            format!("SELECT * WHERE {{ FILTER(1{}) }}", "||1".repeat(40_000)),
            "MAX_ALGEBRA_NODES",
        ),
    ] {
        let start = Instant::now();
        match select(&kernel, &reader(), &query) {
            Err(Error::InvalidArgument { name, detail }) => {
                assert_eq!(name, "query");
                assert!(detail.contains(bound), "{detail}");
            }
            other => panic!("expected {bound} to refuse, got {other:?}"),
        }
        assert!(start.elapsed() < ms(300), "{:?}", start.elapsed());
    }
    assert_eq!(store.overdue_evaluations(), 0);
    // The update doors measure their WHERE the same way.
    let update = format!("DELETE {{ ?s ?p ?o }} WHERE {{ {bgp} }}");
    let refused = issue(
        &kernel,
        &reader(),
        Verb::Sink,
        "urn:iki:store:update",
        &[("content", &update)],
    );
    assert!(
        matches!(&refused, Err(Error::InvalidArgument { name, .. }) if name == "content"),
        "{refused:?}"
    );
}

#[test]
fn a_request_budget_can_only_tighten() {
    let (store, kernel) = store(TimeBudget::new(ms(300)));
    let query = cross(4, "SELECT *");
    let with = |budget: &str| {
        issue(
            &kernel,
            &reader(),
            Verb::Source,
            "urn:iki:store:select",
            &[("query", &query), ("budget", budget)],
        )
    };
    assert_timeout(with("100"), 100);
    settles(&store, Duration::from_secs(5));
    // Asking for an hour gets what the capability gets.
    assert_timeout(with("3600000"), 300);
    settles(&store, Duration::from_secs(5));
    match with("1s") {
        Err(Error::InvalidArgument { name, .. }) => assert_eq!(name, "budget"),
        other => panic!("expected `budget` refused, got {other:?}"),
    }
}

/// ⚠ The half the budget cannot do, pinned so it is not mistaken for done: inside the
/// algebra bounds, oxigraph's join and aggregate loops over rows already in memory never
/// look at the token, so `COUNT(*)` over a cross product keeps its core long after the
/// caller was answered. So it is COUNTED, and at the cap new evaluations are refused rather
/// than each taking another core.
#[test]
fn a_shape_oxigraph_cannot_stop_is_counted_and_capped() {
    let (store, kernel) = store(TimeBudget::new(ms(200)).with_max_overdue(1));
    // 120^6 rows counted: still running after 30 s in a release build.
    let start = Instant::now();
    assert_timeout(
        select(&kernel, &reader(), &cross(6, "SELECT (COUNT(*) AS ?n)")),
        200,
    );
    assert!(
        start.elapsed() < ms(200) + ms(1500),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(store.overdue_evaluations(), 1);
    match select(&kernel, &reader(), "ASK { ?s ?p ?o }") {
        Err(Error::Unavailable(message)) => {
            assert!(message.contains("still running"), "{message}")
        }
        other => panic!("expected the cap to refuse, got {other:?}"),
    }
    // The worker is left to finish; this binary's exit ends it.
}

#[test]
fn an_update_that_runs_out_of_time_writes_nothing_then_or_later() {
    let (store, kernel) = store(TimeBudget::new(ms(100)));
    let before = quads(&kernel);
    // Would add `?s0 <urn:copied> ?o0` for every row of a 4-way cross product.
    let update = cross(4, "INSERT { ?s0 <urn:copied> ?o0 }");
    let result = issue(
        &kernel,
        &reader(),
        Verb::Sink,
        "urn:iki:store:update",
        &[("content", &update)],
    );
    match result {
        Err(Error::Timeout(m)) => assert!(m.contains("100 ms"), "{m}"),
        other => panic!("expected a Timeout, got {other:?}"),
    }
    // However its worker ends — cancelled, or finishing its evaluation and finding it was
    // abandoned — it must not commit. Wait for it, then look.
    settles(&store, Duration::from_secs(120));
    assert_eq!(quads(&kernel), before, "a timed-out update wrote");
    // The same update with time to run does write, so the assertion above is not vacuous.
    let small = "INSERT { ?s <urn:copied> ?o } WHERE { ?s <urn:p> ?o }";
    issue(
        &kernel,
        &reader(),
        Verb::Sink,
        "urn:iki:store:update",
        &[("content", small)],
    )
    .unwrap();
    assert_ne!(quads(&kernel), before);
}

#[test]
fn a_scoped_update_that_runs_out_of_time_writes_nothing_then_or_later() {
    let (store, kernel) = store(TimeBudget::new(ms(100)));
    let before = quads(&kernel);
    let cap = Capability::scoped([
        ikigai_store::cap_write_graph(G),
        ikigai_store::cap_read_graph(G),
    ]);
    let update = format!(
        "INSERT {{ GRAPH <{G}> {{ ?s0 <urn:copied> ?o0 }} }} WHERE {{ GRAPH <{G}> {{ {} }} }}",
        (0..4)
            .map(|i| format!("?s{i} ?p{i} ?o{i} . "))
            .collect::<String>()
    );
    let result = issue(
        &kernel,
        &cap,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[("content", &update), ("graph", G)],
    );
    assert!(matches!(result, Err(Error::Timeout(_))), "{result:?}");
    settles(&store, Duration::from_secs(120));
    assert_eq!(quads(&kernel), before, "a timed-out scoped update wrote");
}
