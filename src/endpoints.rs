//! The seven resources this crate binds, under the namespace it owns.
//!
//! ```text
//! urn:iki:store:select     Source  SPARQL SELECT              urn:cap:store:read
//! urn:iki:store:ask        Source  SPARQL ASK                 urn:cap:store:read
//! urn:iki:store:construct  Source  SPARQL CONSTRUCT           urn:cap:store:read
//! urn:iki:store:describe   Source  SPARQL DESCRIBE            urn:cap:store:read
//! urn:iki:store:info       Source  backing, size, coverage    urn:cap:store:read
//! urn:iki:store:update     Sink    SPARQL UPDATE              urn:cap:store:write
//! urn:iki:store:load       Sink    bulk-load an RDF document  urn:cap:store:write
//! ```
//!
//! **One IRI per query form, following `ikigai-sparql`.** The form fixes the result
//! family, so it fixes the declared `outputs` and the default `as` too — a single
//! `query` IRI would have to declare all six serializations and let an agent guess which
//! three it can actually get. The conformance walk found that concretely: with both
//! families on one endpoint, the RDF face could not be probed at all, because probing it
//! means asking a SELECT to answer in N-Triples. Each form also **refuses** a query of
//! the wrong shape rather than serving it under the wrong IRI.
//!
//! # Why there is a query face here at all
//!
//! This crate's README warns, correctly, that a persistent store must not grow a second
//! query surface: `ikigai-sparql` already has four typed forms, an `as` selector and a
//! conformance walk. Two things force one anyway, and they are worth stating because the
//! warning is otherwise the right instinct:
//!
//! 1. **`ikigai-sparql`'s query endpoints declare no capability.** A `SELECT * { ?s ?p
//!    ?o }` is answered for any attenuated caller. Over a per-query federated dataset
//!    that is defensible; over a host's durable store — which holds whatever anyone ever
//!    put in it — it is not. A read gate is the whole point of `urn:cap:store:read`, and
//!    it can only be declared by the endpoint that serves the read.
//! 2. **Reaching `space_with_store` means handing out the `Arc<Store>`**, which forfeits
//!    golden-thread coverage for the life of the store (see [`crate::DurableStore`]).
//!    A read through *this* face is covered and therefore cacheable; a read through that
//!    one never can be.
//!
//! So the duplication is one `evaluate` and one `serialize` — and it is the price of a
//! gated, cacheable read. The composition the README describes is still available and
//! still supported: ask for `DurableStore::open_shared`
//! and bind `ikigai_sparql::space_with_store` yourself, knowing what it costs.
//!
//! ⚠ **Do not bind both faces over two different stores in one host.** An agent reading
//! the manifold would see two query actions it cannot tell apart, and the one declaring
//! no capability is the one that answers an unrestricted query.

use std::sync::Arc;

use async_trait::async_trait;
use ikigai_core::{
    ArgSpec, Description, Endpoint, EndpointSpace, Error, Exact, Invocation, ReprType,
    Representation, Result, Verb,
};
use oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use oxigraph::model::{GraphName, GraphNameRef, NamedNodeRef};
use oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};

use crate::store::DurableStore;

/// The capability a read requires. Declared, therefore enforced by the kernel before
/// `invoke` and before any cache lookup.
///
/// **A read is not free here, and that is the difference from a query module.** This
/// store holds whatever the host put in it — an explanation archive, an annotation
/// graph, a materialized relational database. `ikigai-sparql`'s query endpoints declare
/// nothing because their dataset is assembled per query from sources the caller already
/// named; this one's dataset is standing state the caller did not name.
pub const CAP_READ: &str = "urn:cap:store:read";

/// The capability a write requires — coarse on purpose, and it is the keys to the store.
///
/// SPARQL UPDATE is not a family of small permissions: `DROP ALL`, `CLEAR ALL` and a
/// bare `DELETE WHERE { ?s ?p ?o }` each empty the dataset. `urn:iki:store:load` is the
/// same authority by another door. One unqualified name, so nothing about it suggests a
/// narrower grant than it is. (`ikigai-sparql`'s `CAP_UPDATE` reasons the same way about
/// the same act, at length.)
pub const CAP_WRITE: &str = "urn:cap:store:write";

/// The golden thread `urn:iki:store:update` cuts on success.
///
/// The kernel cuts the thread named after a mutating request's target, so this needs no
/// code — it is the target IRI. A cacheable read declares both this and [`LOAD_THREAD`].
pub const UPDATE_THREAD: &str = "urn:iki:store:update";

