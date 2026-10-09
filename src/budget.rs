//! A TIME budget on every SPARQL evaluation (ledger #964).
//!
//! [`crate::limits`] bounds the STACK a query can claim, because a stack overflow aborts the
//! whole host. It says nothing about time, and time is the cheaper attack: on oxigraph
//! 0.5.11, release build, measured 2026-10-09 by `tests/sparql_time_measure.rs`,
//!
//! | query | bytes | time |
//! | --- | --- | --- |
//! | `FILTER(1‖1‖…)`, 10,000 terms | 30 KB | 1.4 s |
//! | the same, 40,000 terms | 120 KB | 30 s |
//! | a 1,000-step property path `:p/:p/…` | 3 KB | 19 s |
//! | a 2,000-step property path | 6 KB | > 60 s, killed |
//! | 250 triple patterns `?s <p> ?o0 . ?s <p> ?o1 …` | 4.7 KB | 13 s |
//! | 500 triple patterns | 9 KB | > 30 s, killed |
//! | a cross product of 6 patterns `?s0 ?p0 ?o0 . …` over 120 quads | 118 B | > 30 s, killed |
//!
//! — all inside [`crate::limits`]' byte and nesting bounds, and all reachable by any caller
//! that may read. So every door that evaluates caller SPARQL runs it through `budget::run`, which
//! gives it a deadline.
//!
//! # What the budget does, exactly
//!
//! 1. **The caller is answered at the budget, never later.** The evaluation runs on its own
//!    thread (the one [`crate::limits::on_sparql_stack`] would size), and the caller waits
//!    for it at most the budget. Past that it gets a typed [`Error::Timeout`] naming the
//!    budget — never a partial answer: a bound refuses, it does not truncate.
//! 2. **The evaluation is told to stop**, through oxigraph's
//!    [`CancellationToken`], and this crate's own
//!    serializers check the same token on every row, so a query whose cost is in a huge
//!    result stops between rows.
//! 3. **An update that ran out of time writes nothing.** Its commit happens under the same
//!    lock the caller takes to give up (`Deadline::settle`), so either the caller is told
//!    it succeeded, or it is told it timed out and the transaction is dropped uncommitted. A
//!    write never lands after its caller was told it did not.
//!
//! # ⚠ What it does NOT do: oxigraph does not always stop
//!
//! **The token is cooperative, and oxigraph checks it only when it reads a quad from the
//! dataset.** Measured, same build: a cross product of 4 patterns stopped 43 ms after the
//! token fired. But the expensive part of most of the shapes above never reads a quad —
//! `sparopt`'s optimizer and the plan builder (the `‖` chain, the path, the long BGP: the
//! time is spent inside `execute()`, before evaluation starts) and the join loops over
//! already-materialized solutions (the cross product) — so there the token is not seen until
//! that phase ends: the 1,000-step path returned "cancelled" 18 s after the token fired, the
//! 40,000-term chain finished 29 s after it, and the 6-way cross product was still running
//! when it was killed at 30 s. **There is no way to stop a Rust thread from outside it**, so
//! for those shapes the core stays busy until oxigraph's phase ends, however long that is.
//!
//! What this crate does about the part it cannot stop:
//!
//! - **The caller is still answered at the budget** (1. above), so an async executor thread
//!   is no longer held for the life of the evaluation, and the caller learns what happened.
//! - **It counts them.** An evaluation still running after its caller gave up is OVERDUE;
//!   [`DurableStore::overdue_evaluations`](crate::DurableStore::overdue_evaluations) says
//!   how many there are right now.
//! - **It caps them.** While [`TimeBudget::max_overdue`] evaluations are overdue, every new
//!   evaluation is refused at once with a transient [`Error::Unavailable`] saying why. That
//!   bounds how many cores one store's callers can pin to that number, at the price of
//!   refusing SPARQL (and only SPARQL) until one finishes. Without it, each request pins one
//!   more core and the host as a whole is what stops answering.
//!
//! The real fix is upstream: `spareval` checking its token in the optimizer, the plan
//! builder, path evaluation and the join loops. Until then this is a bound on how long a
//! CALLER waits and on how many cores runaway work may hold, not on how long one runs.
//!
//! # Who sets the budget: the host, per door, through the capability
//!
//! A store has a [`TimeBudget`] (set with
//! [`DurableStore::with_time_budget`](crate::DurableStore::with_time_budget)): a **base**
//! every caller gets, and a **ceiling**. A capability holding
//! `urn:cap:store:budget:<milliseconds>` ([`cap_budget`]) gets the largest such grant, up
//! to the ceiling; **root** gets the ceiling.
//!
//! Why the capability and not an argument or a per-space option: the capability is the one
//! thing a host already stamps per door that a caller cannot forge — and it follows a
//! request down its sub-requests, attenuated, so a handler running a store query on its
//! caller's behalf runs it on its caller's budget. A per-space option could not tell two
//! doors on one kernel apart, and two spaces over one store forfeit caching (see
//! [`DurableStore::spaces_bound`](crate::DurableStore::spaces_bound)).
//!
//! ★ **It is monotone in grants, so a caller cannot raise its own budget.** Attenuating a
//! capability can only REMOVE grants, and removing a budget grant can only lower the budget
//! towards the base. That is why the grants raise and nothing lowers: a "you get less"
//! grant would be one a caller could drop. So an anonymous door gets a small budget by the
//! host setting a small BASE and giving its trusted doors a grant (or root).

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ikigai_core::{Capability, Error, Result};
use oxigraph::sparql::CancellationToken;

