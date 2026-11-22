# A deterministic SPARQL subset for cacheable queries and mappings

A store cache entry should be reusable for the same request, bindings, effective dataset,
scope, and evaluator configuration while its freshness coverage holds. A repeatable
Markdown mapping should produce the same RDF term set for the same structural graph,
document IRI, and mapping configuration. Compare SELECT answers as multisets unless row
order is part of the contract. Compare CONSTRUCT answers as graphs. A byte-for-byte
contract additionally needs a stable serialization order and a policy for blank node
labels. This spike proposes a shared algebra classifier and keeps the runtime paths
untouched.

## Evidence

On store main at `64a46722ad42d73973e31e84baddb9bfe867d09a`, the live select door admitted
each of `RAND()`, `UUID()`, `STRUUID()`, and `BNODE()` into the kernel cache. The
reproducer asks the kernel whether the exact request is cached after issuing it. This
avoids inferring a cache hit from a random value or a blank node label. The test failed
with all four calls listed. Run it explicitly with
`cargo test --test deterministic_sparql volatile_selects_bypass_the_cache -- --ignored`.
The separate NOW-only change also leaves these four calls outside its walker.
[Store PR #101](https://github.com/ikigai-rs/ikigai-store/pull/101)

Two uncached Oxigraph evaluations of the same UUID CONSTRUCT against one in-memory store
produced different named nodes. The local test exercises the evaluator directly.
The Markdown PR records a separate ignored reproduction through public `apply` with
identical mapping inputs. [Markdown PR #37](https://github.com/ikigai-rs/ikigai-markdown/pull/37)

## What the classifier should report

| Construct | Why it matters | SPARQL 1.1 Recommendation | SPARQL 1.2 Working Draft, November 19, 2026 |
| --- | --- | --- | --- |
| `NOW()` | The value is shared by all calls in one execution and depends on the execution instant. | [§17.4.5.1](https://www.w3.org/TR/sparql11-query/#func-now) | [§17.4.5.1](https://www.w3.org/TR/2026/WD-sparql12-query-20261119/#func-now) |
| `RAND()` | Draws a pseudo-random value. | [§17.4.4.5](https://www.w3.org/TR/sparql11-query/#idp2130040) | [§17.4.4.5](https://www.w3.org/TR/2026/WD-sparql12-query-20261119/#func-rand) |
| `UUID()`, `STRUUID()` | Allocate a fresh UUID as an IRI or string. | [§§17.4.2.12–13](https://www.w3.org/TR/sparql11-query/#func-uuid) | [§§17.4.2.18–19](https://www.w3.org/TR/2026/WD-sparql12-query-20261119/#func-uuid) |
| `BNODE()`, `BNODE(str)` | Allocate blank nodes. A repeated string can reuse the node within one solution mapping. The string does not give it an identity across evaluations. | [§17.4.2.9](https://www.w3.org/TR/sparql11-query/#func-bnode) | [§17.4.2.14](https://www.w3.org/TR/2026/WD-sparql12-query-20261119/#func-bnode) |
| `LIMIT`, `OFFSET` | Slice the solution sequence. Missing, tied, or incomparable sort keys can change which solutions cross the slice boundary. | [§§15.1, 15.4–5](https://www.w3.org/TR/sparql11-query/#modOrderBy) | [§§15.1, 15.4–5](https://www.w3.org/TR/2026/WD-sparql12-query-20261119/#modOrderBy) |
| `SAMPLE`, `GROUP_CONCAT` | Choose an arbitrary member or concatenate members in an unspecified order. Sorting a subquery does not impose an order on the aggregate's multiset. | [§§18.5.1.7–8](https://www.w3.org/TR/sparql11-query/#defn_aggGroupConcat) | [§§18.6.1.7–8](https://www.w3.org/TR/2026/WD-sparql12-query-20261119/#aggGroupConcat) |
| `REDUCED` | Allows the evaluator to vary how many duplicate solutions it keeps. | [§15.3.2](https://www.w3.org/TR/sparql11-query/#modReduced) | [§15.3.2](https://www.w3.org/TR/2026/WD-sparql12-query-20261119/#modReduced) |

Flag every slice initially, including nested slices and slices with `ORDER BY`. The
presence of a sort clause says nothing about uniqueness, boundness, or comparability.
A future rule can admit a slice given a proof that any solutions whose relative order
can change are indistinguishable to the rest of the query, or that the sort keys break
all observable ties. This requires reasoning about keys at the slice's own query level.
Outer sorting cannot recover rows discarded in an inner slice.

A strict mapping also needs to flag blank nodes created in CONSTRUCT templates, including
template positions nested in RDF 1.2 terms. BGP blank node labels are scoped matching
syntax. Terms arriving from the input graph need a separate result-level policy where
blank nodes are allowed. For graph isomorphism, consistent renaming of blank nodes alone
is immaterial. For exact RDF terms or stable identifiers, fresh blank nodes are unsuitable.
[SPARQL 1.1 §16.2.1](https://www.w3.org/TR/sparql11-query/#templatesWithBNodes)
[SPARQL 1.2 §16.2.1](https://www.w3.org/TR/2026/WD-sparql12-query-20261119/#templatesWithBNodes)

The absence of these flags is one condition for repeatability. A deployment also needs a
fixed dataset, fixed base and bindings, fixed engine and options, and a policy on extension
functions and aggregates, `SERVICE`, and engine-defined `DESCRIBE`. Refuse unknown extensions
in strict mappings unless their implementations are explicitly known to be pure. Continue
to enforce each consumer's existing SERVICE rules independently of classification. Review
other order sensitivities before promising portability across plans or engines, including
MIN/MAX with incomparable or tied RDF terms and floating-point accumulation in SUM/AVG.
[SPARQL 1.1 §§14, 16.4, 17.6, 18.5.1](https://www.w3.org/TR/sparql11-query/#BasicFederatedQuery)
[SPARQL 1.2 §§14, 16.4, 17.6, 18.6.1](https://www.w3.org/TR/2026/WD-sparql12-query-20261119/#BasicFederatedQuery)

## One parsed-algebra traversal

Extract the shape of `calls_now` from the [PR #101 branch at a88ce82](https://github.com/ikigai-rs/ikigai-store/blob/a88ce82431eb9ac1988546bf7444c64d2481bf7d/src/now.rs).
Take a borrowed `spargebra::Query` and return a set of typed reasons. The walker starts
with all four query forms and visits each graph pattern, expression, and aggregate.
Include both join arms, OPTIONAL's condition, FILTER, Extend for projection expressions
and BIND, sort keys, aggregate arguments, and patterns inside EXISTS and subqueries.
Match named built-in variants and traverse function arguments even when a call already
contributes a reason. Collecting every reason allows a consumer to remove the clock
dependency while still seeing an independent slice or UUID call. Keep structural matches
exhaustive so new algebra shapes require review. Run after existing parse and complexity
guards, on the same parsed query that goes to the evaluator.

Put the production walker in an opt-in SPARQL algebra module in `ikigai-rdf` in
`ikigai-linkeddata`. Store and the linkeddata SPARQL crate already depend on this shared
crate. Markdown currently depends on store for evaluation guards and would take a direct
dependency on the same shared module. A store export would save that manifest edge, but
would make store own a language policy also needed by consumers with no durable store.
Keep the module independent of Oxigraph evaluators, endpoints, and cache APIs. Align the
`spargebra` version with the evaluators so the borrowed types unify. Test the `sep-0006`
(`Lateral`), RDF 1.2, and wasm combinations because Cargo unifies dependency features.
[Shared crate](https://github.com/ikigai-rs/ikigai-linkeddata/tree/main/crates/ikigai-rdf)
[Markdown manifest](https://github.com/ikigai-rs/ikigai-markdown/blob/main/Cargo.toml)

## Consumer policy

- **Store.** Evaluate flagged reads normally, returning the answer as uncached across
  broad and scoped query doors. Existing freshness coverage still applies to eligible
  queries. Report the detected reason in tracing. Ensure no pre-policy cache entries
  survive on a kernel that adopts the policy, since the kernel looks up a hit before
  dispatching the request. Deploy with a fresh kernel cache or change the cache namespace
  if caches can span deployment.
- **Repeatable Markdown mappings.** Use the same check in lint and evaluation. Refuse a
  mapping with a hazard before running its CONSTRUCT and return a diagnostic naming the
  mapping and construct. Validate all mappings before returning any derived output.
  Replace generated IDs with IRIs derived from encoded or hashed stable input keys.
  Replace SAMPLE with MIN/MAX only when the domain and ordering make the chosen RDF term
  unambiguous. For ordered concatenation, collect and sort in a stage that defines the
  sequence explicitly. Document the change for existing mapping authors.
- **Clock-aware rewriting.** PR #100 proposes rewriting every NOW call in the parsed query
  to the same typed `xsd:dateTime` literal from the invocation. Adopt this only for a
  consumer whose contract defines that clock. A live invocation instant still requires
  an uncached answer because the endpoint rewrite runs after cache lookup. A pinned
  instant can qualify when the kernel exposes its provenance and includes it in the key
  and the remaining query passes classification and freshness checks. For a mapping,
  require the instant as an explicit input to the repeatability contract. Preserve the
  semantics of RAND, UUID, STRUUID, and BNODE by bypassing or refusing as appropriate.
  [Store PR #100](https://github.com/ikigai-rs/ikigai-store/pull/100)

## Test boundary

`cargo test --test deterministic_sparql` runs a test-only boolean prototype for the
listed built-ins, slices, SAMPLE, GROUP_CONCAT, and REDUCED. Fixtures include a nested
CONSTRUCT subquery, EXISTS, an aggregate argument, and negative controls for literal
and comment text. A second active test evaluates the UUID CONSTRUCT twice. The ignored
cache assertion preserves the failing reproduction for a future production regression.
CI exercises the two active tests. The prototype does not implement typed reasons,
template blank node detection, extension policy, or consumer enforcement.

## Proposed follow-up items

- **Share a parsed SPARQL repeatability classifier**
  Implement typed reasons in the shared algebra module. Test structural coverage,
  feature unification, and bounded traversal with consumers' real parser versions.
- **Bypass store caching for flagged SPARQL reads**
  Wire the classifier to broad and graph-scoped query doors, preserve cache hits for
  eligible reads, and activate the baseline regression. Verify trace reasons and cache
  rollover behavior.
- **Validate repeatable Markdown mappings before execution**
  Share lint and application validation, promote the UUID reproduction to a refusal
  regression, and document stable input-derived identifiers and migration examples.
- **Bind NOW to an explicit evaluation instant**
  Implement the parsed rewrite and verify live uncached and pinned keyed behavior.
  Test mixed NOW and UUID queries so removing one dependency never hides another.
- **Define ordering and RDF identity guarantees**
  Test equivalent datasets loaded in different orders and compare result multisets,
  graph isomorphism, and serialized bytes where promised. Establish evidence for admitting
  specific ordered slices or aggregate domains and define the output blank node policy.
