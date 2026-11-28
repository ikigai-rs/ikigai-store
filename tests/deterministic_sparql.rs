//! The design spike's evidence (`docs/design/deterministic-sparql.md`), with its cache
//! regression active since the doors adopted the shared classifier (ledger #4418). The
//! boolean walk below is the spike's test-only prototype, kept as a cross-check;
//! `tests/repeatable_uncached.rs` covers the production policy.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use spargebra::algebra::{
    AggregateExpression, AggregateFunction, Expression, Function, GraphPattern, OrderExpression,
};
use spargebra::Query;
use std::sync::Arc;

#[test]
fn volatile_selects_bypass_the_cache() {
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    let capability = Capability::root();
    let mut cached = Vec::new();
    for call in ["RAND()", "UUID()", "STRUUID()", "BNODE()"] {
        let request = Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
            .with_arg(
                "query",
                ArgRef::Inline(format!("SELECT ({call} AS ?value) WHERE {{}}").into_bytes()),
            );
        block_on(kernel.issue(request.clone(), &capability)).unwrap();
        if kernel.is_cached(&request, &capability) {
            cached.push(call);
        }
    }
    assert!(
        cached.is_empty(),
        "volatile calls entered cache: {cached:?}"
    );
}

fn has_repeatability_hazard(query: &Query) -> bool {
    let pattern = match query {
        Query::Select { pattern, .. }
        | Query::Construct { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Ask { pattern, .. } => pattern,
    };
    in_pattern(pattern)
}

fn in_pattern(pattern: &GraphPattern) -> bool {
    match pattern {
        GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => false,
        GraphPattern::Graph { inner, .. }
        | GraphPattern::Service { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner } => in_pattern(inner),
        GraphPattern::Reduced { .. } | GraphPattern::Slice { .. } => true,
        GraphPattern::Join { left, right }
        | GraphPattern::Lateral { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => in_pattern(left) || in_pattern(right),
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => in_pattern(left) || in_pattern(right) || expression.as_ref().is_some_and(in_expr),
        GraphPattern::Filter { expr, inner } => in_expr(expr) || in_pattern(inner),
        GraphPattern::Extend {
            inner, expression, ..
        } => in_expr(expression) || in_pattern(inner),
        GraphPattern::OrderBy { inner, expression } => {
            expression.iter().any(|order| match order {
                OrderExpression::Asc(e) | OrderExpression::Desc(e) => in_expr(e),
            }) || in_pattern(inner)
        }
        GraphPattern::Group {
            inner, aggregates, ..
        } => {
            aggregates.iter().any(|(_, aggregate)| match aggregate {
                AggregateExpression::FunctionCall { name, expr, .. } => {
                    matches!(
                        name,
                        AggregateFunction::Sample | AggregateFunction::GroupConcat { .. }
                    ) || in_expr(expr)
                }
                AggregateExpression::CountSolutions { .. } => false,
            }) || in_pattern(inner)
        }
    }
}

fn in_expr(expression: &Expression) -> bool {
    match expression {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => false,
        Expression::Exists(pattern) => in_pattern(pattern),
        Expression::FunctionCall(
            Function::Now | Function::Rand | Function::Uuid | Function::StrUuid | Function::BNode,
            _,
        ) => true,
        Expression::FunctionCall(_, args) | Expression::Coalesce(args) => args.iter().any(in_expr),
        Expression::In(e, members) => in_expr(e) || members.iter().any(in_expr),
        Expression::If(a, b, c) => in_expr(a) || in_expr(b) || in_expr(c),
        Expression::UnaryPlus(e) | Expression::UnaryMinus(e) | Expression::Not(e) => in_expr(e),
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
        | Expression::Divide(a, b) => in_expr(a) || in_expr(b),
    }
}

#[test]
fn parsed_algebra_distinguishes_hazards_from_text() {
    for (query, expected) in [
        ("SELECT (NOW() AS ?v) WHERE {}", true),
        ("SELECT (RAND() AS ?v) WHERE {}", true),
        ("SELECT (STRUUID() AS ?v) WHERE {}", true),
        ("SELECT (BNODE(\"key\") AS ?v) WHERE {}", true),
        (
            "CONSTRUCT { <urn:s> <urn:p> ?id } WHERE { { SELECT (UUID() AS ?id) WHERE {} } }",
            true,
        ),
        ("ASK { FILTER EXISTS { BIND(UUID() AS ?id) } }", true),
        ("SELECT ?s WHERE { ?s ?p ?o } ORDER BY ?o LIMIT 1", true),
        ("SELECT ?s WHERE { ?s ?p ?o } OFFSET 1", true),
        ("SELECT REDUCED ?s WHERE { ?s ?p ?o }", true),
        ("SELECT (SAMPLE(?o) AS ?v) WHERE { ?s ?p ?o }", true),
        ("SELECT (GROUP_CONCAT(?o) AS ?v) WHERE { ?s ?p ?o }", true),
        ("SELECT (MAX(RAND()) AS ?v) WHERE {}", true),
        ("SELECT (COUNT(*) AS ?v) WHERE { ?s ?p ?o }", false),
        ("SELECT (\"UUID()\" AS ?v) WHERE {} # RAND()", false),
    ] {
        let parsed = spargebra::SparqlParser::new().parse_query(query).unwrap();
        assert_eq!(has_repeatability_hazard(&parsed), expected, "{query}");
    }
}

#[test]
fn uncached_constructs_create_distinct_uuid_iris() {
    let store = oxigraph::store::Store::new().unwrap();
    let construct = || {
        let results = oxigraph::sparql::SparqlEvaluator::new()
            .parse_query("CONSTRUCT { <urn:s> <urn:p> ?id } WHERE { BIND(UUID() AS ?id) }")
            .unwrap()
            .on_store(&store)
            .execute()
            .unwrap();
        let oxigraph::sparql::QueryResults::Graph(triples) = results else {
            panic!("expected a graph");
        };
        let triples = triples.collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(triples.len(), 1);
        assert!(triples[0].object.is_named_node());
        triples
    };
    assert_ne!(construct(), construct());
}
