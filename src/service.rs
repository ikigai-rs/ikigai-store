//! SPARQL that never leaves the process, in any build: the refusing evaluator and the door
//! checks (ledger #1083, #145, #992).
//!
//! ★ **Public so other crates stop needing copies.** Any crate that builds its own oxigraph
//! `SparqlEvaluator` (ikigai-ledger, ikigai-markdown, ikigai-nl and ikigai-script all do) has
//! the hole this module closes for the store, and closes it the same way by building through
//! [`evaluator`] and checking at its door with [`refuse_service`],
//! [`refuse_service_in_update`] and [`refuse_load`].
//!
//! # The hole
//!
//! oxigraph's `SparqlEvaluator` installs a default HTTP service handler whenever
//! `oxigraph/http-client` is on, and no crate can turn that feature off for itself: Cargo
//! unifies features across the whole graph, and rudof_rdf enables `http-client-rustls-native`
//! on every native target, so any host linking `ikigai-shacl` (`ikigai-cli`, `ikigai-web-demo`)
//! has it. There, `SERVICE <http://…>` in a caller's query is an outbound request with no
//! `urn:cap:net:*` anywhere near it. oxigraph's own off switch,
//! `without_default_http_service_handler`, is itself `#[cfg(feature = "http-client")]`, and a
//! crate cannot `cfg` on a dependency's feature being enabled by someone else, so it cannot be
//! called. Until 0.2.10 every one of this store's ten doors had the hole.
//!
//! # What each item covers, exactly
//!
//! | item | covers | does NOT cover |
//! | --- | --- | --- |
//! | [`evaluator`] | every `SERVICE`, constant or variable name, `SILENT` or not, in a query or an update's `WHERE`: refused by the handler, never called, in every build | **`LOAD <url>`**: oxigraph builds `LOAD`'s HTTP client from the evaluator's HTTP settings, not from the service registry, so the handler never sees it |
//! | [`refuse_service`] / [`refuse_service_in_update`] | a `SERVICE` anywhere in the parsed algebra (`EXISTS`, sub-selects, `OPTIONAL`, `LATERAL` included), refused as a typed `InvalidArgument` BEFORE evaluation | `LOAD`; and only what you hand it, so it is a door check, not a guarantee |
//! | [`refuse_load`] | `LOAD <url>`, `SILENT` or not, `INTO GRAPH` or not, as a typed `InvalidArgument` before evaluation | — it is the ONLY guard against `LOAD`: an evaluator built by [`evaluator`] still fetches it in a build with `oxigraph/http-client` on |
//!
//! So a consumer evaluating caller-supplied SPARQL wants **all three**: [`evaluator`] for the
//! guarantee, [`refuse_service`] (or the update form) for a typed answer, and for an update,
//! [`refuse_load`], because nothing else stops `LOAD`. `FROM` / `FROM NAMED` / `GRAPH <iri>`
//! need nothing: oxigraph reads them as names of graphs in the store and never fetches them.
//!
//! The door checks are separate from the evaluator because a `SERVICE SILENT` swallows the
//! handler's refusal (by the spec) and answers with nothing bound, and a plain `SERVICE` fails
//! mid-evaluation as an untyped error. Only a check before evaluation can answer both as
//! "this input is refused".
//!
//! ★ Why `InvalidArgument` and not `Denied` naming `urn:cap:net:*`: no grant opens this. The
//! store never federates, so a `Denied` would send a caller looking for a capability that
//! changes nothing. The refusal texts name the store's own remedy (fetch through the kernel,
//! where the net capability applies, and sink into `urn:iki:store:load`).
//!
//! ⚠ The checks take `spargebra`'s `Query` / `Update`, like [`crate::budget::check_query`]:
//! parse with `spargebra::SparqlParser`, check, then hand the parsed value to
//! `evaluator().for_query(…)` / `.for_update(…)`. Your `spargebra` must be the one oxigraph
//! uses (0.4.x for oxigraph 0.5), which cargo unifies on its own when you name it with a caret.
//!
//! ```
//! use ikigai_core::Error;
//! use ikigai_store::service::{evaluator, refuse_load, refuse_service, refuse_service_in_update};
//! use oxigraph::sparql::QueryResults;
//! use oxigraph::store::Store;
//!
//! let parser = || spargebra::SparqlParser::new();
//! let text = "SELECT ?s WHERE { SERVICE <http://127.0.0.1:9/sparql> { ?s ?p ?o } }";
//!
//! // The door check: a typed refusal, naming the argument, before anything runs.
//! let query = parser().parse_query(text)?;
//! let refused = refuse_service(&query, "query").unwrap_err();
//! assert!(matches!(&refused, Error::InvalidArgument { name, detail }
//!     if name == "query" && detail.contains("`SERVICE` is not available")));
//!
//! // The guarantee: even unchecked, the evaluator refuses the call itself, in every build.
//! let store = Store::new()?;
//! let failure = match evaluator().for_query(query).on_store(&store).execute() {
//!     Err(e) => e.to_string(),
//!     Ok(QueryResults::Solutions(mut rows)) => rows.next().unwrap().unwrap_err().to_string(),
//!     Ok(_) => unreachable!(),
//! };
//! assert!(failure.contains("`SERVICE <http://127.0.0.1:9/sparql>` is not available"));
//!
//! // ⚠ LOAD is NOT a service: the service checks pass it, and only `refuse_load` stops it.
//! let load = parser().parse_update("LOAD <http://127.0.0.1:9/doc.ttl>")?;
//! assert!(refuse_service_in_update(&load, "content").is_ok());
//! assert!(matches!(refuse_load(&load, "content"),
//!     Err(Error::InvalidArgument { name, .. }) if name == "content"));
//! # Ok::<_, Box<dyn std::error::Error>>(())
//! ```

