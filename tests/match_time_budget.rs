//! SPARQL a consumer evaluates ITSELF gets a time budget too (ledger #1327).
//!
//! The claim (the ikigai-layers audit, ikigai-rs/ikigai-layers PR 10): intray's `rd match=`
//! asks a caller's ASK of every pending tuple's graph through this crate's evaluator
//! ([`service::evaluator`]), bounded only by [`ikigai_store::limits`] (size and nesting, not
//! time), and a cross join of k triple patterns under a FILTER over all their variables grows
//! about 7.5x a pattern on a 7-triple tuple — so a ~300-byte template holds a thread for
//! minutes, per pending tuple.
//!
//! What held, reproduced here:
//!
//! - **The evaluator a consumer builds has no time bound** (true): the template runs to the
//!   end, unrefused, and each pattern multiplies its cost by about the tuple's size.
//! - **Its cancellation token alone does not bound it either** (why the fix is a deadline and
//!   not just a token): oxigraph checks the token only where it touches the dataset, and a
//!   cross product spends its time over rows already in memory.
//! - **The store's own doors were already bounded** (the claim's "graph-select / graph-ask"
//!   half is not a defect): `urn:iki:store:graph-ask` answers this template with a typed
//!   `Timeout` at its budget, since 0.2.7 (ledger #964).
//!
//! The fix is [`EvaluationBudget`]: the doors' deadline, overdue count and cap, handed to a
//! consumer for its own evaluations. intray adopting it is ikigai-cli's change, not this one.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::budget::{check_query, EvaluationBudget, TimeBudget};
use ikigai_store::service::{self, refuse_service};
use ikigai_store::{space, DurableStore};
use oxigraph::io::RdfFormat;
use oxigraph::sparql::{CancellationToken, QueryResults};
use oxigraph::store::Store;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A tuple of 7 triples, the size the audit measured on.
fn tuple_turtle() -> Vec<u8> {
    (0..7)
        .map(|i| format!("<urn:t:s> <urn:t:p{i}> \"v{i}\" .\n"))
        .collect::<String>()
        .into_bytes()
}

fn tuple() -> Store {
    let store = Store::new().unwrap();
    store
        .load_from_slice(RdfFormat::Turtle, &tuple_turtle()[..])
        .unwrap();
    store
}

/// The audit's shape: `k` unconstrained triple patterns (a cross product, |tuple|^k rows) and
/// one FILTER over every variable, so nothing can be pushed down into a pattern and the ASK
/// can never stop early: ~300 bytes at k=7.
fn template(k: usize) -> String {
    let patterns: String = (0..k).map(|i| format!("?s{i} ?p{i} ?o{i} . ")).collect();
    let all: Vec<String> = (0..k)
        .map(|i| format!("STR(?s{i}), STR(?p{i}), STR(?o{i})"))
        .collect();
    format!(
        "ASK {{ {patterns}FILTER(CONCAT({}) = \"never\") }}",
        all.join(", ")
    )
}

fn parse(text: &str) -> spargebra::Query {
    spargebra::SparqlParser::new().parse_query(text).unwrap()
}

/// What intray does today: the store's evaluator, no token, no deadline.
fn bare_ask(store: &Store, text: &str) -> (Result<bool, String>, Duration) {
    let start = Instant::now();
    let answer = service::evaluator()
        .for_query(parse(text))
        .on_store(store)
        .execute()
        .map(|r| matches!(r, QueryResults::Boolean(true)))
        .map_err(|e| e.to_string());
    (answer, start.elapsed())
}

/// A consumer's match, the way `EvaluationBudget`'s docs say to write one.
fn budgeted_match(
    budget: &EvaluationBudget,
    caller: &Capability,
    requested: Option<&str>,
    text: String,
) -> ikigai_core::Result<bool> {
    budget.run(&text.clone(), caller, requested, move |token| {
        let query = spargebra::SparqlParser::new()
            .parse_query(&text)
            .map_err(|e| Error::InvalidArgument {
                name: "match".into(),
                detail: e.to_string(),
            })?;
        check_query(&query, "match")?;
        refuse_service(&query, "match")?;
        let store = tuple();
        match service::evaluator()
            .with_cancellation_token(token.clone())
            .for_query(query)
            .on_store(&store)
            .execute()
            .map_err(|e| Error::Endpoint(format!("match: {e}")))?
        {
            QueryResults::Boolean(b) => Ok(b),
            _ => Ok(false),
        }
    })
}

