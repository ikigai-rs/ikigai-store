//! One caller cannot exhaust the whole store's overdue budget (ledger #1002).
//!
//! The claim, reproduced here before it was fixed: the overdue cap (`max_overdue`, a quarter
//! of the cores by default) counted the evaluations in flight across the WHOLE store. A
//! caller that could start evaluations oxigraph cannot cancel (a cross product under an
//! aggregate: the token is only checked where the evaluator touches the dataset) could fill
//! it, and while it was full EVERY evaluation was refused `Unavailable`, the host's own reads
//! and its backup included. On gonk the anonymous HTTP door could do this: its 1 s budget
//! answers the caller, but the work runs on, detached.
//!
//! Reproduced on main at `0bc8b72` (0.2.23): six concurrent anonymous runaways on a store
//! whose cap is 3 left three overdue, and a ROOT `ASK {}` was then refused with "3 SPARQL
//! evaluations on this store are still running … (the most it allows is 3)".
//!
//! ⚠ The runaways this file starts are left to the end of the binary, each holding a core:
//! that is the cap working, as in `tests/overdue_cap_in_flight.rs`.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::budget::{TimeBudget, CAP_RESERVE};
use ikigai_store::{space, DurableStore, CAP_READ};
use std::sync::Arc;
use std::time::Duration;

/// A small dense graph: 60 nodes, two `<urn:p>` edges each, so a cross product of a few
/// unconstrained patterns is 120^n rows (as `tests/overdue_cap_in_flight.rs` seeds it).
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

fn select(kernel: &Kernel, capability: &Capability, query: &str) -> ikigai_core::Result<String> {
    block_on(
        kernel.issue(
            Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
                .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec())),
            capability,
        ),
    )
    .map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

/// What gonk's anonymous HTTP door hands a caller with no session: read, and no principal.
fn anonymous() -> Capability {
    Capability::scoped([CAP_READ])
}

/// A caller signed in as `who`: read, and a principal.
fn signed_in(who: &str) -> Capability {
    Capability::scoped([CAP_READ])
        .with_principal(&format!("urn:example:person:{who}"))
        .unwrap()
}

/// What gonk's scheduled backup fires under: scoped, not root, with no principal, holding the
/// read grant, a budget grant, and now the reserve.
fn backup_job() -> Capability {
    Capability::scoped([
        CAP_READ.to_string(),
        ikigai_store::budget::cap_budget(120_000),
        CAP_RESERVE.to_string(),
    ])
}

fn timeouts(answers: &[Error]) -> usize {
    answers
        .iter()
        .filter(|e| matches!(e, Error::Timeout(_)))
        .count()
}

/// A store whose cap the host set to `cap`, and a kernel over it, seeded.
fn store_with_cap(cap: usize) -> (DurableStore, Arc<Kernel>) {
    let store = DurableStore::in_memory()
        .unwrap()
        .with_time_budget(TimeBudget::new(Duration::from_millis(300)).with_max_overdue(cap));
    let kernel = Arc::new(Kernel::new(Arc::new(space(store.clone()))));
    seed(&kernel);
    (store, kernel)
}

/// Fire `n` runaways at once under `capability` and wait for every caller's answer.
fn runaways(kernel: &Arc<Kernel>, capability: &Capability, n: usize) -> Vec<Error> {
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let (kernel, capability) = (Arc::clone(kernel), capability.clone());
            std::thread::spawn(move || select(&kernel, &capability, &unstoppable()))
        })
        .collect();
    handles
        .into_iter()
        .map(|handle| match handle.join().unwrap() {
            Err(e @ (Error::Timeout(_) | Error::Unavailable(_))) => e,
            other => panic!("expected a Timeout or Unavailable, got {other:?}"),
        })
        .collect()
}

/// ★ The item's case. Before the fix the anonymous caller's runaways filled all three slots
/// and the root read below was refused `Unavailable`.
#[test]
fn one_callers_runaways_do_not_refuse_the_hosts_own_read() {
    let (store, kernel) = store_with_cap(3);
    let answers = runaways(&kernel, &anonymous(), 6);

    let root = select(&kernel, &Capability::root(), "ASK { ?s ?p ?o }");
    assert!(
        matches!(&root, Ok(answer) if answer.contains("true")),
        "the host's own read is answered while one caller's runaways run: {root:?}"
    );

    assert_eq!(
        timeouts(&answers),
        1,
        "only the caller's share (1 of a cap of 3, 1 reserved) started: {answers:?}"
    );
    assert_eq!(store.overdue_evaluations(), 1);
}

