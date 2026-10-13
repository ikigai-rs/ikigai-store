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
//! that may read. Three layers now stand between a caller and that, at every door that
//! evaluates caller SPARQL (the eight query IRIs and both update IRIs).
//!
//! # 1. The algebra is bounded before oxigraph plans it
//!
//! **oxigraph checks its cancellation token only where the evaluator touches the dataset**
//! (a quad-pattern scan, a named-graph scan, internalizing a term). Its PLANNER touches none:
//! `sparopt`'s greedy join reordering is about cubic in the operands of one join, and an
//! `OPTIONAL` chain or a long `‖` is quadratic. Measured, a 1,000-step path whose token
//! fired at 1 s returned "cancelled" 18 s later, and a 40,000-term `‖` chain finished 29 s
//! after it. So the parsed query is measured first — [`MAX_JOIN_OPERANDS`] in any one join,
//! [`MAX_ALGEBRA_NODES`] in the whole query or update — and refused past either with a typed
//! `InvalidArgument` naming the bound, like the stack bounds. Inside both, the worst plan
//! measured is ~125 ms. ([`check_query`], [`check_update`]; the walk and the constants are
//! kept identical to `ikigai-sparql`'s, from which they come.)
//!
//! # 2. A deadline, and the caller is answered at it
//!
//! - **The caller waits at most the budget.** The evaluation runs on another thread (sized as
//!   [`crate::limits::on_sparql_stack`] sizes one): a query on one spawned for it, an update on
//!   the store's one long-lived writer thread, because a write committed on a freshly spawned
//!   thread degrades RocksDB's memtable (ledger #1127, `Writer`). Past the budget the caller
//!   gets a typed [`Error::Timeout`] naming it — never a partial answer: a bound refuses, it
//!   does not truncate. An update's budget includes any time it waited behind another.
//! - **The evaluation is told to stop** through oxigraph's [`CancellationToken`], and this
//!   crate's serializers check the same token on every row, so a query whose cost is in its
//!   rows stops between two of them.
//! - **An update that ran out of time writes nothing**, then or later. Its commit happens
//!   under the same lock the caller takes to give up (`Deadline::settle`), so either the
//!   caller is told it succeeded, or it is told it timed out and the transaction is dropped
//!   uncommitted. A write never lands after its caller was told it did not.
//!
//! # 3. What still cannot be stopped is counted and capped
//!
//! Inside the algebra bounds, oxigraph's operators that consume their whole input before
//! yielding — an aggregate, `ORDER BY`, the build side of a join, and its cross-product and
//! hash-join loops over rows already in memory — do not check the token either: a 4-way
//! cross product under `COUNT(*)` ran seconds past its cancellation, and a 6-way one was still
//! running when killed at 30 s. **There is no way to stop a Rust thread from outside it.** So:
//!
//! - **The caller is still answered at the budget**, and an async executor thread is not held
//!   for the life of the evaluation.
//! - **It is counted.** An evaluation still running after its caller gave up is OVERDUE;
//!   [`DurableStore::overdue_evaluations`](crate::DurableStore::overdue_evaluations) says how
//!   many there are right now.
//! - **It is capped.** While [`TimeBudget::max_overdue`] evaluations are overdue, every new
//!   evaluation is refused at once with a transient [`Error::Unavailable`] saying why. That
//!   bounds how many cores one store's callers can pin, at the price of refusing SPARQL (and
//!   only SPARQL) until one finishes.
//!
//! The real fix for this layer is upstream: `spareval` checking its token in those loops.
//!
//! **A consumer's own evaluations get all three through [`EvaluationBudget`]** (ledger #1327):
//! intray's `rd match=` asks a caller's ASK of a dataset of its own, never through a door, and
//! until 0.2.18 got the stack bound and nothing about time. Layer 1 is [`check_query`], which
//! the consumer calls itself.
//!
//! # Who sets the budget
//!
//! **The host, and a caller can only tighten it.** A store has a [`TimeBudget`] (set with
//! [`DurableStore::with_time_budget`](crate::DurableStore::with_time_budget)):
//!
//! - a **base** every caller gets;
//! - a capability holding `urn:cap:store:budget:<milliseconds>` ([`cap_budget`]) gets the
//!   largest such grant instead, never above the **ceiling**; **root** gets the ceiling;
//! - and any request may carry `budget=<milliseconds>`, which can only LOWER what its
//!   capability gets ([`effective_budget`], the same rule and wording as `ikigai-sparql`'s).
//!
//! Why the capability: it is the one thing a host already stamps per door that a caller
//! cannot forge, and it follows a request down its sub-requests, attenuated, so a handler
//! querying on its caller's behalf runs on its caller's budget, except an ikigai-script run:
//! its capability is narrowed to the script's authority, so a runner's budget grant never
//! follows it on its own; a script DECLARES a budget at publish, and its run gets the least of
//! that, its publisher's grant at publish, the host's ceiling and the runner's own grant
//! (ledger #1384). A per-space
//! option (`ikigai-sparql`'s `space_with_budget`) cannot tell two doors on ONE kernel apart,
//! and two spaces over one store forfeit caching (see
//! [`DurableStore::spaces_bound`](crate::DurableStore::spaces_bound)) — which is why this
//! crate's answer differs from that one's in where the ceiling comes from.
//!
//! ★ **It is monotone in grants, so a caller cannot raise its own budget.** Attenuating a
//! capability can only REMOVE grants, and removing a budget grant can only lower the budget
//! towards the base; `budget=` takes the smaller. That is why the grants raise and nothing in
//! the capability lowers: a "you get less" grant would be one a caller could drop. So an
//! anonymous door gets a small budget by the host setting a small BASE and giving its trusted
//! doors a grant (or root) — or by the door stamping `budget=`, overwriting the caller's.
//!
//! # 4. The SIZE of the answer is bounded too (ledger #970)
//!
//! A deadline bounds how long, not how much: a 49-byte three-pattern cross product over a
//! 120-quad graph serializes 1,728,000 rows — 567 MB of JSON — in 2.2 s, well inside a 5 s
//! budget, all of it held in memory before the first byte leaves (release build,
//! `tests/answer_size_measure.rs`; it is now refused after ~100 ms). So the answer is bounded
//! as well, by the contract this crate shares with `ikigai-sparql`, whose items it copies
//! verbatim ([`DEFAULT_MAX_ROWS`] through [`CappedWriter`]):
//!
//! - **What is counted:** rows for a SELECT, triples for a CONSTRUCT or DESCRIBE, and the
//!   serialized BYTES for every form. **ASK is exempt** (its answer is one boolean). Counting
//!   happens WHILE serializing: the row past the bound is refused before it is written, and
//!   the serializer writes into a [`CappedWriter`], which refuses the write that would cross
//!   the byte bound. Nothing is collected first and measured after.
//! - **Refuse, never truncate.** Past either bound the request is refused with
//!   [`Error::InvalidArgument`] on `query` ([`too_large`]); no partial body ever leaves.
//! - **The base** every caller gets is [`AnswerBound::DEFAULT`] ([`DEFAULT_MAX_ROWS`],
//!   [`DEFAULT_MAX_BYTES`]); **the ceiling** is [`AnswerBound::CEILING`] ([`CEILING_MAX_ROWS`],
//!   [`CEILING_MAX_BYTES`]), and root gets it. Both are a host's to lower ([`AnswerBudget`],
//!   set with [`DurableStore::with_answer_budget`](crate::DurableStore::with_answer_budget)).
//! - **A grant raises it** — this crate's half, where `ikigai-sparql` takes the bound from the
//!   space — monotone in grants for the reason the time budget is:
//!   `urn:cap:store:answer:<rows>` ([`cap_answer`]) and `urn:cap:store:answer:bytes:<bytes>`
//!   ([`cap_answer_bytes`]), each never above its ceiling. "Ask the host for more", in the
//!   refusal, means one of these.
//! - **A request can only lower it**, with `max_rows=` and `max_bytes=` ([`effective_answer`]),
//!   read with [`inline_bound`] as `budget=` is: present but not inline is refused, never
//!   ignored, because ignoring it would answer under the capability's bound where a host's
//!   door meant to stamp a smaller one.
//!
//! ⚠ **What it does not bound: memory oxigraph spends BEFORE the first row.** An `ORDER BY`,
//! `DISTINCT`, `GROUP BY` or the build side of a join materializes its input inside the
//! evaluator, where this crate cannot count it; that is bounded by time (above), not by this.

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ikigai_core::{ArgRef, Capability, Error, Invocation, Result};
use oxigraph::sparql::CancellationToken;
use spargebra::algebra::{
    AggregateExpression, Expression, GraphPattern, OrderExpression, PropertyPathExpression,
};
use spargebra::{GraphUpdateOperation, Query, Update};
use std::io::Write;