/// The budget every caller gets unless the host says otherwise: **10 seconds**.
///
/// The evidence, measured 2026-10-09 (release build, in-memory store) over a copy of gonk's
/// live dataset — 361,607 quads, the heaviest store in the ecosystem: its BACKUP query (every
/// quad in every graph, `ORDER BY ?g ?s ?p ?o`, serialized to 121 MB of JSON) takes 0.44 s;
/// a whole-graph read of the 348,232-quad browse graph 0.30 s; the ledger's own bulk reads
/// (a 43 KB `VALUES` of all 902 items) 11–21 ms. The slowest legitimate query is therefore
/// about 20 times inside this, which leaves room for a RocksDB backing, a slower machine and
/// a dataset several times larger — while the attack shapes in the module docs run for
/// minutes. A host whose real queries are slower raises it; one serving anonymous callers
/// lowers it for them (see the module docs).
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(10);

/// The most any caller may get, and what root gets, unless the host says otherwise:
/// **120 seconds** — twelve times the base, for an owner's whole-dataset work as the
/// dataset grows.
pub const DEFAULT_CEILING: Duration = Duration::from_secs(120);

/// The family of budget grants: `urn:cap:store:budget:<milliseconds>`. Not a requirement of
/// any endpoint — holding none is the ordinary case, and gets the base.
pub const CAP_BUDGET: &str = "urn:cap:store:budget:*";

/// The grant that lets an evaluation run up to `millis` milliseconds (clamped to the
/// store's ceiling).
///
/// ```
/// assert_eq!(ikigai_store::budget::cap_budget(30_000), "urn:cap:store:budget:30000");
/// ```
pub fn cap_budget(millis: u64) -> String {
    format!("{}{millis}", CAP_BUDGET.trim_end_matches('*'))
}

/// How long a store's SPARQL evaluations may run, and how many may still be running after
/// their callers gave up. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeBudget {
    base: Duration,
    ceiling: Duration,
    max_overdue: usize,
}

impl Default for TimeBudget {
    /// [`DEFAULT_BUDGET`], [`DEFAULT_CEILING`], and [`default_max_overdue`].
    fn default() -> Self {
        TimeBudget {
            base: DEFAULT_BUDGET,
            ceiling: DEFAULT_CEILING,
            max_overdue: default_max_overdue(),
        }
    }
}

/// A quarter of this machine's available parallelism, and at least one: how many cores the
/// runaway evaluations of one store may hold before it refuses new ones.
pub fn default_max_overdue() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 4)
        .unwrap_or(1)
        .max(1)
}

impl TimeBudget {
    /// A budget of `base` for every caller; the ceiling is [`DEFAULT_CEILING`] or `base`,
    /// whichever is larger.
    pub fn new(base: Duration) -> Self {
        TimeBudget {
            base,
            ceiling: base.max(DEFAULT_CEILING),
            ..TimeBudget::default()
        }
    }

