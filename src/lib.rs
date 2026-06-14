//! RDF/SPARQL endpoint backed by Oxigraph (in-memory).
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

use std::sync::Arc;

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

impl Endpoint for SparqlEndpoint {
    fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        match inv.request.verb {
            // DESCRIBE the endpoint itself as RDF.
            Verb::Meta => Ok(ikigai_vocab::describe_turtle(&self.describe())),
            _ => self.evaluate(inv.inline_str("query")?),
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
    use ikigai_core::{ArgRef, Bindings, Capability, Iri, Request, Verb};

    fn query_rep(ep: &SparqlEndpoint, sparql: &[u8]) -> Representation {
        let req = Request::new(Verb::Source, Iri::parse("urn:sparql:default").unwrap())
            .with_arg("query", ArgRef::Inline(sparql.to_vec()));
        let bindings = Bindings::new();
        let cap = Capability::root();
        let inv = Invocation {
            request: &req,
            bindings: &bindings,
            capability: &cap,
        };
        ep.invoke(&inv).unwrap()
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
    fn meta_returns_rdf_self_description() {
        let ep = SparqlEndpoint::new().unwrap();
        let req = Request::new(Verb::Meta, Iri::parse("urn:sparql:default").unwrap());
        let bindings = Bindings::new();
        let cap = Capability::root();
        let inv = Invocation {
            request: &req,
            bindings: &bindings,
            capability: &cap,
        };
        let rep = ep.invoke(&inv).unwrap();
        assert_eq!(rep.repr_type.media_type, "text/turtle");
        let ttl = String::from_utf8(rep.bytes).unwrap();
        assert!(ttl.contains("a ik:Endpoint"));
        assert!(ttl.contains("ik:id \"sparql\""));
        assert!(ttl.contains("ik:verb \"Source\", \"Meta\""));
        assert!(ttl.contains("ik:inputName \"query\""));
    }
}