/// The budget every caller gets unless the host says otherwise: **5 seconds**, the same as
/// `ikigai-sparql`'s.
///
/// The evidence, measured 2026-10-09 (release build) over a copy of gonk's live dataset —
/// 361,607 quads, the largest store in the ecosystem — on the RocksDB backing gonk runs: the
/// ledger's bulk reads (a 43 KB `VALUES` of all 902 items) take 44–232 ms, and reading the
/// whole 348,232-quad browse graph 1.4 s. Every query a caller-facing door issues is inside
/// that, so 5 s is 3.5 times the heaviest and over 20 times the ledger's, and ends an attack
/// in seconds instead of minutes.
///
/// ⚠ **Whole-dataset owner work is NOT inside it, and is not meant to be.** gonk's BACKUP
/// query — every quad, `ORDER BY ?g ?s ?p ?o`, 121 MB of JSON — takes **24.6 s** on that
/// RocksDB copy (0.44 s in memory). It runs under a scoped job capability, so it gets the
/// base unless the host grants more: a host running such a job grants it
/// [`cap_budget`] (or runs it as root, which gets the [`DEFAULT_CEILING`]).
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(5);

/// The most any caller may get, and what root gets, unless the host says otherwise:
/// **120 seconds** — about five times gonk's whole-dataset backup on RocksDB today.
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

/// The most operands one join may have after the planner flattens it: **32** — the same
/// number, walk and reasoning as `ikigai-sparql`'s.
///
/// A join's operands are its triple patterns (a sequence path `:a/:b/:c` is parsed into one
/// triple pattern per step) and every nested group, sub-`SELECT` and path pattern joined beside
/// them; an `OPTIONAL`, `UNION` or `MINUS` side is a join of its own. oxigraph's greedy join
/// reordering is about cubic in this number: measured (by the `ikigai-sparql` arc), 32
/// patterns plan in ~4 ms, 64 in ~70 ms, 100 in ~350 ms, 200 in 7 s, and 1,000 path steps in
/// 19 s. The largest join the ecosystem runs has 11 patterns (survey, 2026-10-09). 32 rather
/// than 64 because joins multiply under [`MAX_ALGEBRA_NODES`]: sixteen 63-pattern UNION
/// branches planned in 0.78 s, thirty-two 31-pattern ones in 0.1 s.
pub const MAX_JOIN_OPERANDS: usize = 32;

/// The most algebra nodes a query or update may have: **1024** — as `ikigai-sparql`'s.
///
/// A node is a triple pattern, a path operator, a graph-pattern operator (`OPTIONAL`, `UNION`,
/// `FILTER`, `BIND`, a group, …) or an expression operator (`||`, `=`, a function call, …).
/// Variables, IRIs and literals cost nothing, and neither do `VALUES` rows, `IN (…)` members
/// that are constants, `INSERT DATA`/`DELETE DATA` quads or template triples: those are flat
/// and cost the planner nothing measurable. What this bounds is the shapes the planner is
/// quadratic in: at 4,096 of each, an `OPTIONAL` chain planned in 5 s, an `?o = <x> || …`
/// chain in 3 s, a `BIND` list in 0.5 s. Inside both bounds the worst measured plan is
/// ~125 ms. The largest query the ecosystem runs has about 60 (the work ledger's listing).
pub const MAX_ALGEBRA_NODES: usize = 1024;

/// The budget that applies to one request: what its capability gets (`ceiling` here), or
/// the request's own `budget=` milliseconds when that is SMALLER. A request cannot raise its
/// budget above what its capability gets; a budget that is not a positive whole number of
/// milliseconds is refused, naming the argument. Identical to `ikigai-sparql`'s.
///
/// ```
/// use ikigai_store::budget::effective_budget;
/// use std::time::Duration;
///
/// let ceiling = Duration::from_secs(10);
/// assert_eq!(effective_budget(None, ceiling).unwrap(), ceiling);
/// assert_eq!(effective_budget(Some("250"), ceiling).unwrap(), Duration::from_millis(250));
/// // Asking for more than the ceiling gets the ceiling.
/// assert_eq!(effective_budget(Some("3600000"), ceiling).unwrap(), ceiling);
/// assert!(effective_budget(Some("0"), ceiling).is_err());
/// assert!(effective_budget(Some("1s"), ceiling).is_err());
/// ```
pub fn effective_budget(requested: Option<&str>, ceiling: Duration) -> Result<Duration> {
    let Some(text) = requested else {
        return Ok(ceiling);
    };
    match text.trim().parse::<u64>() {
        Ok(ms) if ms > 0 => Ok(ceiling.min(Duration::from_millis(ms))),
        _ => Err(Error::InvalidArgument {
            name: "budget".to_string(),
            detail: format!(
                "`{text}` is not a time budget: give a positive whole number of milliseconds. \
                 It can only lower the budget this space applies ({} ms), never raise it",
                ceiling.as_millis()
            ),
        }),
    }
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

/// The time budget for SPARQL a CONSUMER evaluates itself, over a dataset of its own rather
/// than through this store's doors (ledger #1327).
///
/// A crate that builds its own oxigraph evaluation (intray's `rd match=`, which asks a
/// caller's ASK of every pending tuple's graph; any crate following [`crate::service`]) gets
/// the stack bound from [`crate::limits`] and nothing about time. The store's doors already run
/// every evaluation within a [`TimeBudget`]; this hands the same machinery out, so a consumer
/// gets the same three promises with no copy of it:
///
/// - **The caller is answered at the budget** with a typed [`Error::Timeout`] naming it, never
///   later and never with a partial answer. `work` runs on a thread of its own, sized for
///   `text` as [`crate::limits::on_sparql_stack`] sizes one, so `run` REPLACES that call.
/// - **The evaluation is told to stop**: `work` gets the oxigraph [`CancellationToken`] to hand
///   `with_cancellation_token`, and a loop of the consumer's own (one evaluation per tuple)
///   checks `is_cancelled()` between steps.
/// - **What still cannot be stopped is counted and capped.** oxigraph checks the token only
///   where it touches the dataset, so a cross product over rows already in memory runs on past
///   it: measured here, an ASK of 8 unconstrained patterns over a 7-triple graph, cancelled
///   at 200 ms, returned 6.3 s later (debug build; `tests/match_time_budget.rs`). There is no
///   way to stop a Rust thread from outside it, so such an evaluation goes on holding a core
///   after its caller was answered. While [`TimeBudget::max_overdue`] are overdue, `run`
///   refuses at once with a transient [`Error::Unavailable`] rather than start another.
///
/// ★ **Make ONE per host (or per space) and clone it**: the clones share the overdue count,
/// which is what caps the cores. One made per request caps nothing.
///
/// The budget a request gets is chosen as the store's doors choose it: the
/// [`TimeBudget`]'s base, raised by a `urn:cap:store:budget:<ms>` grant ([`cap_budget`]) the
/// caller's capability holds, the ceiling for root, and lowered (never raised) by a
/// `requested` milliseconds, which a consumer reads from an argument of its own if it has one
/// ([`effective_budget`]); [`with_argument`](Self::with_argument) names that argument in a
/// timeout's advice, which otherwise names none (ledger #1383).
///
/// ```
/// use ikigai_core::{Capability, Error};
/// use ikigai_store::budget::{check_query, EvaluationBudget, TimeBudget};
/// use ikigai_store::service::{evaluator, refuse_service};
/// use oxigraph::io::RdfFormat;
/// use oxigraph::sparql::QueryResults;
/// use oxigraph::store::Store;
/// use std::time::Duration;
///
/// // Once, when the host builds the space; every request shares it.
/// let budget = EvaluationBudget::new(TimeBudget::new(Duration::from_millis(500)));
///
/// let template = "ASK { ?s <urn:p> \"v\" }".to_string();
/// let tuple = b"<urn:s> <urn:p> \"v\" .".to_vec();
/// let caller = Capability::scoped(["urn:cap:space:read"]);
/// let endpoint = |e: &dyn std::fmt::Display| Error::Endpoint(format!("match: {e}"));
/// let matched = budget.run(&template.clone(), &caller, None, move |token| {
///     // Everything that touches the text happens in here: the parse recurses too.
///     let query = spargebra::SparqlParser::new().parse_query(&template).map_err(|e| {
///         Error::InvalidArgument { name: "match".into(), detail: e.to_string() }
///     })?;
///     check_query(&query, "match")?;
///     refuse_service(&query, "match")?;
///     let store = Store::new().map_err(|e| endpoint(&e))?;
///     store.load_from_slice(RdfFormat::Turtle, &tuple[..]).map_err(|e| endpoint(&e))?;
///     match evaluator()
///         .with_cancellation_token(token.clone())
///         .for_query(query)
///         .on_store(&store)
///         .execute()
///         .map_err(|e| endpoint(&e))?
///     {
///         QueryResults::Boolean(b) => Ok(b),
///         _ => Ok(false),
///     }
/// })?;
/// assert!(matched);
/// assert_eq!(budget.overdue_evaluations(), 0);
/// # Ok::<_, Error>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct EvaluationBudget {
    budget: TimeBudget,
    overdue: Arc<AtomicUsize>,
    /// The consumer's own argument that lowers the budget, for a timeout's advice; `None`
    /// leaves the advice out (ledger #1383).
    argument: Option<Arc<str>>,
}

