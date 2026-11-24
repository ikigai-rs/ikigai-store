//! What SPARQL `NOW()` reads today, and a proof that the rewrite in
//! `docs/design/sparql-now-clock.md` (ledger #2203) gives a query the kernel's instant.
//!
//! The first two tests pin TODAY's behavior, the facts the note builds on, so a change to either
//! is seen. They are not the behavior the note wants:
//!
//! 1. A kernel built with `with_clock(FixedClock)` does not reach `NOW()`: the evaluator reads
//!    the wall clock (`oxsdatatypes` calls `SystemTime::now()`, and `spareval` has no way to be
//!    handed an instant).
//! 2. Nor does a temporal corridor's pinned instant (`Scope::with_named_at`).
//!
//! (A third fact the note started from, that a `NOW()` answer was cached like any covered read,
//! no longer holds: store PR 101, ledger #3989, stopped caching it, and
//! `tests/sparql_now_uncached.rs` pins that.)
//!
//! The last test is the proof. It rewrites every `NOW()` in the parsed query to an
//! `xsd:dateTime` literal taken from the kernel's clock, then sends the rewritten text
//! through the same door, and the answer is the kernel's instant. The rewrite lives in this
//! file only. The store's default behavior does not change.

use futures::executor::block_on;
use ikigai_core::{
    ArgRef, Capability, Clock, EndpointSpace, FixedClock, Iri, Kernel, Request, Scope, Space, Verb,
};
use ikigai_store::{space, DurableStore};
use oxigraph::model::Literal;
use spargebra::algebra::{
    AggregateExpression, Expression, Function, GraphPattern, OrderExpression,
};
use spargebra::{Query, SparqlParser};
use std::sync::Arc;

/// 2001-09-09T01:46:40Z, far enough from any wall clock this test runs under that the two
/// cannot be confused.
const PINNED_MILLIS: u64 = 1_000_000_000_000;
const PINNED_LEXICAL: &str = "2001-09-09T01:46:40Z";

const NOW_QUERY: &str = "SELECT (NOW() AS ?t) WHERE {}";

fn select(kernel: &Kernel, query: &str, scope: Option<Scope>) -> String {
    let request = Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
        .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
        .with_arg("as", ArgRef::Inline(b"text/csv".to_vec()));
    let answer = match scope {
        Some(scope) => block_on(kernel.issue_in(request, &Capability::root(), scope)),
        None => block_on(kernel.issue(request, &Capability::root())),
    }
    .unwrap();
    let csv = String::from_utf8(answer.bytes).unwrap();
    csv.lines().nth(1).unwrap_or_default().trim().to_string()
}

fn pinned_kernel() -> Kernel {
    Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())))
        .with_clock(Arc::new(FixedClock::at(PINNED_MILLIS)))
}

/// The year of an `xsd:dateTime` lexical form.
fn year(lexical: &str) -> i64 {
    lexical.split('-').next().unwrap().parse().unwrap()
}

#[test]
fn now_does_not_read_the_kernel_clock() {
    let kernel = pinned_kernel();
    let t = select(&kernel, NOW_QUERY, None);
    assert!(
        year(&t) > 2001,
        "NOW() answered {t}: if this is the pinned instant {PINNED_LEXICAL}, the kernel clock now \
         reaches the evaluator, and the note's \"today\" section is out of date"
    );
}

#[test]
fn now_does_not_read_a_temporal_corridors_instant() {
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    let corridor: Arc<dyn Space> = Arc::new(EndpointSpace::new());
    let scope = Scope::empty().with_named_at(
        Iri::parse("urn:example:corridor:pinned").unwrap(),
        corridor,
        Arc::new(FixedClock::at(PINNED_MILLIS)),
    );
    let t = select(&kernel, NOW_QUERY, Some(scope));
    assert!(
        year(&t) > 2001,
        "NOW() answered {t} inside a corridor pinned to {PINNED_LEXICAL}"
    );
}

