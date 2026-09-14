//! The twelve resources this crate binds, under the namespace it owns.
//!
//! ```text
//! urn:iki:store:select           Source  SPARQL SELECT            urn:cap:store:read
//! urn:iki:store:ask              Source  SPARQL ASK               urn:cap:store:read
//! urn:iki:store:construct        Source  SPARQL CONSTRUCT         urn:cap:store:read
//! urn:iki:store:describe         Source  SPARQL DESCRIBE          urn:cap:store:read
//! urn:iki:store:graph-select     Source  SELECT, one graph        urn:cap:store:read:graph:<iri>
//! urn:iki:store:graph-ask        Source  ASK, one graph           urn:cap:store:read:graph:<iri>
//! urn:iki:store:graph-construct  Source  CONSTRUCT, one graph     urn:cap:store:read:graph:<iri>
//! urn:iki:store:graph-describe   Source  DESCRIBE, one graph      urn:cap:store:read:graph:<iri>
//! urn:iki:store:info             Source  backing, size, coverage  urn:cap:store:read
//! urn:iki:store:update           Sink    SPARQL UPDATE, all of it urn:cap:store:write
//! urn:iki:store:graph-update     Sink    SPARQL UPDATE, one graph urn:cap:store:write:graph:<iri>
//! urn:iki:store:load             Sink    bulk-load an RDF doc     urn:cap:store:write
//! ```
//!
//! **Two doors in each direction, wide and narrow.** [`CAP_WRITE`] is `DROP ALL`, which
//! makes a module layered over this store hand that authority to everyone who may append
//! one triple; [`CAP_WRITE_GRAPH`] is the boundary that fixes it, enforced on effects
//! rather than syntax so that an update naming its graph with a *variable* — or naming
//! none at all — cannot slip through (`src/confine.rs`). [`CAP_READ`] is the same problem
//! read-side, and until [`CAP_READ_GRAPH`] existed the boundary had a **documented
//! bypass**: a caller holding the broad read grant could query another tenant's graph
//! directly and go around the module that was enforcing access to it. The read half is
//! confined **by construction** — the prepared query's dataset specification is set to
//! the one graph before evaluation, so `graph=G` is exactly `FROM <G> FROM NAMED <G>` and
//! nothing is copied (`src/scope.rs`).
//!
//! ⚠ **A scoped query endpoint declares TWO required by-value inputs** (`graph` and
//! `query`), so a bare pipe into one is ambiguous and the engine says so rather than
//! guessing. Naming the graph — which a caller must do anyway — leaves `query` as the one
//! unnamed required input, so `… | urn:iki:store:graph-select graph=<G>` pipes normally.
//!
//! **A value gets into a query through `bindings=`**, never through the parser. For an
//! update there is no such door — oxigraph binds into a prepared query and offers nothing
//! for a prepared update — so [`crate::sparql`]'s term constructors are the supported
//! path, and both Sinks refuse a `bindings` argument rather than accepting one they would
//! have to honour by rewriting text.
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
use oxigraph::model::{GraphName, GraphNameRef, NamedNode, NamedNodeRef, Term, Variable};
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

/// The **per-graph** write scope, as declared: the wildcard-ACL form the ecosystem
/// already uses for `urn:cap:net:*` and fs path ACLs, meaning "holds SOME grant under
/// this prefix". A held grant names one graph — [`cap_write_graph`].
///
/// ★ This is the boundary a module layered over the store needs in order not to hand
/// [`CAP_WRITE`] to everyone who may append a triple. `urn:iki:store:graph-update`
/// enforces it on **effects rather than syntax**, which is the only way it can be exact;
/// `src/confine.rs` has the mechanism and the table of shapes it closes.
///
/// ⚠ **A grant names exactly one graph and nothing is a prefix of anything.** There is
/// no `urn:cap:store:write:graph:urn:iki:ledger:*` form, because `Capability::allows` is
/// an exact-match set membership and inventing prefix semantics for one token would make
/// this crate's grants mean something different from every other grant in the system. A
/// host that wants a caller to write three graphs grants three scopes — which is the
/// intended use, not a workaround.
pub const CAP_WRITE_GRAPH: &str = "urn:cap:store:write:graph:*";