fn reader() -> Capability {
    Capability::scoped(["urn:cap:space:read"])
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn assert_timeout<T: std::fmt::Debug>(result: ikigai_core::Result<T>, budget_ms: u64) {
    match result {
        Err(Error::Timeout(message)) => assert!(
            message.contains(&format!("{budget_ms} ms")),
            "the refusal names the budget: {message}"
        ),
        other => panic!("expected a Timeout naming {budget_ms} ms, got {other:?}"),
    }
}

/// Wait (bounded) until `count` reads 0, and say how long that took.
fn settles(count: impl Fn() -> usize, within: Duration) -> Duration {
    let start = Instant::now();
    while count() != 0 {
        assert!(
            start.elapsed() < within,
            "an overdue evaluation was still running after {within:?}"
        );
        std::thread::sleep(ms(20));
    }
    start.elapsed()
}

#[test]
fn before_the_bare_evaluator_runs_the_template_to_the_end_whatever_it_costs() {
    let store = tuple();
    let mut times = Vec::new();
    for k in 4..=6 {
        let (answer, took) = bare_ask(&store, &template(k));
        // Not refused: no bound of any kind applies to it.
        assert_eq!(answer, Ok(false), "k={k}");
        eprintln!("k={k} ({} bytes): {took:?}", template(k).len());
        times.push(took);
    }
    // Each pattern multiplies the work by about the tuple's size (7). Asserted loosely, to
    // hold on any machine: the point is that growth is geometric, so a template a few
    // patterns longer than this runs for minutes.
    let growth = times[2].as_secs_f64() / times[1].as_secs_f64();
    eprintln!(
        "growth k=5 -> k=6: {growth:.1}x; so k=10 would take about {:.0} s",
        times[2].as_secs_f64() * growth.powi(4)
    );
    assert!(growth > 3.0, "expected geometric growth, got {growth:.1}x");
}

#[test]
fn a_token_alone_stops_it_late_because_the_cross_product_is_in_memory() {
    let store = tuple();
    let token = CancellationToken::new();
    let fire = token.clone();
    let cancel_at = ms(50);
    let start = Instant::now();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(cancel_at);
        fire.cancel();
    });
    let answer = service::evaluator()
        .with_cancellation_token(token)
        .for_query(parse(&template(7)))
        .on_store(&store)
        .execute()
        .map(|r| matches!(r, QueryResults::Boolean(true)))
        .map_err(|e| e.to_string());
    let took = start.elapsed();
    canceller.join().unwrap();
    eprintln!("k=7, token fired at {cancel_at:?}: {answer:?} after {took:?}");
    // It is stopped, eventually, which is why an overdue evaluation does end: …
    assert!(answer.is_err(), "the token did stop it: {answer:?}");
    // … but not at the token (measured ~0.8 s after a 0.2 s token at k=7 and 6.3 s at k=8,
    // debug), so a token is not a deadline: the caller has to be answered without it.
    assert!(took > cancel_at);
}