/// The golden thread `urn:iki:store:load` cuts on success.
///
/// ⚠ **Two writing IRIs means two threads, and a reader must depend on both.** The
/// kernel's auto-cut is per target and an endpoint cannot cut an arbitrary thread except
/// by resolving `urn:kernel:cut` (which needs `urn:cap:kernel:cut`), so collapsing these
/// into one name would cost a capability this module has no business holding. Depending
/// on both is cheaper and exact.
pub const LOAD_THREAD: &str = "urn:iki:store:load";

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_ANY_URI: &str = "http://www.w3.org/2001/XMLSchema#anyURI";

/// Result serializations, SELECT/ASK first — index 0 is the default and the value a
/// conformance walk synthesizes.
const RESULT_OUTPUTS: [&str; 4] = [
    "application/sparql-results+json",
    "application/sparql-results+xml",
    "text/csv",
    "text/tab-separated-values",
];
/// Graph serializations for CONSTRUCT/DESCRIBE, default first.
const GRAPH_OUTPUTS: [&str; 2] = ["application/n-triples", "text/turtle"];
/// Input syntaxes `urn:iki:store:load` parses, default first.
const LOAD_FORMATS: [&str; 5] = [
    "text/turtle",
    "application/n-triples",
    "application/n-quads",
    "application/trig",
    "application/rdf+xml",
];

/// Bind this store's seven resources into a space.
///
/// The store is moved in: this space and the endpoints in it are the only holders of the
/// dataset unless the caller took a handle at construction (see
/// [`DurableStore`]).
pub fn space(store: DurableStore) -> EndpointSpace {
    let store = Arc::new(store);
    let mut space = EndpointSpace::new();
    for (form, id, graph_shaped) in FORMS {
        space = space.bind(
            Exact::new(format!("urn:iki:store:{form}")),
            QueryEndpoint {
                store: Arc::clone(&store),
                form,
                id,
                graph_shaped,
            },
        );
    }
    space
        .bind(
            Exact::new("urn:iki:store:info"),
            InfoEndpoint {
                store: Arc::clone(&store),
            },
        )
        .bind(
            Exact::new("urn:iki:store:update"),
            UpdateEndpoint {
                store: Arc::clone(&store),
            },
        )
        .bind(Exact::new("urn:iki:store:load"), LoadEndpoint { store })
}

/// Cacheable under both write threads when the store is covered; `Expiry::Always` when
/// the handle was handed out and an invisible writer may exist.
///
/// ★ This is the one place the coverage decision has teeth. `ikigai-sparql` documents
/// why a thread that is right on some writes and wrong on others is worse than no
/// thread — "always fresh" becomes "fresh until someone writes the other way, then stale
/// with no bound and no signal". A covered store has no other way to write.
fn with_freshness(rep: Representation, store: &DurableStore) -> Representation {
    if store.is_covered() {
        rep.cacheable()
            .depends_on(UPDATE_THREAD)
            .depends_on(LOAD_THREAD)
    } else {
        rep
    }
}

// ---------------------------------------------------------------------------- query

/// The four query forms, as `(IRI suffix, description id, graph-shaped?)`.
///
/// Each carries a UNIQUE description id, so their catalog subjects and any id-keyed
/// projection (an MCP tool name) do not collide — with each other, or with
/// `ikigai-sparql`'s `sparql-{form}`.
const FORMS: [(&str, &str, bool); 4] = [
    ("select", "store-select", false),
    ("ask", "store-ask", false),
    ("construct", "store-construct", true),
    ("describe", "store-describe", true),
];

#[derive(Clone)]
struct QueryEndpoint {
    store: Arc<DurableStore>,
    /// The SPARQL form this IRI answers.
    form: &'static str,
    id: &'static str,
    /// Whether this form answers with a graph (CONSTRUCT/DESCRIBE) or a result set.
    graph_shaped: bool,
}

