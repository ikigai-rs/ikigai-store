//! Whether a parsed query calls SPARQL `NOW()` (ledger #3989).
//!
//! A query's answer is cacheable under the store's write threads only if it is a pure function
//! of the data. `NOW()` breaks that: the evaluator reads the wall clock (`oxsdatatypes` calls
//! `SystemTime::now()`, and neither the kernel's clock nor a pinned corridor's instant reaches
//! it, ledger #2203), so a cached answer would keep serving the first instant until some write
//! cut a thread. The query doors answer such a query uncacheable.
//!
//! ★ **Detected on the parsed algebra, never on the text.** A string search would miss nothing
//! but would also flag `"NOW()"` in a literal, a comment, or an IRI, and would be one more
//! parser that disagrees with spargebra's.
//!
//! ⚠ **Exhaustive over `spargebra`'s algebra on purpose**, for the reason `src/scope.rs` gives:
//! a new pattern or expression variant is a compile error here, never a place `NOW()` could
//! hide. The only feature-gated variants in spargebra 0.4.7 are in `Function` (only `Now` is
//! matched, and every call's arguments are walked) and `GraphPattern::Lateral`, gated on
//! `sep-0006`, which this crate's manifest enables itself. Runs after `budget::check_query`, so
//! the recursion is bounded, on the sized SPARQL thread.

use spargebra::algebra::{
    AggregateExpression, Expression, Function, GraphPattern, OrderExpression,
};
use spargebra::Query;

/// Whether `query` calls `NOW()` anywhere: a projection, `BIND`, `FILTER`, `OPTIONAL`'s
/// condition, `ORDER BY`, an aggregate, `EXISTS`, or any of these inside a subquery.
pub(crate) fn calls_now(query: &Query) -> bool {
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
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. } => in_pattern(inner),
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
                AggregateExpression::FunctionCall { expr, .. } => in_expr(expr),
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
        Expression::FunctionCall(Function::Now, _) => true,
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

#[cfg(test)]
mod tests {
    use super::calls_now;

    fn calls(text: &str) -> bool {
        calls_now(&spargebra::SparqlParser::new().parse_query(text).unwrap())
    }

    #[test]
    fn now_is_found_in_every_expression_position() {
        for text in [
            "SELECT (NOW() AS ?t) WHERE {}",
            "SELECT ?t WHERE { BIND(NOW() AS ?t) }",
            "SELECT ?s WHERE { ?s ?p ?o FILTER(?o < NOW()) }",
            "SELECT ?s WHERE { ?s ?p ?o OPTIONAL { ?s ?q ?r FILTER(?r < NOW()) } }",
            "SELECT ?s WHERE { ?s ?p ?o } ORDER BY (NOW())",
            "SELECT (MAX(NOW()) AS ?m) WHERE { ?s ?p ?o }",
            "SELECT ?s WHERE { ?s ?p ?o FILTER EXISTS { BIND(NOW() AS ?n) } }",
            "SELECT ?t WHERE { { SELECT (NOW() AS ?t) WHERE {} } }",
            "SELECT ?s WHERE { GRAPH ?g { ?s ?p ?o FILTER(YEAR(NOW()) > 2000) } }",
            "ASK { FILTER(NOW() = NOW()) }",
            "CONSTRUCT { ?s <urn:p> ?t } WHERE { ?s ?p ?o BIND(NOW() AS ?t) }",
            "SELECT ?s WHERE { ?s ?p ?o FILTER(COALESCE(?o, IF(true, STR(NOW()), \"x\")) != \"\") }",
        ] {
            assert!(calls(text), "{text}");
        }
    }

    #[test]
    fn a_query_without_now_does_not_call_it() {
        for text in [
            "SELECT ?s WHERE { ?s ?p ?o }",
            "SELECT (\"NOW()\" AS ?t) WHERE {}",
            "SELECT ?now WHERE { <urn:now> ?p ?now } # NOW()",
            "SELECT (RAND() AS ?r) (STRUUID() AS ?u) WHERE {}",
        ] {
            assert!(!calls(text), "{text}");
        }
    }
}