/// The reserve is the host's even when every slot its callers share is held: two principals
/// (one share each) fill the pool of 2 (a cap of 3 less 1 reserved), so a third principal is
/// refused, naming the pool, while a scoped job holding `urn:cap:store:reserve` (gonk's
/// backup) and root are both answered. Without the grant the job is an ordinary caller.
#[test]
fn the_reserve_is_the_hosts_while_the_callers_pool_is_full() {
    let (store, kernel) = store_with_cap(3);
    assert_eq!(timeouts(&runaways(&kernel, &signed_in("alice"), 3)), 1);
    assert_eq!(timeouts(&runaways(&kernel, &signed_in("bob"), 3)), 1);
    assert_eq!(store.overdue_evaluations(), 2);

    match select(&kernel, &signed_in("carol"), "ASK { ?s <urn:p> ?o }") {
        Err(Error::Unavailable(message)) => {
            assert!(message.contains("still running"), "{message}");
            assert!(
                message.contains("the most its callers may have together is 2, the cap of 3 less 1 reserved for the host"),
                "{message}"
            );
            assert!(message.contains("The host's own are not."), "{message}");
            assert!(message.contains(CAP_RESERVE), "{message}");
        }
        other => panic!("expected the callers' pool to refuse, got {other:?}"),
    }
    // The same job WITHOUT the reserve grant is one more caller, refused with the pool full.
    let ungranted = Capability::scoped([
        CAP_READ.to_string(),
        ikigai_store::budget::cap_budget(120_000),
    ]);
    assert!(matches!(
        select(&kernel, &ungranted, "ASK { ?s <urn:p> <urn:n1> }"),
        Err(Error::Unavailable(_))
    ));
    let job = select(&kernel, &backup_job(), "ASK { <urn:n0> ?p ?o }");
    assert!(matches!(&job, Ok(a) if a.contains("true")), "{job:?}");
    let root = select(&kernel, &Capability::root(), "ASK { ?s ?p <urn:n2> }");
    assert!(matches!(&root, Ok(a) if a.contains("true")), "{root:?}");
}

/// One caller's share does not hold up another: with a cap of 4 (pool 3, share 1), one
/// principal's runaway leaves room for a second principal to read, and the first principal's
/// next read is refused at once, naming its own share and how a host raises it.
#[test]
fn a_full_share_refuses_its_own_caller_and_names_it() {
    let (_store, kernel) = store_with_cap(4);
    assert_eq!(timeouts(&runaways(&kernel, &signed_in("alice"), 4)), 1);

    let bob = select(&kernel, &signed_in("bob"), "ASK { ?s ?p ?o }");
    assert!(matches!(&bob, Ok(a) if a.contains("true")), "{bob:?}");

    let start = std::time::Instant::now();
    match select(&kernel, &signed_in("alice"), "ASK { ?s ?p ?o }") {
        Err(Error::Unavailable(message)) => {
            assert!(
                start.elapsed() < Duration::from_millis(100),
                "refused at once"
            );
            assert!(
                message.starts_with(
                    "1 SPARQL evaluations by the principal <urn:example:person:alice> on this \
                     store are still running after their time budgets ran out (the most one \
                     caller may have is 1)"
                ),
                "{message}"
            );
            assert!(
                message.contains("Other callers are not held up"),
                "{message}"
            );
            assert!(
                message.contains(
                    "One caller's share of 1 is the default: half of the 3 left to callers"
                ),
                "{message}"
            );
            assert!(
                message.contains("TimeBudget::with_caller_share"),
                "{message}"
            );
            assert!(
                message.contains("The cap of 4 was set by the host"),
                "{message}"
            );
        }
        other => panic!("expected alice's share to refuse, got {other:?}"),
    }
}

/// ★ An anonymous caller is ONE caller: capabilities that name no principal share one share
/// however their scopes differ, so rotating them (as attenuation, a wire's clamp, or gonk's
/// foreign-page filter can) does not buy another share.
#[test]
fn every_capability_without_a_principal_is_one_caller() {
    let (_store, kernel) = store_with_cap(4);
    assert_eq!(timeouts(&runaways(&kernel, &anonymous(), 2)), 1);

    let rotated = Capability::scoped([CAP_READ, "urn:example:door:foreign-page"]);
    match select(&kernel, &rotated, &unstoppable()) {
        Err(Error::Unavailable(message)) => assert!(
            message.contains("by callers that name no principal (one caller together)")
                && message.contains("the most one caller may have is 1"),
            "{message}"
        ),
        other => panic!("expected the one principal-less share to refuse, got {other:?}"),
    }
    // A signed-in caller still has its own share.
    let signed = select(&kernel, &signed_in("alice"), "ASK { ?s ?p ?o }");
    assert!(matches!(&signed, Ok(a) if a.contains("true")), "{signed:?}");
}

/// A host that wants one caller to run more raises its share, still below the cap.
#[test]
fn a_host_set_share_lets_one_caller_run_that_many() {
    let store = DurableStore::in_memory().unwrap().with_time_budget(
        TimeBudget::new(Duration::from_millis(300))
            .with_max_overdue(4)
            .with_caller_share(2),
    );
    let kernel = Arc::new(Kernel::new(Arc::new(space(store.clone()))));
    seed(&kernel);
    assert_eq!(timeouts(&runaways(&kernel, &anonymous(), 4)), 2);
    assert_eq!(store.overdue_evaluations(), 2);
    let root = select(&kernel, &Capability::root(), "ASK { ?s ?p ?o }");
    assert!(matches!(&root, Ok(a) if a.contains("true")), "{root:?}");
}
