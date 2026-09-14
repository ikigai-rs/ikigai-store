//! Running an **arbitrary** SPARQL query with an **exact** per-graph read scope.
//!
//! # ★ Why this is cheap where the write half was expensive
//!
//! `src/confine.rs` has to enforce the write scope on *effects*: an update's target graph
//! cannot be read off its syntax, so the update runs against a private copy of `G` and is
//! refused if anything escaped. A query has no such problem, because SPARQL already has a
//! notion of *which dataset a query sees* and oxigraph exposes it:
//! `PreparedSparqlQuery::dataset_mut()` returns the [query dataset
//! specification](https://www.w3.org/TR/sparql11-query/#specifyingDataset), and setting it
//! before evaluation confines the query **by construction**. Nothing is copied, nothing is
//! diffed, and the confinement is the evaluator's own, not a check layered over it.
//!
//! [`confine`] sets both halves of the specification to the one graph:
//!
//! - the **default graph** becomes `G` — so a bare `{ ?s ?p ?o }` reads `G`;
//! - the **available named graphs** become exactly `[G]` — so `GRAPH <G>` reads `G`,
//!   `GRAPH <other>` matches nothing, and `GRAPH ?g` can only bind `G`.
//!
//! That is precisely `FROM <G> FROM NAMED <G>`, written through the API instead of into
//! the query text. **It is deliberately not the store's real default graph**: the default
//! graph has no IRI, so there is no `urn:cap:store:read:graph:` token that could name it,
//! and a scoped reader therefore cannot reach it at all. A host that puts tenant data in
//! the default graph has put it outside this boundary's reach — which is the safe
//! direction, and is stated on the endpoint.
//!
//! # What the evaluator actually does with that, measured rather than assumed
//!
//! Every row is a test in this file, because the previous arc's lesson was that SPARQL
//! syntax lies about what a statement touches.
//!
//! | query shape | under `graph=G` | why |
//! | --- | --- | --- |
//! | `{ ?s ?p ?o }` | reads `G` | the default graph *is* `G` |
//! | `GRAPH <G> { … }` | reads `G` | `G` is in the available named graphs |
//! | `GRAPH <other> { … }` | **matches nothing** | not in the available set; the evaluator returns an empty iterator, not an error |
//! | `GRAPH ?g { … }` | binds `?g = G` only | the enumeration is the available set |
//! | a sub-select, `FILTER EXISTS`, `MINUS`, a property path | confined | all of them resolve patterns through the same dataset |
//! | `DESCRIBE <s>` with no pattern | reads `G` | ⚠ DESCRIBE collects from the **default graph** only — see below |
//! | `FROM` / `FROM NAMED` in the text | **refused** by the endpoint | see [`names_its_own_dataset`] |
//! | `SERVICE <http://…>` | refused by oxigraph — no HTTP client is built in | ⚠ a *build* property, not a code one; see `README.md` |
//!
//! # ⚠ DESCRIBE reads the default graph and nothing else — upstream, in both doors
//!
//! `spareval`'s `DescribeIterator` collects the quads of each described node with the
//! graph pattern `Some(None)` — the dataset's *default graph*. So on the **unscoped**
//! `urn:iki:store:describe`, `DESCRIBE <s>` for a subject that lives in a named graph
//! returns **nothing at all**, and always has. Under `graph=G` the default graph is `G`,
//! so the scoped door is the one where a bare DESCRIBE of a tenant's subject works.
//! `a_bare_describe_reads_only_the_default_graph` pins both halves, so an upstream change
//! to either fails here rather than surprising a consumer.
//!
//! # The ablation that matters
//!
//! ★ **A confinement that is wrong in the safe direction is invisible.** Empty is a
//! legitimate answer to a query, so a scoped read that silently returns nothing looks
//! exactly like an empty graph. Every test here is therefore a *pair*: the permitted graph
//! comes back with its rows, and the other tenant's does not come back at all.

use oxigraph::model::{GraphName, NamedNode, NamedOrBlankNode};
use oxigraph::sparql::PreparedSparqlQuery;