#[test]
fn rewriting_now_to_the_kernel_instant_answers_that_instant() {
    let kernel = pinned_kernel();
    let mut parsed = SparqlParser::new()
        .parse_query("SELECT ?t ?same WHERE { BIND(NOW() AS ?t) FILTER(NOW() <= NOW()) BIND(?t = NOW() AS ?same) }")
        .unwrap();
    // In the store this would be `inv.now()`, read before the evaluation leaves the invocation.
    let instant = FixedClock::at(PINNED_MILLIS).now();
    let rewritten = pin_now(&mut parsed, &date_time_literal(instant.as_millis()));
    assert_eq!(rewritten, 4, "every NOW() in the query is rewritten");
    let text = parsed.to_string();
    assert!(!text.to_uppercase().contains("NOW()"), "{text}");

    let row = select(&kernel, &text, None);
    assert_eq!(row, format!("{PINNED_LEXICAL},true"));

    // A second instant is a second query text, so a cache keyed on the request cannot serve
    // one instant's answer for the other.
    let mut later = SparqlParser::new().parse_query(NOW_QUERY).unwrap();
    pin_now(&mut later, &date_time_literal(PINNED_MILLIS + 1_000));
    assert_eq!(
        select(&kernel, &later.to_string(), None),
        "2001-09-09T01:46:41Z"
    );
}

/// An `xsd:dateTime` in UTC for `millis` since the Unix epoch, with the fraction only when
/// there is one.
fn date_time_literal(millis: u64) -> Literal {
    let secs = millis / 1000;
    let frac = millis % 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Days to civil date (proleptic Gregorian), H. Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let mut lexical = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
    if frac != 0 {
        lexical.push_str(&format!(".{frac:03}"));
    }
    lexical.push('Z');
    Literal::new_typed_literal(lexical, oxigraph::model::vocab::xsd::DATE_TIME)
}

/// Replace every `NOW()` in `query` with `instant`; the count replaced.
fn pin_now(query: &mut Query, instant: &Literal) -> usize {
    let pattern = match query {
        Query::Select { pattern, .. }
        | Query::Construct { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Ask { pattern, .. } => pattern,
    };
    let mut count = 0;
    in_pattern(pattern, instant, &mut count);
    count
}

fn in_pattern(pattern: &mut GraphPattern, instant: &Literal, count: &mut usize) {
    match pattern {
        GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => {}
        GraphPattern::Join { left, right }
        | GraphPattern::Lateral { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => {
            in_pattern(left, instant, count);
            in_pattern(right, instant, count);
        }
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => {
            in_pattern(left, instant, count);
            in_pattern(right, instant, count);
            if let Some(e) = expression {
                in_expression(e, instant, count);
            }
        }
        GraphPattern::Filter { expr, inner } => {
            in_expression(expr, instant, count);
            in_pattern(inner, instant, count);
        }
        GraphPattern::Extend {
            inner, expression, ..
        } => {
            in_pattern(inner, instant, count);
            in_expression(expression, instant, count);
        }
        GraphPattern::OrderBy { inner, expression } => {
            in_pattern(inner, instant, count);
            for order in expression {
                match order {
                    OrderExpression::Asc(e) | OrderExpression::Desc(e) => {
                        in_expression(e, instant, count)
                    }
                }
            }
        }
        GraphPattern::Group {
            inner, aggregates, ..
        } => {
            in_pattern(inner, instant, count);
            for (_, aggregate) in aggregates {
                if let AggregateExpression::FunctionCall { expr, .. } = aggregate {
                    in_expression(expr, instant, count);
                }
            }
        }
        GraphPattern::Graph { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::Service { inner, .. } => in_pattern(inner, instant, count),
    }
}

fn in_expression(expression: &mut Expression, instant: &Literal, count: &mut usize) {
    if matches!(expression, Expression::FunctionCall(Function::Now, _)) {
        *expression = Expression::Literal(instant.clone());
        *count += 1;
        return;
    }
    match expression {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => {}
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
        | Expression::Divide(a, b) => {
            in_expression(a, instant, count);
            in_expression(b, instant, count);
        }
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
            in_expression(a, instant, count)
        }
        Expression::In(a, list) => {
            in_expression(a, instant, count);
            for e in list {
                in_expression(e, instant, count);
            }
        }
        Expression::If(a, b, c) => {
            in_expression(a, instant, count);
            in_expression(b, instant, count);
            in_expression(c, instant, count);
        }
        Expression::Coalesce(list) | Expression::FunctionCall(_, list) => {
            for e in list {
                in_expression(e, instant, count);
            }
        }
        Expression::Exists(pattern) => in_pattern(pattern, instant, count),
    }
}