impl EvaluationBudget {
    /// Evaluations within `budget`. [`EvaluationBudget::default`] is [`TimeBudget::default`]:
    /// the store's own 5 s base and 120 s ceiling.
    pub fn new(budget: TimeBudget) -> Self {
        EvaluationBudget {
            budget,
            overdue: Arc::default(),
            argument: None,
        }
    }

    /// Name the argument of the consumer's OWN that lowers the budget, for the advice in a
    /// timeout's sentence: `Some("match_budget")` tells the caller "a request's
    /// `match_budget=` can only lower it", and `None` (the default) leaves that clause out
    /// (ledger #1383).
    ///
    /// ★ **Name only an argument your caller can actually pass**, the one whose value you
    /// hand [`run`](Self::run) as `requested`. The store's own doors say `budget=` because
    /// they take it; a consumer that does not take one says nothing rather than send its
    /// caller looking for an argument that is refused. Clones keep the name, as they keep the
    /// overdue count.
    ///
    /// ```
    /// use ikigai_core::{Capability, Error};
    /// use ikigai_store::budget::{EvaluationBudget, TimeBudget};
    /// use std::time::Duration;
    ///
    /// let timeout = |budget: &EvaluationBudget| {
    ///     let (release, held) = std::sync::mpsc::channel::<()>();
    ///     let caller = Capability::scoped(["urn:cap:space:read"]);
    ///     // Work that waits to be released, so it can only time out.
    ///     let result = budget.run("ASK {}", &caller, None, move |_| {
    ///         let _ = held.recv();
    ///         Ok(())
    ///     });
    ///     let _ = release.send(());
    ///     match result {
    ///         Err(Error::Timeout(sentence)) => sentence,
    ///         other => panic!("expected a timeout, got {other:?}"),
    ///     }
    /// };
    ///
    /// let budget = EvaluationBudget::new(TimeBudget::new(Duration::from_millis(50)));
    /// // By default the sentence names no argument: this consumer takes none.
    /// assert!(timeout(&budget).ends_with(
    ///     "The budget is set by the host. Narrow the query — fewer joins, a bound subject, \
    ///      a LIMIT — or ask the host for a larger budget"
    /// ));
    /// // A consumer that reads `requested` from its own `match_budget=` says so.
    /// let budget = budget.with_argument(Some("match_budget"));
    /// assert!(timeout(&budget)
    ///     .contains("The budget is set by the host, and a request's `match_budget=` can only lower it."));
    /// ```
    #[must_use]
    pub fn with_argument(mut self, argument: Option<&str>) -> Self {
        self.argument = argument.map(Arc::from);
        self
    }

    /// The time budget these evaluations run under.
    pub fn time_budget(&self) -> TimeBudget {
        self.budget
    }

