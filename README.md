# ikigai-store

> ## ⚠ UNFINISHED — not deprecated, and not what it says on the tin *yet*
>
> **`ikigai-store` is the persistent RDF store.** Its job is durable RDF: a dataset that
> survives a process restart, opened from a path instead of rebuilt from its sources on
> every boot.
>
> > I think the original purpose was to have a store that was backed by a persistent
> > mechanism like the rocksdb implementation. Keep it and we'll migrate it to that.
> >   — Brian, 2026-09-12
>
> **That store is not built.** What is in `src/lib.rs` is the June 2026 M2 scaffold
> standing in for it — an in-memory Oxigraph store behind one un-gated `Source` — and
> nothing has ever consumed it. The crate carries `publish = false` until it is the thing
> it is named for; the manifest states the exact conditions for lifting that.
>
> **Correction to the record (2026-09-12).** This README previously read *DEPRECATED —
> superseded by `ikigai-sparql`*. The findings behind that verdict were accurate and are
> reproduced below as the work list; the verdict was not. The crate's purpose had never
> been written down anywhere, so an arc compared a placeholder against a finished crate,
> and that comparison can only ever conclude redundancy. The purpose is written down now
> — here and in `Cargo.toml` — because a fact that lives only in someone's head does not
> survive contact with a process. (It has now failed that way twice on this one crate:
> the 2026-08-08 decision not to publish it lived only in a brief, and the lockstep sweep
> published **0.1.70** anyway. That version is in-memory, has zero downloads, and should
> be **yanked**.)

## Why `ikigai-sparql` does not already do this

It is the right question, and the answer is specific: `ikigai-sparql` is a *query*
module, and both of its spaces are explicit about not owning storage.

| | `ikigai-sparql` | what is missing |
| --- | --- | --- |
| `space()` | builds a dataset per query from the `graph=` list and **drops it when the call returns** | nothing persists, by design — `urn:sparql:update` is deliberately left unbound there because a write would vanish microseconds later |
| `space_with_store(Arc<Store>)` | queries and updates a store the **caller** owns | it never says where the caller's store comes from, or where its bytes live. Today every caller passes `Store::new()` — memory |

So "hand a store to `space_with_store`" is not an alternative to this crate; it is this
crate's *output*. The durable dataset is the missing half, and the query surface must
not be written a second time — see "How it composes" below.

## The work list (what a finished `ikigai-store` needs)

Every item is something core #109 found missing from the placeholder:

1. **A durable backend.** `Store::open(path)` behind an explicit feature, the path named
   through the config home (never an env var), and a loud failure when it is unwritable.
2. **A `Verb::Sink`.** `load_turtle` is a Rust-level side door: it mutates the store
   where no capability check and no thread cut can see it.
3. **Declared = enforced capabilities**, both directions. Today a `SELECT * { ?s ?p ?o }`
   is answered for any attenuated caller, with no `requires` at all.
4. **A golden thread the writer cuts**, and reads that may then be `.cacheable()`.
5. **Typed inputs and an `as` selector.** The placeholder declares two output types and
   gives no caller a way to select either; `query` has no `class`, so an
   `ikigai-conformance` walk of it synthesizes no value and **probes nothing**.
6. **An `ikigai-conformance` test from day one.**
7. **A namespace this crate owns.** The placeholder's tests and the example below bind
   `urn:sparql:default` — inside a namespace `ikigai-sparql` owns. A host binding both
   offers an agent two indistinguishable query actions over two different stores, and
   the one declaring no capability is the one that answers an unrestricted query.

## Where this lives