#[async_trait]
impl Endpoint for QueryEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        match inv.request.verb {
            Verb::Source => {
                let query = inv.inline_str("query")?;
                let results = SparqlEvaluator::new()
                    .parse_query(query)
                    .map_err(|e| Error::InvalidArgument {
                        name: "query".to_string(),
                        detail: format!("not a SPARQL query: {e}"),
                    })?
                    .on_store(self.store.dataset())
                    .execute()
                    .map_err(|e| Error::Endpoint(format!("query: {e}")))?;

                // ★ Refuse a query of the wrong shape rather than serving it here. The
                // IRI is a promise about what comes back — it is what fixes this
                // action's declared `outputs` — and answering a CONSTRUCT under
                // `urn:iki:store:select` would make that promise true only by accident.
                let is_graph = matches!(results, QueryResults::Graph(_));
                if is_graph != self.graph_shaped {
                    return Err(Error::InvalidArgument {
                        name: "query".to_string(),
                        detail: format!(
                            "this is `urn:iki:store:{}`, which answers with {}; that query \
                             answers with {}. Resolve the IRI for its form instead",
                            self.form,
                            shape(self.graph_shaped),
                            shape(is_graph)
                        ),
                    });
                }

                let (media, bytes) = if self.graph_shaped {
                    serialize_graph(results, inv.inline_str("as").ok())?
                } else {
                    serialize_solutions(results, inv.inline_str("as").ok())?
                };
                Ok(with_freshness(
                    Representation::new(
                        ReprType::new(&media).with_param("charset", "utf-8"),
                        bytes,
                    ),
                    &self.store,
                ))
            }
            other => Err(unsupported(self.id, other)),
        }
    }

    fn name(&self) -> &str {
        self.id
    }

    fn describe(&self) -> Description {
        let outputs: &[&str] = if self.graph_shaped {
            &GRAPH_OUTPUTS
        } else {
            &RESULT_OUTPUTS
        };
        let desc = Description::new(self.id)
            .title(format!(
                "SPARQL {} over the durable store",
                self.form.to_uppercase()
            ))
            .summary(format!(
                "Evaluate a SPARQL {} against the store this host owns. A query of \
                 another form is refused, not served here.",
                self.form.to_uppercase()
            ))
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .requires(CAP_READ)
            .input(
                ArgSpec::new("query")
                    .summary(format!("A SPARQL {} query.", self.form.to_uppercase()))
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("as")
                    .summary(format!(
                        "Result serialization; one of {}. An `as` this form cannot answer \
                         in is refused, never substituted.",
                        outputs.join(", ")
                    ))
                    .class(XSD_STRING)
                    .one_of(outputs.iter().copied())
                    .default_value(outputs[0])
                    .optional(),
            );
        outputs.iter().fold(desc, |desc, media| desc.output(*media))
    }
}

fn shape(graph: bool) -> &'static str {
    if graph {
        "a graph"
    } else {
        "a result set"
    }
}

// ----------------------------------------------------------------------------- info

#[derive(Clone)]
struct InfoEndpoint {
    store: Arc<DurableStore>,
}

#[async_trait]
impl Endpoint for InfoEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        match inv.request.verb {
            Verb::Source => {
                // ⚠ `Store::len()` is a FULL SCAN, not metadata: measured 2.4 µs empty,
                // 7.3 ms at 100k quads, 72 ms at 1M (see docs/design/handle-model.md).
                // That is affordable here only because this face is cacheable under the
                // write threads on a covered store, so the scan is paid once per write
                // rather than once per read. On a SHARED store it is paid every time —
                // another cost of handing out the handle, and stated in the output.
                let quads = self
                    .store
                    .dataset()
                    .len()
                    .map_err(|e| Error::Endpoint(format!("counting quads: {e}")))?;
                let text = format!(
                    "backing: {}\nquads: {quads}\ncovered: {}\n",
                    self.store.backing(),
                    self.store.is_covered()
                );
                Ok(with_freshness(
                    Representation::new(
                        ReprType::new("text/plain").with_param("charset", "utf-8"),
                        text.into_bytes(),
                    ),
                    &self.store,
                ))
            }
            other => Err(unsupported("store-info", other)),
        }
    }

    fn name(&self) -> &str {
        "store-info"
    }

    fn describe(&self) -> Description {
        Description::new("store-info")
            .title("Store backing and size")
            .summary(
                "Where this store's bytes live, how many quads it holds, and whether its \
                 golden-thread coverage is intact (`covered: false` means the raw handle \
                 was handed out at construction and no read here can be cached).",
            )
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .requires(CAP_READ)
            .output("text/plain")
    }
}

// --------------------------------------------------------------------------- update

#[derive(Clone)]
struct UpdateEndpoint {
    store: Arc<DurableStore>,
}

