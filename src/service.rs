//! `SERVICE` never leaves the process, in any build (ledger #1083, #145).
//!
//! # The hole
//!
//! oxigraph's `SparqlEvaluator` installs a default HTTP service handler whenever
//! `oxigraph/http-client` is on, and this crate cannot turn that feature off: Cargo unifies
//! features across the whole graph, and rudof_rdf enables `http-client-rustls-native` on every
//! native target, so any host linking `ikigai-shacl` (`ikigai-cli`, `ikigai-web-demo`) has it.
//! There, `SERVICE <http://…>` in a caller's query was an outbound request at every one of
//! the ten doors, scoped reads and both update doors included, with no `urn:cap:net:*` anywhere
//! near it: a read grant on one graph was a network client. `without_default_http_service_handler`
//! is itself `#[cfg(feature = "http-client")]`, and a crate cannot `cfg` on a dependency's
//! feature being enabled by someone else, so it cannot be called.
//!
//! # The fix, in two layers
//!
//! 1. **[`evaluator`]** builds every `SparqlEvaluator` this crate evaluates with, and installs
//!    [`Refuse`] as the default service handler. `with_default_service_handler` exists in every
//!    build, and in a build with the feature it also clears the flag that would install the
//!    HTTP handler (oxigraph 0.5.11, `sparql/mod.rs`). So no `SERVICE`, under any name,
//!    constant or variable, `SILENT` or not, reaches a network client. This layer is the
//!    guarantee.
//! 2. **[`refuse_in_query`] / [`refuse_in_update`]** walk the parsed algebra at the door and
//!    refuse any `SERVICE`, before anything is evaluated, as an `InvalidArgument` on the
//!    argument that carried it. This layer is the ANSWER: without it a `SERVICE SILENT` would
//!    succeed with nothing bound (the handler's refusal is swallowed by `SILENT`, by the spec),
//!    and a plain `SERVICE` would fail mid-evaluation as an untyped endpoint error.
//!
//! ★ Why `InvalidArgument` and not `Denied` naming `urn:cap:net:*`: no grant opens this. The
//! store never federates, so a `Denied` would send a caller looking for a capability that
//! changes nothing. It is the same refusal `LOAD <url>` gets (ledger #992), for the same
//! reason, with the same remedy: fetch remote data through the kernel, where the net
//! capability applies, and sink it into `urn:iki:store:load`.
//!
//! ⚠ What this layer does NOT govern: `LOAD <url>`. oxigraph builds `LOAD`'s HTTP client
//! from the evaluator's HTTP settings, not from the service registry, so a refusing service
//! handler leaves it untouched. The door refusal in [`crate::confine`] is the only thing
//! between `LOAD` and the network, and `tests/service_egress.rs` pins that it holds in a
//! build with the feature on. `FROM` / `FROM NAMED` are not fetches at all: oxigraph reads
//! them as names of graphs in the store (the same test pins that too).

use ikigai_core::{Error, Result};
use oxigraph::model::NamedNode;
use oxigraph::sparql::{DefaultServiceHandler, QuerySolutionIter, SparqlEvaluator};
use oxiri::Iri;
use spargebra::algebra::{AggregateExpression, Expression, GraphPattern, OrderExpression};
use spargebra::{GraphUpdateOperation, Query, Update};

/// The evaluator every query and update in this crate runs on: oxigraph's, with [`Refuse`]
/// as its default service handler, so no `SERVICE` reaches a network client in any build.
///
/// ★ Build every `SparqlEvaluator` through this. A unit test below scans `src/` and fails on
/// any other plain constructor outside a test module, because one in a host with
/// `oxigraph/http-client` on is an outbound request with no capability on it.
pub(crate) fn evaluator() -> SparqlEvaluator {
    SparqlEvaluator::new().with_default_service_handler(Refuse)
}

/// A default service handler that refuses every service, by name.
pub(crate) struct Refuse;

/// What [`Refuse`] answers: the service it would not call.
#[derive(Debug)]
pub(crate) struct Refused(NamedNode);

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

/// Refuse a parsed query with a `SERVICE` anywhere in it, naming the argument `arg`.
pub(crate) fn refuse_in_query(query: &Query, arg: &str) -> Result<()> {
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

/// [`refuse_in_query`] for an update: the `WHERE` of every `DELETE`/`INSERT` operation.
/// (`LOAD` is refused separately, by [`crate::confine`]; no other operation has a pattern.)
pub(crate) fn refuse_in_update(update: &Update, arg: &str) -> Result<()> {
    let services = update.operations.iter().any(|operation| match operation {
        GraphUpdateOperation::DeleteInsert { pattern, .. } => has_service(pattern),
        _ => false,
    });
    if services {
        return Err(refusal(arg));
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
            let err = refuse_in_query(&query(&text), "query").expect_err(&text);
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
            refuse_in_query(&query(text), "query").unwrap_or_else(|e| panic!("{text}: {e}"));
        }
    }

    #[test]
    fn the_walk_reads_an_update_s_where() {
        let parse = |t: &str| spargebra::SparqlParser::new().parse_update(t).unwrap();
        let err = refuse_in_update(
            &parse(
                "INSERT DATA { <urn:a> <urn:b> <urn:c> } ; \
                 DELETE { ?s ?p ?o } WHERE { SERVICE <http://127.0.0.1:9/x> { ?s ?p ?o } }",
            ),
            "content",
        )
        .unwrap_err();
        assert!(matches!(&err, Error::InvalidArgument { name, .. } if name == "content"));
        refuse_in_update(
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