/// The **per-graph** read scope, as declared — the same wildcard-ACL form as
/// [`CAP_WRITE_GRAPH`], meaning "holds SOME grant under this prefix". A held grant names
/// one graph: [`cap_read_graph`].
///
/// ★ **Without this, the write boundary has a documented bypass.** 0.2.1 segmented writes
/// and left [`CAP_READ`] as the whole dataset, so a module enforcing its own read
/// capability over a graph it owns could be gone around entirely by querying the store
/// directly under the broad grant. A boundary that holds in one direction is not a
/// boundary.
///
/// ⚠ **A grant names exactly one graph and nothing is a prefix of anything**, for the
/// reason set out on [`CAP_WRITE_GRAPH`]: `Capability::allows` is exact-match set
/// membership, and inventing prefix semantics for one token would make this crate's grants
/// mean something different from every other grant in the system. Three graphs is three
/// grants.
pub const CAP_READ_GRAPH: &str = "urn:cap:store:read:graph:*";

/// The scope a caller must hold to write `graph` through `urn:iki:store:graph-update`.
///
/// ```
/// assert_eq!(
///     ikigai_store::cap_write_graph("urn:iki:ledger:acme"),
///     "urn:cap:store:write:graph:urn:iki:ledger:acme"
/// );
/// ```
///
/// Concatenation is injective for a fixed prefix, so two graph IRIs never collide on one
/// token; and because the grant is matched exactly, a token for a *shorter* IRI grants
/// nothing over a longer one.
pub fn cap_write_graph(graph: &str) -> String {
    format!("urn:cap:store:write:graph:{graph}")
}

/// The scope a caller must hold to read `graph` through `urn:iki:store:graph-{select,
/// ask,construct,describe}`.
///
/// ```
/// assert_eq!(
///     ikigai_store::cap_read_graph("urn:iki:ledger:acme"),
///     "urn:cap:store:read:graph:urn:iki:ledger:acme"
/// );
/// ```
///
/// ⚠ **It is a sibling of [`cap_write_graph`], not a weaker form of it.** Holding the
/// write scope over a graph does not imply the read scope over it and vice versa — the
/// two are separate grants for the same reason the broad pair are, and a host that means
/// a module to do both grants both.
pub fn cap_read_graph(graph: &str) -> String {
    format!("urn:cap:store:read:graph:{graph}")
}

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