    /// How many evaluations run through this (or a clone of it) are still running after their
    /// callers were answered with a timeout, right now. See [`EvaluationBudget`].
    pub fn overdue_evaluations(&self) -> usize {
        self.overdue.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Run `work`, the evaluation of the SPARQL `text`, for a caller holding `capability`,
    /// within the budget that capability gets, or `requested` milliseconds when that is
    /// smaller (a `requested` that is not a positive whole number is refused on `budget`).
    ///
    /// `text` sizes the thread's stack and nothing else: parse it inside `work`. `work` must
    /// own what it uses (`'static`), because its thread may outlive this call.
    ///
    /// Refused with [`Error::Timeout`] naming the budget when it runs past it, and with
    /// [`Error::Unavailable`] at once while [`TimeBudget::max_overdue`] are overdue. On wasm
    /// there are no threads and `work` runs inline, unbounded in time.
    pub fn run<T, F>(
        &self,
        text: &str,
        capability: &Capability,
        requested: Option<&str>,
        work: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&CancellationToken) -> Result<T> + Send + 'static,
    {
        let budget = effective_budget(requested, self.budget.for_capability(capability))?;
        let deadline = Deadline {
            token: CancellationToken::new(),
            state: Arc::new(Mutex::new(State::Running)),
            budget,
            write: false,
            argument: self.argument.clone(),
        };
        on_its_own_thread(
            text,
            &self.overdue,
            self.budget.max_overdue(),
            None,
            deadline,
            move |deadline| work(&deadline.token),
        )
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
    /// Whether this evaluation is an update, for the refusal's wording.
    write: bool,
    /// The argument the refusal tells its caller lowers the budget, if the caller has one
    /// (ledger #1383): `budget` at this store's doors, the consumer's own name (or none)
    /// through an [`EvaluationBudget`].
    argument: Option<Arc<str>>,
}

impl Deadline {
    /// A deadline nobody gives up on, for a unit test that calls an evaluation step directly.
    #[cfg(test)]
    pub(crate) fn never() -> Deadline {
        Deadline {
            token: CancellationToken::new(),
            state: Arc::new(Mutex::new(State::Running)),
            budget: Duration::from_secs(3600),
            write: true,
            argument: Some(DOOR_ARGUMENT.into()),
        }
    }

    /// The token, for `SparqlEvaluator::with_cancellation_token` and for a serializer to
    /// check between rows.
    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Refuse if the caller has given up: for a loop this crate owns (a serializer) to call
    /// between rows.
    pub(crate) fn check(&self) -> Result<()> {
        if self.token.is_cancelled() {
            return Err(timeout(self.budget, self.write, self.argument.as_deref()));
        }
        Ok(())
    }

    /// Run `commit` only if the caller is still waiting, and hold the caller off while it
    /// runs. An update's transaction commits through this, so a write lands exactly when its
    /// caller is told it did.
    pub(crate) fn settle<R>(&self, commit: impl FnOnce() -> Result<R>) -> Result<R> {
        let mut state = lock(&self.state);
        if *state == State::Abandoned {
            return Err(timeout(self.budget, self.write, self.argument.as_deref()));
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

/// The argument this store's own doors read a request's lower bound from (`budget=`).
const DOOR_ARGUMENT: &str = "budget";

fn timeout(budget: Duration, write: bool, argument: Option<&str>) -> Error {
    // At the doors (`argument` is `budget`) the wording is `ikigai-sparql`'s, so a consumer
    // reading both sees one sentence. Through an `EvaluationBudget` it names the consumer's
    // own argument, or none: a caller cannot pass an argument its door does not take
    // (ledger #1383).
    let lower = match argument {
        Some(arg) => format!(", and a request's `{arg}=` can only lower it"),
        None => String::new(),
    };
    Error::Timeout(format!(
        "{} ran past its time budget of {} ms and was stopped (ledger #964).{} The budget is \
         set by the host{lower}. Narrow the query — fewer joins, a bound subject, a LIMIT — \
         or ask the host for a larger budget",
        if write {
            "this SPARQL update"
        } else {
            "this SPARQL query"
        },
        budget.as_millis(),
        if write { " Nothing was written." } else { "" },
    ))
}

/// Run one evaluation of `text` within `budget`, on a thread sized for it: a read on a
/// thread of its own, an update on `writer`'s thread when one is given (ledger #1127 — see
/// [`Writer`]).
///
/// `overdue` is the store's count of evaluations still running after their callers gave up;
/// at `max_overdue` this refuses before starting anything. See the module docs.
///
/// A timeout's advice names the doors' `budget=`; [`EvaluationBudget::run`] names its own.
pub(crate) fn run<T, F>(
    text: &str,
    write: bool,
    budget: Duration,
    overdue: &Arc<AtomicUsize>,
    max_overdue: usize,
    writer: Option<&Writer>,
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
        write,
        argument: Some(DOOR_ARGUMENT.into()),
    };
    on_its_own_thread(text, overdue, max_overdue, writer, deadline, work)
}

/// The one long-lived thread a store's UPDATES and LOADS run on (ledger #1127, #1209).
///
/// ★ **Why a write may not run on a thread spawned for it.** RocksDB draws the height of
/// every memtable skiplist node from a THREAD-LOCAL generator seeded from a hash of the
/// thread's id (`Random::GetTLSInstance`). A thread spawned after the previous one exited
/// usually gets the same id back — the C library reuses its memory — so a store that commits
/// each write on a fresh thread draws the same few heights for every transaction, and the
/// memtable's skiplists stop growing upper levels. Every lookup then walks a long top level:
/// measured on gonk's 750-append fill (debug, oxigraph 0.5.11), a read right after the writes
/// took 2.3x to 20x what it took on a fresh copy of the same data, depending on which seed the
/// run reused, until the memtable flushed. Committing every write on ONE thread lets its
/// generator advance across transactions, and the same replay measured 1.0x
/// (`tests/post_bulk_read.rs` has the measurement and the bound).
///
/// - **Writes were already one at a time** — every write door takes the store's write lock
///   — so running them on one thread serializes nothing that ran in parallel before; a write
///   waits in this queue where it used to wait on that lock.
/// - **Reads stay on threads of their own.** A read inserts nothing into a memtable, and
///   reads do run in parallel.
/// - **The stack grows, never shrinks.** The thread is started with a stack that fits the
///   first update ([`crate::limits::sparql_stack_size`], rounded up to a power of two); an
///   update that needs more replaces it with a larger one, and the old thread finishes what
///   was queued on it and exits. A stack is reserved address space and only touched as deep
///   as an update recurses, but what was touched stays resident for the thread's life.
/// - **A write whose caller gave up while it waited is not run**, and leaves the overdue
///   count when it is skipped.
/// - **One per store**, shared by every clone of it, and gone with the last clone: dropping
///   the store drops the queue's sender and the thread exits once it is empty. It never holds
///   the dataset itself, so a store can be reopened at the same path as soon as it is dropped.
/// - On wasm there are no threads, and an update runs inline like everything else.
///
/// - **A job may not queue another on the thread it runs on.** It would wait for a job queued
///   behind itself (a host's write forever, an update until its budget ran out), so
///   [`submit`](Writer::submit) refuses at once instead.
///
/// ⚠ It covers the writes THIS crate performs (`urn:iki:store:load` since 0.2.15, ledger
/// #1209), and the host's own writes when it makes them through
/// [`SharedWriter`](crate::SharedWriter) (ledger #1207), which runs them here, or on a
/// writer the handle owns for [`SharedWriter::over`](crate::SharedWriter::over) on a store the
/// host opened itself, one per handle however many `over` writers it has (ledger #1324; on a
/// handle a shared constructor gave out, `over` runs them here too, ledger #1276). A host
/// that writes through the raw handle (`open_shared`) or commits inside
/// [`crate::limits::on_sparql_stack`], which spawns a thread per call, on a fresh thread per
/// write meets the same degeneration, and the fix is the same: write from a thread that lives.
#[derive(Default)]
pub(crate) struct Writer {
    #[cfg(not(target_family = "wasm"))]
    thread: Mutex<Option<WriterThread>>,
}

#[cfg(not(target_family = "wasm"))]
struct WriterThread {
    jobs: std::sync::mpsc::Sender<Job>,
    stack: usize,
    /// The thread's id: a job running on it that queues another here is refused.
    id: std::thread::ThreadId,
}

#[cfg(not(target_family = "wasm"))]
type Job = Box<dyn FnOnce() + Send + 'static>;

#[cfg(not(target_family = "wasm"))]
impl Writer {
    /// Queue `job` on the writer thread, first starting one — or a larger one, when the
    /// current stack is smaller than `stack` — if need be.
    fn submit(&self, stack: usize, job: Job) -> Result<()> {
        let mut slot = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        let job = match slot.as_ref() {
            // From a job already running on this thread, the new one would wait behind the
            // job that waits for it. (When the stack does not fit, the job goes to a NEW
            // thread below, and nothing waits on itself.)
            Some(current)
                if current.stack >= stack && current.id == std::thread::current().id() =>
            {
                return Err(Error::Endpoint(
                    "a write on this store was asked for from inside another write on its \
                     writer thread, and would wait for itself: finish the first write, then make \
                     the second"
                        .into(),
                ));
            }
            Some(current) if current.stack >= stack => match current.jobs.send(job) {
                Ok(()) => return Ok(()),
                // The thread is gone, which nothing here does on purpose (a job catches its own
                // panic): start another rather than lose the write.
                Err(std::sync::mpsc::SendError(job)) => job,
            },
            _ => job,
        };
        let size = stack
            .checked_next_power_of_two()
            .unwrap_or(stack)
            .min(crate::limits::sparql_stack_size_for_len(
                crate::limits::MAX_SPARQL_BYTES,
            ))
            .max(stack);
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        let started = std::thread::Builder::new()
            .name(WRITER_THREAD_NAME.to_string())
            .stack_size(size)
            .spawn(move || {
                for job in queue {
                    job();
                }
            })
            .map_err(|e| {
                Error::Unavailable(format!(
                    "could not start a {} MiB thread to run this store's writes on: {e}",
                    size >> 20
                ))
            })?;
        let id = started.thread().id();
        jobs.send(job)
            .map_err(|_| Error::Endpoint("internal: a new store writer thread ended".into()))?;
        *slot = Some(WriterThread {
            jobs,
            stack: size,
            id,
        });
        Ok(())
    }

    /// Run `work` on the writer thread and wait for its answer, with no deadline: the door
    /// for a host's own writes ([`SharedWriter`](crate::SharedWriter), ledger #1207). A panic
    /// in `work` is caught there, so the thread lives on, and resumed here.
    pub(crate) fn call<T: Send + 'static>(
        &self,
        stack: usize,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T> {
        let (answer, answered) = std::sync::mpsc::sync_channel(1);
        self.submit(
            stack,
            Box::new(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                // The caller waits for this; it can only be gone if it panicked itself.
                let _ = answer.send(outcome);
            }),
        )?;
        match answered.recv() {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(panic)) => std::panic::resume_unwind(panic),
            Err(_) => Err(Error::Endpoint(
                "internal: the store's writer thread ended without answering".into(),
            )),
        }
    }

    /// The writer thread's stack, or `None` before the first write. For tests.
    #[cfg(test)]
    fn stack(&self) -> Option<usize> {
        let slot = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map(|current| current.stack)
    }
}

/// The name of a store's writer thread, which a host can see in a profile or a panic message.
#[cfg(not(target_family = "wasm"))]
pub(crate) const WRITER_THREAD_NAME: &str = "ikigai-store-writer";

/// On wasm there are no threads: a host's write runs inline.
#[cfg(target_family = "wasm")]
impl Writer {
    pub(crate) fn call<T>(&self, stack: usize, work: impl FnOnce() -> T) -> Result<T> {
        let _ = stack;
        Ok(work())
    }
}

/// On wasm there are no threads: the work runs inline, and so is unbounded in time. The
/// README says so.
#[cfg(target_family = "wasm")]
fn on_its_own_thread<T, F>(
    text: &str,
    overdue: &Arc<AtomicUsize>,
    max_overdue: usize,
    writer: Option<&Writer>,
    deadline: Deadline,
    work: F,
) -> Result<T>
where
    F: FnOnce(&Deadline) -> Result<T>,
{
    let _ = (text, overdue, max_overdue, writer);
    work(&deadline)
}

#[cfg(not(target_family = "wasm"))]
fn on_its_own_thread<T, F>(
    text: &str,
    overdue: &Arc<AtomicUsize>,
    max_overdue: usize,
    writer: Option<&Writer>,
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
    let (budget, write) = (deadline.budget, deadline.write);
    let argument = deadline.argument.clone();
    let stack = crate::limits::sparql_stack_size(text);
    let (tx, rx) = std::sync::mpsc::channel();
    let state = Arc::clone(&deadline.state);
    let token = deadline.token.clone();
    let worker_overdue = Arc::clone(overdue);
    let body = move || {
        // A write can wait in the writer's queue past its caller's budget: then the caller has
        // already been answered and counted it overdue, and running it would only be work
        // whose commit `settle` refuses.
        if *lock(&deadline.state) == State::Abandoned {
            worker_overdue.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&deadline)));
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
    };
    match writer {
        // ★ Never on a fresh thread: see `Writer` (ledger #1127).
        Some(writer) => writer.submit(stack, Box::new(body))?,
        None => {
            std::thread::Builder::new()
                .name("ikigai-store-sparql".to_string())
                .stack_size(stack)
                .spawn(body)
                .map_err(|e| {
                    Error::Unavailable(format!(
                        "could not start a {} MiB thread to evaluate this SPARQL text on: {e}",
                        stack >> 20
                    ))
                })?;
        }
    }
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
                    return Err(timeout(budget, write, argument.as_deref()));
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

// ------------------------------------------------------------------ the size of an answer

// ★ COPY: from `ikigai-sparql` 0.1.13's `budget` module (ikigai-rs/ikigai-linkeddata PR #29),
// VERBATIM from `DEFAULT_MAX_ROWS` through `CappedWriter`, apart from the crate name in the
// doctests — so ledger #976 can fold both into one crate mechanically. Change both or neither.
// What is this crate's own is below the copy: where a caller's bound comes from (a capability,
// `AnswerBudget`) rather than from the space.

/// The most rows an answer may hold when its host names no other bound: **100,000**.
///
/// A row is a solution of a `SELECT` or a triple of a `CONSTRUCT` or `DESCRIBE`; an `ASK` has
/// none. The heaviest legitimate query found in the ecosystem (the survey behind
/// [`DEFAULT_BUDGET`]) answers with 11,445 triples, so this is almost nine times that. A
/// caller that wants more pages with `LIMIT` and `OFFSET`, which is what a bound should make
/// it do. Counted while the answer is serialized, and the row past the bound is refused, never
/// dropped.
pub const DEFAULT_MAX_ROWS: u64 = 100_000;

/// The most serialized bytes an answer may hold when its host names no other bound:
/// **16 MiB**.
///
/// Rows vary in width far more than in count, so rows alone do not bound memory: a
/// three-variable `SELECT` row of small literals is about 275 bytes as SPARQL JSON results
/// (measured: 1,000,000 rows, 274,760,059 bytes), and a row of long literals can be any size.
/// At that width this bound binds first, at about 61,000 rows. Counted at every write the
/// serializer makes, and the write past the bound is refused, never cut.
pub const DEFAULT_MAX_BYTES: u64 = 16 << 20;

/// The most rows any host may allow an answer: **10,000,000**. A host asking for more when
/// it builds its bound is refused ([`AnswerBound::new`]).
pub const CEILING_MAX_ROWS: u64 = 10_000_000;

/// The most serialized bytes any host may allow an answer: **1 GiB**. A host asking for more
/// when it builds its bound is refused ([`AnswerBound::new`]).
pub const CEILING_MAX_BYTES: u64 = 1 << 30;

/// How large one answer may be: at most [`rows`](AnswerBound::rows) rows and at most
/// [`bytes`](AnswerBound::bytes) serialized bytes, both at least 1 and at most
/// [`AnswerBound::CEILING`].
///
/// ```
/// use ikigai_store::budget::{AnswerBound, CEILING_MAX_ROWS, DEFAULT_MAX_BYTES};
///
/// let bound = AnswerBound::new(1_000_000, DEFAULT_MAX_BYTES).unwrap();
/// assert_eq!(bound.rows(), 1_000_000);
/// assert!(AnswerBound::new(CEILING_MAX_ROWS + 1, DEFAULT_MAX_BYTES).is_err());
/// assert!(AnswerBound::new(0, DEFAULT_MAX_BYTES).is_err());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnswerBound {
    rows: u64,
    bytes: u64,
}

