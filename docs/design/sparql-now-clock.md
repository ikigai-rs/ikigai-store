# SPARQL `NOW()` and the kernel clock

Ledger #2203. **Option A implemented for 0.6.2 (ledger #4284).** The query and update doors
rewrite `NOW()` to the invocation's instant when one is available. Without a clock, evaluation
still uses the wall clock. All queries that call `NOW()` remain uncached, including inside
pinned corridors. The tables and analysis below record the original design spike.
The integration tests now exercise the implemented behavior through the real doors.

## The claim, and what held

The claim, from the wasm-gates audit (ikigai-devtools PR 242): SPARQL `NOW()` reads the system
clock on every target, so neither `Kernel::with_clock` nor a temporal corridor's pinned instant
reaches a query, and a query that uses `NOW()` cannot be replayed.

All of it holds. `tests/sparql_now_clock.rs` shows each part through the store's own door
(`urn:iki:store:select`):

| Test | What it shows |
|---|---|
| `now_does_not_read_the_kernel_clock` | A kernel built with `with_clock(FixedClock::at(2001-09-09…))` still gets the current year from `NOW()`. |
| `now_does_not_read_a_temporal_corridors_instant` | A request issued in `Scope::with_named_at(…, FixedClock)` also gets the wall clock. |
| `a_now_answer_is_cached_like_any_covered_read` | A repeat 20 ms later gets the first answer from the cache, and a kernel with no cache entry reads a different instant. The cached `NOW()` stays stale until some write cuts a thread. |
| `rewriting_now_to_the_kernel_instant_answers_that_instant` | The proof. Rewriting each `NOW()` in the parsed algebra to an `xsd:dateTime` literal from the clock gives exactly that instant. All four `NOW()`s in one query agree, and a second instant is a second query text. |

The third row is not in the item. It is the more urgent half. Today a `NOW()` answer is
**wrong while looking right** on any kernel with a cache: it does not even show the wall clock
of the current request.

## Where the instant comes from today

These are the versions this crate resolves to: oxigraph 0.5.13, spareval 0.2.9 and
oxsdatatypes 0.2.4.

- `spareval::SimpleEvaluator::new` sets `now: DateTime::now()` **eagerly**, on every
  evaluation, whether or not the query uses `NOW()` (`eval.rs`, around line 441). The
  `QueryEvaluator` expression path reads `DateTime::now` lazily through
  `get_or_insert_with` (`lib.rs`, around line 446). Neither path takes an instant from its
  caller. `QueryEvaluator` has `with_service_handler`, `with_custom_function`,
  `with_cancellation_token` and others, but no `with_now`.
- `oxsdatatypes::DateTime::now()` calls `since_unix_epoch()`. That function reads one of four
  sources, fixed at compile time:
  - `SystemTime::now()` by default;
  - `js_sys::Date::now()` on `wasm32-unknown-unknown` with the `js` feature, which this crate
    turns on for wasm;
  - zero on `zkvm`;
  - the `custom-now` feature, which calls an `unsafe extern "Rust" fn custom_ox_now()` that the
    final binary must define.
- `spareval`'s `Timer`, used for `compute_statistics`, also reads `DateTime::now()`.
- The store hands oxigraph a parsed `spargebra` algebra in exactly two places:
  - queries, in `QueryEndpoint::invoke` (`src/endpoints.rs`): `parse_query`, then
    `evaluator().for_query(parsed)`;
  - updates, in `confine::parse` (`src/confine.rs`), which both update doors share:
    `parse_update`, then `for_update(parsed)`.

  Both places already walk the algebra (`check_query`, `refuse_service`,
  `register_destinations`), so a third walk adds no new structure.
- ikigai-core gives an endpoint `Invocation::now()`. It returns the chain's pinned clock first
  (the clock of the innermost temporal corridor), then the issuer's (the kernel's), and `None`
  when there is neither. `Kernel::new` installs no clock. Core is deliberate that `None` means
  "no time", not "use the wall clock".
- The kernel caches the store's covered reads under golden threads with no expiry. The cache
  key holds the request, the capability and the chain fingerprint, and that fingerprint
  includes a temporal corridor's instant (ledger #3869). It never includes a live kernel
  clock.

## Options

### A. Rewrite `NOW()` to a literal from `inv.now()` before evaluation (recommended)

Right after each parse, replace every `Expression::FunctionCall(Function::Now, [])` in the
algebra with `Expression::Literal("<instant>"^^xsd:dateTime)`. The walk covers queries and the
`WHERE` of `DELETE/INSERT`, including `EXISTS`, subqueries, `ORDER BY` and aggregates. Read
`inv.now()` before the evaluation moves to its sized thread, because the closure there does not
hold the invocation.

- **Correct by SPARQL's own rule.** SPARQL 1.1 §17.4.5.1 says `NOW()` returns the same value for
  every call in one query execution. A single literal gives that by construction. The proof
  test checks it with four calls.
- **No dependency change, no upstream wait.** It works with the oxigraph we have and on every
  target.
- **Replayable.** The answer is a function of the request, the instant `inv.now()` gave and the
  data. A replay harness that supplies the same clock gets the same answer, and an update gets
  the same written quads.