/// The golden thread `urn:iki:store:graph-update` cuts on success.
///
/// ★ **A third writing IRI means a third thread, and a reader must depend on all
/// three.** Adding a write door without adding its thread would have left every
/// cacheable read serving stale bytes after a scoped write — silently, on the branch
/// that looks like success. `a_scoped_write_invalidates_a_cached_read` is the test that
/// keeps it true; the reason an endpoint cannot simply cut [`UPDATE_THREAD`] instead is
/// on [`LOAD_THREAD`].
pub const GRAPH_UPDATE_THREAD: &str = "urn:iki:store:graph-update";

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
    for (form, id, scoped_id, graph_shaped) in FORMS {
        space = space.bind(
            Exact::new(format!("urn:iki:store:{form}")),
            QueryEndpoint {
                store: Arc::clone(&store),
                form,
                id,
                graph_shaped,
                scoped: false,
            },
        );
        space = space.bind(
            Exact::new(format!("urn:iki:store:graph-{form}")),
            QueryEndpoint {
                store: Arc::clone(&store),
                form,
                id: scoped_id,
                graph_shaped,
                scoped: true,
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
        .bind(
            Exact::new("urn:iki:store:graph-update"),
            GraphUpdateEndpoint {
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
            .depends_on(GRAPH_UPDATE_THREAD)
    } else {
        rep
    }
}

// ---------------------------------------------------------------------------- query

/// The four query forms, as `(IRI suffix, description id, scoped description id,
/// graph-shaped?)`.
///
/// Each carries a UNIQUE description id, so their catalog subjects and any id-keyed
/// projection (an MCP tool name) do not collide — with each other, with the scoped twin,
/// or with `ikigai-sparql`'s `sparql-{form}`.
///
/// ⚠ **Eight IRIs rather than a `graph=` argument on four, and that is a real surface
/// cost paid deliberately.** The declared `requires` differs between the broad and the
/// scoped form ([`CAP_READ`] vs [`CAP_READ_GRAPH`]), and the kernel's capability
/// pre-check runs *before* `invoke` can see an argument — so one IRI taking an optional
/// `graph=` would have to declare the weaker of the two and let the endpoint decide,
/// which is exactly the over-offer the module recipe forbids. Same argument the write
/// door made in 0.2.1.
const FORMS: [(&str, &str, &str, bool); 4] = [
    ("select", "store-select", "store-graph-select", false),
    ("ask", "store-ask", "store-graph-ask", false),
    (
        "construct",
        "store-construct",
        "store-graph-construct",
        true,
    ),
    ("describe", "store-describe", "store-graph-describe", true),
];

#[derive(Clone)]
struct QueryEndpoint {
    store: Arc<DurableStore>,
    /// The SPARQL form this IRI answers.
    form: &'static str,
    id: &'static str,
    /// Whether this form answers with a graph (CONSTRUCT/DESCRIBE) or a result set.
    graph_shaped: bool,
    /// Whether this is the graph-scoped twin: takes `graph=`, requires a grant for that
    /// graph, and sees nothing else. See `src/scope.rs`.
    scoped: bool,
}

#[async_trait]
impl Endpoint for QueryEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        match inv.request.verb {
            Verb::Source => {
                let query = inv.inline_str("query")?;
                // ★ The parameterized half of the capability, checked before anything is
                // parsed or evaluated: the kernel's pre-check can only see the wildcard
                // (this caller holds SOME grant under `urn:cap:store:read:graph:`), and
                // this is where it is checked that the grant names the graph asked for.
                let target = self.scope(inv)?;
                let bound = match inv.inline_str("bindings") {
                    Ok(json) => crate::sparql::parse_bindings(json)?,
                    Err(_) => Vec::new(),
                };
                let mut prepared = SparqlEvaluator::new().parse_query(query).map_err(|e| {
                    Error::InvalidArgument {
                        name: "query".to_string(),
                        detail: format!("not a SPARQL query: {e}"),
                    }
                })?;
                if let Some(target) = &target {
                    // ⚠ `FROM` / `FROM NAMED` is a SECOND way to name a dataset. It could
                    // not widen the scope — `confine` overwrites the specification the
                    // parser built from it — but answering `FROM <other>` with this
                    // graph's rows would label one tenant's data with another's graph
                    // name, so it is refused rather than silently overridden.
                    if crate::scope::names_its_own_dataset(&prepared) {
                        return Err(Error::InvalidArgument {
                            name: "query".to_string(),
                            detail: format!(
                                "this query carries its own `FROM` / `FROM NAMED` clauses, and \
                                 `urn:iki:store:graph-{}` already fixes the dataset: `graph=` IS \
                                 the dataset, exactly `FROM <{}> FROM NAMED <{}>`. The clauses \
                                 are refused rather than overridden, because answering a `FROM` \
                                 naming another graph with this graph's rows would be a wrong \
                                 answer that looked right. Drop them",
                                self.form,
                                target.as_str(),
                                target.as_str(),
                            ),
                        });
                    }
                    crate::scope::confine(&mut prepared, target);
                }
                for (name, term) in bound.iter().cloned() {
                    // `Variable::new` cannot fail here: `parse_bindings` already held the
                    // name to the same character set, and named the offending key when it
                    // did — which the evaluator's own error does not.
                    let variable = Variable::new(&name).map_err(|e| Error::InvalidArgument {
                        name: "bindings".to_string(),
                        detail: format!("`{name}` is not a variable name: {e}"),
                    })?;
                    prepared = prepared.substitute_variable(variable, term);
                }
                let results = prepared
                    .on_store(self.store.dataset())
                    .execute()
                    .map_err(|e| self.query_error(e, &bound))?;

                // ★ Refuse a query of the wrong shape rather than serving it here. The
                // IRI is a promise about what comes back — it is what fixes this
                // action's declared `outputs` — and answering a CONSTRUCT under
                // `urn:iki:store:select` would make that promise true only by accident.
                let is_graph = matches!(results, QueryResults::Graph(_));
                if is_graph != self.graph_shaped {
                    return Err(Error::InvalidArgument {
                        name: "query".to_string(),
                        detail: format!(
                            "this is `{}`, which answers with {}; that query answers with {}. \
                             Resolve the IRI for its form instead",
                            self.iri(),
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
        let form = self.form.to_uppercase();
        let desc = if self.scoped {
            Description::new(self.id)
                .title(format!("SPARQL {form} confined to one named graph"))
                .summary(format!(
                    "Evaluate a SPARQL {form} against the named graph given by `graph` and \
                     nothing else, under a grant for that graph alone. The confinement is the \
                     evaluator's own dataset specification, not a check over the query text: \
                     `graph=G` means exactly `FROM <G> FROM NAMED <G>`, so a `GRAPH <other>` \
                     block matches nothing, a `GRAPH ?g` can only bind G, and the store's own \
                     default graph is unreachable. A query of another form is refused, not \
                     served here, and so is one carrying its own `FROM` clauses.",
                ))
                .verb(Verb::Source)
                .verb(Verb::Meta)
                .requires(CAP_READ_GRAPH)
                .input(
                    ArgSpec::new("graph")
                        .summary(
                            "The one named graph this query may read. The caller must hold \
                             `urn:cap:store:read:graph:<this IRI>`.",
                        )
                        .class(XSD_ANY_URI),
                )
        } else {
            Description::new(self.id)
                .title(format!("SPARQL {form} over the durable store"))
                .summary(format!(
                    "Evaluate a SPARQL {form} against the store this host owns. A query of \
                     another form is refused, not served here.",
                ))
                .verb(Verb::Source)
                .verb(Verb::Meta)
                .requires(CAP_READ)
        };
        let desc = desc
            .input(
                ArgSpec::new("query")
                    .summary(format!("A SPARQL {form} query."))
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("bindings")
                    .summary(
                        "Values for variables in the query, as a JSON object of name → \
                         value, so a value never passes through the SPARQL parser as \
                         syntax. A bare JSON string is a plain literal; a number, an \
                         `xsd:integer` or `xsd:double`; `true`/`false`, an `xsd:boolean`; \
                         and the SPARQL-results term shape \
                         (`{\"type\":\"uri\",\"value\":…}`) names an IRI or a typed or \
                         language-tagged literal. Every bound variable must appear in the \
                         query's projection — `SELECT *` if in doubt — and a binding the \
                         query does not mention is refused, not ignored.",
                    )
                    .class(XSD_STRING)
                    .optional(),
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

impl QueryEndpoint {
    /// This endpoint's IRI, for a refusal that has to name it.
    fn iri(&self) -> String {
        if self.scoped {
            format!("urn:iki:store:graph-{}", self.form)
        } else {
            format!("urn:iki:store:{}", self.form)
        }
    }

    /// The graph this invocation may read, or `None` on the broad form.
    ///
    /// ★ Declared and enforced are the same scope, which is the whole contract. The
    /// kernel checks the declared [`CAP_READ_GRAPH`] wildcard before `invoke`; this
    /// checks the grant against the graph actually named, because an argument is not
    /// visible to a pre-check.
    ///
    /// ⚠ **[`CAP_READ`] does NOT satisfy this door and is not meant to.** A broad holder
    /// uses `urn:iki:store:select` and sees the whole dataset. Accepting both here would
    /// mean the declared scope was not the enforced one — and the ablation runs the other
    /// way too: a graph grant does not open the broad door, which is what makes it a
    /// boundary rather than a hint.
    fn scope(&self, inv: &Invocation<'_>) -> Result<Option<NamedNode>> {
        if !self.scoped {
            return Ok(None);
        }
        let graph = inv.inline_str("graph")?;
        let target = NamedNode::new(graph).map_err(|e| Error::InvalidArgument {
            name: "graph".to_string(),
            detail: format!("`{graph}` is not an IRI: {e}"),
        })?;
        let scope = cap_read_graph(target.as_str());
        if !inv.capability.allows(&scope) {
            return Err(Error::Denied(format!(
                "reading graph <{}> through `{}` needs the grant `{scope}`, which this \
                 capability does not hold. A grant names exactly one graph; holding \
                 `{CAP_READ}` does not imply it, and is instead the authority for \
                 `urn:iki:store:{}` over the whole dataset",
                target.as_str(),
                self.iri(),
                self.form,
            )));
        }
        Ok(Some(target))
    }

    /// Make oxigraph's refusal of an unusable binding say what to do about it.
    ///
    /// ★ **The refuse-or-ignore question is settled upstream, in the right direction.**
    /// oxigraph rejects a substitution for a variable the query does not project rather
    /// than dropping it, so a binding that would have done nothing is an error instead of
    /// a query that silently ran unconstrained — which is exactly the failure this
    /// argument exists to prevent, arriving by the back door. This crate does not have to
    /// choose; it only has to explain, because the upstream sentence ("does not contains
    /// variable ?o in its SELECT projection") is true and tells a caller nothing about
    /// the fix.
    ///
    /// ⚠ Matched on text, like [`crate::store`]'s lock matcher and for the same reason:
    /// the evaluator reports it as an untyped evaluation error.
    /// `an_unprojected_binding_is_refused_with_the_fix` pins it against a real query, so
    /// an upstream rewording fails a test rather than quietly degrading the message.
    fn query_error(&self, e: impl std::fmt::Display, bound: &[(String, Term)]) -> Error {
        let text = e.to_string();
        if bound.is_empty() || !text.contains("projection") {
            return Error::Endpoint(format!("query: {text}"));
        }
        let names: Vec<String> = bound.iter().map(|(n, _)| format!("?{n}")).collect();
        Error::InvalidArgument {
            name: "bindings".to_string(),
            detail: format!(
                "{text}. Every bound variable must appear in the query's projection, and this \
                 query projects fewer than the {} bound here ({}). Either name it in the \
                 SELECT clause or write `SELECT *`; ASK, CONSTRUCT and DESCRIBE have no \
                 projection to widen and accept any variable in the pattern. A binding the \
                 query does not mention is refused rather than ignored, so that a filter you \
                 thought was applied can never silently not be",
                names.len(),
                names.join(", "),
            ),
        }
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
                no_bindings(inv, "urn:iki:store:update")?;
                let _writes = self.store.write_lock();
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

// --------------------------------------------------------------- graph-scoped update

/// `urn:iki:store:graph-update` — an arbitrary SPARQL UPDATE that can only affect one
/// named graph.
///
/// See [`CAP_WRITE_GRAPH`] for the capability and `src/confine.rs` for the mechanism
/// and the table of update shapes it closes.
#[derive(Clone)]
struct GraphUpdateEndpoint {
    store: Arc<DurableStore>,
}

#[async_trait]
impl Endpoint for GraphUpdateEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        match inv.request.verb {
            Verb::Sink => {
                let update = inv.inline_str("content")?;
                no_bindings(inv, "urn:iki:store:graph-update")?;
                let graph = inv.inline_str("graph")?;
                let target = NamedNode::new(graph).map_err(|e| Error::InvalidArgument {
                    name: "graph".to_string(),
                    detail: format!("`{graph}` is not an IRI: {e}"),
                })?;

                // ★ The parameterized half of the capability, enforced here because the
                // kernel's pre-check can only see the wildcard: it knows the caller holds
                // SOME grant under `urn:cap:store:write:graph:`, and this is where it is
                // checked that the grant is for the graph actually named. Declared
                // (`CAP_WRITE_GRAPH`) and enforced (here) are the same scope, which is
                // the whole contract — the fs path ACLs are shaped identically.
                //
                // ⚠ `urn:cap:store:write` does NOT satisfy this door and is not meant to.
                // A broad holder uses `urn:iki:store:update`, which copies nothing and
                // sees the whole dataset. Accepting both here would mean the declared
                // scope was not the enforced one.
                let scope = cap_write_graph(target.as_str());
                if !inv.capability.allows(&scope) {
                    return Err(Error::Denied(format!(
                        "writing graph <{}> through `urn:iki:store:graph-update` needs the grant \
                         `{scope}`, which this capability does not hold. A grant names exactly \
                         one graph; holding `urn:cap:store:write` does not imply it, and is \
                         instead the authority for `urn:iki:store:update` over the whole dataset",
                        target.as_str()
                    )));
                }

                let _writes = self.store.write_lock();
                let applied = crate::confine::scoped_update(self.store.dataset(), &target, update)?;
                Ok(plain(format!(
                    "updated <{}>: +{} -{} quads\n",
                    target.as_str(),
                    applied.added,
                    applied.removed
                )))
            }
            other => Err(unsupported("store-graph-update", other)),
        }
    }

    fn name(&self) -> &str {
        "store-graph-update"
    }

    fn describe(&self) -> Description {
        Description::new("store-graph-update")
            .title("SPARQL UPDATE confined to one named graph")
            .summary(
                "Apply a SPARQL 1.1 UPDATE that can only affect the named graph given by \
                 `graph`, under a grant for that graph alone. The update is evaluated \
                 against a dataset containing that graph and nothing else, and is refused \
                 in full — with the offending statement named — if anything would land in \
                 another graph or in the default graph. It therefore cannot READ another \
                 graph either, which is the point: this is a boundary, not a filter. Cuts \
                 the golden thread `urn:iki:store:graph-update`.",
            )
            .verb(Verb::Sink)
            .verb(Verb::Meta)
            .requires(CAP_WRITE_GRAPH)
            .input(
                ArgSpec::new("content")
                    .summary(
                        "A SPARQL 1.1 UPDATE. Statements must be inside a `GRAPH <…>` \
                         block naming the scoped graph: a bare `INSERT DATA { … }` writes \
                         the default graph and is refused.",
                    )
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("graph")
                    .summary(
                        "The one named graph this update may affect. The caller must hold \
                         `urn:cap:store:write:graph:<this IRI>`.",
                    )
                    .class(XSD_ANY_URI),
            )
            .output("text/plain")
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
                let _writes = self.store.write_lock();
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

/// Refuse a `bindings` argument on a write, rather than accepting one that would do
/// nothing.
///
/// ★ **The asymmetry is upstream's and it is worth saying out loud rather than letting a
/// caller discover it.** oxigraph's `PreparedSparqlQuery` has `substitute_variable`; its
/// `PreparedSparqlUpdate` has nothing, and a parsed update's AST is private, so there is
/// no way to bind a value into an update without rewriting the update's TEXT — which is
/// string interpolation with a longer name, in the one place where getting it wrong is
/// `DROP ALL`. An undeclared argument the kernel simply ignores would be the worst of
/// both: a caller who believed the value was bound, and a query that interpolated
/// nothing.
fn no_bindings(inv: &Invocation<'_>, iri: &str) -> Result<()> {
    if inv.inline_str("bindings").is_err() {
        return Ok(());
    }
    Err(Error::InvalidArgument {
        name: "bindings".to_string(),
        detail: format!(
            "`{iri}` takes a SPARQL UPDATE and cannot bind values into it: oxigraph offers \
             variable substitution for queries and none for updates, and this crate will not \
             fake it by rewriting the update's text. Build the terms with \
             `ikigai_store::sparql::{{literal, iri, typed_literal, integer, boolean}}`, which \
             do not escape anything — they construct an RDF term and let oxigraph serialize \
             it, so there is one escaper in the system and it is the one that owns the grammar"
        ),
    })
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
        //
        // ★ This match is exhaustive over a THIRD-PARTY enum with no catch-all, which is
        // the shape that broke 0.2.2 in `sparql.rs`. It is kept, and the difference is
        // the whole rule: **`QueryResults`'s three variants are ungated**, while
        // `oxrdf::Term`'s fourth is `#[cfg(feature = "rdf-12")]`. An ungated variant
        // added upstream appears in THIS crate's build too, so exhaustiveness turns it
        // into a compile error here — on our CI, before a consumer ever sees it, which
        // is exactly what we want. A GATED variant appears only in builds we do not
        // control, so the same exhaustiveness is a landmine that detonates downstream.
        // ⚠ Nothing checks that distinction automatically: if spareval ever puts a
        // variant behind a feature, this match joins the trap silently. Audited
        // 2026-09-13 against spareval 0.2.7, along with `serde_json::Value` in
        // `sparql::json_to_term` (no gated variants either) and `GraphName` in
        // `confine.rs` (ungated, and it has a catch-all regardless).
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