    /// Set the ceiling: what root gets, and the most a budget grant can lift a caller to. A
    /// ceiling below the base lowers the base to it, so no caller ever gets more than this.
    pub fn with_ceiling(mut self, ceiling: Duration) -> Self {
        self.ceiling = ceiling;
        self.base = self.base.min(ceiling);
        self
    }

    /// How many evaluations may still be running after their callers gave up before the
    /// store refuses new ones (at least 1). See the module docs for what that trades.
    pub fn with_max_overdue(mut self, n: usize) -> Self {
        self.max_overdue = n.max(1);
        self
    }

    /// What every caller gets.
    pub fn base(&self) -> Duration {
        self.base
    }

    /// What root gets, and the most anyone gets.
    pub fn ceiling(&self) -> Duration {
        self.ceiling
    }

    /// The cap on overdue evaluations.
    pub fn max_overdue(&self) -> usize {
        self.max_overdue
    }

    /// The budget a caller holding `capability` gets: the ceiling for root; otherwise the
    /// largest `urn:cap:store:budget:<ms>` grant it holds, never below the base and never
    /// above the ceiling.
    ///
    /// ```
    /// use ikigai_core::Capability;
    /// use ikigai_store::budget::{cap_budget, TimeBudget};
    /// use std::time::Duration;
    ///
    /// let budget = TimeBudget::new(Duration::from_millis(500)).with_ceiling(Duration::from_secs(60));
    /// let anonymous = Capability::scoped(["urn:cap:store:read"]);
    /// let signed_in = Capability::scoped(["urn:cap:store:read".to_string(), cap_budget(5_000)]);
    /// let greedy = Capability::scoped([cap_budget(3_600_000)]);
    /// assert_eq!(budget.for_capability(&anonymous), Duration::from_millis(500));
    /// assert_eq!(budget.for_capability(&signed_in), Duration::from_secs(5));
    /// assert_eq!(budget.for_capability(&greedy), Duration::from_secs(60));
    /// assert_eq!(budget.for_capability(&Capability::root()), Duration::from_secs(60));
    /// ```
    pub fn for_capability(&self, capability: &Capability) -> Duration {
        let Some(scopes) = capability.scopes() else {
            return self.ceiling;
        };
        let prefix = CAP_BUDGET.trim_end_matches('*');
        let granted = scopes
            .iter()
            .filter(|scope| capability.allows(scope))
            .filter_map(|scope| scope.strip_prefix(prefix)?.parse::<u64>().ok())
            .max()
            .map(Duration::from_millis)
            .unwrap_or_default();
        granted.max(self.base).min(self.ceiling)
    }
}

/// Where one evaluation stands, shared by its worker and the caller waiting for it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    /// The worker reached its commit point (or ended) while the caller was still waiting:
    /// the caller takes its answer, whatever the clock says now.
    Settled,
    /// The caller gave up: the worker's answer is discarded and it must not commit.
    Abandoned,
}

/// What an evaluation is given: the token to hand oxigraph, and the commit point.
pub(crate) struct Deadline {
    token: CancellationToken,
    state: Arc<Mutex<State>>,
    budget: Duration,
}

impl Deadline {
    /// The token, for `SparqlEvaluator::with_cancellation_token` and for a serializer to
    /// check between rows.
    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Refuse if the caller has given up: for a loop this crate owns (a serializer) to call
    /// between rows.
    pub(crate) fn check(&self) -> Result<()> {
        if self.token.is_cancelled() {
            return Err(timeout(self.budget, false));
        }
        Ok(())
    }

