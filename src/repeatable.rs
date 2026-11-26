//! Never cache a SPARQL read that is not repeatable (ledger #4418).
//!
//! `RAND()`, `UUID()`, `STRUUID()` and `BNODE()` give a different answer on every
//! evaluation, and the same holds for the other hazards `ikigai-rdf`'s shared classifier
//! names: `NOW()`, `LIMIT`/`OFFSET`, `SAMPLE`, `GROUP_CONCAT`, `REDUCED` and blank nodes in a
//! `CONSTRUCT` template. A cached answer to such a query would replay one evaluation as
//! though it were the only one. The query doors still evaluate it, and answer it uncached.
//! The spec is `docs/design/deterministic-sparql.md`.
//!
//! The classifier runs on the same parsed query the evaluator gets, after the budget check
//! and the `SERVICE` refusal and before `NOW()` is rewritten, so a pinned `NOW()` is still
//! seen. Its walk is iterative and has limits of its own. A query past them is answered
//! uncached as well, because nothing then shows that it is repeatable.

use ikigai_rdf::algebra::repeatability_reasons;
use spargebra::Query;

/// Why `query`'s answer must not be cached, in plain words, or `None` when the classifier
/// found nothing that stops it from being repeatable.
pub(crate) fn why_not_cacheable(query: &Query) -> Option<String> {
    match repeatability_reasons(query) {
        Ok(reasons) if reasons.is_empty() => None,
        Ok(reasons) => {
            let reasons: Vec<String> = reasons.iter().map(ToString::to_string).collect();
            Some(format!(
                "the query can give a different answer each time it runs, so its answer is \
                 never cached. {}",
                reasons.join(" ")
            ))
        }
        Err(limit) => Some(format!(
            "the query could not be checked for repeatability, so its answer is never \
             cached. {limit}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn why(text: &str) -> Option<String> {
        why_not_cacheable(&spargebra::SparqlParser::new().parse_query(text).unwrap())
    }

    #[test]
    fn an_ordinary_select_is_cacheable() {
        assert_eq!(why("SELECT ?s WHERE { ?s ?p ?o }"), None);
        assert_eq!(why("SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }"), None);
    }

    #[test]
    fn every_reason_is_named() {
        let note = why("SELECT (UUID() AS ?u) (RAND() AS ?r) WHERE {} LIMIT 1").unwrap();
        assert!(note.contains("UUID()"), "{note}");
        assert!(note.contains("RAND()"), "{note}");
        assert!(note.contains("LIMIT or OFFSET"), "{note}");
    }

    #[test]
    fn a_query_past_the_walk_limits_is_not_cacheable() {
        // Two hundred `||` nest one expression inside the next, deeper than the walk goes.
        let chain = vec!["?x"; 200].join(" || ");
        let note = why(&format!("SELECT ?x WHERE {{ FILTER({chain}) }}")).unwrap();
        assert!(note.contains("could not be checked"), "{note}");
        assert!(note.contains("depth limit"), "{note}");
    }
}
