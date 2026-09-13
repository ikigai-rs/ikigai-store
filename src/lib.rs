//! **The persistent RDF store — UNFINISHED. What is in this file is a placeholder.**
//!
//! # What this crate is for
//!
//! `ikigai-store` is meant to own **durable RDF**: a dataset that survives a process
//! restart, opened from a path rather than rebuilt from its sources on every boot.
//!
//! > I think the original purpose was to have a store that was backed by a persistent
//! > mechanism like the rocksdb implementation. Keep it and we'll migrate it to that.
//! >   — Brian, 2026-09-12
//!
//! Nothing in the ecosystem does that today, and the gap is load-bearing: every host
//! that materializes an expensive graph recomputes it at startup. [`ikigai-sparql`]
//! deliberately does not fill it — its `space()` builds a dataset per query and drops
//! it, and `space_with_store(Arc<Store>)` takes a store the **caller** owns without
//! ever saying where that store's bytes live. This crate is meant to be the answer to
//! *where they live*, and then to hand its `Arc<Store>` to `space_with_store` so the
//! query surface is written once, in the crate that already has four typed forms, an
//! `as` selector and a conformance walk.
//!
//! # What is actually here (read this before believing the paragraph above)
//!
//! [`SparqlEndpoint`] is the **M2 scaffold** — `Store::new()` (in-memory), a
//! [`load_turtle`](SparqlEndpoint::load_turtle) side door, and one un-gated `Source`.
//! It has stood unchanged and unconsumed since commit `47ea17e`. It is a stand-in for
//! the store described above, and every difference between it and a finished module is
//! the work list: no durable backend, no `Sink`, no `requires` on a query that reads
//! everything, no golden thread, untyped inputs, two declared outputs a caller cannot
//! choose between, and no `ikigai-conformance` test. The manifest states the whole list
//! as the condition for lifting `publish = false`.
//!
//! # A correction to the record
//!
//! Until 2026-09-12 these docs said **DEPRECATED — superseded by `ikigai-sparql`**. The
//! findings behind that were accurate; the conclusion was not. The crate's purpose had
//! never been written down anywhere, so an arc compared a placeholder against a finished
//! crate — which can only conclude redundancy. The purpose is written down now, here and
//! in the manifest, which is the only form in which it survives the next process that
//! looks at this directory. The published 0.1.70 (in-memory, zero downloads) should be
//! yanked; it wears the name of a store it is not.
//!
//! [`ikigai-sparql`]: https://crates.io/crates/ikigai-sparql
//!
//! ---
//!
//! # The placeholder, as it stands
//!
//! [`SparqlEndpoint`] wraps an in-memory Oxigraph store and answers `Source`
//! requests by evaluating the SPARQL query supplied in the `query` argument,
//! returning the result as a typed [`Representation`]:
//!
//! - `SELECT` / `ASK` → `application/sparql-results+json`
//! - `CONSTRUCT` / `DESCRIBE` → `application/n-triples`
//!
//! The store is shared (`Arc`) and the endpoint is synchronous — in-memory
//! evaluation needs no async runtime, and the crate stays WASM-able (it depends
//! on Oxigraph with `default-features = false`, i.e. no RocksDB; the `js`
//! feature is added only for the wasm target).
//!
//! ⚠ That last sentence is about the placeholder, not about the crate's future.
//! Oxigraph gates `oxrocksdb-sys` out of wasm by target, so enabling its `rocksdb`
//! feature does not break a wasm build — the durable backend costs a *native* compile,
//! not the wasm face. See the README's "Where this should live".

use std::sync::Arc;

use async_trait::async_trait;
use ikigai_core::{
    ArgSpec, Description, Endpoint, Error, Invocation, ReprType, Representation, Result, Verb,
};
use oxigraph::io::{RdfFormat, RdfSerializer};
use oxigraph::model::GraphName;
use oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

/// An in-memory RDF store exposed as an ikigai endpoint.
#[derive(Clone)]
pub struct SparqlEndpoint {
    store: Arc<Store>,
}

impl SparqlEndpoint {
    /// Create an empty in-memory store.
    pub fn new() -> Result<Self> {
        Ok(SparqlEndpoint {
            store: Arc::new(Store::new().map_err(endpoint_err)?),
        })
    }

    /// Load Turtle data into the store (convenience for setup and tests).
    pub fn load_turtle(&self, turtle: &str) -> Result<()> {
        self.store
            .load_from_slice(RdfFormat::Turtle, turtle)
            .map_err(endpoint_err)
    }

    /// Borrow the underlying Oxigraph store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    fn evaluate(&self, query: &str) -> Result<Representation> {
        let results = SparqlEvaluator::new()
            .parse_query(query)
            .map_err(endpoint_err)?
            .on_store(&self.store)
            .execute()
            .map_err(endpoint_err)?;

