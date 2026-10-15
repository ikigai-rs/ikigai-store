# ikigai-store

**The persistent RDF store for [ikigai](https://github.com/ikigai-rs)**: a dataset that
survives a process restart, opened from a path instead of rebuilt from its sources on
every boot.

Nothing else in the ecosystem does that, and the gap is load-bearing: every host that
materializes an expensive graph recomputes it at startup. The reading room's books graph
derives from a 4.3 MB Zotero export; materializing a whole relational database is on the
roadmap. A store that survives a restart is the difference between those being viable and
being a warm-up cost.

```rust
use ikigai_core::Kernel;
use ikigai_store::{space, DurableStore, StoreConfig};
use std::sync::Arc;

let config = StoreConfig::load(Some("my-host"))?;   // ~/.config/ikigai/store.toml
let store = DurableStore::open(&config.path)?;      // takes the write lock, for good
let kernel = Kernel::new(Arc::new(space(store.clone())));
```

### ⚠ `DurableStore::open` ONCE per process, then clone the handle

That `.clone()` is the whole point of the line and it is why this snippet changed.
RocksDB refuses a second open of the same directory **from the same process**, through
its own in-process registry — so a host with more than one kernel (several CLI modes, a
test binary with a dozen), written the obvious way, gets `Error::Unavailable` on the
second one. This is not hypothetical: the first host to bind this crate hit exactly
that and memoized the SPACE, which is the wrong half to share (see the last bullet below):
memoize the store and bind a space per kernel.

`DurableStore` is `Clone` and holds an `Arc<Store>`, so the fix is to open once and hand
each kernel a clone: they share the dataset, the write lock, and the coverage flag.

```rust
let store = DurableStore::open(&config.path)?;      // once, at startup
let one = Kernel::new(Arc::new(space(store.clone())));
let two = Kernel::new(Arc::new(space(store.clone())));
```

⚠ **Each kernel still has its own cache**, and a write through `one` cuts `one`'s golden
threads, not `two`'s. Until ledger #751 that let `two` serve a stale cached read with no
bound while `urn:iki:store:info` said `covered: true` through both. Now the store counts
the spaces bound over it (`DurableStore::spaces_bound`):

- **Two live spaces forfeit coverage for both** — every read is `Expiry::Always` and
  `info` says `covered: false` through each — exactly the cost of handing out the raw
  handle. Drop one and caching resumes.
- **A space bound AFTER another has cached a read refuses every request**, with a
  sentence saying why. Nothing can evict the first kernel's cached answers, so a write
  through the late space would leave them stale; refusing keeps the first kernel correct.
  So **bind every space before the first read** — which a host does at startup anyway.
- ⚠ **One `Arc<EndpointSpace>` handed to two kernels is ONE binding to this crate and two
  caches to the kernels**, so it cannot be seen and is not covered by either rule: a write
  through one kernel leaves the other serving a stale cached read with no bound, while
  `info` says `covered: true` through both (ledger #761,
  `tests/two_spaces.rs::one_space_shared_by_two_kernels_is_the_hazard_this_crate_cannot_see`).
  The same goes for a composite space (a `Fallback`) that contains this one. It cannot be
  closed from here: an endpoint is not told which kernel invoked it, and nothing in this
  crate can reach a kernel's cache. **Bind a space per kernel**, from one memoized
  `DurableStore`.

That is still a property of the kernel and the reason to prefer one kernel per process
where you can.

## What it binds

| resource | verb | what it does | capability |
| --- | --- | --- | --- |
| `urn:iki:store:select` | `Source` | SPARQL SELECT | `urn:cap:store:read` |
| `urn:iki:store:ask` | `Source` | SPARQL ASK | `urn:cap:store:read` |
| `urn:iki:store:construct` | `Source` | SPARQL CONSTRUCT | `urn:cap:store:read` |
| `urn:iki:store:describe` | `Source` | SPARQL DESCRIBE | `urn:cap:store:read` |
| `urn:iki:store:graph-select` | `Source` | SPARQL SELECT, named graphs (one or several) | `urn:cap:store:read:graph:<iri>`, every one |
| `urn:iki:store:graph-ask` | `Source` | SPARQL ASK, named graphs (one or several) | `urn:cap:store:read:graph:<iri>`, every one |
| `urn:iki:store:graph-construct` | `Source` | SPARQL CONSTRUCT, named graphs (one or several) | `urn:cap:store:read:graph:<iri>`, every one |
| `urn:iki:store:graph-describe` | `Source` | SPARQL DESCRIBE, named graphs (one or several) | `urn:cap:store:read:graph:<iri>`, every one |
| `urn:iki:store:info` | `Source` | backing, quad count, coverage | `urn:cap:store:read` |
| `urn:iki:store:graphs` | `Source` | which named graphs you may read | `urn:cap:store:read*` (either form) |
| `urn:iki:store:graph-info` | `Source` | one named graph's quad count, and the scoped write's copy bound | `urn:cap:store:read:graph:<iri>` |
| `urn:iki:store:dump` | `Source` | every quad, every graph, as sorted N-Quads | `urn:cap:store:read`, **and nothing narrower** |
| `urn:iki:store:update` | `Sink` | SPARQL UPDATE, whole dataset | `urn:cap:store:write` |
| `urn:iki:store:graph-update` | `Sink` | SPARQL UPDATE, one named graph | `urn:cap:store:write:graph:<iri>` |
| `urn:iki:store:load` | `Sink` | bulk-load an RDF document | `urn:cap:store:write` |

One IRI per query form, following `ikigai-sparql`: the form fixes the result family, so
it fixes the declared outputs and the default `as` too — and a query of the other
**family** is **refused**, not served under an IRI that promised something else. SELECT
and ASK are one family (result sets) and CONSTRUCT and DESCRIBE the other (graphs), so an
ASK sent to `select` is answered, in a result-set syntax `select` declares; a CONSTRUCT
sent to `select` is refused. That is deliberate — the promise is the outputs — and the
docs once claimed more than the code did (ledger #751).

★ **An argument that is present but unreadable is refused, never treated as absent.** A
`bindings`, `as`, or `load`'s `graph`/`format` passed by reference, by content id, or as
bytes that are not UTF-8 is an `InvalidArgument` naming it (ledger #751). Read as absent,
each did the opposite of what was asked: a query ran unfiltered, a document landed in the
default graph, the default serialization was substituted. `urn:iki:store:load` likewise
refuses a `format` outside the five it declares, rather than parsing anything oxigraph
recognizes (N3's formulas would land in blank-node graphs nothing can name).

⚠ **Which dataset a query reads depends on the door, and neither door is "everything".**
The broad forms read SPARQL's default dataset: a bare `{ ?s ?p ?o }` sees the store's
**default graph only**, and a quad in a named graph is reached through `GRAPH` (ledger
#373). A whole-dataset read is spelled

```sparql
SELECT ?s ?p ?o WHERE { { GRAPH ?g { ?s ?p ?o } } UNION { ?s ?p ?o } }
```

The scoped forms read exactly the graphs named in `graph=` — their merge is the default
graph — and never the store's own default graph.

**A read is not free here.** That is the difference from a query module: `ikigai-sparql`
assembles its dataset per call from sources the caller already named, so gating it would
gate nothing. This store holds standing state the caller did not name — whatever the host
ever put in it.

The namespace is `urn:iki:store:`, **born migrated**. The ecosystem is moving 77
namespaces under `urn:iki:` one at a time, and a brand-new namespace is the only kind
whose migration is free.

## ★ Getting a value in without it becoming syntax

A query and an update are *strings*, so a consumer that interpolates a comment body into
one is standing on a security boundary whether it means to be or not: with
`urn:cap:store:write` in hand, `" } ; DROP ALL ; INSERT DATA { <urn:x> <urn:y> "` is not
a rendering bug. `ikigai-ledger` wrote its own escaper and a hostile-content test;
`ikigai-cli` was the second consumer to face it. There is now one answer here, so there
need not be a third.

**For a query, use `bindings=`** — the value never reaches the SPARQL parser at all.

```text
source urn:iki:store:select \
  query='SELECT * WHERE { ?s <http://purl.org/dc/terms/title> ?title }' \
  bindings='{"title": "\" } ; DROP ALL ; INSERT DATA { <urn:x> <urn:y> \""}'
```

It is a JSON object of variable name → value. A bare JSON string is a plain literal, a
number is `xsd:integer` or `xsd:double`, `true`/`false` is `xsd:boolean`, and the SPARQL
results term shape (`{"type":"uri","value":"urn:…"}`, or `"literal"` with `datatype` or
`xml:lang`) names anything else — the same shape a row comes back in, so a value read out
of one query binds into the next unchanged.

⚠ **Every bound variable must appear in the query's projection.** `SELECT ?s WHERE { ?s
?p ?o }` cannot bind `o`; `SELECT *` can, and ASK, CONSTRUCT and DESCRIBE have no
projection to widen and accept any variable in the pattern. That is oxigraph's rule and
oxigraph enforces it, which settles a design question in the right direction: **a binding
the query does not mention is refused, not ignored**, so a filter you thought was applied
can never silently not be. The refusal explains the projection rule, because upstream's
sentence does not.

⚠ **Except a variable that occurs only inside `FILTER (NOT) EXISTS` or a `MINUS`'s right
side** (ledger #149, measured). It is not in scope in the query's own pattern, so `SELECT *`
does not project it and ASK, CONSTRUCT and DESCRIBE refuse it too. Name it explicitly
(`SELECT ?s ?without WHERE { … FILTER NOT EXISTS { ?s :hidden ?without } }`): the binding is
then accepted and applies inside the `NOT EXISTS`. Since 0.2.22 the refusal says exactly this
instead of suggesting the `SELECT *` the caller already wrote.

**For an update, build the terms** — `urn:iki:store:update` refuses a `bindings`
argument rather than pretending:

```rust
use ikigai_store::sparql::{iri, literal};

let update = format!(
    "INSERT DATA {{ GRAPH <urn:iki:ledger:acme> {{ {} <http://purl.org/dc/terms/title> {} }} }}",
    iri(item, "about")?,
    literal(user_text),
);
```

★ **Nothing in `ikigai_store::sparql` escapes anything, and that is the point.** Each
function builds an `oxigraph::model::Term` and asks *oxigraph* to serialize it: the only
correct escaper for a grammar is the one that owns the grammar. `NamedNode::new` refuses
`>`, a space and `{` at construction — so `iri` refuses rather than mangles, because
quietly storing a different IRI than the caller named is an injection arriving by
politeness — and `Literal`'s serializer escapes `\`, `"`, `\n`, `\r`, `\t` and every
other control character. Both are pinned by tests here, so an upstream change that
weakened either fails in this crate rather than in a consumer.

The asymmetry between the two doors is upstream's: oxigraph's prepared *query* has
`substitute_variable` and its prepared *update* has nothing, and a parsed update's AST is
private. Binding into an update would mean rewriting the update's text, which is string
interpolation with a longer name, in the one place where getting it wrong is `DROP ALL`.

## ★ A per-graph write scope: `urn:cap:store:write:graph:<iri>`

`urn:cap:store:write` is all-or-nothing and it means `DROP ALL`. Because a sub-request
carries the caller's capability unchanged, a module layered over this store makes its
callers hold that authority to append one triple — so the moment such a module is bound,
any session that may file an item may also destroy everything. That is why the first host
to bind this crate kept the store off its served space and off its HTTP door entirely,
and why a host that wants either should reach for the narrow door below instead.

`urn:iki:store:graph-update` is the narrow door. It takes an arbitrary SPARQL UPDATE and
a `graph=` IRI, and requires `urn:cap:store:write:graph:<that IRI>`.

```text
sink urn:iki:store:graph-update \
  graph=urn:iki:ledger:acme \
  content='INSERT DATA { GRAPH <urn:iki:ledger:acme> { <urn:iki:ledger:acme:item:1> <urn:p> "x" } }'
```

**The scope is enforced on effects, not on syntax**, which is the only way it can be
exact. The update runs against a private store holding part of that one graph and nothing
else; if anything lands anywhere else the whole request is refused, naming where it escaped
to (never the data), and otherwise the difference is applied to the real graph in one
transaction. So the shapes that defeat a syntactic check are handled by construction:

| update | what happens |
| --- | --- |
| `INSERT DATA { GRAPH <G> { … } }` | applied |
| `INSERT DATA { <s> <p> <o> }` | **refused** — the default graph is not `G` |
| `INSERT DATA { GRAPH <other> { … } }`, `WITH <other> …`, `COPY`/`MOVE`/`ADD … TO <other>` | **refused** |
| `CREATE GRAPH <other>` | **refused** — no quad, but it registers a graph |
| `DROP ALL`, `CLEAR ALL`, `DELETE WHERE { GRAPH ?g { … } }` | applied to `G` alone — the private copy held only `G`, and emptying `G` is within a grant over `G` |

⚠ **A scoped update cannot READ outside its graph either**, so the same update run
through `urn:iki:store:update` can behave differently. That is deliberate and it is the
right way round: a grant that let one tenant's agent read another's graph in order to
decide what to write in its own would be a filter, not a boundary.

★ **And an update with a `WHERE` needs the READ grant too** (ledger #751). A `WHERE`
reads the graph it matches against, so a caller holding only the write grant could once
read `G` by copying its quads into an escaping pattern and reading them out of the
refusal — or, with the refusal redacted, by guessing a value and watching whether the
update was refused. So:

| update | needs |
| --- | --- |
| `INSERT DATA`, `DELETE DATA`, `CLEAR`, `DROP`, `CREATE` | `urn:cap:store:write:graph:<G>` |
| anything with a `WHERE` — `INSERT/DELETE … WHERE`, `DELETE WHERE`, `WITH`, and `COPY`/`MOVE`/`ADD` (the parser rewrites them into one) | that, **and** `urn:cap:store:read:graph:<G>` (or `urn:cap:store:read`) |

The decision is made on the update's text and the caller's grants **before anything is
evaluated**, so whether it is refused says nothing about `G`. A refusal never quotes a
quad, and a caller who may not read `G` gets `updated <G>` without the `+N -M` counts,
which are a read of their own (`+0` after an `INSERT DATA` means the quad was already
there). The broad door applies the same rule one level up: `urn:iki:store:update` with a
`WHERE` needs `urn:cap:store:read` as well as `urn:cap:store:write`, and both broad write
doors report counts only to a caller holding `urn:cap:store:read`.

★ **And a caller who may not read a graph is not told whether it exists** (ledger #761).
A non-`SILENT` `DROP GRAPH <G>` or `CLEAR GRAPH <G>` fails when `G` does not exist and
`CREATE GRAPH <G>` fails when it does, so each was one bit of `G`'s state per call. For a
caller without the read grant on `G` (on the broad door, without `urn:cap:store:read`),
those three run as if the update had said `SILENT`: the answer is the same success line
whatever `G` held, and the effect is still what was asked for (after `DROP`, `G` is gone;
after `CREATE`, it exists). A caller who may read `G` keeps the standard error.

⚠ **The declared `requires` cannot say this, and is unchanged.** `requires` is ALL-of and
unconditional, while this requirement depends on the update's shape; declaring the read
grant would deny a write-only caller the `INSERT DATA` it is entitled to. So the manifold
offers the door on the write grant — true for every update without a `WHERE` — and the
summary and the `content` argument state the rest.

Three more things, each of them a decision rather than an omission:

- **A grant names exactly one graph.** `Capability::allows` is exact-match set
  membership, and inventing prefix semantics for one token would make this crate's grants
  mean something other than every other grant in the system. A host that wants a caller
  to write three graphs grants three scopes — which is the intended use, and several
  different graph scopes held by several different callers at once is the case this was
  designed for.
- **`urn:cap:store:write` does not satisfy this door**, and is not meant to. A broad
  holder uses `urn:iki:store:update`, which copies nothing and sees the whole dataset.
  Accepting both here would mean the declared scope was not the enforced one.
- **A scoped write copies only the part of the graph the update can observe** (0.2.13,
  ledger #1132): the quads matching some pattern of its `WHERE`, its `DELETE` templates or its
  `DELETE DATA`, by their constants. A ledger append names its counter and its key, so it
  copies a quad or two however large the ledger is: on the RocksDB backing, an append took
  23, 144 and 614 ms in debug at 100, 750 and 3,000 items while the whole graph was copied
  per write (3, 25 and 122 ms in release), and now takes about 1 ms in debug and 0.15 ms in
  release at all three (`tests/scoped_append_cost.rs`). An update that observes the graph
  as a whole still copies all of it: `CLEAR`, `DROP`, a pattern with no constant
  (`?s ?p ?o`), and a path that can be zero-length (`p*`, `p?`) or is negated (`!p`). The
  boundary does not depend on which quads are copied — the private store never holds
  anything outside the graph — and `src/confine.rs` gives the argument, with a differential
  test, that the update computes the same answer on the part as on the whole. The unscoped
  door copies nothing.
- **…and the copy is bounded** (ledger #146): at most 250,000 quads per update unless the host
  sets another bound (`DurableStore::with_max_scoped_copy_quads`). A whole-graph copy is held
  three times at about 2 KB a quad: measured in release, 3.7 s and +538 MB at 250,000 quads,
  9.3 s and +2.2 GB at a million, 40 s and +4.9 GB at three million
  (`tests/scoped_copy_measure.rs`). Past the bound the update is refused with nothing applied,
  and the refusal names `urn:iki:store:update`, which evaluates in place and copies nothing.
  `urn:iki:store:graph-info` reports the bound beside the graph's size. ⚠ The bound applies
  only to a caller who may **read** the graph: for a write-only caller, refused-or-not would
  say whether the graph holds more than the bound, the one-bit oracle ledger #761 closed. A
  write-only caller's whole-graph updates are exactly `CLEAR` and `DROP` (anything with a
  `WHERE` needs the read grant), and the time budget bounds those.

`urn:iki:store:load` keeps the broad scope: a graph-scoped caller has no need of it,
since `INSERT DATA { GRAPH <G> { … } }` through the narrow door does the same work under
the narrow grant.

## ★ A per-graph read scope: `urn:cap:store:read:graph:<iri>`

The write scope alone left the boundary with a documented bypass. A module enforcing its
own read capability over the graph it owns — a named ledger, a layer, a tenant's
annotations — could be gone around entirely by a caller who holds `urn:cap:store:read` and
queries this store directly. **A boundary with a documented bypass is not a boundary**, so
0.2.2 closes the other half.

`urn:iki:store:graph-{select,ask,construct,describe}` each take a `graph=` IRI and require
`urn:cap:store:read:graph:<that IRI>`.

```text
source urn:iki:store:graph-select \
  graph=urn:iki:ledger:acme \
  query='SELECT ?item ?filed WHERE { ?item <urn:filed> ?filed }'
```

**The confinement is by construction, not by inspection**, and this is where it differs
from the write half. A query's dataset is a first-class thing in SPARQL and oxigraph
exposes it: the prepared query's dataset specification is set before evaluation, so
`graph=G` means exactly `FROM <G> FROM NAMED <G>` — written through the API rather than
into the query text. Nothing is copied and nothing is diffed. What that does to the shapes
that defeat a syntactic check:

| query | what happens |
| --- | --- |
| `{ ?s ?p ?o }` (no `GRAPH` block) | reads `G` — it *is* the default graph |
| `GRAPH <G> { … }` | reads `G` |
| `GRAPH <other> { … }` | **refused**, naming `<other>` (since 0.2.22, ledger #388); through 0.2.21 it matched nothing, an empty result |
| `GRAPH ?g { … }` | binds `?g` to `G` and to nothing else |
| a sub-select, `FILTER EXISTS`/`NOT EXISTS`, a property path over another graph | confined the same way |
| `DESCRIBE <s>` with no pattern | reads `G` |
| `FROM` / `FROM NAMED` in the query text | **refused** — see below |
| `SERVICE <http://…>` | **refused**, in every build — see `SERVICE` below |

Four decisions worth stating:

- **A constant `GRAPH <other>` is refused rather than answered empty** (0.2.22, ledger
  #388). The confinement already kept it unreachable, but an empty answer to a mistyped
  or forgotten graph looks exactly like an empty graph. The endpoint walks the whole parsed
  algebra (sub-selects, `OPTIONAL`, `MINUS`, `UNION`, `LATERAL` and every `EXISTS`) for
  constant graph names outside `graph=` and refuses with `InvalidArgument` on `query`,
  naming each. The walk is exhaustive over `spargebra`'s algebra, so a variant it does not
  handle is a compile error rather than a silently skipped pattern. `GRAPH ?g` names no
  graph and is unaffected: the dataset keeps it inside the set.
- **`FROM` / `FROM NAMED` is refused rather than overridden.** It is a second way to name
  a dataset, and it cannot widen the scope — the confinement overwrites whatever the
  parser built from it. But answering `FROM <other>` with `G`'s rows would label one
  tenant's data with another tenant's graph name, which is a wrong answer that looks
  right. `graph=` *is* the dataset.
- **The store's own default graph is unreachable from a scoped read.** The default graph
  has no IRI, so no `urn:cap:store:read:graph:` token could name it. A host that keeps
  tenant data in the default graph has put it outside this boundary's reach — which is the
  safe direction, but it is a thing to know before choosing where data lives.
- **`urn:cap:store:read` does not satisfy this door**, and a graph read scope does not
  open `urn:iki:store:select`. Both directions are ablated in `tests/read_scope.rs`.
- **Read and write scopes over the same graph are separate grants.** Neither implies the
  other; a host that means a module to do both grants both.

⚠ **`DESCRIBE` reads the dataset's default graph and nothing else** — that is upstream
behaviour, not this crate's, and it applies to the *unscoped* `urn:iki:store:describe`
too: `DESCRIBE <s>` for a subject that lives in a named graph has always returned nothing
there. Under `graph=G` the default graph *is* `G`, so the scoped door is the one where
describing a tenant's subject works.

⚠ **`ikigai-conformance` cannot check any of this.** Its `AUTHORITY` check is silent on
endpoints that declare a scope, and no check anywhere can see the *parameterized* half of
a wildcard grant — the interesting case is always a caller holding a grant for one graph
reaching for another. `tests/read_scope.rs` is the only evidence, and every test in it is
a pair: the permitted graph comes back **with its rows**, beside the refusal. That pairing
is the point, because empty is a legitimate answer to a query and a confinement that is
wrong in the safe direction is otherwise indistinguishable from an empty graph.

⚠ **A scoped query endpoint has two required by-value inputs, so a bare pipe into one is
ambiguous.** The engine fills the single unnamed *required* argument from a pipe; with
both `graph` and `query` unnamed it refuses with `accepts multiple arguments; name one
with key=value`. Naming the graph — which a caller must do anyway — leaves `query` as the
one unnamed required input, so `… | urn:iki:store:graph-select graph=<G>` pipes normally.

## ★ A scoped read over several graphs — the join (0.2.5, ledger #380)

Until 0.2.5 a scoped read took ONE graph, so a caller holding the grants for a ledger and a
browse graph could read each and never join them — and the join it did write came back
**empty**, not refused, because `GRAPH <other>` inside a one-graph dataset matches nothing.
The tenancy model could say "you may read A and you may read B" and could not run the one
query anyone wants over both.

`graph=` now takes **one IRI or several, separated by ASCII whitespace** (space, tab,
newline — not commas, and not non-ASCII spaces such as U+3000, both of which an IRI may
contain):

```text
source urn:iki:store:graph-select \
  graph="urn:iki:ledger:default urn:iki:browse:graph" \
  query='SELECT ?item ?note WHERE { ?item <urn:annotated-by> ?note . ?note <urn:body> ?b }'
```

- **The dataset is the set, both halves.** `graph="G1 G2"` is exactly `FROM <G1> FROM <G2>
  FROM NAMED <G1> FROM NAMED <G2>`: a bare pattern reads — and joins across — the merge,
  `GRAPH ?g` binds only members, `GRAPH <other>` is refused by name (0.2.22; it matched
  nothing before), and the store's default graph stays unreachable. One graph is the degenerate case of the same rule, so every
  existing single-graph caller is unchanged. (Why not named graphs alone: `src/scope.rs`.)
- ★ **Every graph must be granted, and one missing grant REFUSES the read** with a typed
  `Denied` naming each missing `urn:cap:store:read:graph:<iri>`. It is never answered over
  the graphs the caller does hold: a dataset quietly narrower than the one asked for
  returns rows that look right and are wrong, which is the bug class this closes.
- **Whitespace, not commas.** An IRI may contain a comma, so `graph=A,B` is one graph —
  refused, with a note saying what was probably meant. And `graph=A graph=B` does NOT work:
  a request carries one value per argument name and the engine keeps the last, so that
  reaches the store as `graph=B`. Quote the list.
- **Order and repetition do not matter**, to the answer or to the cache: a non-canonical
  spelling is re-issued as the canonical one (sorted, single-spaced), so `"B A"`, `"A B"`
  and `"A B A"` share one computed cache entry. An empty list is refused.
- ⚠ **Duplicates across members are a bag, not a set.** oxigraph evaluates a several-graph
  default graph once per member, so a triple present in two graphs matches a bare pattern
  twice. Use `SELECT DISTINCT` where partitions can repeat a triple; under `GRAPH ?g` one
  row per member is the right answer anyway.
- ★ **A read over several graphs is cached only if EVERY graph in it is covered.** In a host
  whose sharer writes one of the graphs — gonk's browse graph is exactly that — **a join
  including that graph is never cached**, while a read of the covered graph alone still
  is. That is correct, not a regression; it is written here for whoever wonders why the
  join is slow.

`tests/multi_graph_read.rs` is the evidence, and the escape probes over a set (property
paths, sub-selects, `SERVICE`) are beside the mechanism in `src/scope.rs`.

## ★ Enumeration: `urn:iki:store:graphs` (0.2.5)

A graph-scoped read is confined to a graph the caller **already named**, so it enumerates
nothing. That leaves a module partitioning its state by graph — a named ledger, a tenant, a
layer — with no way to answer *which partitions are there*, and the workaround every such
module writes is the same two-path branch: its own grant list as the candidate set, and a
broad store query for root, through a door the module does not declare.

`urn:iki:store:graphs` is that answer, once, in the crate that owns the boundary.

**The name is a noun, and that is the rule rather than a preference.** The resource *is*
the graphs; `-list` would have named the shape of one representation instead of the thing,
and the representation is the part of a resource that is free to change. It carried that
name while 0.2.5 was being built and was renamed outright before publishing — the only
window in which correcting a name costs nothing, because a name with no consumers has
nobody to keep faith with.

```text
source urn:iki:store:graphs
urn:iki:ledger:graph:acme
urn:iki:ledger:graph:bosatsu
```

Sorted IRIs, one per line, no angle brackets; an empty body when there is nothing to list.

**It answers `exists AND may-read`, and the argument is that only one half is new.** The
may-read half is already in the caller's hands — it *is* the caller's capability — so a
resource serving it back would be a round trip for something the caller could compute.
Existence is the half the caller cannot know, so it is the half worth serving, and the half
that has to be gated. ("Both, distinguished" was considered and is not answerable
uniformly: root's may-read set is not enumerable, which is what root means, so the
granted-but-absent column would be empty for root and populated for a tenant — two
different documents under one IRI.)

**It is not an oracle**, and that is the property to re-check before changing anything
about it. A tenant's lines are a subset of the graphs its own grants name: existence is
disclosed only for a graph the caller may already read. A caller learns nothing about a
graph it holds no grant for — not that it exists, not that it does not, not how many there
are.

⚠ Precisely: `urn:iki:store:graph-ask` over its own graph already tells a tenant whether
that graph holds a quad, so almost every line here was reachable before. The one thing this
adds is the **registered-but-empty** case — a graph created by `CREATE GRAPH` with nothing
in it, which an `ASK` cannot see. That is still a graph the caller holds a grant for, so it
crosses no boundary; it is just not literally true that nothing new is disclosed.

**Two paths, one shape.** A caller holding `urn:cap:store:read` — the broad reader, and
**root**, which allows every scope — may read the whole dataset, so its answer is every
named graph in it, read from the store. Any other caller holding a per-graph grant gets its
own candidates confirmed one point lookup each. Both produce the same bytes in the same
order for the part of the store they can both see, so nothing that reads this resource has
to know which kind of caller it is running as.

⚠ **The default graph is never listed.** It has no IRI, so no grant names it and no scoped
read reaches it — an empty answer does **not** mean an empty store. Blank-node graph names
are skipped for the same reason.

### The declared scope is `urn:cap:store:read*`, and the `*` is not at a segment boundary

`Description::requires` is **all-of**, and the honest requirement here is **any-of**: a
tenant holds `urn:cap:store:read:graph:<iri>` grants, a broad reader holds
`urn:cap:store:read`, and both are asking the same question. Declaring both tokens would
demand both and deny each of them; declaring one would deny the other at the kernel's
pre-check, before the endpoint can say why. So the declaration names the family both tokens
belong to, and the endpoint enforces which half the caller actually holds — the same
declared-wildcard / enforced-exactly shape as `urn:cap:store:read:graph:*`, one segment
further up.

The predicate is a plain prefix match with the `*` stripped, so `urn:cap:store:read*` is
satisfied by exactly the read family: `urn:cap:store:read:*` would miss the broad token (no
trailing colon on it), and `urn:cap:store:*` would admit a write-only caller, who would
then pass the pre-check and be handed an empty listing — which reads like an answer instead
of the denial it should be. `tests/graphs.rs` pins all three.

⚠ **This is `ikigai-core` PENDING §57's any-of problem arriving in a third place.** With an
any-of form in `requires`, the declaration would be `any_of([CAP_READ, CAP_READ_GRAPH])` and
nothing else here would change.

### Cacheable under the three write threads, per candidate

The answer depends on which graphs exist, and on a covered store a graph can only come into
existence through one of this crate's three write doors — so it is cacheable under exactly
`urn:iki:store:{update,load,graph-update}` and needs no fourth thread. The kernel keys its
cache on the capability fingerprint, which is what makes an answer that differs by caller
safe to cache at all.

On a **shared** store the root/broad listing is live, like every other read that sees the
whole dataset: an invisible writer can create a graph. A tenant's listing under a
`SharerWrites` declaration is cacheable **iff every one of its candidates** is a graph the
sharer cannot write — one writable candidate is enough for an invisible write to change the
answer.

⚠ **Not to be confused with `DurableStore::reserved_graphs_fingerprint`.** That one
enumerates graphs internally for a host's own tripwire, errors without a `SharerWrites`
declaration, scans every quad and is documented test-time only. This one is public,
capability-scoped and for production callers. Two different questions, and the reason no
public `named_graphs()` accessor was ever added to `DurableStore`: a raw list on the store
type bypasses the read boundary entirely.

## ★ One graph's size: `urn:iki:store:graph-info` (ledger #147)

`urn:iki:store:info` counts the whole store under the broad `urn:cap:store:read`, so a tenant
holding only `urn:cap:store:read:graph:<G>` could not ask how large its own graph is.
`urn:iki:store:graph-info` answers under exactly that grant:

```text
source urn:iki:store:graph-info graph=urn:iki:ledger:acme

graph: <urn:iki:ledger:acme>
exists: true
quads: 27013
graph-update copy bound: 250000
```

The last line is the scoped write door's copy bound, in the same unit as the count, so a tenant
can see how close a whole-graph rewrite is to being refused before sending one. ⚠ Counting is a
scan of the graph (oxigraph keeps no per-graph size), run within the caller's time budget and
cached under the three write threads when the graph is covered. Like the scoped query doors it
takes the per-graph grant only: a broad reader counts with
`SELECT (COUNT(*) AS ?n) WHERE { GRAPH <G> { ?s ?p ?o } }`.

## ★ The whole dataset out: `urn:iki:store:dump` (ledger #374)

There was a way in for a whole dataset (`urn:iki:store:load`) and no way out, so the first host
that needed a backup wrote `SELECT ?g ?s ?p ?o`, held the result set in memory and turned it
back into N-Quads by hand, owning literal escaping, language tags and datatypes, and knowing in
advance that CONSTRUCT folds every graph into one and that an unconfined query reads the default
graph only. `urn:iki:store:dump` reads the store's quads directly, default graph included, and
writes them with oxigraph's own N-Quads serializer:

```text
ikigai --quiet -c 'source urn:iki:store:dump' > backup.nq
ikigai --quiet -c 'sink urn:iki:store:load format=application/n-quads' < backup.nq
```

- ⚠⚠ **It requires the BROAD `urn:cap:store:read`, and no per-graph grant satisfies it.** A dump
  reads every graph, which is exactly the authority the per-graph tenancy boundary exists not
  to hand out. Grant it to a backup job, never to a tenant; a tenant reads its own graph through
  `urn:iki:store:graph-construct`.
- **The lines are sorted**, so two dumps of the same dataset are the same bytes on either
  backing and a backup diffs. N-Quads is one quad per line, so sorting reorders quads and
  changes none.
- **Bounded, and refused past the bound, never truncated.** A representation is bytes, so the
  dump is built before it is answered: what is streamed is the inside, one pass from the store's
  iterator through the serializer with no result set in between. It takes the query doors'
  answer budget (quads against `max_rows`, bytes against `max_bytes`: 100,000 quads and 16 MiB
  by default, up to 10,000,000 and 1 GiB for root or a `urn:cap:store:answer:*` grant) and the
  caller's time budget. A dataset past either is refused, saying which.

## Three things a consumer cannot learn any other way

Each of these cost a consumer real time, and none of them is visible from the API.

**Multi-operation atomicity is real, and you may depend on it.** One request whose
`content` is several `;`-separated operations is applied atomically: a malformed
operation anywhere means none of it ran. A state change split across three *requests* is
observably half-applied; the same change as one request is not. `ikigai-ledger` found
this the hard way and now pins it with a test. The same holds through
`urn:iki:store:graph-update`, where a refusal happens before anything reaches the real
store.

**Values come back in the engine's canonical lexical form — never compare against what
you wrote.** Write `2026-09-13T00:00:00.000Z` and read back `2026-09-13T00:00:00Z`. A
strict parser returned `None`, rows were silently dropped, and every newly filed item
read back as if it had never been written — a failure that looks like a broken UPDATE and
is not. Compare typed values after parsing them, or compare in SPARQL, where
`xsd:dateTime` equality is by value.

**Composition is N sub-requests per operation, with no shared-transaction seam.** The
`Store` handle is `pub(crate)` on purpose — see the cacheability section below, where
handing it out is a constructor-level decision with a permanent cost — and the
consequence is that an in-process module built over this store reaches it the same way a
remote one does: through the kernel, one request at a time. There is no way to put a read
and a write in one transaction across that seam. Where a module needs atomicity, it needs
it *inside* one request: one `;`-separated UPDATE, which is exactly the guarantee above.

## ⚠ One writer per directory — the constraint that shapes everything

RocksDB permits one writer per directory and enforces it with a `LOCK` file. That is a
property of the storage engine, not a detail, and it is why this module is
**service-shaped**:

- `DurableStore::open` takes the lock **for the life of the process** and does not give
  it back. A second host trying the same directory gets a legible `Error::Unavailable`
  naming the path — not a panic out of RocksDB's guts.
- So a second process reaches this data **over the wire**: ikigai's IPC and QUIC
  transports and mount-over-wire federation. The runbook for that is the owning host's.
- A read-only opener is **not** the way around it. `Store::open_read_only` succeeds
  beside a live writer and is a **frozen snapshot** — it never sees a later commit, and
  nothing says so. This crate offers no read-only face, and
  `a_read_only_opener_is_a_frozen_snapshot` keeps the reason reproducible.

The alternative — open per request, so the lock is held only during a call — was costed
against measurement and rejected. [`docs/design/handle-model.md`](docs/design/handle-model.md)
has the numbers (`Store::open` is ~5 ms and **flat** in population; a second open in the
*same* process is refused), the decision, and what would reopen it.

## Cacheable reads, and the one thing that forfeits them

One process owning every write is exactly the **coverage** `ikigai-sparql`'s
`UPDATE_THREAD` documents as the missing precondition for caching a shared store. The
kernel cuts the thread named after a mutating request's target, so a read depending on
`urn:iki:store:update` and `urn:iki:store:load` is invalidated by every write the kernel
can see — and under `DurableStore::open` there are no others.

⚠ **Handing out the `Arc<Store>` hands out a writer the kernel cannot see.** That is not
held by prose: it is held by the constructors.

```rust
let owned = DurableStore::open(&path)?;           // reads .cacheable() under the
                                                  // write threads
let (shared, handle) = DurableStore::open_shared(&path)?;  // reads Expiry::Always,
                                                           // permanently
```

There is no accessor that turns the first into the second. A host that wants
`ikigai_sparql::space_with_store` over this dataset asks for the `_shared` constructor
and pays for it in cacheability, at the call site, on the line where the choice is made.
`urn:iki:store:info` reports `covered: true|false` so an operator can see which it got.

### …unless the host can say where the sharer writes (0.2.4)

That blanket is one bit for the whole store, and it is expensive in a way nothing
catches: expiry **propagates**, so a module whose reads are sub-requests to
`urn:iki:store:graph-select` silently loses its own caching the day its host adds a
second face over the same dataset. Measured here on a 250-item tenant graph: **1.05 ms
per read shared, 32.5 µs owned** (`tests/shared_coverage.rs::measure_the_cost_of_sharing`).

A **scoped** read is confined to the named graphs it names by construction, and the
default graph has no IRI, so no scoped read can reach it. (Over several graphs, the read is
covered only if every one of them is.) A host that knows its sharer writes only the
default graph therefore knows the sharer cannot change any scoped read's answer:

```rust
// `ikigai-browse` writes annotations, explanations and review passes into the default
// graph, hard-coded. Say so, and every scoped read is cacheable again — 31.1 µs.
let (store, handle) = DurableStore::open_shared_declaring(
    &path,
    SharerWrites::only_the_default_graph(),
)?;

// A sharer that also owns one named graph of its own names it; reads of THAT graph stay
// bare, reads of every other named graph do not.
let (store, handle) = DurableStore::open_shared_declaring(
    &path,
    SharerWrites::only_the_default_graph().and_named_graph("urn:iki:browse:notes"),
)?;
```

The broad faces and `urn:iki:store:info` see the whole dataset, default graph included,
so they stay `Expiry::Always` on any shared store whatever was declared.

⚠⚠ **This is a promise, and a false one is silent, unbounded staleness** — the worst
failure this crate has, and the exact thing `with_freshness` exists to prevent. Declare a
graph the sharer does write and reads of it are cached against threads that write never
cuts: pre-write bytes, for the life of the process, with no error and no signal. So the
promise names every graph, there is no "trust me" form, and a host that does not know
keeps `open_shared`, which costs only speed.

**Pin it in your own tests** rather than trusting the prose —
`DurableStore::reserved_graphs_fingerprint()` is the tripwire, taken before and after
driving the sharer:

```rust
let before = store.reserved_graphs_fingerprint()?;
// …drive the sharer exactly as this host does…
let changed = store.reserved_graphs_fingerprint()?.changed_since(&before);
assert!(changed.is_empty(), "the sharer wrote {changed:?}, which it promised not to");
```

It fingerprints **quads**, not the set of graph names, so it catches a sharer writing
into a graph that already exists as well as one creating a new graph — and it refuses
outright on a store that declared nothing, rather than passing vacuously.

## Cache ejection: a file, not a shared store

`ikigai-core/docs/design/cache-ejection.md` works out cross-process cache export and stops
because there is nowhere durable to put it. The obvious reading — two instances sharing
one store — collides head-on with the one-writer rule above, so the design settles the
other way: **the bundle is a file the second instance imports into its own store.**

Nothing exports and nothing imports yet. [`docs/design/cache-bundle.md`](docs/design/cache-bundle.md)
records which of that design's constraints land here (four of its five are already
satisfied by `urn:iki:store:load` being a capability-gated, thread-cutting write of
untrusted input), which one this crate must not pretend to satisfy, and the one thing that
must *not* be built here.

## Features

| feature | what it adds | cost |
| --- | --- | --- |
| *(default)* | in-memory only, wasm-clean | — |
| `persistent` | `DurableStore::open`, the RocksDB backend | `oxrocksdb-sys` (~494 s of CPU on a cache miss) and `libclang` in the toolchain |
| `rdf-12` | nothing you would use — it turns on `oxigraph/rdf-12` so CI compiles this crate the way a consumer's graph does | a few seconds of `oxrdfio`/`spareval` |
| `http-client` | nothing you would use — it turns on `oxigraph/http-client` so CI tests the `SERVICE` refusal the way `ikigai-cli`'s graph builds it | `oxhttp` and `url`, a few seconds |

The same trade `ikigai-cli` makes for `quic` and `web`: in-memory is the default so the
wasm face and cheap tests survive, and the heavy backend is opted into.

⚠ **Cargo feature unification is global and additive**, and `rocksdb` is a *default*
feature of `oxigraph` that this crate turns off. The moment any crate in a host's graph
enables `persistent`, **every** `oxigraph` consumer in that build gets the RocksDB
backend, `ikigai-sparql`'s copy included. That is exactly what makes the
`space_with_store` composition work — one `Store` type either way — and it means "off by
default" is a property of the whole build, not of this crate.

### ★ `rdf-12` is a gate, and 0.2.2 is what it is for

Unification cuts the other way too, and that is how **0.2.2 shipped to crates.io not
compiling for `ikigai-cli`**. `oxrdf::Term` grows a fourth variant under `rdf-12`;
`ikigai-cli` has a crate that enables it (rudof, through `ikigai-shacl`); an exhaustive
`match` here stopped being exhaustive there, with `error[E0004]`. Nothing this crate
declared could see that, because the feature belonged to a *dependency* and was turned on
by a *sibling*.

`ci.yml` therefore passes `features: "*"` — `--all-features`, which keeps `persistent` on
(the durable backend lives behind a `cfg(feature)` inside a *library*, which is not a
target, so `--all-targets` never reaches it and `ci-drift`'s `required-features` check
cannot see the omission) and additionally reaches `rdf-12`.

⚠ **Know what that does not cover.** `--all-features` enables the features *this manifest
declares*; it cannot enumerate what a consumer's graph might switch on. It catches this
break because `rdf-12 = ["oxigraph/rdf-12"]` was added by hand for it to reach. A future
unification break through a feature nobody declared here would be exactly as invisible as
this one was, and the gate that covers the class is a build of this crate inside a real
consumer's graph, run on a clock that ticks when nothing happens.

⚠ **A consumer's graph can also change what lands on disk.** Oxigraph's binary encoder
gives quoted triples and directional language strings their own on-disk type bytes, all
of them `#[cfg(feature = "rdf-12")]`. Read from the encoder, not measured end to end: a
build without the feature has no arm for those type bytes, and the decoder's answer to an
unknown one is `CorruptionError`, so any quad an `rdf-12` build wrote using such a term
cannot be read back by a build without it. For a *persistent* store that makes a sibling
crate's feature a data-format decision, and dropping that sibling later a downgrade.

So a host that persists a store should know which kind of build it is (ledger #153). In the
host's workspace:

```text
cargo tree -e features -i oxigraph --prefix none | grep 'oxigraph feature "rdf-12"'
```

A line means the build writes the RDF 1.2 encodings, and every later build that opens the same
directory has to keep the feature on (`ikigai-store`'s own `rdf-12`, or the sibling that brought
it). ⚠ `urn:iki:store:info` does not report this, deliberately: it could only report the build
that is RUNNING, and the hazard is about the build that WROTE the data, which an older or newer
binary on the same directory does not know. Making that visible would mean recording the
writer's encoding in the store itself, which is a data-format decision, not a field.

### ⚠ `error[E0277]` inside `spareval` after `cargo add`: two `digest` crates (ledger #1148)

If adding this crate to a project that already has a `Cargo.lock` ends in

```text
error[E0277]: the trait bound `CoreWrapper<CtVariableCoreWrapper<Sha256VarCore, _, _>>: Digest` is not satisfied
   --> …/spareval-0.2.7/src/expression.rs:1146:70
```

the lock is the problem, not your code and not this crate's. `spareval` 0.2.7 (the SPARQL
evaluator under every oxigraph 0.5.10 and 0.5.11) takes `md-5`, `sha1` and `sha2` each as
`>=0.10, <0.12`, **independently**, and then bounds all five hash functions by `md-5`'s
`Digest` trait. So the three must come from one family (`digest` 0.10 or `digest` 0.11),
and nothing in any manifest says so.

**When it happens.** A fresh resolve (no lock, or a plain `cargo update`) takes the highest
of each, 0.11, and builds. An incremental one (`cargo add`, editing a manifest, `cargo update
-p …`) prefers versions already in the lock *edge by edge*, so a lock that already holds
`sha2` 0.10 (very common) keeps it for `spareval` while the new `md-5` and `sha1` arrive as
0.11. Reproduced from scratch: a project depending on `sha2 = "0.10"`, then `cargo add
ikigai-store`, fails exactly as above.

**The fix.** See which edge is behind, then unlock just that copy so `spareval`'s edge is
chosen again:

```sh
cargo tree -p spareval -e normal --depth 1      # md-5 v0.11.0, sha1 v0.11.0, sha2 v0.10.9
cargo update -p sha2@0.10.9                     # the 0.10 copy it names; spareval moves to 0.11
```

Your own `sha2 = "0.10"` keeps its 0.10 copy beside the new 0.11 one; only `spareval`'s edge
moves. A plain `cargo update` works too, and moves everything else with it.

**What does not fix it.** Adding `md-5`, `sha1` and `sha2` to your own (or this crate's)
`[dependencies]` at one shared version: a direct dependency is a separate edge, and Cargo
never ties another crate's edges to it. Measured both ways: direct `0.11` pins beside a locked
`sha2` 0.10 still leave `spareval` on 0.10, and direct `0.10` pins in a fresh resolve still
leave it on 0.11. The defense belongs upstream ([oxigraph/oxigraph#1712]), where one
`digest` family can be required or each hash bounded by its own crate's trait.

[oxigraph/oxigraph#1712]: https://github.com/oxigraph/oxigraph/issues/1712

## ★ A query that would overflow the stack is refused, or run on one big enough (ledger #915)

oxigraph's SPARQL parser and evaluator are recursive, and a stack overflow is not an error
a caller gets back: Rust aborts the **whole process**. Through 0.2.6, `SELECT * WHERE {
FILTER(((…1…))) }` with ~1,000 parentheses — 3 KB — aborted any host that handed it to
this store, at every door that parses caller text (the eight query IRIs and both update
IRIs), and a host's anonymous read grant was enough to reach it.

Two layers now stand in front of the parser, at all ten doors:

| bound | value | what it stops |
| --- | --- | --- |
| `limits::MAX_SPARQL_BYTES` | 1 MiB | any query or update larger, refused before parsing |
| `limits::MAX_SPARQL_NESTING` | 64 | brackets `(` `{` `[`, and runs of `!`, nested deeper — refused before parsing |
| `depth::MAX_RDF_NESTING` | 64 | RDF 1.2 triple terms `<<( … )>>` and `<< … >>` nested deeper — counted apart from the brackets (0.2.19, ledger #1354), refused before parsing |
| the SPARQL thread | 16 MiB + 512 bytes per byte of text (a debug build: + 64 MiB, and 2 KiB a byte) | everything else that recurses, run on a stack sized for it |

**The bounds refuse, never truncate**: the refusal is an `InvalidArgument` on `query` (or
`content`) that names the bound. Real queries nest about 5 deep (a scan of ~700 across the
ecosystem); 64 is an order of magnitude of headroom, and the largest legitimate queries —
`VALUES` lists, `INSERT DATA` — are flat and do not recurse at all, which is why the byte
bound can be generous.

**Why a thread as well.** Nesting is not the only recursion. On a 2 MiB thread (a tokio
worker's), a release build overflows on 1,879 triple patterns, 2,087 `FILTER`s or a 2,086-way
`||` — shapes a generated query can really have, so nothing lexical may refuse them. The
parse, the evaluation and the serialization therefore run on a thread whose stack is
reserved in proportion to the text (an ordinary query's thread is ~16 MiB of address space,
touched only as deep as it recurses), sized from the worst cost per byte measured (~370
bytes of stack per byte, a property path).

**The scan follows every reading.** Counting brackets while skipping strings, IRIs and
comments is wrong in a way an attacker can use: in an expression, `?a<'> ) '` is a
less-than followed by a string, and a scan that skipped `<'>` as an IRI never sees the
string open. So at each `<` where a less-than is grammatically possible, the scan follows
both readings and refuses on the deeper (`src/limits.rs` has the argument). `<<(` and `)>>`
are single tokens, and the scan reads them so: through 0.2.18 it read `<<(`'s `(` as an
expression's, every level of a triple-term template kept one more reading alive, and a template
**7 deep** was refused as too ambiguous to bound (`MAX_SCAN_BRANCHES`), with a message about
less-thans (ledger #1354). Since 0.2.19 a template nests to `MAX_RDF_NESTING`, the depth a load
allows, and the refusal past it names that bound. A text at both bounds at once (64 function
calls around 64 triple terms) recurses 128 levels, which the SPARQL thread holds with room to
spare in either build (`tests/sparql_stack_measure.rs`).

⚠ What this does **not** do, plainly:

- **The thread is sized for the build** (ledger #1003). An unoptimized build spends up to
  ~70× the stack per level, and through 0.2.9 a debug host aborted on a `1*1*…` chain of ~405
  terms, well inside the algebra bound below. A build with `debug_assertions` now adds 64 KiB
  for each of the 1,024 algebra nodes that bound admits, and takes 2 KiB a byte in place of
  512 (an `IN` list is one node however long, and recurses once a member), so a debug and a
  release host admit and answer the same queries. `limits::sparql_stack_size` is the one
  formula. ⚠ It keys on `debug_assertions`, the only compile-time signal there is: a release
  profile that turns optimization off without turning debug assertions on is the one it
  undersizes.
- **On wasm there are no threads**, so only the two bounds apply.
- **It does not bound time.** The evaluator is quadratic in an operator chain: 40,000 `||1`
  terms (120 KB) take ~30 s of a core, 80,000 more than two minutes. The next section does
  (ledger #964).
- **The bounds are constants, not configuration.** They protect the process, not a policy,
  so a per-host knob would only be a way to turn the protection off; a host that needs a
  tighter limit can refuse earlier at its own door with `ikigai_store::limits::check_sparql`,
  which is public for that, and must parse on `limits::on_sparql_stack` if it parses
  SPARQL itself (or on `budget::EvaluationBudget::run`, which sizes the same stack AND bounds
  the time: next section).

`tests/sparql_nesting.rs` reproduces the abort in a child process on a 2 MiB thread at all
ten doors (it fails, with the child killed by `SIGABRT`, against 0.2.6);
`tests/sparql_stack_measure.rs` re-measures every number above when oxigraph moves.

## ★ A loaded document is bounded in triple-term depth (ledger #992)

In a build with RDF 1.2 on — `oxigraph/rdf-12`, which `ikigai-cli` gets by feature unification
through rudof, never by asking this crate — a triple term may be the object of a triple term,
and the RDF library copies, renames and drops a nested one **recursively**, once per level,
inside the parser. Through 0.2.9, `urn:iki:store:load` parsed a caller's document inline on the
caller's thread, under the store's write lock, so about 1,000 levels of `<<( … )>>` (some 22 KB
of Turtle) aborted a debug host on a 2 MiB thread — in Turtle, TriG, N-Triples, N-Quads and
RDF/XML (`rdf:parseType="Triple"`) alike.

Now `depth::check_rdf_nesting` scans the document before the parser sees it, and a document
nesting triple terms deeper than **`depth::MAX_RDF_NESTING` (64)** is refused as an
`InvalidArgument` on `content`, with nothing loaded. The scan keeps no stack and reads the text
**the way the parser's lexer does, in the mode this crate parses in** (strict; oxttl's Turtle
mode for Turtle and TriG, its N-Triples mode for N-Triples and N-Quads, quick-xml's markup rules
for RDF/XML), so a `<<` inside a literal, an IRI or a comment is text, and nothing the parser
reads as code is skipped. That lesson is `ikigai-shacl`'s (PR 17): a scan that read the grammar
instead of the lexer could be bypassed. The Turtle scan is a copy of that crate's, and ledger
#976 is where the two fold into one.

## ★ A loaded document is bounded in size and in what it expands to (ledger #1428)

Through 0.2.20 depth was the load door's only bound. It took a document of any length, and
oxrdfxml resolves an `<!ENTITY>` value against the entities declared before it **at declaration
time**, so a "billion laughs" chain of eleven entities, four references each, made a 525-byte
RDF/XML document expand one reference to ~3 MB, and ~16 levels (~680 bytes) to ~3 GB — parsed on
the store's writer thread, under the write lock. The same review found the same shape in the
other syntaxes without any entities: a Turtle or TriG `@prefix` or `@base`, an RDF/XML namespace
or `xml:base`, and the door's own `graph` argument are each written once and copied into every
statement that uses them. N-Triples and N-Quads write every term out in full, and do not amplify.

So before the store's writer is asked for anything (`limits::check_load`), a document is refused
as an `InvalidArgument` on `content`, with nothing loaded, when it is

- over **`limits::MAX_LOAD_BYTES` (256 MiB)**, or the bound its host set with
  `DurableStore::with_max_load_bytes`;
- RDF/XML that declares an XML entity (`<!ENTITY`, in any case, anywhere: oxrdfxml takes a
  declaration only from those bytes, so the search needs no reading of the markup); a DOCTYPE
  that declares none still loads;
- nested past the depth bound above, or not parseable;
- or when its statements spell out more than the same bound in bytes of terms: it is parsed
  once without storing anything, counting what the parser PRODUCES (ledger #1331's rule).

The document is parsed again on the writer to store it: keeping the statements from the first
pass would hold a document's whole expansion in memory per concurrent caller, where the writer
parses one at a time. A whole-dataset restore as N-Quads passes on the byte bound alone (gonk's
was 84 MB on 2026-10-13, a third of the default); a host restoring a larger one sets a larger
bound. `tests/load_bounds.rs` pins each refusal, and that a refusal is answered while another
write holds the writer.

**SPARQL `LOAD <url>` is refused** at both update doors, as an `InvalidArgument` on `content`,
before evaluation. oxigraph fetches and parses the document itself, with nothing between the two
where a scan could stand: with `oxigraph/http-client` on (it is, in `ikigai-cli`'s graph), a
fetched document 50,000 levels deep aborted a debug host on the SPARQL thread. That fetch is also an
outbound request no `urn:cap:net:*` gates (ledger #145). Without the feature `LOAD` already
failed at evaluation; it now fails at the door in every build. To bring a remote graph in,
source it through the kernel, where the net capability applies, and sink it into
`urn:iki:store:load`.

### ★ `SERVICE` never leaves the process (ledger #1083)

Any host whose graph enables `oxigraph/http-client` gets an HTTP service handler installed in
every plain `SparqlEvaluator` — and rudof_rdf enables it on every native target, so any host
linking `ikigai-shacl` (`ikigai-cli`, `ikigai-web-demo`'s server) has it, whether or not it ever
asked. Through 0.2.9, `SERVICE <http://…>` in a caller's query was then an **outbound request
at every one of the ten doors**, scoped reads and both update doors included, with no
`urn:cap:net:*` anywhere near it (reproduced against a stub on 127.0.0.1: one request per door).
This crate cannot turn a sibling's feature off, and oxigraph's own off switch is itself behind
the feature.

Two layers now close it, in every build, feature or not:

- **Every evaluator this crate builds refuses every service itself.** `src/service.rs` installs
  a refusing default service handler, which oxigraph accepts in every build and which, with the
  feature on, replaces the HTTP one. No `SERVICE` — constant or variable name, `SILENT` or not —
  reaches a network client. A unit test fails on any evaluator built another way.
- **Every door refuses a query or update with a `SERVICE` anywhere in it**, before evaluating
  anything, as an `InvalidArgument` on `query` (or `content`). Not a `Denied` naming
  `urn:cap:net:*`: no grant opens this, so naming one would send a caller looking for a remedy
  that does not exist. To bring remote data in, source it through the kernel, where the net
  capability applies, and sink it into `urn:iki:store:load` — the same answer `LOAD` gets.

**Other crates can use the same layers.** `ikigai_store::service` exports them: `evaluator()`
(the refusing evaluator), `refuse_service` / `refuse_service_in_update` (the door checks) and
`refuse_load`. A crate that evaluates caller SPARQL with its own `SparqlEvaluator` should
build it with `evaluator()` and check at its door. ⚠ For an update it also needs `refuse_load`,
because nothing in the evaluator stops `LOAD`. The module docs have a table of exactly what each
item covers, and a doctest that pins it.

The door checks above word the refusal for **this store** (sink into `urn:iki:store:load`),
which is the wrong advice inside a markdown mapping, a SHACL shape or a script. A crate raising
it at its own door uses `refuse_service_with(&query, arg, remedy)` (or
`refuse_service_in_update_with`): the same walk and the same typed `InvalidArgument`, with a
detail of `SERVICE_REFUSAL` (identical in every crate) followed by the caller's own remedy, or
nothing when it passes `None`. `has_service` / `update_has_service` return the walk alone, for a
crate that words its whole refusal itself (ledger #1094).

`LOAD <url>` is **not** governed by the service handler (oxigraph builds `LOAD`'s client
separately); the door refusal above is the only guard, and it holds. `FROM` / `FROM NAMED` are
never fetched: oxigraph reads them as graph names in the store. `tests/service_egress.rs` pins
all three against a local stub, and CI runs it twice: in the default build, and with this crate's
`http-client` gate feature on (`features: "*"`), where a control proves raw oxigraph really does
reach the stub — so the refusals are not vacuous.

`tests/triple_term_nesting.rs` reproduces the load abort in a child process in every syntax (it
fails, with the child killed by `SIGABRT`, against 0.2.9 with `--features rdf-12`).

### ★ An update cannot grow a stored triple term past the bound (ledger #1333)

`INSERT DATA` text is bounded by the text scan (triple terms against `MAX_RDF_NESTING`), but an
update can wrap a STORED term and store the result: `DELETE { <a> <p> ?t } INSERT { <a> <p> ?n }
WHERE { <a> <p> ?t BIND(TRIPLE(<s>, <p>, …56 levels… ?t) AS ?n) }` adds 56 levels a request (a
`<<( … )>>` template adds up to 64 since 0.2.19, and added 6 before it), and every request is
inside every text bound.
oxigraph encodes, decodes and serializes a stored triple term recursively, so before 0.2.18 the
term grew until a request touching it aborted the host (in a debug build, at about 11,800
levels on the store's writer thread), through `urn:iki:store:update` and `urn:iki:store:graph-update`
alike, by either constructor.

Now both doors keep an invariant: **no update adds a quad whose object nests triple terms deeper
than `depth::MAX_RDF_NESTING` (64)**, the depth a loaded document may have. An update that would
is refused before it commits, as an `InvalidArgument` on `content` naming `MAX_RDF_NESTING`, and
nothing of it is applied, its `DELETE` half included. What it costs:

- **`graph-update`** checks the quads it is about to add, which it has in hand already: free.
- **`update`** commits through oxigraph's own transaction, which does not say what it added, so
  before commit it reads, as the transaction sees them, the quads its `INSERT` templates can
  reach (by each template's subject, predicate and graph constants), and only when the text can
  build a triple term at all (`<<`, `{|`, `~`, `TRIPLE` in any case, or a `\u` escape:
  over-approximate on purpose). An update with constant objects, `INSERT DATA`, `DELETE WHERE`
  and `CLEAR` read nothing extra.

A quad the store held before the update is not the update's doing and is passed over, so a deep
term written before this check does not make every later update fail; copying it to a new quad is
refused like any other.

⚠ What this does **not** reach. A host writing through its own handle (the `Arc<Store>` from a
`*_shared` constructor, or a `SharedWriter` closure) writes what it builds; it can keep the same
invariant with `depth::nests_deeper_than_the_bound`. And a store that **already holds** a term
deeper than about a thousand levels, written before 0.2.18, still aborts whatever reads it:
oxigraph decodes a stored term recursively before any code here sees it, so the read cannot be
refused instead. Delete such a quad through a handle on a thread with a large stack.

`tests/triple_term_growth.rs` reproduces the growth in a child process on a 2 MiB thread, at both
doors and with both constructors (it fails, with the child killed by `SIGABRT`, against 0.2.17
with `--features rdf-12`).

## ★ Every evaluation has a time budget (ledger #964)

Inside the byte and nesting bounds, oxigraph is still superlinear in shapes no lexical bound
can refuse. Measured on 0.2.7 (release build, `tests/sparql_time_measure.rs`):

| query | size | time |
| --- | --- | --- |
| `FILTER(1‖1‖…)`, 40,000 terms | 120 KB | 30 s |
| a property path `:p/:p/…`, 1,000 steps | 3 KB | 19 s |
| a property path, 2,000 steps | 6 KB | over 60 s (killed) |
| 500 triple patterns sharing a subject | 9 KB | over 30 s (killed) |
| a cross product of 6 unconstrained patterns, over 120 quads | 118 bytes | over 30 s (killed) |

So every door that evaluates caller SPARQL (the eight query IRIs and both update IRIs) now
has three layers, aligned with `ikigai-sparql`'s (ledger #964; the hub means to fold the
two into one shared module):

1. **The algebra is bounded before oxigraph plans it.** oxigraph checks its cancellation
   token only where it touches the dataset, and its planner touches none — the 3 KB path
   above spends its 19 s planning and saw the token 18 s after it fired. So the parsed query
   is measured first, and refused with an `InvalidArgument` naming the bound past
   `budget::MAX_JOIN_OPERANDS` (**32** operands in one join; a sequence path counts one a
   step) or `budget::MAX_ALGEBRA_NODES` (**1024** operators in the whole query or update;
   `VALUES` rows, constant `IN` members and `INSERT DATA` quads cost nothing). The largest
   join the ecosystem runs has 11 patterns; the worst plan inside both bounds is ~125 ms.
   ★ **A join's operands are counted the way the planner sees them after its normalization**
   (ledger #1426, #1429): every wrapper it CAN erase (`GRAPH`, `FILTER`, `BIND`, `ORDER BY`,
   `OFFSET`/`LIMIT`, an `OPTIONAL`'s or `MINUS`'s left side, the wider side of a `LATERAL`)
   passes its patterns through to the ENCLOSING join, whether or not it is erased this time.
   Through 0.2.20 a `GRAPH` block counted as one operand, and six blocks of 31 patterns planned
   as one join of 186: 3.8 s on an empty store, uncancellable, on the writer thread when it was
   an update's `WHERE`. The same six blocks each closed by `FILTER(true)`, `OPTIONAL {}` or
   `BIND(?unbound AS ?x)` did the same until the rule covered every erasable wrapper. A
   `UNION` branch, an `OPTIONAL` or `MINUS` right side and a sub-`SELECT` are joins of their
   own. The rule never under-counts; its cost is an over-count when such a wrapper survives
   (two groups of 20 each closed by a real `FILTER` count as one join of 40): split such a
   query, or move a pattern into a sub-`SELECT` (`tests/join_graph_blocks.rs`,
   `tests/join_normalized_wrappers.rs`; the rule is the doc comment on the walk's
   `Cost::join`, kept identical to `ikigai-sparql`'s).
   ★ **To state your own number** (0.2.22, ledger #1464): `budget::measure_query(&parsed)`
   and `budget::measure_update(&parsed)` return an `AlgebraCost` whose `widest_join()` and
   `nodes()` are exactly what the check compares against the two bounds, so a consumer can
   pin how far under them its widest query sits in its own test instead of copying the walk.
2. **A deadline, and the caller is answered at it**, with a typed `Error::Timeout` naming
   the budget — never later, never a partial answer. The evaluation is cancelled, and this
   crate's serializers check the token between rows. **An update that runs out of time
   writes nothing**, then or later: its transaction commits only while the caller is still
   waiting, under the lock the caller takes to give up.
3. **What still cannot be stopped is counted and capped.** Inside the bounds, an aggregate
   or join over rows already in memory (`COUNT(*)` over a cross product) still ignores the
   token. Such an evaluation is counted as overdue (`DurableStore::overdue_evaluations`).
   ★ **The cap is on reads IN FLIGHT** (ledger #1427): a read takes one of
   `TimeBudget::max_overdue` slots (default: a quarter of the machine's cores, at least one)
   before its thread starts and gives it back when the thread ends, so at most that many reads
   evaluate at once, and so at most that many can outlive their callers. A read that finds
   every slot taken waits for one within its own budget, and is refused with a transient
   `Error::Unavailable` if none comes free; while the slots are filled by OVERDUE
   evaluations, a new one is refused at once. Both refusals end by naming the cap, whether it
   is the default or the host set it, the machine's available parallelism, and that
   `TimeBudget::with_max_overdue` raises it (ledger #1500), because the default is 1 on a
   machine under 8 cores and 4 on an 18-core laptop, and the same refusal reads like two
   different failures. Updates run on the store's one writer thread,
   so they hold at most one more core. Through 0.2.20 the cap was checked against the overdue
   count alone, which rises only at a timeout, so eight requests arriving together at a cap
   of 1 all started and left eight cores pinned (`tests/overdue_cap_in_flight.rs`).

`tests/sparql_time_budget.rs` pins all of it, including waiting for an abandoned update's
worker to end and checking the store is unchanged, at both update doors.

### The default, and why

**5 s for every caller** (as `ikigai-sparql`), **120 s for root** and as the most any caller
can be lifted to. The evidence, measured over a copy of gonk's live dataset (361,607 quads,
the largest store in the ecosystem; release build, on RocksDB as gonk runs it): the ledger's
bulk reads (a 43 KB `VALUES` of all 902 items) take 44–232 ms, and reading the whole
348,232-quad browse graph 1.4 s. So the base is 3.5 times the heaviest caller-facing read.

⚠ **Whole-dataset owner work is not inside it.** gonk's BACKUP query — every quad,
`ORDER BY ?g ?s ?p ?o`, 121 MB of JSON — takes **24.6 s** on that RocksDB copy (0.44 s in
memory), and gonk runs it under a scoped job capability, which gets the base. A host running
such a job must grant it a budget (below) or run it as root.

### How a host sets it — per door, through the capability; a caller can only tighten

```rust
use ikigai_store::{budget::{cap_budget, TimeBudget}, DurableStore};
use std::time::Duration;

let store = DurableStore::open(&config.path)?
    .with_time_budget(
        TimeBudget::new(Duration::from_millis(1000))     // what EVERY caller gets
            .with_ceiling(Duration::from_secs(120)),     // root, and the most a grant lifts to
    );
// The anonymous door's capability holds no budget grant: 1 s.
// The signed-in door's holds `cap_budget(5_000)` = `urn:cap:store:budget:5000`: 5 s.
// The backup job's holds `cap_budget(120_000)`; the owner's socket holds root: 120 s.
```

- A capability holding `urn:cap:store:budget:<milliseconds>` gets the largest such grant,
  never below the base and never above the ceiling; root gets the ceiling.
- **Any request may carry `budget=<milliseconds>`, which can only LOWER that** (the same
  argument, rule and wording as `ikigai-sparql`'s). A door may stamp it — overwriting the
  caller's — instead of, or as well as, setting the base.

The capability is the channel because it is the one thing a host already stamps per door
that a caller cannot forge, and it follows a request down its sub-requests — a handler
querying on its caller's behalf runs on its caller's budget, except an ikigai-script run: its
capability is narrowed to the script's authority, so a runner's budget grant never follows it
on its own; a script DECLARES a budget at publish, and its run gets the least of that, its
publisher's grant at publish, the host's ceiling and the runner's own grant. (`ikigai-sparql` puts
its ceiling on the space instead; a store cannot, because two spaces over one store forfeit
caching, so a per-space ceiling could not tell two doors on one kernel apart.) ★ **A caller
cannot raise its own budget**: attenuating a capability only removes grants, removing a
budget grant only lowers the budget towards the base, and `budget=` takes the smaller. A
budget grant is never REQUIRED, so it appears in no endpoint's declared `requires`.

### ⚠ What it does not do

- **An aggregate or join over rows already in memory cannot be stopped partway** (layer 3).
  The caller is answered at the budget and the cores are capped, but the core is not
  released until oxigraph's loop ends. The real fix is upstream: `spareval` checking its
  token in those loops.
- **An overdue update keeps the write lock** until its worker ends, so other writes wait (and
  time out) behind it. It still commits nothing.
- **`urn:iki:store:load` is not budgeted**: it parses RDF, not SPARQL, and is linear.
- **Memory is not budgeted here** — the answer's SIZE is, in the next section (0.2.9).
- **On wasm there are no threads**, so an evaluation runs inline: only the algebra bounds
  apply.

### A consumer's OWN evaluations: `EvaluationBudget` (0.2.18, ledger #1327)

A crate that builds its own oxigraph evaluation over a dataset of its own (intray's `rd
match=`, which asks a caller's ASK of every pending tuple's graph; any crate following
`ikigai_store::service`) got the stack bound from `limits` and nothing about time. The
ikigai-layers audit measured what that costs: a cross join of k unconstrained triple patterns
under one FILTER over all their variables, asked of a 7-triple tuple, grows about 7.5x a
pattern, so a ~300-byte template holds a thread for minutes, once per pending tuple
(`tests/match_time_budget.rs` reproduces it: k=4, 5, 6 take 12 ms, 98 ms, 836 ms in a debug
build, and k=10 extrapolates past an hour). The store's own doors already refused it at their
budget; a consumer's evaluation did not.

`budget::EvaluationBudget` hands the doors' machinery out:

```rust
use ikigai_store::budget::{check_query, EvaluationBudget, TimeBudget};

// ONCE, when the host builds the space, and cloned into it: the clones share the overdue
// count, which is what caps the cores. One made per request caps nothing.
let budget = EvaluationBudget::new(TimeBudget::new(Duration::from_secs(2)));

// Per request: the caller's capability chooses the budget as the doors choose it (a
// `urn:cap:store:budget:<ms>` grant raises it, root gets the ceiling) and `requested` (an
// argument of the consumer's own, if it has one) can only lower it.
let matched = budget.run(&text.clone(), inv.capability, None, move |token| {
    let query = spargebra::SparqlParser::new().parse_query(&text)/* … */;
    check_query(&query, "match")?;               // the algebra bounds still apply
    for tuple in pending {                        // a loop of your own checks the token
        if token.is_cancelled() { break; }
        // evaluator().with_cancellation_token(token.clone()).for_query(query.clone())…
    }
    Ok(ids)
})?;
```

- **The caller is answered at the budget** with `Error::Timeout` naming it, on a thread sized as
  `limits::on_sparql_stack` sizes one, so `run` replaces that call.
- **The token is not a deadline.** oxigraph checks it only where it touches the dataset, and a
  cross product spends its time over rows already in memory: cancelled at 50 ms, the k=7
  template returned ~0.9 s later, and k=8 6.3 s after a 200 ms token (debug). So the worker
  runs on after its caller is answered, holding a core; it is COUNTED
  (`EvaluationBudget::overdue_evaluations`), and while `TimeBudget::max_overdue` are overdue,
  `run` refuses at once with a transient `Error::Unavailable`. That is the price of the
  deadline without a hook inside oxigraph's loops: a burst of such templates can make SPARQL
  (only SPARQL, only through that budget) unavailable until one finishes, rather than let each
  pin another core.
- **A timeout's advice names only an argument your caller can pass** (0.2.20, ledger #1383).
  The doors tell their caller that "a request's `budget=` can only lower it", because they
  take `budget=`; a consumer's sentence leaves that clause out by default, and
  `EvaluationBudget::with_argument(Some("match_budget"))` names the consumer's own argument
  instead (the one whose value it hands `run` as `requested`). Clones keep it.
- ★ **Each `EvaluationBudget::new` is its own count under its own cap — so state yours**
  (ledger #1500). Clones share a count; two `new`s never do, and neither shares the store's
  doors'. The default cap is `budget::default_max_overdue()`, a quarter of the machine's
  available parallelism and at least one, which is **1 on anything under 8 cores**: on a
  4-core CI runner one runaway `sh:select` made every later SHACL validation in the process
  `Unavailable` until it ended (ikigai-rs/ikigai-shacl PR 34). A consumer that would rather
  hold more cores than refuse says so:
  `EvaluationBudget::new(TimeBudget::new(base).with_max_overdue(n))`, and a host decides
  whether consumers share one budget (one cap across them) or each keep their own. The
  refusal at the cap now says which it hit: the cap, whether it is the default or the host
  set it, the machine's available parallelism, and that `TimeBudget::with_max_overdue` raises
  it.

## ★ Every answer has a size bound, and past it is refused (ledger #970)

A deadline bounds how long, not how much. Measured (release build,
`tests/answer_size_measure.rs`): over a 120-quad graph, the 49-byte
`SELECT * WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i }` serializes **1,728,000 rows — 567 MB of
JSON — in 2.2 s**, well inside a 5 s budget, all of it held in memory before the first byte
leaves. So every query door bounds the answer too, by the contract it shares with
`ikigai-sparql` 0.1.13 — whose constants, `AnswerBound`, `effective_answer`, `inline_bound`,
`too_large` and `CappedWriter` this crate's `budget` module copies verbatim, so ledger #976
can fold the two mechanically:

- **Counted:** rows for a SELECT, triples for a CONSTRUCT or DESCRIBE, and serialized
  **bytes** for every form. **ASK is exempt.** Counting happens while serializing, and the
  serializer writes into a buffer that refuses the write crossing the byte bound, so the
  answer never grows past it. That query is now refused after **103 ms**.
- **Refused, never truncated:** `InvalidArgument` on `query`, beginning
  `the answer exceeds 100000 rows; add LIMIT, narrow the query, or ask the host for more`
  (or `… 16777216 bytes; …`, or `… triples; …`). No partial body leaves.
- **Base** `budget::DEFAULT_MAX_ROWS` = **100,000** and `budget::DEFAULT_MAX_BYTES` =
  **16 MiB**; **ceiling** `budget::CEILING_MAX_ROWS` = **10,000,000** and
  `budget::CEILING_MAX_BYTES` = **1 GiB**, which root gets.
- **A grant raises it** — "ask the host for more" means this — monotone like a time grant:
  `urn:cap:store:answer:<rows>` (`cap_answer`) and `urn:cap:store:answer:bytes:<bytes>`
  (`cap_answer_bytes`), each never above its ceiling. A row grant does not raise bytes; an
  export holds both. (Here the bound comes from the capability; in `ikigai-sparql` from the
  space, as with the time budget.)
- **A request can only lower it**, with `max_rows=` and `max_bytes=`. Either one present but
  not inline (a reference, a content id, bytes that are not UTF-8) is refused, never ignored,
  as `budget=` is: ignoring it would answer under the ceiling a door's stamp meant to lower.

```rust
use ikigai_store::budget::{cap_answer, cap_answer_bytes, AnswerBound, AnswerBudget};

let store = DurableStore::open(&config.path)?
    .with_answer_budget(
        AnswerBudget::new(AnswerBound::new(10_000, 4 << 20)?)   // what EVERY caller gets
            .with_ceiling(AnswerBound::CEILING),                 // root; the most a grant lifts to
    );
// A whole-dataset export job holds `cap_answer(10_000_000)` and `cap_answer_bytes(1 << 30)`.
```

⚠ **gonk's backup needs both grants.** Its query (every quad, sorted, SPARQL JSON) answers
**361,607 rows and 121,625,465 bytes** (116 MiB) over the 2026-10-09 dataset, so under the
base it is refused on bytes. Measured on a RocksDB store loaded from that backup: refused
after 25.8–28.8 s under its job scopes as they are, answered in 23.4–23.7 s with
`urn:cap:store:answer:10000000` and `urn:cap:store:answer:bytes:1073741824` added.

⚠ **What it does not bound: memory oxigraph spends before the first row.** An `ORDER BY`,
`DISTINCT`, `GROUP BY` or a join's build side materializes inside the evaluator, where no
row has been serialized yet — which is why the backup above is refused only after its
sort, 25 s in. That memory is bounded by time, not by this.

## ★ Every update on a store runs on one thread (0.2.12, ledger #1127)

Through 0.2.11 each update ran on a thread spawned for it, and on the RocksDB backing that
made every read after a run of writes slower, until the memtable flushed. RocksDB draws the
height of each memtable skiplist node from a thread-local generator seeded from the thread's
id, and a thread spawned after the previous one exited usually gets the same id back, so
every commit drew the same few heights and the skiplists stopped growing upper levels.
Every term a query decodes is a lookup in that memtable, so the cost landed on reads:

| | read after the writes, against a fresh copy of the same data |
| --- | --- |
| 750 writes, debug, 0.2.11 | 1.2x to 8.4x (the store alone); 3.3x to 8.4x (through `ikigai-ledger`'s append) |
| 5,000 writes, debug, 0.2.11 | 3.9x to 37x |
| 750 writes, release, 0.2.11 | 2.8x to 13.9x |
| 750 to 3,000 writes, debug and release, 0.2.12 | 0.94x to 1.10x |

How bad it got depended on which seed a run reused, which is why gonk's CI saw it once and
a rerun passed. Writes through one graph got faster too, because the scoped door reads the
graph it copies: 750 of them took 70 s in a debug build and 33 s in release, and now take
20 s and 3.5 s.

Every update on a store now runs on that store's one long-lived writer thread, which keeps
its generator advancing. Writes were already serialized by the store's write lock, so this
serializes nothing new; reads still run on threads of their own. The thread's stack grows to
fit the largest update it has run and never shrinks.

⚠ **It covers the writes this crate performs** (`urn:iki:store:load` too, since 0.2.15: it
committed on the caller's thread until then, and 750 loads asked from fresh threads left reads
4.8x to 14.8x slower; now 0.88x to 1.21x, ledger #1209), and a host's writes through a
`SharedWriter` (below). A host that writes through the raw handle (`open_shared`), or commits
inside `limits::on_sparql_stack` (which spawns a thread per call), from a fresh thread per
write meets the same degeneration. Write from a thread that lives. `tests/post_bulk_read.rs` measures it and holds a read after 4,000 writes to 3x a
fresh copy; `budget`'s `every_write_on_a_store_runs_on_one_thread` pins the mechanism.

### A host's own writes, on the same thread: `SharedWriter` (0.2.14, ledger #1207)

A host holding a shared store's raw handle does not need a writer thread of its own. Ask the
store for its writer and commit each record as ONE transaction on the store's writer thread,
the thread its update and load doors use:

```rust
let (store, handle) = DurableStore::open_shared_declaring(&path, writes)?;
let writer = store.shared_writer().expect("a shared store has a writer");
writer.write_declared(move |txn| {
    for quad in &record {
        txn.insert(quad)?;
    }
    Ok::<_, StorageError>(())
})?;
```

The record commits whole or not at all (an `Err` from the closure writes nothing), it is
ordered with the store's own writes under the same write lock, a panic in it reaches the
caller and the writer lives on, and a write asked for from inside a write is refused rather
than left waiting for itself, through this type or through the `load` door. `shared_writer()`
is `None` on an owned store, whose every write goes through a kernel door. It changes which
thread a sharer's writes commit on, not what the kernel can see: the reads of a shared store
are cached exactly as before. `SharedWriter::store()` is the same raw handle, so a module can
take this one value for both its reads and its writes.

### ★ Declared = enforced (0.2.15, ledger #1209)

A `SharerWrites` declaration is what lets a shared store cache scoped reads of every graph the
sharer does not write, and a false one is silent, unbounded staleness. Through 0.2.14 a write
through a `SharedWriter` could land in any graph. Now a writer carries its declaration and
keeps it:

- **`write_declared`** hands the closure a `DeclaredTransaction` (insert, remove, extend, and
  reads). A write to a named graph the declaration does not name, or to a blank-node graph,
  returns an `UndeclaredGraph` refusal (recognize it with `UndeclaredGraph::of`), and **the
  whole transaction is rolled back**, even if the closure ignored the refusal. The default
  graph is always writable, as `SharerWrites` has always said.
- **`write`** hands the closure oxigraph's raw `Transaction`, which can write any graph, so it
  is **refused before the closure runs** on a writer carrying a declaration. It stays for a
  writer with nothing declared (`open_shared`), which refuses nothing.

The raw handle a shared constructor returned can still write anything:
`reserved_graphs_fingerprint` stays the tripwire for that.

### A host with a bare `Store`: `SharedWriter::over` (0.2.15, ledger #1207)

A host that opened the store itself (`Store::open`) holds no `DurableStore` to ask. It builds
the same type over its handle, naming what it writes:

```rust
let store = Arc::new(Store::open(&path)?);
let writer = SharedWriter::over(Arc::clone(&store), SharerWrites::only_the_default_graph());
```

| | `DurableStore::shared_writer()` | `SharedWriter::over(store, writes)` |
| --- | --- | --- |
| writer thread | the store's own, shared with its doors | on a handle a `DurableStore` gave out, the store's own; on a store the host opened, the handle's own, started by the first `over` and shared by every later one |
| write lock | the store's own: ordered with every door | the same split: ordered with every door, or with every `over` writer on that handle |
| declaration | the store's (`open_shared_declaring`), or none (`open_shared`) | `writes`, narrowed to the store's declaration, or to the first `over`'s on a store the host opened |

Every `over` on one handle shares one writer (0.2.16 for a handle a `DurableStore` gave out,
ledger #1276; 0.2.17 for a store the host opened itself, ledger #1324). Before that, two
writers on one RocksDB dataset each had a thread and a lock, and a read-modify-write through
one could lose half of a record rewritten through the other (`tests/provenance.rs`: a
15-quad record left as 1 quad).

- A later `over` is **narrowed** to the declaration it joins: a graph the first `over` (or the
  store) did not declare is refused through it. Make the widest writer first: the host's,
  before it hands the handle to a module. A module can then take
  `SharedWriter::over(Arc::clone(writer.store()), <its graph>)` and be both ordered with the
  host's writes and fenced to its own graph.
- ★ **Construction order is the trap, and since 0.2.22 it is not silent** (ledger #1479).
  `ikigai-browse` built a default-graph-only archive writer before its mount's writer, and
  every write to the mount's named graph was refused under a declaration the mount never
  wrote. `SharedWriter::narrowed_away()` names the graphs a join dropped (assert it is empty
  right after construction), and the refusal of a write to one of them says the join dropped
  it, names the declaration that won (the store's, or the first `over`'s), and is recognized
  by `UndeclaredGraph::narrowed_by_join()`.
- Once every writer on a handle is dropped, the next `over` starts a fresh writer, and its
  declaration is the new ceiling.
- ⚠ The record is of the `Arc`, not the dataset: `Arc::new(store.clone())` is a handle nobody
  recorded, and `over` on it makes a second writer. Pass the handle on as it came.

## Configuration

`~/.config/ikigai/store.toml` (or `$XDG_CONFIG_HOME/ikigai/store.toml`), layered the way
every ikigai config is — `<app>.store.toml` beside it overrides the keys one host differs
on:

```toml
path = "/Volumes/fast/ikigai-store"   # optional; default is ~/.ikigai/store
```

Config home for the setting, data home for the bytes. A relative `path` is resolved
against the data home, never against the working directory. **No environment variable
names the path**, ever — an env var is invisible to `ikigai config`, is not inherited by
a launchd agent, and two processes that disagree about it never meet. An unknown key, an
empty `path` and an unwritable directory are all loud.

## Status

**0.2.5 adds `urn:iki:store:graphs`** — the enumeration a graph-scoped read cannot
perform, and the reason every module that partitions by graph was writing the same two-path
branch. One new resource, one new exported constant (`CAP_READ_ANY`), nothing existing
changed: the new read reuses the three write threads rather than adding a fourth. The only
observable effect on a host that never resolves it is a thirteenth action in the manifold.

**0.2.5 also deprecates `DurableStore::is_covered` in favour of `is_sole_writer`**, an
exact delegate with the same value on every store mode. The old name is the one thing in
0.2.4 a consumer could read wrong with no signal at all: it kept its name, its signature
and its return type across a release that **inverted** what it means for a store built
with `open_shared_declaring` — `false` there, while that store's scoped reads *are*
cached. A caller asking it the obvious question (*may I cache, or do I need a freshness
wrapper*) gets the answer backwards, and the wrong branch is slower rather than wrong, so
no build error, no lint and no test catches it. `is_sole_writer` answers whole-dataset
provenance — *did the handle leave this crate* — and `read_is_covered(graph)` is, and
always was, the caching question. The `covered:` line in `urn:iki:store:info` keeps its
wire spelling: it reports provenance, and 0.2.4 shipped those bytes.

**0.2.4 lets a host say WHERE a shared handle's holder writes**, so a scoped read of a graph
the sharer cannot write is cacheable again instead of the whole store forfeiting caching for
one bit.

**0.2.3 fixes a crate that did not compile for a real class of consumer** — one match arm
over `oxrdf::Term`, plus the gate that should have caught it; see the Features section.
Nothing in the API changed. **0.2.2 should be treated as broken** wherever `oxrdf/rdf-12`
is enabled anywhere in the build.

**0.2.2 is purely additive, and it carries two arcs' work: 0.2.1 was never published**
(ledger #154; crates.io goes from 0.2.0 to 0.2.2). The read-scope arc added the four
`urn:iki:store:graph-{select,ask,construct,describe}` IRIs and `urn:cap:store:read:graph:*`,
closing the read half of the tenancy boundary. The arc versioned 0.2.1 had added
`bindings=`, `ikigai_store::sparql`, `urn:iki:store:graph-update` and its capability, and
a third golden thread, and also the exhaustive `Term` match that 0.2.3 fixed, so that code
first shipped in 0.2.2. **Nothing existing changed shape in either**, so the two live
consumers — `ikigai-ledger` on crates.io and `ikigai-cli` behind a feature — need no
change at all; one that wants the new API pins `0.2.2`. Both are patches rather than a
minor bump because in cargo's 0.x rules the second number is the breaking one, and forcing
a manifest edit for a release that breaks nothing is the ceiling trap from the other
side.



The crate carried `publish = false` until it met seven conditions, and 0.2.0 meets all
seven — durable backend, a `Sink` that writes through the kernel, declared = enforced
capabilities in both directions, a golden thread the writer cuts, typed inputs and an `as`
selector that refuses rather than substitutes, an `ikigai-conformance` walk, and a
namespace this crate owns. 0.1.70 was yanked and the guard came off.

**Why 0.2.0 and not a patch.** 0.1.70 was the in-memory placeholder, shipped by a lockstep
sweep on 2026-09-12 and yanked the next day at 9 downloads. crates.io never re-uses a
yanked version, and this code has none of the shape that number was attached to. That
accident is also half the reason this crate left the `ikigai-core` workspace: a release
here is now a deliberate act on one crate rather than a side effect of a kernel release.

## Where this lives, and why

Its own repo since 2026-09-13, split out of the `ikigai-core` workspace with the crate's
13 commits of history carried across by `git subtree split`.

**Not because of wasm** — that claim is false and should stop being repeated. Core's
`wasm-check` runs with *default* features, and Oxigraph target-gates `oxrocksdb-sys` off
wasm regardless: measured 2026-09-12, a crate with RocksDB on `cargo check`s clean for
`wasm32-unknown-unknown` in 10 s, pulling no sys crate at all.

The reasons that survive that check: **native compile cost charged to the foundation**
(`oxrocksdb-sys` at 494 s of CPU on a cache miss, paid even under `cargo check` because
it is a build script, on the most-frequently-built workspace in the ecosystem, plus
`libclang` in its toolchain expectations and an edge runbook that already records an
OOM-killed compiler); **lockstep versioning**, which is how this crate got published by
accident; and **precedent twice in that same workspace** — `ikigai-fs` and `ikigai-shacl`
both left it the same way, the latter a near-exact analogy.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option.
