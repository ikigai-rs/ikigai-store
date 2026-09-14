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
that and had to memoize the space.

`DurableStore` is `Clone` and holds an `Arc<Store>`, so the fix is to open once and hand
each kernel a clone: they share the dataset, the write lock, and the coverage flag.

```rust
let store = DurableStore::open(&config.path)?;      // once, at startup
let one = Kernel::new(Arc::new(space(store.clone())));
let two = Kernel::new(Arc::new(space(store.clone())));
```

⚠ **Each kernel still has its own cache.** A write through `one` cuts `one`'s golden
threads, not `two`'s, so a second kernel over the same store can serve a stale cached
read. That is a property of the kernel, not of this crate, and it is the reason to prefer
one kernel per process where you can.

## What it binds

| resource | verb | what it does | capability |
| --- | --- | --- | --- |
| `urn:iki:store:select` | `Source` | SPARQL SELECT | `urn:cap:store:read` |
| `urn:iki:store:ask` | `Source` | SPARQL ASK | `urn:cap:store:read` |
| `urn:iki:store:construct` | `Source` | SPARQL CONSTRUCT | `urn:cap:store:read` |
| `urn:iki:store:describe` | `Source` | SPARQL DESCRIBE | `urn:cap:store:read` |
| `urn:iki:store:graph-select` | `Source` | SPARQL SELECT, one named graph | `urn:cap:store:read:graph:<iri>` |
| `urn:iki:store:graph-ask` | `Source` | SPARQL ASK, one named graph | `urn:cap:store:read:graph:<iri>` |
| `urn:iki:store:graph-construct` | `Source` | SPARQL CONSTRUCT, one named graph | `urn:cap:store:read:graph:<iri>` |
| `urn:iki:store:graph-describe` | `Source` | SPARQL DESCRIBE, one named graph | `urn:cap:store:read:graph:<iri>` |
| `urn:iki:store:info` | `Source` | backing, quad count, coverage | `urn:cap:store:read` |
| `urn:iki:store:update` | `Sink` | SPARQL UPDATE, whole dataset | `urn:cap:store:write` |
| `urn:iki:store:graph-update` | `Sink` | SPARQL UPDATE, one named graph | `urn:cap:store:write:graph:<iri>` |
| `urn:iki:store:load` | `Sink` | bulk-load an RDF document | `urn:cap:store:write` |

One IRI per query form, following `ikigai-sparql`: the form fixes the result family, so
it fixes the declared outputs and the default `as` too — and a query of another form is
**refused**, not served under an IRI that promised something else.

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
exact. The update runs against a private copy of that one graph; if anything lands
anywhere else the whole request is refused with the offending statement named, and
otherwise the difference is applied to the real graph in one transaction. So the shapes
that defeat a syntactic check are handled by construction:

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
- **The graph is copied into memory on every scoped write**, twice (a before and an after
  set). This door is priced for a graph a module owns — a ledger, a layer, an annotation
  set — not for a materialized database. The unscoped door copies nothing.

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
| `GRAPH <other> { … }` | matches nothing — an empty result, not an error |
| `GRAPH ?g { … }` | binds `?g` to `G` and to nothing else |
| a sub-select, `FILTER EXISTS`/`NOT EXISTS`, a property path over another graph | confined the same way |
| `DESCRIBE <s>` with no pattern | reads `G` |
| `FROM` / `FROM NAMED` in the query text | **refused** — see below |
| `SERVICE <http://…>` | refused: no HTTP client is built in — but see the feature warning below |

Four decisions worth stating:

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

The same trade `ikigai-cli` makes for `quic` and `web`: in-memory is the default so the
wasm face and cheap tests survive, and the heavy backend is opted into.

⚠ **Cargo feature unification is global and additive**, and `rocksdb` is a *default*
feature of `oxigraph` that this crate turns off. The moment any crate in a host's graph
enables `persistent`, **every** `oxigraph` consumer in that build gets the RocksDB
backend, `ikigai-sparql`'s copy included. That is exactly what makes the
`space_with_store` composition work — one `Store` type either way — and it means "off by
default" is a property of the whole build, not of this crate.

`ci.yml` passes `features: persistent`, and must keep doing so: the durable backend lives
behind a `cfg(feature)` inside a *library*, which is not a target, so `--all-targets`
never reaches it and `ci-drift`'s `required-features` check cannot see the omission.

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

**0.2.1 and 0.2.2 are purely additive.** 0.2.2 adds the four `urn:iki:store:graph-{select,ask,construct,describe}` IRIs and
`urn:cap:store:read:graph:*`, closing the read half of the tenancy boundary. 0.2.1 added
`bindings=`, `ikigai_store::sparql`, `urn:iki:store:graph-update` and its capability, and
a third golden thread. **Nothing existing changed shape in either**, so the two live
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