impl AnswerBound {
    /// [`DEFAULT_MAX_ROWS`] and [`DEFAULT_MAX_BYTES`]: the bound of a space whose host named
    /// none.
    pub const DEFAULT: AnswerBound = AnswerBound {
        rows: DEFAULT_MAX_ROWS,
        bytes: DEFAULT_MAX_BYTES,
    };

    /// [`CEILING_MAX_ROWS`] and [`CEILING_MAX_BYTES`]: the largest bound a host may set.
    pub const CEILING: AnswerBound = AnswerBound {
        rows: CEILING_MAX_ROWS,
        bytes: CEILING_MAX_BYTES,
    };

    /// A bound of `rows` rows and `bytes` bytes. Either one zero or over its ceiling is
    /// refused, naming `max_rows` or `max_bytes`: a host configuration that asks for more than
    /// the ceiling is a mistake to say out loud, not one to quietly clamp.
    pub fn new(rows: u64, bytes: u64) -> Result<Self> {
        let check = |name: &str, value: u64, ceiling: u64, unit: &str| {
            if value == 0 || value > ceiling {
                Err(Error::InvalidArgument {
                    name: name.to_string(),
                    detail: format!(
                        "an answer bound of {value} {unit} is out of range: it must be at least \
                         1 and at most {ceiling} {unit}"
                    ),
                })
            } else {
                Ok(value)
            }
        };
        Ok(AnswerBound {
            rows: check("max_rows", rows, CEILING_MAX_ROWS, "rows")?,
            bytes: check("max_bytes", bytes, CEILING_MAX_BYTES, "bytes")?,
        })
    }

    /// The most rows (solutions, or triples) an answer may hold.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// The most serialized bytes an answer may hold.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Default for AnswerBound {
    fn default() -> Self {
        AnswerBound::DEFAULT
    }
}

/// The bound that applies to one answer: the space's, with each of the request's `max_rows=`
/// and `max_bytes=` applied when it is SMALLER. Like [`effective_budget`], a request can only
/// lower its bound, and a value that is not a positive whole number is refused, naming the
/// argument.
///
/// ```
/// use ikigai_store::budget::{effective_answer, AnswerBound};
///
/// let space = AnswerBound::DEFAULT;
/// assert_eq!(effective_answer(None, None, space).unwrap(), space);
/// let lowered = effective_answer(Some("10"), Some("4096"), space).unwrap();
/// assert_eq!((lowered.rows(), lowered.bytes()), (10, 4096));
/// // Asking for more than the space's bound gets the space's bound.
/// let asked = effective_answer(Some("999999999"), None, space).unwrap();
/// assert_eq!(asked.rows(), space.rows());
/// assert!(effective_answer(Some("0"), None, space).is_err());
/// assert!(effective_answer(None, Some("16MiB"), space).is_err());
/// ```
pub fn effective_answer(
    max_rows: Option<&str>,
    max_bytes: Option<&str>,
    space: AnswerBound,
) -> Result<AnswerBound> {
    let lower = |name: &str, requested: Option<&str>, bound: u64, unit: &str| {
        let Some(text) = requested else {
            return Ok(bound);
        };
        match text.trim().parse::<u64>() {
            Ok(n) if n > 0 => Ok(bound.min(n)),
            _ => Err(Error::InvalidArgument {
                name: name.to_string(),
                detail: format!(
                    "`{text}` is not an answer bound: give a positive whole number of {unit}. \
                     It can only lower the bound this space applies ({bound} {unit}), never \
                     raise it"
                ),
            }),
        }
    };
    Ok(AnswerBound {
        rows: lower("max_rows", max_rows, space.rows, "rows")?,
        bytes: lower("max_bytes", max_bytes, space.bytes, "bytes")?,
    })
}

/// A bounding argument (`budget=`, `max_rows=`, `max_bytes=`) as inline text: `None` when
/// the request does not carry it, and a refusal naming it when it is given any other way — by
/// reference, as content, or as bytes that are not UTF-8.
///
/// Refused, not ignored: an ignored bound falls back to the space's own, and a host door that
/// stamps a smaller bound only when the caller sent none would see one present and stamp
/// nothing. The caller would have bypassed the door by sending its bound as a reference.
pub fn inline_bound<'a>(inv: &Invocation<'a>, name: &str) -> Result<Option<&'a str>> {
    let request = inv.request;
    let refuse = |how: &str| Error::InvalidArgument {
        name: name.to_string(),
        detail: format!(
            "`{name}=` was given {how}: a bound is read only as an inline value, and one given \
             any other way is refused rather than ignored, because ignoring it would apply this \
             space's own bound in place of the one a host's door meant to stamp (ledger #970)"
        ),
    };
    match request.args.get(name) {
        None => Ok(None),
        Some(ArgRef::Inline(bytes)) => std::str::from_utf8(bytes)
            .map(Some)
            .map_err(|_| refuse("as bytes that are not UTF-8 text")),
        Some(ArgRef::Reference(_)) => Err(refuse("by reference")),
        Some(ArgRef::Content(_)) => Err(refuse("as interned content")),
    }
}

/// What an answer bound counts, for [`too_large`]'s wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {
    /// Solutions of a `SELECT`.
    Rows,
    /// Triples of a `CONSTRUCT` or `DESCRIBE`; bounded by `max_rows=` all the same.
    Triples,
    /// Serialized bytes, of any form but `ASK`.
    Bytes,
}

/// The refusal for an answer over its [`AnswerBound`]: [`Error::InvalidArgument`] on `query`,
/// naming the bound, its value and the cure. Never cached, like every refusal.
///
/// ```
/// use ikigai_store::budget::{too_large, Measure};
///
/// let text = too_large(Measure::Rows, 100_000).to_string();
/// assert!(text.contains(
///     "the answer exceeds 100000 rows; add LIMIT, narrow the query, or ask the host for more"
/// ));
/// ```
pub fn too_large(measure: Measure, bound: u64) -> Error {
    let (unit, arg) = match measure {
        Measure::Rows => ("rows", "max_rows"),
        Measure::Triples => ("triples", "max_rows"),
        Measure::Bytes => ("bytes", "max_bytes"),
    };
    Error::InvalidArgument {
        name: "query".to_string(),
        detail: format!(
            "the answer exceeds {bound} {unit}; add LIMIT, narrow the query, or ask the host for \
             more. It was refused, not truncated: no part of it was sent (ledger #970). A \
             request's `{arg}=` can only lower this bound"
        ),
    }
}

