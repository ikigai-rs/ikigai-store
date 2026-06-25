# ikigai-store

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