    /// Run `commit` only if the caller is still waiting, and hold the caller off while it
    /// runs. An update's transaction commits through this, so a write lands exactly when its
    /// caller is told it did.
    pub(crate) fn settle<R>(&self, commit: impl FnOnce() -> Result<R>) -> Result<R> {
        let mut state = lock(&self.state);
        if *state == State::Abandoned {
            return Err(timeout(self.budget, true));
        }
        *state = State::Settled;
        commit()
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    // A panic in `commit` poisons this; the state it guards is a plain enum that is valid
    // either way, so poisoning carries no meaning here.
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn timeout(budget: Duration, write: bool) -> Error {
    Error::Timeout(format!(
        "this SPARQL evaluation ran past its time budget of {} ms and was stopped; nothing \
         was answered{}. The budget is the host's (`urn:cap:store:budget:<ms>` grants raise \
         it, up to the store's ceiling) and a caller cannot raise its own. Narrow the query \
         (fewer patterns, a shorter path or chain, a selective pattern first) or split it",
        budget.as_millis(),
        if write {
            " and nothing was written"
        } else {
            ""
        },
    ))
}

/// Run one evaluation of `text` within `budget`, on a thread sized for it.
///
/// `overdue` is the store's count of evaluations still running after their callers gave up;
/// at `max_overdue` this refuses before starting anything. See the module docs.
pub(crate) fn run<T, F>(
    text: &str,
    budget: Duration,
    overdue: &Arc<AtomicUsize>,
    max_overdue: usize,
    work: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Deadline) -> Result<T> + Send + 'static,
{
    let deadline = Deadline {
        token: CancellationToken::new(),
        state: Arc::new(Mutex::new(State::Running)),
        budget,
    };
    on_its_own_thread(text, overdue, max_overdue, deadline, work)
}

/// On wasm there are no threads: the work runs inline, and so is unbounded in time. The
/// README says so.
#[cfg(target_family = "wasm")]
fn on_its_own_thread<T, F>(
    text: &str,
    overdue: &Arc<AtomicUsize>,
    max_overdue: usize,
    deadline: Deadline,
    work: F,
) -> Result<T>
where
    F: FnOnce(&Deadline) -> Result<T>,
{
    let _ = (text, overdue, max_overdue);
    work(&deadline)
}

#[cfg(not(target_family = "wasm"))]
fn on_its_own_thread<T, F>(
    text: &str,
    overdue: &Arc<AtomicUsize>,
    max_overdue: usize,
    deadline: Deadline,
    work: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Deadline) -> Result<T> + Send + 'static,
{
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::RecvTimeoutError;

    let running = overdue.load(Ordering::SeqCst);
    if running >= max_overdue {
        return Err(Error::Unavailable(format!(
            "{running} SPARQL evaluations on this store are still running after their \
             callers' time budgets ran out (the most it allows is {max_overdue}); oxigraph \
             cannot always stop one partway, so new ones are refused until one finishes, \
             rather than letting each request hold another core. Retry shortly"
        )));
    }
    let budget = deadline.budget;
    let stack = crate::limits::STACK_BASE
        .saturating_add(text.len().saturating_mul(crate::limits::STACK_PER_BYTE));
    let (tx, rx) = std::sync::mpsc::channel();
    let state = Arc::clone(&deadline.state);
    let token = deadline.token.clone();
    let worker_overdue = Arc::clone(overdue);
    std::thread::Builder::new()
        .name("ikigai-store-sparql".to_string())
        .stack_size(stack)
        .spawn(move || {
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&deadline)));
            {
                let mut state = lock(&deadline.state);
                match *state {
                    State::Abandoned => {
                        worker_overdue.fetch_sub(1, Ordering::SeqCst);
                    }
                    _ => *state = State::Settled,
                }
            }
            // The caller may be gone (it gave up); that is the case `Abandoned` counted.
            let _ = tx.send(outcome);
        })
        .map_err(|e| {
            Error::Unavailable(format!(
                "could not start a {} MiB thread to evaluate this SPARQL text on: {e}",
                stack >> 20
            ))
        })?;
    let lost = || Error::Endpoint("internal: a SPARQL worker ended without answering".into());
    let outcome = match rx.recv_timeout(budget) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => {
            {
                let mut state = lock(&state);
                if *state == State::Running {
                    *state = State::Abandoned;
                    overdue.fetch_add(1, Ordering::SeqCst);
                    token.cancel();
                    return Err(timeout(budget, false));
                }
            }
            // Settled: it reached its commit point in time, and its answer is on the way.
            rx.recv().map_err(|_| lost())?
        }
        Err(RecvTimeoutError::Disconnected) => return Err(lost()),
    };
    match outcome {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    fn counter() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    #[test]
    fn grants_raise_the_budget_and_nothing_lowers_it() {
        let budget = TimeBudget::new(Duration::from_millis(200));
        let held = |grants: &[String]| budget.for_capability(&Capability::scoped(grants.to_vec()));
        assert_eq!(held(&[]), Duration::from_millis(200));
        // A grant below the base is not a way to get less: the base is the floor.
        assert_eq!(held(&[cap_budget(50)]), Duration::from_millis(200));
        assert_eq!(
            held(&[cap_budget(1_000), cap_budget(4_000)]),
            Duration::from_secs(4)
        );
        assert_eq!(held(&[cap_budget(u64::MAX)]), DEFAULT_CEILING);
        // Not a number: not a budget grant.
        assert_eq!(
            held(&["urn:cap:store:budget:forever".to_string()]),
            Duration::from_millis(200)
        );
        assert_eq!(budget.for_capability(&Capability::root()), budget.ceiling());
    }

    #[test]
    fn a_ceiling_below_the_base_lowers_the_base() {
        let budget = TimeBudget::new(Duration::from_secs(30)).with_ceiling(Duration::from_secs(5));
        assert_eq!(budget.base(), Duration::from_secs(5));
        assert_eq!(
            budget.for_capability(&Capability::root()),
            Duration::from_secs(5)
        );
        assert_eq!(TimeBudget::default().with_max_overdue(0).max_overdue(), 1);
    }

    #[test]
    fn an_answer_inside_the_budget_is_returned() {
        let overdue = counter();
        let got = run("q", Duration::from_secs(5), &overdue, 1, |_| Ok(7)).unwrap();
        assert_eq!(got, 7);
        assert_eq!(overdue.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn past_the_budget_the_caller_is_answered_with_a_timeout_and_the_work_is_told_to_stop() {
        let overdue = counter();
        let start = Instant::now();
        let err = run("q", Duration::from_millis(100), &overdue, 4, |deadline| {
            // Cooperative work: stops when told.
            while deadline.check().is_ok() {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            matches!(&err, Error::Timeout(m) if m.contains("100 ms")),
            "{err}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        // The worker sees the token and ends; the count goes back to zero.
        let wait = Instant::now();
        while overdue.load(Ordering::SeqCst) != 0 {
            assert!(
                wait.elapsed() < Duration::from_secs(5),
                "worker never ended"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn work_that_cannot_be_stopped_is_counted_and_caps_new_work() {
        let overdue = counter();
        let (release, held) = std::sync::mpsc::channel::<()>();
        // Ignores the token entirely, like oxigraph's optimizer.
        let err = run("q", Duration::from_millis(50), &overdue, 1, move |_| {
            let _ = held.recv();
            Ok(())
        })
        .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)));
        assert_eq!(overdue.load(Ordering::SeqCst), 1);
        let refused = run("q", Duration::from_secs(5), &overdue, 1, |_| Ok(())).unwrap_err();
        assert!(
            matches!(&refused, Error::Unavailable(m) if m.contains("still running")),
            "{refused}"
        );
        release.send(()).unwrap();
        let wait = Instant::now();
        while overdue.load(Ordering::SeqCst) != 0 {
            assert!(wait.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        run("q", Duration::from_secs(5), &overdue, 1, |_| Ok(())).unwrap();
    }

    #[test]
    fn a_commit_after_the_caller_gave_up_does_not_happen() {
        let overdue = counter();
        let committed = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&committed);
        let (done, finished) = std::sync::mpsc::channel::<Result<()>>();
        let err = run(
            "q",
            Duration::from_millis(50),
            &overdue,
            4,
            move |deadline| {
                // Slow work that does not look at the token, then a commit.
                std::thread::sleep(Duration::from_millis(300));
                let outcome = deadline.settle(|| {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                });
                let _ = done.send(outcome.clone());
                outcome
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)));
        let outcome = finished.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(outcome, Err(Error::Timeout(m)) if m.contains("nothing was written")));
        assert_eq!(committed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_panic_in_the_work_reaches_the_caller() {
        let overdue = counter();
        let caught = std::panic::catch_unwind(|| {
            let _ = run(
                "q",
                Duration::from_secs(5),
                &overdue,
                1,
                |_| -> Result<()> { panic!("boom") },
            );
        });
        assert!(caught.is_err());
        assert_eq!(overdue.load(Ordering::SeqCst), 0);
    }
}
