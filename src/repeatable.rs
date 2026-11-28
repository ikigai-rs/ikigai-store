//! Never cache a SPARQL read that makes a volatile call (ledger #4418).
//!
//! `RAND()`, `UUID()`, `STRUUID()` and `BNODE()` give a different answer on every
//! evaluation, and so does `NOW()` when no temporal corridor's pinned instant applies. A cached answer
//! to such a query would replay one evaluation as though it were the only one. The query
//! doors still evaluate it, and answer it uncached. The spec is
//! `docs/design/deterministic-sparql.md`, whose store policy section says why.
//!
//! ★ Only volatile calls count (decided 2026-11-27). `ikigai-rdf`'s shared classifier also
//! reports `LIMIT`/`OFFSET`, `SAMPLE`, `GROUP_CONCAT`, `REDUCED` and blank nodes in a
//! `CONSTRUCT` template. SPARQL does not promise those are repeatable, but this store answers
//! them the same way over unchanged data, and the kernel cache is already cut when the data
//! changes, so they stay cacheable. Losing the cache on every paged read is the larger cost.
//!
//! The classifier runs on the same parsed query the evaluator gets, after the budget check
//! and the `SERVICE` refusal and before `NOW()` is rewritten. Its walk is iterative and has
//! limits of its own. A query past them is answered uncached, because nothing then shows
//! that it makes no volatile call.

use ikigai_rdf::algebra::{repeatability_reasons, RepeatabilityReason};
use spargebra::Query;

/// Whether `reason` is a volatile call: one that can change the answer between two
/// evaluations over the same data. `pinned` is whether `NOW()` is rewritten to a temporal
/// corridor's pinned instant, which the kernel's cache key holds (`src/now.rs`, ledger
/// #4286); such a `NOW()` is repeatable, so it is not named here. A `NOW()` on the kernel's
/// live clock, or on the wall clock, is.
fn is_volatile(reason: RepeatabilityReason, pinned: bool) -> bool {
    match reason {
        RepeatabilityReason::Rand
        | RepeatabilityReason::Uuid
        | RepeatabilityReason::StrUuid
        | RepeatabilityReason::BNode => true,
        RepeatabilityReason::Now => !pinned,
        // `LIMIT`/`OFFSET`, `SAMPLE`, `GROUP_CONCAT`, `REDUCED`, CONSTRUCT template blank
        // nodes, and any reason a later classifier adds: answered the same way over unchanged
        // data by this store, so cacheable. The module docs say why.
        _ => false,
    }
}

/// Why `query`'s answer must not be cached, in plain words, or `None` when the classifier
/// found no volatile call in it.
pub(crate) fn why_not_cacheable(query: &Query, pinned: bool) -> Option<String> {
    match repeatability_reasons(query) {
        Ok(reasons) => {
            let volatile: Vec<String> = reasons
                .into_iter()
                .filter(|r| is_volatile(*r, pinned))
                .map(|r| r.to_string())
                .collect();
            if volatile.is_empty() {
                return None;
            }
            Some(format!(
                "the query can give a different answer each time it runs, so its answer is \
                 never cached. {}",
                volatile.join(" ")
            ))
        }
        Err(limit) => Some(format!(
            "the query could not be checked for volatile calls, so its answer is never \
             cached. {limit}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn why_pinned(text: &str, pinned: bool) -> Option<String> {
        let query = spargebra::SparqlParser::new().parse_query(text).unwrap();
        why_not_cacheable(&query, pinned)
    }

    fn why(text: &str) -> Option<String> {
        why_pinned(text, false)
    }

    #[test]
    fn an_ordinary_select_is_cacheable() {
        assert_eq!(why("SELECT ?s WHERE { ?s ?p ?o }"), None);
        assert_eq!(why("SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }"), None);
    }

    #[test]
    fn every_volatile_call_is_named() {
        let note = why(
            "SELECT (UUID() AS ?u) (RAND() AS ?r) (STRUUID() AS ?s) (BNODE() AS ?b) \
             (NOW() AS ?n) WHERE {}",
        )
        .unwrap();
        for call in ["UUID()", "RAND()", "STRUUID()", "BNODE()", "NOW()"] {
            assert!(note.contains(call), "{call} missing: {note}");
        }
    }

    #[test]
    fn reasons_that_are_not_volatile_calls_stay_cacheable() {
        for text in [
            "SELECT ?s WHERE { ?s ?p ?o } LIMIT 1",
            "SELECT ?s WHERE { ?s ?p ?o } ORDER BY ?s OFFSET 2",
            "SELECT (SAMPLE(?o) AS ?x) WHERE { ?s ?p ?o }",
            "SELECT (GROUP_CONCAT(?o) AS ?x) WHERE { ?s ?p ?o }",
            "SELECT REDUCED ?s WHERE { ?s ?p ?o }",
            "CONSTRUCT { _:b <urn:p> ?o } WHERE { ?s ?p ?o }",
        ] {
            assert_eq!(why(text), None, "{text}");
        }
    }

    #[test]
    fn a_volatile_call_is_named_beside_a_cacheable_reason_but_the_other_is_not() {
        let note = why("SELECT (UUID() AS ?u) WHERE {} LIMIT 1").unwrap();
        assert!(note.contains("UUID()"), "{note}");
        assert!(!note.contains("LIMIT"), "{note}");
    }

    #[test]
    fn a_pinned_now_is_left_to_its_own_rewrite() {
        assert_eq!(why_pinned("SELECT (NOW() AS ?n) WHERE {}", true), None);
        let note = why_pinned("SELECT (NOW() AS ?n) (RAND() AS ?r) WHERE {}", true).unwrap();
        assert!(note.contains("RAND()") && !note.contains("NOW()"), "{note}");
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