**Its own repo, like every other module — moved 2026-09-13** (decided in `ikigai-core`
#110, executed as the first and only step of its own change, before any backend work).
The crate's 13 commits of history came across with `git subtree split`, so `git log` here
still reaches the M2 scaffold. The argument, from the CI and the manifests rather than
from the shape of the ecosystem:

**The wasm gate is *not* the reason, and the widely-repeated version of that claim is
wrong.** `ikigai-core`'s `ci.yml` does call the shared workflow with `features: "*"` and
`wasm-check: true` — but `wasm-check` runs `cargo check --workspace --target
wasm32-unknown-unknown` with **default features**, so `"*"` never reaches it. And even if
it did, Oxigraph declares `oxrocksdb-sys` under `[target.'cfg(not(target_family =
"wasm"))'.dependencies]` and gates `Store::open` on `all(not(target_family = "wasm"),
feature = "rocksdb")`. Measured 2026-09-12: a crate depending on `oxigraph = "0.5"` with
**default features (RocksDB on)** `cargo check`s clean for `wasm32-unknown-unknown` in
10 s, pulling no sys crate at all. A `persistent` feature could sit in this workspace and
CI would stay green.

The reasons that survive that check:

- **Compile cost, charged to the foundation.** `--all-features` is a *native* job, and it
  would build RocksDB from C++ source on every cache miss. Measured on the same probe:
  **494 s of CPU** (44 s wall at 12× parallelism) for `oxrocksdb-sys` alone — under
  `cargo check`, because it is a build script, so even the cheapest gate pays in full. A
  two-core CI runner pays that as wall clock. `ikigai-core` is the most-frequently-built
  workspace in the ecosystem and describes itself as "the core, dependency-light layer".
  Adding `clang`/`libclang` as a build prerequisite of *that* workspace taxes everyone,
  and the edge runbook already records the small-VPS failure mode: the compiler
  OOM-killed.
- **Lockstep versioning, which is how this crate got published by accident.** Workspace
  members share `version.workspace`, so every `ikigai-store` release would be an
  `ikigai-core` release and vice versa. A store that grows a C++ dependency and a
  storage-format compatibility story does not want the kernel's release cadence, and the
  kernel does not want the store's.
- **Precedent inside this exact workspace, twice.** `ikigai-fs` (#21) and `ikigai-shacl`
  (#52) were both core-workspace crates removed in favour of their own repos.
  `ikigai-shacl` is the near-exact analogy: a module whose backend is native-only (rudof
  gates its validator off wasm) with a separate browser story. `ikigai-store` is the last
  module crate left in a workspace that is otherwise kernel + vocabulary.
- **The module recipe assumes a repo**: its own CI, its own conformance walk, its own
  release cadence, one session owning one tree.

**What keeping it in the core workspace would have cost, stated fairly**, because the CI
objection did not survive: a `[features]` split (`memory` default, `persistent` opt-in),
roughly six minutes of CI on cache-miss builds, `libclang` in the toolchain expectations
for anyone building the kernel with `--all-features`, and the lockstep coupling above.
None of that is fatal. It is simply worse than a repo, and worse in the direction the
ecosystem has already chosen twice.

**Why it moved before the backend rather than with it.** #110 recommended moving it as
the first step of the backend arc; Brian's call on 2026-09-13 was to move it first and
separately — *"I don't mind yanking ikigai-store as a subcrate of core, but we should
move it to a stand alone module to build on."* That is the better order for the reason
the brief itself gave: a relocation is high-churn and low-risk, the backend is the
opposite, and the two do not belong in one diff. What it costs is one extra round of
churn on the references — `ikigai-core`'s README crate table and its walkthrough card
(both removed in the companion core PR) and `ikigai-tutorial`, which names the crate in
`books/ikigai/src/repositories.md` and is another repo's to change.

## How it composes (the design constraint that keeps this small)

A persistent `ikigai-store` **must not grow a second query surface.** `ikigai-sparql`
already has four typed forms, an `as` selector, a refusal (not a substitution) on an
unknown target, and a conformance walk. `Store` is one type whether it was opened from a
path or created in memory — `Store::open` is purely additive behind a feature — so the
composition is:

```text
ikigai-store  ->  opens the durable dataset, owns writes, cuts the thread
                  hands the same Arc<Store> to
ikigai-sparql ->  space_with_store(store)  ->  urn:sparql:{select,ask,describe,construct}
```

That may be the whole design. Test it before writing an endpoint that duplicates it.

⚠ One consequence to know before enabling the feature: Cargo unifies features across the
graph, and Oxigraph's `rocksdb` is a **default** feature. The moment any crate in a host
turns it on, every other Oxigraph consumer in that build gets it too. Additive, but it is
why the placeholder's `default-features = false` never protected anything it did not also
have to protect elsewhere.

---

Everything below describes the placeholder as it stands today.

An RDF/SPARQL **store endpoint** for the
[ikigai-core](https://crates.io/crates/ikigai-core) resolution kernel, backed by
[Oxigraph](https://github.com/oxigraph/oxigraph).

In ROC terms, this binds an addressable triple store into an ikigai address
space: a `Source` request carries a SPARQL query as the `query` argument, the
endpoint evaluates it against an in-memory Oxigraph store, and hands back a typed
[`Representation`](https://crates.io/crates/ikigai-core) — so query results flow
through the same resolution, capability, and caching machinery as any other
resource.

```rust
use ikigai_store::SparqlEndpoint;

# fn demo() -> ikigai_core::Result<()> {
let ep = SparqlEndpoint::new()?;
ep.load_turtle(r#"@prefix ex: <http://ex/> . ex:a ex:name "Alice" ."#)?;
// Bind `ep` into an `EndpointSpace` and resolve `Verb::Source` requests
// whose `query` argument is a SPARQL SELECT / ASK / CONSTRUCT / DESCRIBE.
# Ok(()) }
```

## What's here

- **`SparqlEndpoint`** — an in-memory Oxigraph `Store` (shared via `Arc`) exposed
  as an `Endpoint`. Synchronous evaluation, no async runtime required.
- **`load_turtle(..)`** — load Turtle data into the store, for setup and tests.
- **`store()`** — borrow the underlying Oxigraph `Store` directly when you need it.

## Verbs and representations

| Verb | Behaviour | Output |
| --- | --- | --- |
| `Source` | Evaluate the `query` argument | see below |
| `Meta` | Routed by the kernel to a `MetaRenderer` (self-description) | renderer-defined |

| Query form | Result media type |
| --- | --- |
| `SELECT` / `ASK` | `application/sparql-results+json` |
| `CONSTRUCT` / `DESCRIBE` | `application/n-triples` |

## Native and WebAssembly

Oxigraph is pulled in with `default-features = false`, dropping the RocksDB
(C++) backend so the store is purely in-memory — and WASM-able. On wasm targets
the crate enables Oxigraph's `js` feature for its `getrandom` backend, so the
same store runs natively, in the browser, or embedded.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option.