        match results {
            QueryResults::Solutions(solutions) => {
                let variables = solutions.variables().to_vec();
                let mut serializer = QueryResultsSerializer::from_format(QueryResultsFormat::Json)
                    .serialize_solutions_to_writer(Vec::new(), variables)
                    .map_err(endpoint_err)?;
                for solution in solutions {
                    serializer
                        .serialize(&solution.map_err(endpoint_err)?)
                        .map_err(endpoint_err)?;
                }
                Ok(Representation::new(
                    sparql_results_json(),
                    serializer.finish().map_err(endpoint_err)?,
                ))
            }
            QueryResults::Boolean(value) => {
                let bytes = QueryResultsSerializer::from_format(QueryResultsFormat::Json)
                    .serialize_boolean_to_writer(Vec::new(), value)
                    .map_err(endpoint_err)?;
                Ok(Representation::new(sparql_results_json(), bytes))
            }
            QueryResults::Graph(triples) => {
                let mut serializer =
                    RdfSerializer::from_format(RdfFormat::NTriples).for_writer(Vec::new());
                for triple in triples {
                    let quad = triple
                        .map_err(endpoint_err)?
                        .in_graph(GraphName::DefaultGraph);
                    serializer.serialize_quad(&quad).map_err(endpoint_err)?;
                }
                Ok(Representation::new(
                    ReprType::new("application/n-triples"),
                    serializer.finish().map_err(endpoint_err)?,
                ))
            }
        }
    }
}

#[async_trait]
impl Endpoint for SparqlEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        match inv.request.verb {
            // `Meta` is routed by the kernel (it renders `describe()` via the
            // transform layer); the endpoint only serves data.
            Verb::Source => self.evaluate(inv.inline_str("query")?),
            other => Err(Error::Endpoint(format!(
                "sparql endpoint does not support the {other:?} verb"
            ))),
        }
    }

    fn name(&self) -> &str {
        "sparql"
    }

    fn describe(&self) -> Description {
        Description::new("sparql")
            .title("SPARQL query endpoint")
            .summary(
                "Evaluates a SPARQL query (the `query` argument) against an in-memory RDF store.",
            )
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .input(
                ArgSpec::new("query")
                    .summary("A SPARQL SELECT, ASK, CONSTRUCT, or DESCRIBE query."),
            )
            .output("application/sparql-results+json")
            .output("application/n-triples")
    }
}

fn sparql_results_json() -> ReprType {
    ReprType::new("application/sparql-results+json")
}

fn endpoint_err(e: impl std::fmt::Display) -> Error {
    Error::Endpoint(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use ikigai_core::{
        ArgRef, Bindings, Capability, EndpointSpace, Exact, Iri, Kernel, Request, Verb,
    };
    use ikigai_vocab::TurtleRenderer;
    use std::sync::Arc;

    fn query_rep(ep: &SparqlEndpoint, sparql: &[u8]) -> Representation {
        let req = Request::new(Verb::Source, Iri::parse("urn:sparql:default").unwrap())
            .with_arg("query", ArgRef::Inline(sparql.to_vec()));
        let bindings = Bindings::new();
        let cap = Capability::root();
        let inv = Invocation::detached(&req, &bindings, &cap);
        block_on(ep.invoke(&inv)).unwrap()
    }

    #[test]
    fn select_over_loaded_data() {
        let ep = SparqlEndpoint::new().unwrap();
        ep.load_turtle(r#"@prefix ex: <http://ex/> . ex:a ex:name "Alice" . ex:b ex:name "Bob" ."#)
            .unwrap();
        let rep = query_rep(&ep, b"SELECT ?name WHERE { ?s <http://ex/name> ?name }");
        assert_eq!(rep.repr_type.media_type, "application/sparql-results+json");
        let json = String::from_utf8(rep.bytes).unwrap();
        assert!(
            json.contains("Alice") && json.contains("Bob"),
            "got: {json}"
        );
    }

    #[test]
    fn ask_returns_boolean() {
        let ep = SparqlEndpoint::new().unwrap();
        ep.load_turtle("<http://ex/a> <http://ex/p> <http://ex/b> .")
            .unwrap();
        let rep = query_rep(&ep, b"ASK { ?s ?p ?o }");
        assert!(String::from_utf8(rep.bytes).unwrap().contains("true"));
    }

    #[test]
    fn construct_returns_ntriples() {
        let ep = SparqlEndpoint::new().unwrap();
        ep.load_turtle("<http://ex/a> <http://ex/p> <http://ex/b> .")
            .unwrap();
        let rep = query_rep(&ep, b"CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }");
        assert_eq!(rep.repr_type.media_type, "application/n-triples");
        assert!(String::from_utf8(rep.bytes)
            .unwrap()
            .contains("http://ex/a"));
    }

    #[test]
    fn meta_renders_self_description_via_kernel() {
        let space = EndpointSpace::new().bind(
            Exact::new("urn:sparql:default"),
            SparqlEndpoint::new().unwrap(),
        );
        let kernel = Kernel::with_meta_renderer(Arc::new(space), Arc::new(TurtleRenderer));
        let cap = Capability::root();
        let rep = block_on(kernel.issue(
            Request::new(Verb::Meta, Iri::parse("urn:sparql:default").unwrap()),
            &cap,
        ))
        .unwrap();
        assert_eq!(rep.repr_type.media_type, "text/turtle");
        let ttl = String::from_utf8(rep.bytes).unwrap();
        assert!(ttl.contains("a ik:Endpoint"));
        assert!(ttl.contains("ik:id \"sparql\""));
        assert!(ttl.contains("ik:inputName \"query\""));
    }
}