- **Precision.** `Time` is in milliseconds, so the literal is too, in UTC (`Z`). Today's
  evaluator gives finer precision. A caller that compares two `NOW()` answers across requests
  could see a change, but nothing in SPARQL promises sub-millisecond precision.
- **Maintenance.** The walk must be exhaustive, with no `_ =>` arm, like the store's other
  walks. Then a new `spargebra` variant (for example from `sep-0002` or `sep-0006`) fails to
  compile instead of letting a `NOW()` through. The test's walk is about 120 lines.
- **What it does not fix.** `SimpleEvaluator::new` still reads the platform clock once per
  evaluation, with the result unused. On a wasm target without `js` that read is still
  reached. Only option B removes it.

**When `inv.now()` is `None`** (a kernel with no clock and no pinned corridor), there are two
choices. This is decision 1 below.

- (i) Leave `NOW()` alone, which keeps today's wall-clock answer.
- (ii) Refuse a query that uses `NOW()` with `InvalidArgument`, saying the kernel has no clock.
  This is core's reading of `None`, but it would break a host that has never installed a clock.

### B. Upstream `with_now` on spareval and oxigraph (worth filing, does not replace A)

This is the cleaner long-term seam. It also removes the eager wall-clock read and separates the
statistics timer from the `NOW()` instant. It does not change what the store must decide about
caching (below), and the store would adopt it only after an oxigraph release, so A comes first.
B can replace the rewrite later without changing any answer.

Draft issue text for oxigraph/oxigraph, laid out in its **Feature request** template (which adds the
`enhancement` label). **Not filed**: filing it is Brian's (anything posted outside ikigai-rs is). A
search of oxigraph's issues for `NOW()` and `with_now` on 2026-11-24 found no existing request.

> **Title:** Allow the caller to provide the instant `NOW()` returns (`QueryEvaluator::with_now`)
>
> **Is your feature request related to a problem? Please describe.**
>
> `spareval` reads `NOW()` from `oxsdatatypes::DateTime::now()`, eagerly, in
> `SimpleEvaluator::new` on every evaluation. The only way to change it is the `custom-now`
> feature: a process-global `extern fn` that the final binary defines once. An embedder that
> evaluates queries on behalf of callers with different notions of "now" cannot use that. For
> example: a deterministic replay harness, a test pinned to an instant, or a request evaluated
> "as of" a past or future instant, all in one process. The instant cannot vary per evaluation,
> and because Cargo features unify, turning `custom-now` on for one crate turns it on for every
> crate in the binary.
>
> **Describe the solution you'd like**
>
> - `QueryEvaluator::with_now(self, now: DateTime) -> Self`. When set, every `NOW()` in that
>   evaluation returns `now`, so the per-query rule of SPARQL 1.1 §17.4.5.1 still holds. When
>   not set, behavior is unchanged.
> - The same on `oxigraph::sparql::SparqlEvaluator`, and on the update evaluator, so that
>   `DELETE/INSERT … WHERE` with `BIND(NOW() AS ?t)` writes the given instant.
> - Read `DateTime::now()` only when no instant was given. Today it is read on every evaluation,
>   which costs a clock read even for queries that never call `NOW()`, and on targets where
>   reading the clock is expensive or unavailable.
> - Optionally, keep the statistics `Timer` on its own clock source, so that a pinned `now`
>   (or `custom-now`) does not make every measured duration zero.
>
> **Describe alternatives you've considered**
>
> - The `custom-now` feature: process-global, one instant source for the whole binary, cannot
>   vary per evaluation, and it also moves the statistics timer.
> - Rewriting `Function::Now` in the `spargebra` algebra to an `xsd:dateTime` literal before
>   evaluation. This works and is what we do today, but every embedder needs an exhaustive walk
>   of the algebra (queries and updates) that must follow each new `spargebra` variant, and the
>   evaluator still reads the clock it no longer uses.
>
> **Additional context**
>
> - SPARQL 1.1 Query, §17.4.5.1 `NOW`: "Returns an XSD dateTime value for the current query
>   execution. All calls to this function in any one query execution must return the same
>   value." A caller-supplied instant keeps that rule; it only chooses which value.
> - We would adopt `with_now` in place of our algebra rewrite, and keep our tests that pin
>   `NOW()` to an injected instant.

### C. oxsdatatypes `custom-now` (rejected)

This is a process-global `unsafe extern` hook. It cannot vary per request or per corridor, so a
corridor's pinned instant and a live request in the same process could not both be right.
Cargo features unify, so turning it on for the store turns it on for every crate in the binary
that uses oxsdatatypes, and the binary must define the symbol or fail to link. It also moves
`spareval`'s statistics timer. It would be acceptable only for a single-tenant binary with one
clock, and the store is not that.

### D. Leave `NOW()` on the wall clock, and only stop caching it

This fixes the stale answer and nothing else. It is the minimum if A is deferred, and part of A
either way (below).

## What caching and replay need