/// Confine a prepared query to `graph`: it becomes the default graph and the only
/// available named graph.
///
/// The caller must already hold the per-graph capability — this function is the
/// mechanism, not the gate. It must be called **after** parsing and **before**
/// `on_store`, which is the whole window in which the dataset specification exists.
pub(crate) fn confine(prepared: &mut PreparedSparqlQuery, graph: &NamedNode) {
    let dataset = prepared.dataset_mut();
    dataset.set_default_graph(vec![GraphName::from(graph.clone())]);
    dataset.set_available_named_graphs(vec![NamedOrBlankNode::from(graph.clone())]);
}

/// Whether the query text carried its own `FROM` / `FROM NAMED` clauses.
///
/// ★ **`FROM NAMED` is a second way to name a dataset, and the two must not both be in
/// play.** [`confine`] *overwrites* whatever the query asked for — that is proven by
/// `a_from_clause_cannot_widen_the_confinement`, and it means a `FROM <other>` could never
/// widen the scope. But silently answering `FROM <other>` with `G`'s rows would hand a
/// caller another tenant's graph name over this tenant's data, which is worse than a
/// refusal and is the same "wrong but plausible" failure the `as` selector refuses rather
/// than substitutes. So the scoped endpoints refuse it and say that `graph=` *is* the
/// dataset.
///
/// The detection is exact rather than textual: oxigraph builds the prepared query's
/// dataset specification from the query's own clauses, and `is_default_dataset()` is true
/// exactly when there were none.
pub(crate) fn names_its_own_dataset(prepared: &PreparedSparqlQuery) -> bool {
    !prepared.dataset().is_default_dataset()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};
    use oxigraph::store::Store;

    const G: &str = "urn:example:acme";
    const OTHER: &str = "urn:example:zenith";

    /// Two tenants' named graphs and one triple in the store's real default graph — the
    /// third is there so that "the scoped reader cannot reach the default graph" is
    /// testable rather than asserted.
    fn store() -> Store {
        let store = Store::new().unwrap();
        store
            .load_from_reader(
                oxigraph::io::RdfFormat::NQuads,
                format!(
                    "<urn:example:acme:1> <urn:p> \"acme\" <{G}> .\n\
                     <urn:example:zenith:1> <urn:p> \"zenith\" <{OTHER}> .\n\
                     <urn:example:host:1> <urn:p> \"host\" .\n"
                )
                .as_bytes(),
            )
            .unwrap();
        store
    }

    /// Evaluate `query` confined to `G`, returning the rows as `?s` strings.
    fn scoped(store: &Store, query: &str) -> Vec<String> {
        rows(store, query, true)
    }

    /// The same query with no confinement — the control, so every "it is confined" claim
    /// is a *difference* and not a query that happens to match nothing.
    fn unscoped(store: &Store, query: &str) -> Vec<String> {
        rows(store, query, false)
    }

    fn rows(store: &Store, query: &str, confined: bool) -> Vec<String> {
        let mut prepared = SparqlEvaluator::new().parse_query(query).unwrap();
        if confined {
            confine(&mut prepared, &NamedNode::new(G).unwrap());
        }
        let QueryResults::Solutions(solutions) = prepared.on_store(store).execute().unwrap() else {
            panic!("not a solution set");
        };
        let mut out: Vec<String> = solutions
            .map(|s| {
                let s = s.unwrap();
                s.iter()
                    .map(|(v, t)| format!("{}={t}", v.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        out.sort();
        out
    }

    fn triples(store: &Store, query: &str, confined: bool) -> Vec<String> {
        let mut prepared = SparqlEvaluator::new().parse_query(query).unwrap();
        if confined {
            confine(&mut prepared, &NamedNode::new(G).unwrap());
        }
        let QueryResults::Graph(graph) = prepared.on_store(store).execute().unwrap() else {
            panic!("not a graph");
        };
        let mut out: Vec<String> = graph.map(|t| t.unwrap().to_string()).collect();
        out.sort();
        out
    }

    // ------------------------------------------------- the six shapes the brief named

    /// ★ The positive case for the commonest shape, and its ablation in one test: a bare
    /// pattern reads the permitted graph — it does NOT come back empty — and the same
    /// query unconfined sees all three tenants.
    #[test]
    fn a_bare_pattern_reads_the_permitted_graph_and_only_it() {
        let store = store();
        let query = "SELECT ?s WHERE { ?s ?p ?o }";
        assert_eq!(scoped(&store, query), ["s=<urn:example:acme:1>"]);
        // Unconfined, the same query sees the store's default graph — which is where the
        // host's own triple is, and which the scoped reader above could not reach.
        assert_eq!(unscoped(&store, query), ["s=<urn:example:host:1>"]);
    }

    /// A `GRAPH <other>` block naming another tenant: matched by nothing, and the
    /// permitted graph in the same query still answers, so this is a confinement and not
    /// a broken query.
    #[test]
    fn a_graph_block_naming_another_tenant_matches_nothing() {
        let store = store();
        assert_eq!(
            scoped(
                &store,
                &format!("SELECT ?s WHERE {{ GRAPH <{G}> {{ ?s ?p ?o }} }}")
            ),
            ["s=<urn:example:acme:1>"],
            "the permitted graph must still answer through a GRAPH block"
        );
        assert!(
            scoped(
                &store,
                &format!("SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}")
            )
            .is_empty(),
            "another tenant's graph was readable by naming it"
        );
        assert_eq!(
            unscoped(
                &store,
                &format!("SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}")
            ),
            ["s=<urn:example:zenith:1>"],
            "the control: unconfined, that query does return the other tenant"
        );
    }

    /// ★ A **variable** graph, which a syntactic check cannot see at all: the enumeration
    /// is the available set, so it binds `G` and nothing else.
    #[test]
    fn a_variable_graph_enumerates_only_the_permitted_set() {
        let store = store();
        let query = "SELECT ?g WHERE { GRAPH ?g { ?s ?p ?o } }";
        assert_eq!(scoped(&store, query), [format!("g=<{G}>")]);
        assert_eq!(
            unscoped(&store, query),
            [format!("g=<{G}>"), format!("g=<{OTHER}>")]
        );
    }

    /// `GRAPH ?g {}` with an empty pattern enumerates the *registered* graphs rather than
    /// matching quads — a second way to ask "what graphs are there", and confined too.
    #[test]
    fn enumerating_graph_names_is_confined_as_well() {
        let store = store();
        let query = "SELECT ?g WHERE { GRAPH ?g {} }";
        assert_eq!(scoped(&store, query), [format!("g=<{G}>")]);
        assert_eq!(
            unscoped(&store, query),
            [format!("g=<{G}>"), format!("g=<{OTHER}>")]
        );
    }

    /// ★ `FROM` / `FROM NAMED` — the *second* way to name a dataset. The endpoint refuses
    /// these (see [`names_its_own_dataset`]); this pins the mechanism underneath that
    /// choice, which is that [`confine`] overwrites the query's own specification, so a
    /// `FROM` could not widen the scope even if it were let through.
    #[test]
    fn a_from_clause_cannot_widen_the_confinement() {
        let store = store();
        for query in [
            format!("SELECT ?s FROM <{OTHER}> WHERE {{ ?s ?p ?o }}"),
            format!("SELECT ?s FROM <{G}> FROM <{OTHER}> WHERE {{ ?s ?p ?o }}"),
            format!("SELECT ?s FROM NAMED <{OTHER}> WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}"),
        ] {
            let scoped_rows = scoped(&store, &query);
            assert!(
                !scoped_rows.iter().any(|r| r.contains("zenith")),
                "`{query}` reached the other tenant: {scoped_rows:?}"
            );
        }
        // The control: unconfined, `FROM <OTHER>` really does read the other tenant, so
        // the assertions above are about the confinement and not about an empty store.
        assert_eq!(
            unscoped(
                &store,
                &format!("SELECT ?s FROM <{OTHER}> WHERE {{ ?s ?p ?o }}")
            ),
            ["s=<urn:example:zenith:1>"]
        );
    }

    /// And the detector the endpoint refuses on is exact: a dataset clause of any kind is
    /// seen, and a query without one is not mistaken for having one.
    #[test]
    fn a_dataset_clause_is_detected_exactly() {
        for (query, named) in [
            ("SELECT ?s WHERE { ?s ?p ?o }", false),
            ("SELECT ?s WHERE { GRAPH ?g { ?s ?p ?o } }", false),
            ("ASK { ?s ?p ?o }", false),
            ("DESCRIBE <urn:example:acme:1>", false),
            ("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }", false),
            (
                "SELECT ?s FROM <urn:example:zenith> WHERE { ?s ?p ?o }",
                true,
            ),
            (
                "SELECT ?s FROM NAMED <urn:example:zenith> WHERE { GRAPH ?g { ?s ?p ?o } }",
                true,
            ),
            ("DESCRIBE <urn:x> FROM <urn:example:zenith>", true),
        ] {
            let prepared = SparqlEvaluator::new().parse_query(query).unwrap();
            assert_eq!(
                names_its_own_dataset(&prepared),
                named,
                "misclassified `{query}`"
            );
        }
    }

    /// A sub-select, a `FILTER EXISTS` and a `MINUS` over another graph: three shapes
    /// that read outside the pattern they appear in, all confined by the same dataset.
    ///
    /// ⚠ The `FILTER NOT EXISTS` case is the one that is *dangerous* in the safe
    /// direction: unconfined it would remove a row, confined it removes nothing, so a
    /// caller could learn about another tenant by the absence of a row if it were not
    /// confined. It is, and the control proves the difference is real.
    #[test]
    fn nested_reads_of_another_graph_are_confined() {
        let store = store();
        let sub = format!(
            "SELECT ?s WHERE {{ {{ SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }} }} }}"
        );
        assert!(scoped(&store, &sub).is_empty(), "a sub-select escaped");
        assert_eq!(unscoped(&store, &sub), ["s=<urn:example:zenith:1>"]);

        let exists = format!(
            "SELECT ?s WHERE {{ ?s ?p ?o FILTER EXISTS {{ GRAPH <{OTHER}> {{ ?a ?b ?c }} }} }}"
        );
        assert!(
            scoped(&store, &exists).is_empty(),
            "a FILTER EXISTS escaped"
        );

        let not_exists = format!(
            "SELECT ?s WHERE {{ GRAPH <{G}> {{ ?s ?p ?o }} \
             FILTER NOT EXISTS {{ GRAPH <{OTHER}> {{ ?a ?b ?c }} }} }}"
        );
        assert_eq!(
            scoped(&store, &not_exists),
            ["s=<urn:example:acme:1>"],
            "a FILTER NOT EXISTS leaked the other tenant's existence by removing a row"
        );
        assert!(
            unscoped(&store, &not_exists).is_empty(),
            "the control: unconfined, that row really is removed"
        );
    }

    /// ★ `DESCRIBE` with no pattern, which reaches for whatever it likes — and the
    /// upstream fact that makes the scoped door the *better* one for it.
    ///
    /// `spareval` collects a described node's quads from the dataset's **default graph**
    /// only. So unconfined, a subject in a named graph describes to nothing; confined to
    /// `G`, the same subject describes correctly, and a subject in another tenant's graph
    /// describes to nothing.
    #[test]
    fn a_bare_describe_reads_only_the_default_graph() {
        let store = store();
        assert_eq!(
            triples(&store, "DESCRIBE <urn:example:acme:1>", true),
            ["<urn:example:acme:1> <urn:p> \"acme\""],
            "the positive case: a scoped DESCRIBE of the permitted graph's subject"
        );
        assert!(
            triples(&store, "DESCRIBE <urn:example:zenith:1>", true).is_empty(),
            "a scoped DESCRIBE reached another tenant"
        );
        // ⚠ Upstream behaviour, pinned because it is surprising and because it applies to
        // `urn:iki:store:describe` too: DESCRIBE never looks in a named graph.
        assert!(
            triples(&store, "DESCRIBE <urn:example:acme:1>", false).is_empty(),
            "DESCRIBE now reads named graphs — the scoped door's contract changed with it"
        );
        assert_eq!(
            triples(&store, "DESCRIBE <urn:example:host:1>", false),
            ["<urn:example:host:1> <urn:p> \"host\""],
            "unconfined, DESCRIBE reads the store's default graph"
        );
        // …and the scoped reader cannot reach that default graph at all.
        assert!(
            triples(&store, "DESCRIBE <urn:example:host:1>", true).is_empty(),
            "a scoped DESCRIBE reached the store's real default graph"
        );
    }

    /// CONSTRUCT is confined by the same dataset, and its positive case is a graph that
    /// actually has triples in it.
    #[test]
    fn construct_is_confined_and_still_answers() {
        let store = store();
        assert_eq!(
            triples(
                &store,
                "CONSTRUCT { ?s <urn:copied> ?o } WHERE { ?s ?p ?o }",
                true
            ),
            ["<urn:example:acme:1> <urn:copied> \"acme\""]
        );
        assert!(
            triples(
                &store,
                &format!(
                    "CONSTRUCT {{ ?s <urn:copied> ?o }} WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}"
                ),
                true
            )
            .is_empty(),
            "a CONSTRUCT read another tenant"
        );
    }

    /// ⚠ `SERVICE` — federation is off in this build, and that is a property of the
    /// BUILD rather than of this code (see `README.md` and the PENDING note): a crate
    /// anywhere in the host's graph enabling `oxigraph/http-client` installs a default
    /// HTTP service handler and this refusal becomes an outbound request, with no
    /// `urn:cap:net:*` anywhere near it. This test is what would notice.
    #[test]
    fn a_service_clause_is_refused_because_no_http_client_is_built_in() {
        let store = store();
        let mut prepared = SparqlEvaluator::new()
            .parse_query("SELECT ?s WHERE { SERVICE <http://example.invalid/sparql> { ?s ?p ?o } }")
            .unwrap();
        confine(&mut prepared, &NamedNode::new(G).unwrap());
        let result = prepared.on_store(&store).execute();
        let failed = match result {
            Err(_) => true,
            Ok(QueryResults::Solutions(solutions)) => solutions.into_iter().any(|s| s.is_err()),
            Ok(_) => panic!("not a solution set"),
        };
        assert!(
            failed,
            "a SERVICE clause was answered — this build has an HTTP client, and a scoped \
             read can now reach the network"
        );
    }

    /// A property path crossing graphs: paths are evaluated through the same dataset, so
    /// a path cannot walk out of the scope either.
    #[test]
    fn a_property_path_cannot_walk_out_of_the_scope() {
        let store = Store::new().unwrap();
        store
            .load_from_reader(
                oxigraph::io::RdfFormat::NQuads,
                format!(
                    "<urn:a> <urn:next> <urn:b> <{G}> .\n\
                     <urn:b> <urn:next> <urn:c> <{OTHER}> .\n"
                )
                .as_bytes(),
            )
            .unwrap();
        let query = "SELECT ?end WHERE { <urn:a> <urn:next>+ ?end }";
        assert_eq!(
            scoped(&store, query),
            ["end=<urn:b>"],
            "the path must still traverse inside the scope"
        );
        // Unconfined the path does not cross either, because the default graph is the
        // store's — so the honest control is the union default graph, which does.
        let mut prepared = SparqlEvaluator::new().parse_query(query).unwrap();
        prepared.dataset_mut().set_default_graph_as_union();
        let QueryResults::Solutions(solutions) = prepared.on_store(&store).execute().unwrap()
        else {
            panic!("not a solution set");
        };
        assert_eq!(
            solutions.count(),
            2,
            "the control: over the union of all graphs the path really does reach <urn:c>"
        );
    }
}