#[test]
fn the_store_doors_already_bounded_this_template() {
    const G: &str = "urn:example:tuples";
    let store = DurableStore::in_memory()
        .unwrap()
        .with_time_budget(TimeBudget::new(ms(50)));
    let kernel = Kernel::new(Arc::new(space(store.clone())));
    let seed = format!(
        "INSERT DATA {{ GRAPH <{G}> {{ {} }} }}",
        String::from_utf8(tuple_turtle()).unwrap()
    );
    block_on(
        kernel.issue(
            Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap())
                .with_arg("content", ArgRef::Inline(seed.into_bytes())),
            &Capability::root(),
        ),
    )
    .expect("seeding");
    let start = Instant::now();
    let answer = block_on(
        kernel.issue(
            Request::new(Verb::Source, Iri::parse("urn:iki:store:graph-ask").unwrap())
                .with_arg("query", ArgRef::Inline(template(7).into_bytes()))
                .with_arg("graph", ArgRef::Inline(G.as_bytes().to_vec())),
            &Capability::scoped([ikigai_store::cap_read_graph(G)]),
        ),
    );
    assert_timeout(answer, 50);
    assert!(start.elapsed() < ms(1_050), "{:?}", start.elapsed());
    settles(|| store.overdue_evaluations(), Duration::from_secs(120));
}

#[test]
fn after_a_consumer_is_answered_at_its_budget_and_the_worker_is_counted_until_it_ends() {
    let budget = EvaluationBudget::new(TimeBudget::new(ms(50)));
    let start = Instant::now();
    assert_timeout(budgeted_match(&budget, &reader(), None, template(7)), 50);
    let answered = start.elapsed();
    assert!(answered < ms(1_050), "answered after {answered:?}");
    // The worker outlived its caller (the token reaches it late, above), and is counted.
    assert_eq!(budget.overdue_evaluations(), 1);
    // A clone shares the count: that is what makes one per host cap anything.
    assert_eq!(budget.clone().overdue_evaluations(), 1);
    let ended = settles(|| budget.overdue_evaluations(), Duration::from_secs(120));
    eprintln!("answered after {answered:?}; the worker ended {ended:?} later");
}

#[test]
fn after_at_the_cap_a_new_evaluation_is_refused_at_once_until_one_ends() {
    let budget = EvaluationBudget::new(TimeBudget::new(ms(50)).with_max_overdue(1));
    assert_timeout(budgeted_match(&budget, &reader(), None, template(7)), 50);
    let start = Instant::now();
    match budgeted_match(&budget, &reader(), None, template(2)) {
        Err(Error::Unavailable(message)) => assert!(
            message.contains("the most it allows is 1"),
            "the refusal says why: {message}"
        ),
        other => panic!("expected Unavailable at the cap, got {other:?}"),
    }
    assert!(start.elapsed() < ms(50), "refused at once");
    settles(|| budget.overdue_evaluations(), Duration::from_secs(120));
    assert_eq!(
        budgeted_match(&budget, &reader(), None, template(2)),
        Ok(false)
    );
}

#[test]
fn after_a_legitimate_match_answers_and_a_request_can_only_lower_the_budget() {
    let budget = EvaluationBudget::default();
    assert_eq!(budget.time_budget(), TimeBudget::default());
    let matching = "ASK { ?s <urn:t:p3> \"v3\" }".to_string();
    assert_eq!(
        budgeted_match(&budget, &reader(), None, matching.clone()),
        Ok(true)
    );
    assert_eq!(
        budgeted_match(&budget, &reader(), None, template(3)),
        Ok(false)
    );
    // `requested` lowers the 5 s base to 50 ms …
    assert_timeout(
        budgeted_match(&budget, &reader(), Some("50"), template(7)),
        50,
    );
    // … and is refused, naming the argument, when it is not a positive whole number.
    assert!(matches!(
        budgeted_match(&budget, &reader(), Some("0"), matching),
        Err(Error::InvalidArgument { name, .. }) if name == "budget"
    ));
    settles(|| budget.overdue_evaluations(), Duration::from_secs(120));
}

#[test]
fn after_the_algebra_bounds_still_refuse_before_anything_runs() {
    // A consumer's work runs `check_query` itself, so the planner-quadratic shapes are still
    // refused as `InvalidArgument`, not timed out: 33 operands in one join.
    let budget = EvaluationBudget::default();
    let wide: String = (0..33).map(|i| format!("?s <urn:p{i}> ?o{i} . ")).collect();
    assert!(matches!(
        budgeted_match(&budget, &reader(), None, format!("ASK {{ {wide}}}")),
        Err(Error::InvalidArgument { name, .. }) if name == "match"
    ));
}
