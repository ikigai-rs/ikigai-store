//! Pin SPARQL `NOW()` to the invocation's instant when it has one (ledger #4284).
//!
//! Detect calls on the parsed algebra, so a comment, literal, or IRI mentioning `NOW()`
//! does not affect caching. An answer computed from a temporal corridor's pinned instant is
//! cacheable, and one computed from a live clock is not (ledger #4286, [`instant`]).
//!
//! The pattern and expression matches are exhaustive on purpose: a new variant is a compile
//! error, never a place `NOW()` could hide. Both doors run the walk after the budget check,
//! so recursion is bounded, on the sized SPARQL thread.

use ikigai_core::{Invocation, Time};
use oxigraph::model::{vocab::xsd, Literal};
use spargebra::algebra::{
    AggregateExpression, Expression, Function, GraphPattern, OrderExpression,
};
use spargebra::{GraphUpdateOperation, Query, Update};

/// The instant a query's `NOW()` is rewritten to, and whether an answer computed from it may
/// be cached (ledger #4286).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NowInstant {
    /// [`Invocation::now`]: a temporal corridor's clock, else the kernel's; `None` with
    /// neither, and then `NOW()` stays on oxigraph's wall clock.
    pub(crate) time: Option<Time>,
    /// Whether `time` is the instant the kernel's cache key covers: the pinned instant of the
    /// innermost temporal corridor ([`Invocation::pinned_now`]), which the chain fingerprint
    /// hashes beside the corridor's name. Only then is a `NOW()` answer a pure function of the
    /// key and the data. The kernel's own clock is live time and is in no key.
    ///
    /// ⚠ `now()` and `pinned_now()` can disagree under a corridor: `now()` reads the
    /// corridor's clock again at each call while `pinned_now()` is what it read at injection,
    /// and an invocation's own `with_clock` wins over both. The literal is always `now()`'s,
    /// so the answer is cacheable only when the two are equal, never under a ticking clock
    /// that moved since the corridor was injected.
    pub(crate) pinned: bool,
}

/// Read [`NowInstant`] for `inv`.
pub(crate) fn instant(inv: &Invocation<'_>) -> NowInstant {
    let time = inv.now();
    let pinned = time.is_some() && inv.pinned_now() == time;
    NowInstant { time, pinned }
}

/// Rewrite all calls when a clock is available, and report whether the original query
/// called `NOW()` for the cache decision. With no clock, leave evaluation to Oxigraph.
pub(crate) fn pin_query(query: &mut Query, time: Option<Time>) -> bool {
    let pattern = match query {
        Query::Select { pattern, .. }
        | Query::Construct { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Ask { pattern, .. } => pattern,
    };
    let mut walk = NowWalk::new(time);
    walk.in_pattern(pattern);
    walk.found
}

/// Use the same invocation instant for every WHERE in the update request.
pub(crate) fn pin_update(update: &mut Update, time: Time) {
    let mut walk = NowWalk::new(Some(time));
    for operation in &mut update.operations {
        if let GraphUpdateOperation::DeleteInsert { pattern, .. } = operation {
            walk.in_pattern(pattern);
        }
    }
}

struct NowWalk {
    time: Option<Time>,
    literal: Option<Literal>,
    found: bool,
}

impl NowWalk {
    fn new(time: Option<Time>) -> Self {
        Self {
            time,
            literal: None,
            found: false,
        }
    }

    fn in_pattern(&mut self, pattern: &mut GraphPattern) {
        match pattern {
            GraphPattern::Bgp { .. } | GraphPattern::Path { .. } | GraphPattern::Values { .. } => {}
            GraphPattern::Join { left, right }
            | GraphPattern::Lateral { left, right }
            | GraphPattern::Union { left, right }
            | GraphPattern::Minus { left, right } => {
                self.in_pattern(left);
                self.in_pattern(right);
            }
            GraphPattern::LeftJoin {
                left,
                right,
                expression,
            } => {
                self.in_pattern(left);
                self.in_pattern(right);
                if let Some(e) = expression {
                    self.in_expression(e);
                }
            }
            GraphPattern::Filter { expr, inner } => {
                self.in_expression(expr);
                self.in_pattern(inner);
            }
            GraphPattern::Extend {
                inner, expression, ..
            } => {
                self.in_pattern(inner);
                self.in_expression(expression);
            }
            GraphPattern::OrderBy { inner, expression } => {
                self.in_pattern(inner);
                for order in expression {
                    match order {
                        OrderExpression::Asc(e) | OrderExpression::Desc(e) => self.in_expression(e),
                    }
                }
            }
            GraphPattern::Group {
                inner, aggregates, ..
            } => {
                self.in_pattern(inner);
                for (_, aggregate) in aggregates {
                    match aggregate {
                        AggregateExpression::FunctionCall { expr, .. } => self.in_expression(expr),
                        AggregateExpression::CountSolutions { .. } => {}
                    }
                }
            }
            GraphPattern::Graph { inner, .. }
            | GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Slice { inner, .. }
            | GraphPattern::Service { inner, .. } => self.in_pattern(inner),
        }
    }

    fn in_expression(&mut self, expression: &mut Expression) {
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
                self.in_expression(a);
                self.in_expression(b);
            }
            Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
                self.in_expression(a)
            }
            Expression::In(a, list) => {
                self.in_expression(a);
                for e in list {
                    self.in_expression(e);
                }
            }
            Expression::If(a, b, c) => {
                self.in_expression(a);
                self.in_expression(b);
                self.in_expression(c);
            }
            Expression::FunctionCall(Function::Now, _) => {
                self.found = true;
                if let Some(time) = self.time {
                    let literal = self
                        .literal
                        .get_or_insert_with(|| date_time_literal(time.as_millis()));
                    *expression = Expression::Literal(literal.clone());
                }
            }
            Expression::Coalesce(list) | Expression::FunctionCall(_, list) => {
                for e in list {
                    self.in_expression(e);
                }
            }
            Expression::Exists(pattern) => self.in_pattern(pattern),
        }
    }
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
    Literal::new_typed_literal(lexical, xsd::DATE_TIME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls(text: &str) -> bool {
        let mut parsed = spargebra::SparqlParser::new().parse_query(text).unwrap();
        let found = pin_query(&mut parsed, Some(Time::from_millis(0)));
        assert!(
            !pin_query(&mut parsed, None),
            "an unreplaced call in {text}"
        );
        found
    }

    #[test]
    fn the_literal_preserves_epoch_milliseconds_in_utc() {
        for (millis, lexical) in [
            (0, "1970-01-01T00:00:00Z"),
            (951_827_696_007, "2000-02-29T12:34:56.007Z"),
        ] {
            let literal = date_time_literal(millis);
            assert_eq!(literal.value(), lexical);
            assert_eq!(literal.datatype(), xsd::DATE_TIME);
        }
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