The kernel looks up the cache **before** the endpoint runs, and keys on the raw request. A
rewrite inside the endpoint therefore cannot make the key carry the instant. The store must
instead choose the freshness it declares for any answer whose evaluation used `NOW()`
(detected by the same walk: count greater than zero):

| Where the instant came from | Declare | Why |
|---|---|---|
| A pinned temporal corridor (`Scope::with_named_at`) | `.cacheable()` under the usual golden threads | The chain fingerprint already holds the corridor's name and instant, so the answer is a pure function of the key plus the data. Core says to do this for as-of data: "declare it cacheable, not cacheable_until". |
| The kernel's own clock, or the wall clock under decision 1 (i) | **Not cacheable** | A live clock is not in the key. Any cached answer is a past instant served as "now". `cacheable_until(now)` would be the same as not caching. |

Data that no query touched keeps today's freshness. A query without `NOW()` is unaffected,
and stays cached exactly as today.

**One gap, in core.** An endpoint cannot tell these two sources apart today.
`Invocation::now()` merges them, and `Invocation::scope()` is `pub(crate)`. The store needs
either `Invocation::pinned_now() -> Option<Time>` (the chain's instant only) or an equivalent
flag. Without it, the safe choice is to declare every `NOW()` answer not cacheable, including
under a pinned corridor. That loses the corridor's cacheability but is never wrong. This is
follow-up 3.

**Updates.** `INSERT { … } WHERE { BIND(NOW() AS ?t) }` writes an instant into durable data.
With A, that instant is the kernel's, so replaying the same writes under the same clock
rebuilds the same quads. A store write is never cached, so updates raise no caching question.

**The other evaluator.** `ikigai-sparql` evaluates SPARQL over a space with the same
oxigraph, so it has the same defect. The walk should live where both crates can reach it:
`ikigai-rdf`, which the store already depends on, or `ikigai-sparql`. That is decision 2.

## Decisions (Brian, 2026-11-24)

- **Option A is accepted:** `NOW()` is rewritten to a literal from `inv.now()` after the store's
  two parse points. Option B (upstream `with_now`) is still worth filing, and filing it is
  Brian's; option C stays rejected.
- **Decision 1 takes the recommendation, (i):** a kernel with no clock keeps `NOW()` on the
  wall clock, and those answers are uncached.
- **Decision 3:** ship A with every `NOW()` answer uncached (already true since store PR 101),
  and enable corridor caching once core has `pinned_now`.
- **Decision 2 stays open** in its follow-up item. Store PR 101 already added a `NOW()` walk
  in `src/now.rs` (`calls_now`), which the rewrite should reuse or extend rather than duplicate.

## Decisions to make

1. **A kernel with no clock.** (i) Keep `NOW()` on the wall clock (recommended for the first
   release: it changes no answer for a host without a clock, and those answers become
   uncached), or (ii) refuse `NOW()` with `InvalidArgument`, naming the missing clock.
   Choosing (ii) changes the text of a refusal, so `ACCESSIBILITY.md`'s wording rules apply to
   it.
2. **Where the walk lives.** In this crate, or shared through `ikigai-rdf` so that
   `ikigai-sparql` uses the same one.
3. **Whether to wait for core's `pinned_now`** before shipping A, or to ship A with every
   `NOW()` answer uncached and enable corridor caching once core has it.

## Proposed follow-up items

1. **Done: store PR 101 (ledger #3989, released in 0.6.1).** Stop caching an answer whose query uses `NOW()`. In `QueryEndpoint::invoke`, a query
   whose algebra contains `Function::Now` is answered without `with_freshness`, so a repeat is
   evaluated again instead of being served a stale instant. This flips
   `a_now_answer_is_cached_like_any_covered_read`. It stands alone, and it fixes the more urgent
   half.
2. **Pin `NOW()` to `inv.now()` in the store's query and update doors.** Add an exhaustive
   `spargebra` walk (queries, and `DELETE/INSERT` `WHERE`) that rewrites `NOW()` to an
   `xsd:dateTime` literal from the invocation's clock. Apply it after parsing in
   `QueryEndpoint::invoke` and `confine::parse`. The behavior with no clock follows decision 1.
   This flips the first two tests in `tests/sparql_now_clock.rs`, and moves the proof's walk
   into `src/`.
3. **ikigai-core: let an endpoint read the pinned instant on its own.** Add
   `Invocation::pinned_now() -> Option<Time>`, the chain's instant from `with_named_at` only,
   or an equivalent. Then an endpoint can declare an answer computed from a pinned instant
   `.cacheable()` and one computed from a live clock not cacheable.
4. **Cache `NOW()` answers under a pinned corridor.** Once item 3 lands, declare a `NOW()`
   answer computed from the pinned instant `.cacheable()` under the usual threads. Test that two
   corridors at two instants never share an entry.
5. **File the upstream `with_now` issue on oxigraph.** Use the draft text in this note. When it
   ships, replace the store's rewrite with `with_now(inv.now())` and keep the tests.
6. **ikigai-sparql: apply the same `NOW()` pinning.** Once decision 2 is settled, use the shared
   walk in `ikigai-sparql`'s evaluator, with the same caching rule.
