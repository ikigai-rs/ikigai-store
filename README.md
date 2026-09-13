# ikigai-store

> ## ⚠ DEPRECATED — do not adopt this crate
>
> **Use [`ikigai-sparql`](https://crates.io/crates/ikigai-sparql) instead**, whose
> `space_with_store` and `urn:sparql:update` supersede everything here.
>
> This crate shipped inside the `ikigai-core` workspace in June 2026, was never
> consumed by anything, and was published for the first and only time on
> 2026-09-12 (0.1.70) by a lockstep workspace release — against a decision made
> five weeks earlier to grow `ikigai-sparql` rather than publish this. It carries
> `publish = false` now; `crates/ikigai-store` is removed from the workspace once
> 0.1.70 is yanked.
>
> **What replaces it**, in `ikigai-sparql` 0.1.9:
>
> | this crate | the successor |
> | --- | --- |
> | `SparqlEndpoint::new()` holds an `Arc<Store>` | `space_with_store(Arc<Store>)` — the store is *caller-owned*, and `pub use oxigraph::store::Store` gives every host one canonical type to unify on |
> | `load_turtle(&str)` — a Rust-level side door | `urn:sparql:update`, a `Verb::Sink`: SPARQL 1.1 UPDATE in one transaction, so a *resource* consumer can load data. `INSERT DATA { GRAPH <g> { … } }` loads a named graph; `DROP GRAPH <g>` drops one |
> | no `requires` — `SELECT * { ?s ?p ?o }` to any attenuated caller | `CAP_UPDATE` (`urn:cap:sparql:update`) declared on the Sink, and therefore enforced by the kernel before `invoke` |
> | no golden thread | `UPDATE_THREAD`, cut by the kernel's automatic target-named cut on a successful sink |
> | one `Source` on `urn:sparql:default`, `query` untyped, two declared outputs and no way to ask for either | `urn:sparql:{select,ask,describe,construct}`, every input carrying an `xsd` class, `as` with `one_of` and a default, outputs declared per form, and a refusal rather than a substitution on an unknown target |
>
> The only surface this crate has that the successor does not is `Store::new()`,
> `Store::load_from_slice(RdfFormat::Turtle, …)` and a `&Store` borrow — three
> lines of Oxigraph. Against that it binds `urn:sparql:default`, an IRI inside a
> namespace `ikigai-sparql` owns, under a *different* contract: a host binding
> both offers an agent two SPARQL query actions over two different stores, with
> nothing in the action manifold to tell them apart, and the one that declares no
> capability is the one that reads everything.
>
> An `ikigai-conformance` 0.2.0 walk of the published 0.1.70 reports two findings
> and, with no fixture, **probes nothing at all** — the endpoint cannot be reached
> from its own declarations, because `query` has no `class` for the walk to
> synthesize a value from. With a fixture supplying a query it serves
> `application/sparql-results+json` when asked for its own declared
> `application/n-triples` face: a declared output no caller can select.
>
> Everything below this line describes the crate as it was, and is retained so the
> published 0.1.70 has honest documentation until it is yanked.

---

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

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