#[async_trait]
impl Endpoint for UpdateEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        match inv.request.verb {
            // `content` by name: the engine routes a pipe's (or a trailing) value into
            // `content` for every Sink, so a mutating verb that can be piped into
            // declares it.
            Verb::Sink => {
                let update = inv.inline_str("content")?;
                let before = self.len()?;
                SparqlEvaluator::new()
                    .parse_update(update)
                    .map_err(|e| Error::InvalidArgument {
                        name: "content".to_string(),
                        detail: format!("not a SPARQL update: {e}"),
                    })?
                    .on_store(self.store.dataset())
                    .execute()
                    .map_err(|e| Error::Endpoint(format!("update: {e}")))?;
                let after = self.len()?;
                Ok(plain(format!("updated: {before} -> {after} quads\n")))
            }
            other => Err(unsupported("store-update", other)),
        }
    }

    fn name(&self) -> &str {
        "store-update"
    }

    fn describe(&self) -> Description {
        Description::new("store-update")
            .title("SPARQL UPDATE against the durable store")
            .summary(
                "Apply a SPARQL 1.1 UPDATE to the store. Cuts the golden thread \
                 `urn:iki:store:update`, so every cacheable read of this store \
                 recomputes.",
            )
            .verb(Verb::Sink)
            .verb(Verb::Meta)
            .requires(CAP_WRITE)
            .input(
                ArgSpec::new("content")
                    .summary("A SPARQL 1.1 UPDATE request (INSERT DATA, DELETE WHERE, …).")
                    .class(XSD_STRING),
            )
            .output("text/plain")
    }
}

impl UpdateEndpoint {
    fn len(&self) -> Result<usize> {
        self.store
            .dataset()
            .len()
            .map_err(|e| Error::Endpoint(format!("counting quads: {e}")))
    }
}

// ----------------------------------------------------------------------------- load

#[derive(Clone)]
struct LoadEndpoint {
    store: Arc<DurableStore>,
}

#[async_trait]
impl Endpoint for LoadEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        match inv.request.verb {
            Verb::Sink => {
                let bytes = inv.inline_arg("content")?;
                let media = inv.inline_str("format").unwrap_or(LOAD_FORMATS[0]);
                let format =
                    RdfFormat::from_media_type(media).ok_or_else(|| Error::InvalidArgument {
                        name: "format".to_string(),
                        detail: format!(
                            "unknown RDF syntax `{media}` — one of {}",
                            LOAD_FORMATS.join(", ")
                        ),
                    })?;
                let mut parser = RdfParser::from_format(format);
                if let Ok(graph) = inv.inline_str("graph") {
                    let name = NamedNodeRef::new(graph).map_err(|e| Error::InvalidArgument {
                        name: "graph".to_string(),
                        detail: format!("`{graph}` is not an IRI: {e}"),
                    })?;
                    parser = parser.with_default_graph(GraphNameRef::from(name));
                }
                let before = self.len()?;
                self.store
                    .dataset()
                    .load_from_reader(parser, bytes)
                    .map_err(|e| Error::InvalidArgument {
                        name: "content".to_string(),
                        detail: format!("parsing {media}: {e}"),
                    })?;
                let after = self.len()?;
                Ok(plain(format!(
                    "loaded {media}: {before} -> {after} quads\n"
                )))
            }
            other => Err(unsupported("store-load", other)),
        }
    }

    fn name(&self) -> &str {
        "store-load"
    }

    fn describe(&self) -> Description {
        Description::new("store-load")
            .title("Bulk-load an RDF document into the durable store")
            .summary(
                "Parse an RDF document and add its statements to the store. The kernel \
                 door that replaces the old `load_turtle` side entrance: this one is \
                 capability-gated and cuts the golden thread `urn:iki:store:load`.",
            )
            .verb(Verb::Sink)
            .verb(Verb::Meta)
            .requires(CAP_WRITE)
            .input(
                ArgSpec::new("content")
                    .summary("The RDF document, in the syntax named by `format`.")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("format")
                    .summary("The document's syntax.")
                    .class(XSD_STRING)
                    .one_of(LOAD_FORMATS)
                    .default_value(LOAD_FORMATS[0])
                    .optional(),
            )
            .input(
                ArgSpec::new("graph")
                    .summary(
                        "Load into this named graph instead of the default graph. \
                         Ignored by quad syntaxes, which name their own graphs.",
                    )
                    .class(XSD_ANY_URI)
                    .optional(),
            )
            .output("text/plain")
    }
}

impl LoadEndpoint {
    fn len(&self) -> Result<usize> {
        self.store
            .dataset()
            .len()
            .map_err(|e| Error::Endpoint(format!("counting quads: {e}")))
    }
}

// ---------------------------------------------------------------------------- shared

fn plain(text: String) -> Representation {
    Representation::new(
        ReprType::new("text/plain").with_param("charset", "utf-8"),
        text.into_bytes(),
    )
}

fn unsupported(id: &str, verb: Verb) -> Error {
    Error::Endpoint(format!("{id} does not support the {verb:?} verb"))
}