use ikigai_core::{Error, Result};
use oxigraph::model::NamedNode;
use oxigraph::sparql::{DefaultServiceHandler, QuerySolutionIter, SparqlEvaluator};
use oxiri::Iri;
use spargebra::algebra::{AggregateExpression, Expression, GraphPattern, OrderExpression};
use spargebra::{GraphUpdateOperation, Query, Update};

/// A `SparqlEvaluator` with [`Refuse`] as its default service handler, so no `SERVICE` reaches
/// a network client, in any build. Chain the rest as usual (`with_cancellation_token`,
/// `for_query`, `parse_update`, …).
///
/// `with_default_service_handler` exists in every build, and with `oxigraph/http-client` on it
/// also clears the flag that would install oxigraph's HTTP handler (oxigraph 0.5.11,
/// `sparql/mod.rs`); that is what makes this work where `without_default_http_service_handler`
/// cannot be called. ⚠ It does NOT stop `LOAD <url>`: see [`refuse_load`].
///
/// ★ This crate builds every `SparqlEvaluator` through this; a unit test scans `src/` and fails
/// on any other plain constructor outside a test module.
pub fn evaluator() -> SparqlEvaluator {
    SparqlEvaluator::new().with_default_service_handler(Refuse)
}

/// A default service handler that refuses every service, by name, without calling it. Install
/// it on an evaluator you build some other way with `with_default_service_handler(Refuse)`.
pub struct Refuse;

/// What [`Refuse`] answers: the service it would not call.
#[derive(Debug)]
pub struct Refused(pub NamedNode);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`SERVICE <{}>` is not available through this store: it would be an outbound \
             request no network capability gates",
            self.0.as_str()
        )
    }
}

impl std::error::Error for Refused {}

impl DefaultServiceHandler for Refuse {
    type Error = Refused;

    fn handle(
        &self,
        service_name: &NamedNode,
        _pattern: &GraphPattern,
        _base_iri: Option<&Iri<String>>,
    ) -> std::result::Result<QuerySolutionIter<'static>, Refused> {
        Err(Refused(service_name.clone()))
    }
}