/// A [`Write`] that keeps what is written in memory and refuses the write that would take it
/// past `cap` bytes, remembering that it did. An answer serializer writes into one, so the
/// byte bound is enforced as the answer is produced, not measured after it was all held.
///
/// The serializer's own error for the refused write is not the answer's refusal: check
/// [`over`](CappedWriter::over) and refuse with [`too_large`].
///
/// ```
/// use ikigai_store::budget::CappedWriter;
/// use std::io::Write;
///
/// let mut out = CappedWriter::new(8);
/// out.write_all(b"12345678").unwrap();
/// assert!(!out.over());
/// assert!(out.write_all(b"9").is_err());
/// assert!(out.over());
/// assert_eq!(out.into_bytes(), b"12345678");
/// ```
#[derive(Debug)]
pub struct CappedWriter {
    bytes: Vec<u8>,
    cap: u64,
    over: bool,
}

impl CappedWriter {
    /// An empty writer that holds at most `cap` bytes.
    pub fn new(cap: u64) -> Self {
        CappedWriter {
            bytes: Vec::new(),
            cap,
            over: false,
        }
    }

    /// Whether a write was refused for passing the cap.
    pub fn over(&self) -> bool {
        self.over
    }

    /// What was written, never more than the cap.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for CappedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len() as u64 + buf.len() as u64 > self.cap {
            self.over = true;
            return Err(std::io::Error::other(format!(
                "the answer passed its bound of {} bytes",
                self.cap
            )));
        }
        // Grow by doubling, as a Vec would, but never past the cap: the bound is on memory,
        // and a plain Vec's doubling would reserve up to twice the cap.
        let needed = self.bytes.len() + buf.len();
        if needed > self.bytes.capacity() {
            let target = (self.bytes.capacity() * 2)
                .max(needed)
                .min(usize::try_from(self.cap).unwrap_or(usize::MAX));
            self.bytes.reserve_exact(target - self.bytes.len());
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ★ END COPY. What follows is this crate's own: the bound comes from the CAPABILITY.

/// The family of answer-size grants: `urn:cap:store:answer:<rows>` and
/// `urn:cap:store:answer:bytes:<bytes>`. Like [`CAP_BUDGET`], required by no endpoint —
/// holding none is the ordinary case, and gets the base.
pub const CAP_ANSWER: &str = "urn:cap:store:answer:*";

/// The grant that lets an answer have up to `rows` rows or triples (clamped to the store's
/// ceiling).
///
/// ```
/// assert_eq!(ikigai_store::budget::cap_answer(500_000), "urn:cap:store:answer:500000");
/// ```
pub fn cap_answer(rows: u64) -> String {
    format!("{}{rows}", CAP_ANSWER.trim_end_matches('*'))
}

/// The grant that lets an answer serialize to up to `bytes` bytes (clamped to the store's
/// ceiling).
///
/// ```
/// assert_eq!(
///     ikigai_store::budget::cap_answer_bytes(268_435_456),
///     "urn:cap:store:answer:bytes:268435456"
/// );
/// ```
pub fn cap_answer_bytes(bytes: u64) -> String {
    format!("{}bytes:{bytes}", CAP_ANSWER.trim_end_matches('*'))
}

/// How large a store's SPARQL answers may be: a base every caller gets, and a ceiling that
/// root gets and grants are clamped to. See the module docs, section 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnswerBudget {
    base: AnswerBound,
    ceiling: AnswerBound,
}

impl Default for AnswerBudget {
    /// [`AnswerBound::DEFAULT`] for every caller, under [`AnswerBound::CEILING`].
    fn default() -> Self {
        AnswerBudget {
            base: AnswerBound::DEFAULT,
            ceiling: AnswerBound::CEILING,
        }
    }
}

impl AnswerBudget {
    /// A base of `base` for every caller, under [`AnswerBound::CEILING`].
    pub fn new(base: AnswerBound) -> Self {
        AnswerBudget {
            base,
            ceiling: AnswerBound::CEILING,
        }
    }

    /// Set the ceiling: what root gets, and the most a grant can lift a caller to. A ceiling
    /// below the base lowers the base to it, so no caller ever gets more than this.
    pub fn with_ceiling(mut self, ceiling: AnswerBound) -> Self {
        self.ceiling = ceiling;
        self.base = AnswerBound {
            rows: self.base.rows.min(ceiling.rows),
            bytes: self.base.bytes.min(ceiling.bytes),
        };
        self
    }

    /// What every caller gets.
    pub fn base(&self) -> AnswerBound {
        self.base
    }

    /// What root gets, and the most anyone gets.
    pub fn ceiling(&self) -> AnswerBound {
        self.ceiling
    }

    /// The bound a caller holding `capability` gets: the ceiling for root; otherwise, for rows
    /// and bytes each, the largest grant it holds ([`cap_answer`], [`cap_answer_bytes`]),
    /// never below the base and never above the ceiling.
    ///
    /// ```
    /// use ikigai_core::Capability;
    /// use ikigai_store::budget::{cap_answer, cap_answer_bytes, AnswerBound, AnswerBudget};
    ///
    /// let budget = AnswerBudget::new(AnswerBound::new(1_000, 1 << 20)?)
    ///     .with_ceiling(AnswerBound::new(50_000, 64 << 20)?);
    /// let anonymous = Capability::scoped(["urn:cap:store:read"]);
    /// let export = Capability::scoped([cap_answer(20_000), cap_answer_bytes(32 << 20)]);
    /// let greedy = Capability::scoped([cap_answer(u64::MAX)]);
    /// assert_eq!(budget.for_capability(&anonymous), budget.base());
    /// assert_eq!(budget.for_capability(&export), AnswerBound::new(20_000, 32 << 20)?);
    /// // A row grant raises rows and nothing else.
    /// assert_eq!(budget.for_capability(&greedy), AnswerBound::new(50_000, 1 << 20)?);
    /// assert_eq!(budget.for_capability(&Capability::root()), budget.ceiling());
    /// # Ok::<(), ikigai_core::Error>(())
    /// ```
    pub fn for_capability(&self, capability: &Capability) -> AnswerBound {
        let Some(scopes) = capability.scopes() else {
            return self.ceiling;
        };
        let prefix = CAP_ANSWER.trim_end_matches('*');
        let (mut rows, mut bytes) = (0u64, 0u64);
        for grant in scopes
            .iter()
            .filter(|scope| capability.allows(scope))
            .filter_map(|scope| scope.strip_prefix(prefix))
        {
            if let Some(n) = grant.strip_prefix("bytes:") {
                if let Ok(n) = n.parse::<u64>() {
                    bytes = bytes.max(n);
                }
            } else if let Ok(n) = grant.parse::<u64>() {
                rows = rows.max(n);
            }
        }
        AnswerBound {
            rows: rows.max(self.base.rows).min(self.ceiling.rows),
            bytes: bytes.max(self.base.bytes).min(self.ceiling.bytes),
        }
    }
}