/// Serialize a SELECT/ASK result, **refusing** an `as` the form cannot answer in.
///
/// A bound must refuse, not substitute: `as=text/turtle` on a SELECT returning JSON
/// would make the declared `outputs` list true by accident, and a typo would come back
/// as a plausible answer in the wrong syntax with nothing said.
fn serialize_solutions(results: QueryResults, as_type: Option<&str>) -> Result<(String, Vec<u8>)> {
    let io = |e: std::io::Error| Error::Endpoint(format!("serialize: {e}"));
    let format = results_format(as_type)?;
    let bytes = match results {
        QueryResults::Solutions(solutions) => {
            let variables = solutions.variables().to_vec();
            let mut serializer = QueryResultsSerializer::from_format(format)
                .serialize_solutions_to_writer(Vec::new(), variables)
                .map_err(io)?;
            for solution in solutions {
                let solution = solution.map_err(|e| Error::Endpoint(format!("query: {e}")))?;
                serializer.serialize(&solution).map_err(io)?;
            }
            serializer.finish().map_err(io)?
        }
        QueryResults::Boolean(value) => QueryResultsSerializer::from_format(format)
            .serialize_boolean_to_writer(Vec::new(), value)
            .map_err(io)?,
        // Unreachable: the caller checked the shape against the IRI first, and that
        // check is the point — this arm is here so a future refactor that drops it
        // fails loudly rather than serving a graph as a result set.
        QueryResults::Graph(_) => {
            return Err(Error::Endpoint(
                "internal: a graph reached the result-set serializer".to_string(),
            ))
        }
    };
    Ok((bare_media(format.media_type()).to_string(), bytes))
}

/// Serialize a CONSTRUCT/DESCRIBE result, refusing an unusable `as` the same way.
fn serialize_graph(results: QueryResults, as_type: Option<&str>) -> Result<(String, Vec<u8>)> {
    let io = |e: std::io::Error| Error::Endpoint(format!("serialize: {e}"));
    let format = graph_format(as_type)?;
    let QueryResults::Graph(triples) = results else {
        return Err(Error::Endpoint(
            "internal: a result set reached the graph serializer".to_string(),
        ));
    };
    let mut serializer = RdfSerializer::from_format(format).for_writer(Vec::new());
    for triple in triples {
        let triple = triple.map_err(|e| Error::Endpoint(format!("query: {e}")))?;
        serializer
            .serialize_quad(&triple.in_graph(GraphName::DefaultGraph))
            .map_err(io)?;
    }
    Ok((
        bare_media(format.media_type()).to_string(),
        serializer.finish().map_err(io)?,
    ))
}

/// A media type without its parameters.
///
/// ⚠ **`QueryResultsFormat::media_type()` is not always bare**: `Csv` reports
/// `text/csv; charset=utf-8` and `Tsv` likewise, while `Json` and `Xml` report no
/// parameter. Handing that string straight to `ReprType::new` and then calling
/// `.with_param("charset", "utf-8")` yields a canonical form with the parameter TWICE —
/// `ReprType` stores the media type verbatim and appends params, it does not parse them
/// out. The conformance `OUTPUTS` check strips `;charset=` before comparing, so nothing
/// catches it; `an_as_the_query_form_cannot_answer_in_is_refused` does, by asserting the
/// bare type. Same family as declaring a face you do not serve.
fn bare_media(media: &str) -> &str {
    media.split(';').next().unwrap_or(media).trim()
}

fn results_format(as_type: Option<&str>) -> Result<QueryResultsFormat> {
    let Some(spec) = as_type else {
        return Ok(QueryResultsFormat::Json);
    };
    QueryResultsFormat::from_media_type(spec)
        .filter(|f| RESULT_OUTPUTS.contains(&bare_media(f.media_type())))
        .ok_or_else(|| Error::InvalidArgument {
            name: "as".to_string(),
            detail: format!(
                "SELECT/ASK cannot answer in `{spec}` — one of {}. A graph syntax is not \
                 among them: only CONSTRUCT and DESCRIBE answer with a graph",
                RESULT_OUTPUTS.join(", ")
            ),
        })
}

fn graph_format(as_type: Option<&str>) -> Result<RdfFormat> {
    let Some(spec) = as_type else {
        return Ok(RdfFormat::NTriples);
    };
    RdfFormat::from_media_type(spec)
        .filter(|f| GRAPH_OUTPUTS.contains(&bare_media(f.media_type())))
        .ok_or_else(|| Error::InvalidArgument {
            name: "as".to_string(),
            detail: format!(
                "CONSTRUCT/DESCRIBE cannot answer in `{spec}` — one of {}",
                GRAPH_OUTPUTS.join(", ")
            ),
        })
}