/// Refuse a parsed query with a `SERVICE` anywhere in it, as `InvalidArgument` naming `arg`.
/// A door check, before evaluation; [`evaluator`] is the guarantee behind it.
pub fn refuse_service(query: &Query, arg: &str) -> Result<()> {
    let pattern = match query {
        Query::Select { pattern, .. }
        | Query::Ask { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Construct { pattern, .. } => pattern,
    };
    if has_service(pattern) {
        return Err(refusal(arg));
    }
    Ok(())
}

/// [`refuse_service`] for an update: the `WHERE` of every `DELETE`/`INSERT` operation. No
/// other update operation has a pattern. ⚠ It passes `LOAD`, which is not a `SERVICE`: check
/// that with [`refuse_load`].
pub fn refuse_service_in_update(update: &Update, arg: &str) -> Result<()> {
    let services = update.operations.iter().any(|operation| match operation {
        GraphUpdateOperation::DeleteInsert { pattern, .. } => has_service(pattern),
        _ => false,
    });
    if services {
        return Err(refusal(arg));
    }
    Ok(())
}

/// Refuse an update with a `LOAD <url>` in it, as `InvalidArgument` naming `arg`, before
/// anything is evaluated (ledger #992).
///
/// ★ The ONLY guard against `LOAD`: the fetch is oxigraph's own, built from the evaluator's
/// HTTP settings rather than its service registry, so [`Refuse`] never sees it. In a host whose
/// graph enables `oxigraph/http-client` (`ikigai-cli` does, through rudof) an unchecked `LOAD`
/// is an outbound request no `urn:cap:net:*` gates (ledger #145), and the fetched document is
/// parsed inside oxigraph with nothing between fetch and parse for a depth scan
/// ([`crate::depth`]) to stand in: a document nesting ~50,000 triple terms aborted the host on
/// the `ikigai-store-sparql` thread. Without the feature `LOAD` fails at evaluation anyway; this
/// makes it fail at the door, by name, in every build. To bring a remote graph in, source it
/// through the kernel (where the net capability applies) and sink it into
/// `urn:iki:store:load`.
pub fn refuse_load(update: &Update, arg: &str) -> Result<()> {
    let loads = update
        .operations
        .iter()
        .any(|op| matches!(op, GraphUpdateOperation::Load { .. }));
    if loads {
        return Err(Error::InvalidArgument {
            name: arg.to_string(),
            detail: "`LOAD` is not available through this store: it would fetch and parse a \
                     document inside the SPARQL engine, unscanned for depth and ungated by any \
                     network capability. Nothing was evaluated. Source the document through the \
                     kernel and sink it into `urn:iki:store:load`"
                .to_string(),
        });
    }
    Ok(())
}

fn refusal(arg: &str) -> Error {
    Error::InvalidArgument {
        name: arg.to_string(),
        detail: "`SERVICE` is not available through this store: a federated call would be an \
                 outbound request from inside a SPARQL string, gated by no network capability, \
                 and no grant opens it here. Nothing was evaluated. Fetch remote data through \
                 the kernel, where the net capability applies, and sink it into \
                 `urn:iki:store:load`"
            .to_string(),
    }
}

/// Whether `pattern` contains a `SERVICE`, anywhere, including inside `EXISTS`.
///
/// ⚠ Exhaustive over `spargebra`'s enums on purpose, for the reason `budget::Cost` gives: the
/// only feature-gated variants in spargebra 0.4.7 are in `Function` (never matched: a call is
/// walked by its arguments) and `GraphPattern::Lateral`, gated on `sep-0006`, which this
/// crate's manifest enables itself. A new variant upstream is then a compile error here, not a
/// pattern this walk silently skips. Recursive, and bounded: it runs after
/// `budget::check_query`, on the sized SPARQL thread.
fn has_service(pattern: &GraphPattern) -> bool {
    match pattern {
        GraphPattern::Service { .. } => true,
        GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => false,
        GraphPattern::Join { left, right }
        | GraphPattern::Lateral { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => has_service(left) || has_service(right),
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => {
            has_service(left)
                || has_service(right)
                || expression.as_ref().is_some_and(expression_has_service)
        }
        GraphPattern::Filter { expr, inner } => expression_has_service(expr) || has_service(inner),
        GraphPattern::Extend {
            inner, expression, ..
        } => expression_has_service(expression) || has_service(inner),
        GraphPattern::OrderBy { inner, expression } => {
            expression.iter().any(|order| match order {
                OrderExpression::Asc(e) | OrderExpression::Desc(e) => expression_has_service(e),
            }) || has_service(inner)
        }
        GraphPattern::Group {
            inner, aggregates, ..
        } => {
            aggregates.iter().any(|(_, aggregate)| match aggregate {
                AggregateExpression::FunctionCall { expr, .. } => expression_has_service(expr),
                AggregateExpression::CountSolutions { .. } => false,
            }) || has_service(inner)
        }
        GraphPattern::Graph { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. } => has_service(inner),
    }
}

fn expression_has_service(expression: &Expression) -> bool {
    match expression {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => false,
        Expression::Exists(pattern) => has_service(pattern),
        Expression::In(e, members) => {
            expression_has_service(e) || members.iter().any(expression_has_service)
        }
        Expression::Coalesce(args) | Expression::FunctionCall(_, args) => {
            args.iter().any(expression_has_service)
        }
        Expression::If(a, b, c) => {
            expression_has_service(a) || expression_has_service(b) || expression_has_service(c)
        }
        Expression::UnaryPlus(e) | Expression::UnaryMinus(e) | Expression::Not(e) => {
            expression_has_service(e)
        }
        Expression::Or(a, b)
        | Expression::And(a, b)
        | Expression::Equal(a, b)
        | Expression::SameTerm(a, b)
        | Expression::Greater(a, b)
        | Expression::GreaterOrEqual(a, b)
        | Expression::Less(a, b)
        | Expression::LessOrEqual(a, b)
        | Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => expression_has_service(a) || expression_has_service(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::sparql::QueryResults;
    use oxigraph::store::Store;

    fn query(text: &str) -> Query {
        spargebra::SparqlParser::new().parse_query(text).unwrap()
    }

    /// The walk finds `SERVICE` wherever the grammar can put it.
    #[test]
    fn the_walk_finds_service_in_every_position() {
        let s = "SERVICE <http://127.0.0.1:9/x> { ?s ?p ?o }";
        for text in [
            format!("SELECT * WHERE {{ {s} }}"),
            format!("SELECT * WHERE {{ ?a ?b ?c . {s} }}"),
            format!("SELECT * WHERE {{ ?a ?b ?c OPTIONAL {{ {s} }} }}"),
            format!("SELECT * WHERE {{ {{ ?a ?b ?c }} UNION {{ {s} }} }}"),
            format!("SELECT * WHERE {{ ?a ?b ?c MINUS {{ {s} }} }}"),
            format!("SELECT * WHERE {{ GRAPH ?g {{ {s} }} }}"),
            format!("SELECT * WHERE {{ ?a ?b ?c FILTER EXISTS {{ {s} }} }}"),
            format!("SELECT * WHERE {{ ?a ?b ?c FILTER (!EXISTS {{ {s} }} || false) }}"),
            format!("SELECT * WHERE {{ ?a ?b ?c BIND(IF(EXISTS {{ {s} }}, 1, 0) AS ?x) }}"),
            format!("SELECT * WHERE {{ {{ SELECT ?s WHERE {{ {s} }} LIMIT 1 }} }}"),
            format!("SELECT * WHERE {{ ?a ?b ?c }} ORDER BY (EXISTS {{ {s} }})"),
            format!("SELECT (COUNT(EXISTS {{ {s} }}) AS ?n) WHERE {{ ?a ?b ?c }}"),
            format!("SELECT * WHERE {{ BIND(1 AS ?x) LATERAL {{ {s} }} }}"),
            "SELECT * WHERE { BIND(<http://127.0.0.1:9/x> AS ?e) LATERAL { SERVICE ?e { ?s ?p ?o } } }"
                .to_string(),
            "ASK { SERVICE SILENT <http://127.0.0.1:9/x> { ?s ?p ?o } }".to_string(),
            format!("CONSTRUCT {{ ?s ?p ?o }} WHERE {{ {s} }}"),
            format!("DESCRIBE ?s WHERE {{ {s} }}"),
        ] {
            let err = refuse_service(&query(&text), "query").expect_err(&text);
            assert!(
                matches!(&err, Error::InvalidArgument { name, .. } if name == "query"),
                "{text}: {err}"
            );
        }
    }

    /// …and nowhere it is not: an IRI that merely names a service, or the word in a literal.
    #[test]
    fn the_walk_does_not_refuse_what_is_not_a_service() {
        for text in [
            "SELECT * WHERE { ?s ?p ?o }",
            "SELECT * WHERE { <http://127.0.0.1:9/x> ?p \"SERVICE <x> { }\" }",
            "SELECT * WHERE { GRAPH <http://127.0.0.1:9/x> { ?s ?p ?o } }",
            "SELECT * WHERE { ?s ?p ?o FILTER EXISTS { ?s ?p ?o } }",
            "DESCRIBE <http://127.0.0.1:9/x>",
        ] {
            refuse_service(&query(text), "query").unwrap_or_else(|e| panic!("{text}: {e}"));
        }
    }

    #[test]
    fn the_walk_reads_an_update_s_where() {
        let parse = |t: &str| spargebra::SparqlParser::new().parse_update(t).unwrap();
        let err = refuse_service_in_update(
            &parse(
                "INSERT DATA { <urn:a> <urn:b> <urn:c> } ; \
                 DELETE { ?s ?p ?o } WHERE { SERVICE <http://127.0.0.1:9/x> { ?s ?p ?o } }",
            ),
            "content",
        )
        .unwrap_err();
        assert!(matches!(&err, Error::InvalidArgument { name, .. } if name == "content"));
        refuse_service_in_update(
            &parse("DELETE { ?s ?p ?o } WHERE { ?s ?p ?o } ; CLEAR GRAPH <urn:g>"),
            "content",
        )
        .unwrap();
    }

    /// The guarantee layer on its own: an evaluator built by [`evaluator`] answers a `SERVICE`
    /// with the refusal, never with a call, in every build.
    #[test]
    fn the_evaluator_refuses_every_service_itself() {
        let store = Store::new().unwrap();
        let results = evaluator()
            .parse_query("SELECT ?s WHERE { SERVICE <http://127.0.0.1:9/x> { ?s ?p ?o } }")
            .unwrap()
            .on_store(&store)
            .execute();
        let error = match results {
            Err(e) => e.to_string(),
            Ok(QueryResults::Solutions(solutions)) => solutions
                .into_iter()
                .find_map(|s| s.err())
                .expect("a SERVICE was answered")
                .to_string(),
            Ok(_) => panic!("not a solution set"),
        };
        assert!(
            error.contains("`SERVICE <http://127.0.0.1:9/x>` is not available"),
            "{error}"
        );
    }

    /// ★ Every `SparqlEvaluator` outside a test module is built by [`evaluator`]. One plain
    /// constructor added to a door later is an outbound request in any host with
    /// `oxigraph/http-client` on, and nothing else would notice: the door's own algebra walk
    /// would still refuse the query, so even `tests/service_egress.rs` stays green until the
    /// walk is skipped too.
    #[test]
    fn every_evaluator_in_this_crate_is_built_by_evaluator() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let file = entry.unwrap().path();
            if file.extension() != Some(std::ffi::OsStr::new("rs")) {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            // Library code only: everything from the first test module down is tests.
            let library = text.split("#[cfg(test)]").next().unwrap_or("");
            let constructor = ["SparqlEvaluator", "::new()"].concat();
            let count = library.matches(&constructor).count();
            let allowed = usize::from(file.ends_with("service.rs"));
            if count != allowed {
                offenders.push(format!("{}: {count}", file.display()));
            }
        }
        assert!(
            offenders.is_empty(),
            "build every SparqlEvaluator through `service::evaluator()`: {offenders:?}"
        );
    }
}