/// Refuse a parsed query whose algebra exceeds [`MAX_JOIN_OPERANDS`] or [`MAX_ALGEBRA_NODES`],
/// naming the argument `arg` and the bound. Called after parsing and before planning.
pub fn check_query(query: &Query, arg: &str) -> Result<()> {
    let mut cost = Cost::default();
    match query {
        Query::Select { pattern, .. }
        | Query::Ask { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Construct { pattern, .. } => cost.pattern(pattern),
    }
    cost.verdict(arg)
}

/// [`check_query`] for an update: the `WHERE` of every `DELETE`/`INSERT` operation counts
/// toward one total, and each join is bounded on its own.
pub fn check_update(update: &Update, arg: &str) -> Result<()> {
    let mut cost = Cost::default();
    for operation in &update.operations {
        if let GraphUpdateOperation::DeleteInsert { pattern, .. } = operation {
            cost.pattern(pattern);
        }
    }
    cost.verdict(arg)
}

/// The algebra walk. ★ Kept identical to `ikigai-sparql`'s `budget::Cost` (ledger #964) so
/// the hub can fold the two into one shared module; change both or neither.
///
/// ⚠ The matches are exhaustive over `spargebra`'s enums on purpose, and that is safe here
/// for the reason `endpoints::serialize_solutions` gives: the only feature-gated variants in
/// spargebra 0.4.7 are in `Function` (never matched — a call is walked by its arguments) and
/// `GraphPattern::Lateral`, gated on `sep-0006`, which this crate's manifest enables itself.
#[derive(Default)]
struct Cost {
    nodes: usize,
    widest_join: usize,
}

impl Cost {
    fn verdict(&self, arg: &str) -> Result<()> {
        if self.widest_join > MAX_JOIN_OPERANDS {
            return Err(Error::InvalidArgument {
                name: arg.to_string(),
                detail: format!(
                    "this SPARQL text joins {} patterns in one group, and this endpoint refuses \
                     more than {MAX_JOIN_OPERANDS} (MAX_JOIN_OPERANDS) before planning: the \
                     planner's join ordering grows about with the cube of that number and \
                     cannot be interrupted (ledger #964). A sequence path counts one pattern a \
                     step. Split the query, or move a list of values into VALUES",
                    self.widest_join
                ),
            });
        }
        if self.nodes > MAX_ALGEBRA_NODES {
            return Err(Error::InvalidArgument {
                name: arg.to_string(),
                detail: format!(
                    "this SPARQL text has {} algebra operators (patterns, OPTIONAL/UNION/FILTER/\
                     BIND, path and expression operators), and this endpoint refuses more than \
                     {MAX_ALGEBRA_NODES} (MAX_ALGEBRA_NODES) before planning: the planner is \
                     quadratic in several of them and cannot be interrupted (ledger #964). \
                     Constants in VALUES and IN (…) cost nothing; use them for long lists",
                    self.nodes
                ),
            });
        }
        Ok(())
    }

    fn pattern(&mut self, pattern: &GraphPattern) {
        match pattern {
            GraphPattern::Bgp { patterns } => {
                self.nodes += patterns.len();
                self.widest_join = self.widest_join.max(patterns.len());
            }
            GraphPattern::Join { .. } => {
                // The planner flattens nested joins and reorders all their operands at once.
                let mut operands = 0;
                let mut todo = vec![pattern];
                while let Some(next) = todo.pop() {
                    match next {
                        GraphPattern::Join { left, right } => {
                            self.nodes += 1;
                            todo.push(left);
                            todo.push(right);
                        }
                        GraphPattern::Bgp { patterns } => {
                            self.nodes += patterns.len();
                            operands += patterns.len();
                        }
                        other => {
                            operands += 1;
                            self.pattern(other);
                        }
                    }
                }
                self.widest_join = self.widest_join.max(operands);
            }
            GraphPattern::Path { path, .. } => {
                self.widest_join = self.widest_join.max(1);
                self.path(path);
            }
            GraphPattern::LeftJoin {
                left,
                right,
                expression,
            } => {
                self.nodes += 1;
                self.pattern(left);
                self.pattern(right);
                if let Some(expression) = expression {
                    self.expression(expression);
                }
            }
            GraphPattern::Lateral { left, right }
            | GraphPattern::Union { left, right }
            | GraphPattern::Minus { left, right } => {
                self.nodes += 1;
                self.pattern(left);
                self.pattern(right);
            }
            GraphPattern::Filter { expr, inner } => {
                self.nodes += 1;
                self.expression(expr);
                self.pattern(inner);
            }
            GraphPattern::Extend {
                inner, expression, ..
            } => {
                self.nodes += 1;
                self.expression(expression);
                self.pattern(inner);
            }
            GraphPattern::OrderBy { inner, expression } => {
                self.nodes += 1;
                for order in expression {
                    match order {
                        OrderExpression::Asc(e) | OrderExpression::Desc(e) => self.expression(e),
                    }
                }
                self.pattern(inner);
            }
            GraphPattern::Group {
                inner, aggregates, ..
            } => {
                self.nodes += 1;
                for (_, aggregate) in aggregates {
                    if let AggregateExpression::FunctionCall { expr, .. } = aggregate {
                        self.expression(expr);
                    }
                }
                self.pattern(inner);
            }
            GraphPattern::Graph { inner, .. }
            | GraphPattern::Service { inner, .. }
            | GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Slice { inner, .. } => {
                self.nodes += 1;
                self.pattern(inner);
            }
            GraphPattern::Values { .. } => self.nodes += 1,
        }
    }

    fn path(&mut self, path: &PropertyPathExpression) {
        self.nodes += 1;
        match path {
            PropertyPathExpression::NamedNode(_)
            | PropertyPathExpression::NegatedPropertySet(_) => {}
            PropertyPathExpression::Reverse(p)
            | PropertyPathExpression::ZeroOrMore(p)
            | PropertyPathExpression::OneOrMore(p)
            | PropertyPathExpression::ZeroOrOne(p) => self.path(p),
            PropertyPathExpression::Sequence(a, b) | PropertyPathExpression::Alternative(a, b) => {
                self.path(a);
                self.path(b);
            }
        }
    }

    fn expression(&mut self, expression: &Expression) {
        match expression {
            Expression::NamedNode(_)
            | Expression::Literal(_)
            | Expression::Variable(_)
            | Expression::Bound(_) => {}
            Expression::In(e, members) => {
                self.nodes += 1;
                self.expression(e);
                for member in members {
                    self.expression(member);
                }
            }
            Expression::Exists(pattern) => {
                self.nodes += 1;
                self.pattern(pattern);
            }
            Expression::Coalesce(args) | Expression::FunctionCall(_, args) => {
                self.nodes += 1;
                for arg in args {
                    self.expression(arg);
                }
            }
            Expression::If(a, b, c) => {
                self.nodes += 1;
                self.expression(a);
                self.expression(b);
                self.expression(c);
            }
            Expression::UnaryPlus(e) | Expression::UnaryMinus(e) | Expression::Not(e) => {
                self.nodes += 1;
                self.expression(e);
            }
            Expression::Or(a, b)
            | Expression::And(a, b)
            | Expression::Equal(a, b)
            | Expression::SameTerm(a, b)
            | Expression::Greater(a, b)
            | Expression::GreaterOrEqual(a, b)
            | Expression::Less(a, b)
            | Expression::LessOrEqual(a, b)
            | Expression::Add(a, b)
            | Expression::Subtract(a, b)
            | Expression::Multiply(a, b)
            | Expression::Divide(a, b) => {
                self.nodes += 1;
                self.expression(a);
                self.expression(b);
            }
        }
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
        let got = run(
            "q",
            false,
            Duration::from_secs(5),
            &overdue,
            1,
            None,
            |_| Ok(7),
        )
        .unwrap();
        assert_eq!(got, 7);
        assert_eq!(overdue.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn past_the_budget_the_caller_is_answered_with_a_timeout_and_the_work_is_told_to_stop() {
        let overdue = counter();
        let start = Instant::now();
        let err = run(
            "q",
            false,
            Duration::from_millis(100),
            &overdue,
            4,
            None,
            |deadline| {
                // Cooperative work: stops when told.
                while deadline.check().is_ok() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            },
        )
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
        let err = run(
            "q",
            false,
            Duration::from_millis(50),
            &overdue,
            1,
            None,
            move |_| {
                let _ = held.recv();
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)));
        assert_eq!(overdue.load(Ordering::SeqCst), 1);
        let refused = run(
            "q",
            false,
            Duration::from_secs(5),
            &overdue,
            1,
            None,
            |_| Ok(()),
        )
        .unwrap_err();
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
        run(
            "q",
            false,
            Duration::from_secs(5),
            &overdue,
            1,
            None,
            |_| Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn a_commit_after_the_caller_gave_up_does_not_happen() {
        let overdue = counter();
        let committed = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&committed);
        let (done, finished) = std::sync::mpsc::channel::<Result<()>>();
        let err = run(
            "q",
            true,
            Duration::from_millis(50),
            &overdue,
            4,
            None,
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
        assert!(matches!(outcome, Err(Error::Timeout(m)) if m.contains("Nothing was written")));
        assert_eq!(committed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_panic_in_the_work_reaches_the_caller() {
        let overdue = counter();
        let caught = std::panic::catch_unwind(|| {
            let _ = run(
                "q",
                false,
                Duration::from_secs(5),
                &overdue,
                1,
                None,
                |_| -> Result<()> { panic!("boom") },
            );
        });
        assert!(caught.is_err());
        assert_eq!(overdue.load(Ordering::SeqCst), 0);
    }

    /// ★ The deterministic half of ledger #1127 (`tests/post_bulk_read.rs` measures the
    /// symptom, whose size depends on luck): every update on one store runs on the SAME
    /// thread, never one spawned for it, so RocksDB's thread-local skiplist generator keeps
    /// advancing across commits. A read still gets a thread of its own.
    #[test]
    fn every_write_on_a_store_runs_on_one_thread() {
        let overdue = counter();
        let writer = Writer::default();
        let on = |write: bool| {
            run(
                "q",
                write,
                Duration::from_secs(5),
                &overdue,
                1,
                write.then_some(&writer),
                |_| Ok(std::thread::current().id()),
            )
            .unwrap()
        };
        let first = on(true);
        assert_ne!(first, std::thread::current().id());
        for _ in 0..5 {
            assert_eq!(on(true), first, "an update ran on a thread of its own");
        }
        assert_ne!(on(false), first, "a read waited for the writer");
    }

    #[test]
    fn an_update_needing_more_stack_moves_the_writer_to_a_larger_thread() {
        let overdue = counter();
        let writer = Writer::default();
        let write = |text: &str| {
            run(
                text,
                true,
                Duration::from_secs(5),
                &overdue,
                1,
                Some(&writer),
                |_| Ok(std::thread::current().id()),
            )
            .unwrap()
        };
        let small = write("q");
        let first_stack = writer.stack().unwrap();
        assert!(first_stack >= crate::limits::sparql_stack_size("q"));
        assert!(first_stack.is_power_of_two());
        // Short enough that its rounded-up stack still fits the first thread: same thread.
        assert_eq!(write("qq"), small);
        let long = "q".repeat(64 << 10);
        let large = write(&long);
        assert_ne!(large, small);
        assert!(writer.stack().unwrap() >= crate::limits::sparql_stack_size(&long));
        // And the larger thread serves the smaller updates from then on.
        assert_eq!(write("q"), large);
    }

    #[test]
    fn a_queued_write_whose_caller_gave_up_is_not_run() {
        let overdue = counter();
        let writer = Arc::new(Writer::default());
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let blocker = {
            let (overdue, writer) = (Arc::clone(&overdue), Arc::clone(&writer));
            std::thread::spawn(move || {
                run(
                    "q",
                    true,
                    Duration::from_secs(30),
                    &overdue,
                    4,
                    Some(&writer),
                    move |_| {
                        hold.recv().unwrap();
                        Ok(())
                    },
                )
            })
        };
        // Wait until the blocker holds the writer thread.
        let started = Instant::now();
        while writer.stack().is_none() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        let ran = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&ran);
        let err = run(
            "q",
            true,
            Duration::from_millis(50),
            &overdue,
            4,
            Some(&writer),
            move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::Timeout(m) if m.contains("Nothing was written")),
            "{err}"
        );
        assert_eq!(overdue.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        blocker.join().unwrap().unwrap();
        let wait = Instant::now();
        while overdue.load(Ordering::SeqCst) != 0 {
            assert!(wait.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(ran.load(Ordering::SeqCst), 0, "an abandoned write was run");
    }

    #[test]
    fn a_panic_in_a_write_reaches_its_caller_and_the_writer_lives_on() {
        let overdue = counter();
        let writer = Writer::default();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run(
                "q",
                true,
                Duration::from_secs(5),
                &overdue,
                1,
                Some(&writer),
                |_| -> Result<()> { panic!("boom") },
            );
        }));
        assert!(caught.is_err());
        let got = run(
            "q",
            true,
            Duration::from_secs(5),
            &overdue,
            1,
            Some(&writer),
            |_| Ok(7),
        )
        .unwrap();
        assert_eq!(got, 7);
        assert_eq!(overdue.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn answer_grants_are_monotone_and_clamped() {
        let budget = AnswerBudget::default();
        let held = |grants: &[String]| budget.for_capability(&Capability::scoped(grants.to_vec()));
        assert_eq!(held(&[]), AnswerBound::DEFAULT);
        // A grant below the base is not a way to get less.
        assert_eq!(held(&[cap_answer(10)]).rows(), DEFAULT_MAX_ROWS);
        assert_eq!(
            held(&[cap_answer(200_000), cap_answer(400_000)]).rows(),
            400_000
        );
        assert_eq!(held(&[cap_answer(u64::MAX)]).rows(), CEILING_MAX_ROWS);
        assert_eq!(
            held(&[cap_answer_bytes(u64::MAX)]).bytes(),
            CEILING_MAX_BYTES
        );
        // A byte grant raises bytes and nothing else.
        assert_eq!(held(&[cap_answer_bytes(u64::MAX)]).rows(), DEFAULT_MAX_ROWS);
        // Not a number: not a grant.
        assert_eq!(
            held(&[
                "urn:cap:store:answer:all".to_string(),
                "urn:cap:store:answer:bytes:lots".to_string()
            ]),
            AnswerBound::DEFAULT
        );
        assert_eq!(
            budget.for_capability(&Capability::root()),
            AnswerBound::CEILING
        );
        let tight = AnswerBudget::new(AnswerBound::new(500_000, 1 << 30).unwrap())
            .with_ceiling(AnswerBound::new(1_000, 1 << 20).unwrap());
        assert_eq!(tight.base(), AnswerBound::new(1_000, 1 << 20).unwrap());
    }

    #[test]
    fn the_shared_constants_and_wording_are_the_contracts() {
        // ledger #970: identical in `ikigai-sparql`; change both or neither.
        assert_eq!(DEFAULT_MAX_ROWS, 100_000);
        assert_eq!(DEFAULT_MAX_BYTES, 16 * 1024 * 1024);
        assert_eq!(CEILING_MAX_ROWS, 10_000_000);
        assert_eq!(CEILING_MAX_BYTES, 1024 * 1024 * 1024);
        assert!(too_large(Measure::Triples, 7).to_string().contains(
            "the answer exceeds 7 triples; add LIMIT, narrow the query, or ask the host for more"
        ));
    }

    fn cost(text: &str) -> (usize, usize) {
        let mut cost = Cost::default();
        match spargebra::SparqlParser::new().parse_query(text).unwrap() {
            Query::Select { pattern, .. }
            | Query::Ask { pattern, .. }
            | Query::Describe { pattern, .. }
            | Query::Construct { pattern, .. } => cost.pattern(&pattern),
        }
        (cost.widest_join, cost.nodes)
    }

    fn query(text: &str) -> Result<()> {
        check_query(
            &spargebra::SparqlParser::new().parse_query(text).unwrap(),
            "query",
        )
    }

    fn patterns(n: usize) -> String {
        (0..n).map(|i| format!("?s <urn:p> ?o{i} . ")).collect()
    }

    #[test]
    fn a_join_counts_its_patterns_its_path_steps_and_its_nested_groups() {
        assert_eq!(cost("SELECT * WHERE { ?s ?p ?o }").0, 1);
        assert_eq!(
            cost(&format!("SELECT * WHERE {{ {} }}", patterns(10))).0,
            10
        );
        // A sequence path is parsed into one pattern a step.
        assert_eq!(
            cost("SELECT * WHERE { ?s <urn:a>/<urn:b>/<urn:c> ?o }").0,
            3
        );
        // Nested groups are flattened into one join, as the planner flattens them.
        let nested = format!(
            "SELECT * WHERE {{ {{ {} }} {{ {} }} {{ SELECT * {{ ?x ?y ?z }} }} }}",
            patterns(5),
            patterns(6)
        );
        assert_eq!(cost(&nested).0, 5 + 6 + 1);
        // OPTIONAL and UNION are separate joins: each side is bounded on its own.
        let split = format!(
            "SELECT * WHERE {{ {} OPTIONAL {{ {} }} }}",
            patterns(30),
            patterns(30)
        );
        assert_eq!(cost(&split).0, 30);
    }

    #[test]
    fn constants_in_values_and_in_lists_cost_nothing() {
        let values = format!(
            "SELECT * WHERE {{ VALUES ?s {{ {} }} ?s ?p ?o }}",
            "<urn:x> ".repeat(5_000)
        );
        assert!(cost(&values).1 < 10);
        let list = format!(
            "SELECT * WHERE {{ ?s ?p ?o FILTER(?o IN ({})) }}",
            vec!["<urn:x>"; 5_000].join(", ")
        );
        assert!(cost(&list).1 < 10);
    }

    #[test]
    fn the_algebra_bounds_admit_at_the_bound_and_refuse_one_past_it_by_name() {
        query(&format!(
            "SELECT * WHERE {{ {} }}",
            patterns(MAX_JOIN_OPERANDS)
        ))
        .unwrap();
        let err = query(&format!(
            "SELECT * WHERE {{ {} }}",
            patterns(MAX_JOIN_OPERANDS + 1)
        ))
        .unwrap_err();
        assert!(
            matches!(&err, Error::InvalidArgument { name, detail } if name == "query"
                && detail.contains("MAX_JOIN_OPERANDS")),
            "{err}"
        );
        // `1||1||…`: n terms are n-1 operators, plus the FILTER and the empty group.
        let chain = |terms: usize| format!("SELECT * WHERE {{ FILTER(1{}) }}", "||1".repeat(terms));
        let (_, nodes) = cost(&chain(0));
        query(&chain(MAX_ALGEBRA_NODES - nodes)).unwrap();
        let err = query(&chain(MAX_ALGEBRA_NODES - nodes + 1)).unwrap_err();
        assert!(err.to_string().contains("MAX_ALGEBRA_NODES"), "{err}");
    }

    #[test]
    fn an_update_counts_every_where_clause_toward_one_total() {
        let parse = |text: &str| spargebra::SparqlParser::new().parse_update(text).unwrap();
        let half = MAX_ALGEBRA_NODES / 2 + 1;
        let op = format!(
            "DELETE {{ ?s ?p ?o }} WHERE {{ ?s ?p ?o FILTER({}) }}",
            vec!["?o = 1"; half / 2].join(" || ")
        );
        check_update(&parse(&op), "content").unwrap();
        let err = check_update(&parse(&format!("{op} ; {op} ; {op}")), "content").unwrap_err();
        assert!(err.to_string().contains("MAX_ALGEBRA_NODES"), "{err}");
        // INSERT DATA is flat, however long.
        let data = format!(
            "INSERT DATA {{ {} }}",
            "<urn:s> <urn:p> <urn:o> . ".repeat(10_000)
        );
        check_update(&parse(&data), "content").unwrap();
    }
}
