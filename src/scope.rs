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
//! [`confine`] sets both halves of the specification to the caller's graph set — one
//! graph, or several:
//!
//! - the **default graph** becomes the merge of the set — so a bare `{ ?s ?p ?o }` reads
//!   every graph in it, and a join across them is an ordinary basic graph pattern;
//! - the **available named graphs** become exactly the set — so `GRAPH <G1>` reads `G1`,
//!   `GRAPH <other>` matches nothing, and `GRAPH ?g` can only bind members of the set.
//!
//! For `graph=G` that is precisely `FROM <G> FROM NAMED <G>`, and for `graph=G1 G2` it is
//! `FROM <G1> FROM <G2> FROM NAMED <G1> FROM NAMED <G2>` — written through the API instead
//! of into the query text.
//!
//! # ★ Why BOTH halves for a set, and not named graphs alone
//!
//! A named-graphs-only dataset (`FROM NAMED` for each, an empty default graph) was the
//! other honest option, and was rejected for three reasons:
//!
//! 1. **One graph is the degenerate case of the rule, not an exception to it.** Every
//!    existing single-graph caller writes bare patterns (`ikigai-ledger` does); a
//!    named-only rule would either break them or make one graph mean something different
//!    from two.
//! 2. **Nothing is lost.** Provenance is still available — `GRAPH ?g { … }` binds which
//!    member each quad came from — so a caller who needs to tell the graphs apart can.
//! 3. **The merged default graph is the join people actually write.** `?item :p ?x . ?x
//!    :q ?y` across two partitions needs no `GRAPH` block at all.
//!
//! ⚠ **The "merge" is a BAG in oxigraph, not a set — measured, and not what the spec's
//! RDF merge would give.** A triple present in two members matches a bare pattern TWICE,
//! once per member (`a_triple_in_two_members_matches_once_per_member_in_the_default_graph`
//! pins it, so an upstream change fails here). A caller joining partitions that can
//! repeat a triple writes `SELECT DISTINCT`; a CONSTRUCT over such a set can emit a
//! triple twice. Under `GRAPH ?g` it is one row per member, as it should be.
//!
//! **It is deliberately not the store's real default graph**: the default
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
//! | `SERVICE <http://…>` | **refused** by the endpoint, and by the evaluator itself | in every build, HTTP client or not: see `crate::service` (ledger #1083) |
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

use std::collections::BTreeSet;

use ikigai_core::{Error, Result};
use oxigraph::model::{GraphName, NamedNode, NamedOrBlankNode};
use oxigraph::sparql::PreparedSparqlQuery;

/// The graphs a scoped read names: **non-empty, sorted, and free of duplicates**.
///
/// # ★ The wire shape: whitespace-separated IRIs in ONE `graph` argument
///
/// Not `graph=A graph=B`, because a request cannot carry that: `Request::args` is a
/// `BTreeMap<String, ArgRef>`, one value per name, and `ArgSpec` has no way to say
/// "repeatable". ⚠ Worse than cannot — the engine builds named arguments into that map, so
/// a repeated key is LAST-WINS, silently: `graph=A graph=B` reaches this crate as
/// `graph=B`, and the join over A comes back empty. That is upstream of anything this
/// crate can see, and is reported rather than worked around.
///
/// ASCII whitespace and not commas (which `ikigai-sparql` also accepts), because **no IRI
/// can contain ASCII whitespace and an IRI CAN contain a comma**: `urn:a,urn:b` is one
/// valid IRI. Splitting on ASCII whitespace alone is therefore exact — every graph string
/// has one parse — and on a boundary that is the property worth having. The comma case is
/// not silent either: it demands a token nobody holds, and the refusal says what was
/// probably meant.
///
/// ⚠ **ASCII whitespace, NOT Unicode whitespace** (ledger #751). RFC 3987's `ucschar`
/// admits U+00A0, U+3000 and the other non-ASCII spaces, and oxiri accepts them, so
/// `urn:a\u{3000}urn:b` is ONE IRI: the write door takes it as one graph and
/// `urn:iki:store:graphs` lists it as one. `str::split_whitespace` split it into two, so
/// the scoped read door demanded two grants nobody holds and the graph could be written
/// and listed but never read. The separators are space, tab, line feed, form feed and
/// carriage return — the ones no IRI can contain.
///
/// # Order and repetition
///
/// `A B`, `B A` and `A B A` are one dataset, so they parse to one set. `is_canonical`
/// says whether the caller spelled it the one canonical way (sorted, single spaces, no
/// repetition, no surrounding whitespace) — which matters to the CACHE, not to the answer:
/// the kernel keys a cache entry on the request's raw bytes, so the endpoint re-issues a
/// non-canonical spelling under the canonical one (`src/endpoints.rs`) and every spelling
/// shares one computed entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphSet {
    graphs: Vec<NamedNode>,
    canonical: bool,
}

impl GraphSet {
    /// Parse a `graph` argument, refusing an empty list and any member that is not an IRI.
    ///
    /// ⚠ **An empty list is refused, never read as "no graphs".** A dataset with no
    /// default graph and no named graphs answers every query with nothing, which is a
    /// plausible-looking wrong answer — exactly the failure this crate refuses elsewhere.
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let mut set = BTreeSet::new();
        for token in raw.split_ascii_whitespace() {
            let graph = NamedNode::new(token).map_err(|e| Error::InvalidArgument {
                name: "graph".to_string(),
                detail: format!("`{token}` is not an IRI: {e}"),
            })?;
            set.insert(graph);
        }
        if set.is_empty() {
            return Err(Error::InvalidArgument {
                name: "graph".to_string(),
                detail: "names no graph. A scoped read needs at least one named-graph IRI \
                         (several are separated by whitespace); an empty dataset would answer \
                         every query with nothing, so it is refused rather than evaluated"
                    .to_string(),
            });
        }
        let graphs: Vec<NamedNode> = set.into_iter().collect();
        let canonical = raw == join(&graphs);
        Ok(GraphSet { graphs, canonical })
    }

    /// The members, sorted by IRI.
    pub(crate) fn graphs(&self) -> &[NamedNode] {
        &self.graphs
    }

    /// Whether the argument was already spelled [`Self::canonical`].
    pub(crate) fn is_canonical(&self) -> bool {
        self.canonical
    }

    /// The one spelling of this set: sorted IRIs separated by single spaces.
    pub(crate) fn canonical(&self) -> String {
        join(&self.graphs)
    }
}

fn join(graphs: &[NamedNode]) -> String {
    graphs
        .iter()
        .map(NamedNode::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Confine a prepared query to `graphs`: their merge becomes the default graph, and they
/// are exactly the available named graphs.
///
/// The caller must already hold the per-graph capability for **every** member — this
/// function is the mechanism, not the gate. It must be called **after** parsing and
/// **before** `on_store`, which is the whole window in which the dataset specification
/// exists.
///
/// ⚠ `graphs` must not be empty: an empty default-graph list is not "nothing" to every
/// evaluator, and [`GraphSet::parse`] is what guarantees it.
pub(crate) fn confine(prepared: &mut PreparedSparqlQuery, graphs: &[NamedNode]) {
    debug_assert!(!graphs.is_empty(), "confine to an empty graph set");
    let dataset = prepared.dataset_mut();
    dataset.set_default_graph(graphs.iter().cloned().map(GraphName::from).collect());
    dataset
        .set_available_named_graphs(graphs.iter().cloned().map(NamedOrBlankNode::from).collect());
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
            confine(&mut prepared, &[NamedNode::new(G).unwrap()]);
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
            confine(&mut prepared, &[NamedNode::new(G).unwrap()]);
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

    /// `SERVICE` under a scope: refused by the evaluator this crate builds, in EVERY build.
    /// ⚠ Through 0.2.9 this test used a plain evaluator and passed only because no HTTP
    /// client was built in — a property of the BUILD, which a host with `oxigraph/http-client`
    /// on (any host linking `ikigai-shacl`) did not have, and there a scoped read reached the
    /// network (ledger #1083). The guarantee now lives in `crate::service::evaluator`, and the
    /// doors refuse `SERVICE` by name before this layer is ever reached
    /// (`tests/service_egress.rs`, which also runs with the feature on).
    #[test]
    fn a_service_clause_is_refused_by_the_evaluator_this_crate_builds() {
        let store = store();
        let mut prepared = crate::service::evaluator()
            .parse_query("SELECT ?s WHERE { SERVICE <http://example.invalid/sparql> { ?s ?p ?o } }")
            .unwrap();
        confine(&mut prepared, &[NamedNode::new(G).unwrap()]);
        let result = prepared.on_store(&store).execute();
        let failed = match result {
            Err(_) => true,
            Ok(QueryResults::Solutions(solutions)) => solutions.into_iter().any(|s| s.is_err()),
            Ok(_) => panic!("not a solution set"),
        };
        assert!(
            failed,
            "a SERVICE clause was answered under a scope: the evaluator this crate builds \
             no longer refuses it, and in a host with an HTTP client a scoped read reaches \
             the network"
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

    // ------------------------------------------------------------ several graphs (#380)

    /// A third tenant, outside every set below, so "nothing reaches outside the set" is a
    /// difference and not a query over two graphs that happen to be all there is.
    const THIRD: &str = "urn:example:nadir";

    /// `G` and `OTHER` joinable (an item in `G` links to a note in `OTHER`), `THIRD`
    /// reachable from `OTHER` by the same predicate, one triple duplicated across `G` and
    /// `OTHER`, and a triple in the store's own default graph.
    fn three_tenants() -> Store {
        let store = Store::new().unwrap();
        store
            .load_from_reader(
                oxigraph::io::RdfFormat::NQuads,
                format!(
                    "<urn:item:1> <urn:next> <urn:note:1> <{G}> .\n\
                     <urn:note:1> <urn:body> \"joined\" <{OTHER}> .\n\
                     <urn:note:1> <urn:next> <urn:secret:1> <{OTHER}> .\n\
                     <urn:secret:1> <urn:body> \"third\" <{THIRD}> .\n\
                     <urn:shared> <urn:p> \"both\" <{G}> .\n\
                     <urn:shared> <urn:p> \"both\" <{OTHER}> .\n\
                     <urn:example:host:1> <urn:p> \"host\" .\n"
                )
                .as_bytes(),
            )
            .unwrap();
        store
    }

    fn pair() -> Vec<NamedNode> {
        vec![NamedNode::new(G).unwrap(), NamedNode::new(OTHER).unwrap()]
    }

    /// Rows of `query` confined to `graphs`, or unconfined when `graphs` is `None`.
    fn rows_over(store: &Store, query: &str, graphs: Option<&[NamedNode]>) -> Vec<String> {
        let mut prepared = SparqlEvaluator::new().parse_query(query).unwrap();
        if let Some(graphs) = graphs {
            confine(&mut prepared, graphs);
        }
        let QueryResults::Solutions(solutions) = prepared.on_store(store).execute().unwrap() else {
            panic!("not a solution set");
        };
        let mut out: Vec<String> = solutions
            .map(|s| {
                s.unwrap()
                    .iter()
                    .map(|(v, t)| format!("{}={t}", v.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn a_graph_set_parses_to_one_sorted_deduplicated_set() {
        let one = GraphSet::parse(G).unwrap();
        assert_eq!(one.graphs(), [NamedNode::new(G).unwrap()]);
        assert!(
            one.is_canonical(),
            "a single IRI is its own canonical spelling"
        );

        let canonical = format!("{G} {OTHER}");
        for spelling in [
            canonical.clone(),
            format!("{OTHER} {G}"),
            format!("{G} {OTHER} {G}"),
            format!("  {OTHER}\n{G}\t"),
            format!("{G}\n{OTHER}\n"),
        ] {
            let set = GraphSet::parse(&spelling).unwrap();
            assert_eq!(set.graphs(), pair(), "`{spelling}`");
            assert_eq!(set.canonical(), canonical, "`{spelling}`");
            assert_eq!(set.is_canonical(), spelling == canonical, "`{spelling}`");
        }
        // A trailing newline — what a pipe delivers (field guide 9h) — is not canonical,
        // so it is normalized rather than cached under a second key.
        assert!(!GraphSet::parse(&format!("{G}\n")).unwrap().is_canonical());
    }

    #[test]
    fn an_empty_graph_list_is_refused_not_read_as_no_graphs() {
        for raw in ["", "   ", "\n\t"] {
            let err = GraphSet::parse(raw).expect_err("an empty graph list");
            assert!(
                matches!(&err, Error::InvalidArgument { name, detail }
                    if name == "graph" && detail.contains("names no graph")),
                "`{raw:?}`: {err:?}"
            );
        }
    }

    #[test]
    fn one_bad_member_refuses_the_whole_set_and_names_it() {
        let err = GraphSet::parse(&format!("{G} not-an-iri {OTHER}")).expect_err("a bad member");
        assert!(
            matches!(&err, Error::InvalidArgument { name, detail }
                if name == "graph" && detail.contains("`not-an-iri`")),
            "{err:?}"
        );
    }

    /// The other half of the same rule: a non-ASCII space is legal inside an IRI too, so
    /// it is not a separator either (ledger #751).
    #[test]
    fn a_unicode_space_inside_an_iri_does_not_separate_graphs() {
        for g in [
            "urn:a\u{3000}urn:b",
            "urn:a\u{a0}urn:b",
            "urn:a\u{2003}urn:b",
        ] {
            assert!(NamedNode::new(g).is_ok(), "oxiri takes {g:?} as one IRI");
            let set = GraphSet::parse(g).unwrap();
            assert_eq!(set.graphs().len(), 1, "{g:?}");
            assert_eq!(set.graphs()[0].as_str(), g);
            assert!(set.is_canonical());
        }
        // ASCII whitespace still separates, in every spelling.
        let set = GraphSet::parse(&format!("{G}\t{OTHER}\n")).unwrap();
        assert_eq!(set.graphs().len(), 2);
    }

    /// ⚠ The reason the separator is whitespace alone: a comma is legal inside an IRI, so
    /// `G,OTHER` is ONE graph — and the endpoint's refusal is what says so.
    #[test]
    fn a_comma_does_not_separate_graphs() {
        let set = GraphSet::parse(&format!("{G},{OTHER}")).unwrap();
        assert_eq!(set.graphs().len(), 1);
        assert_eq!(set.graphs()[0].as_str(), format!("{G},{OTHER}"));
    }

    /// ★ The join #380 asks for, both ways a caller would write it — and the control that
    /// one graph alone cannot produce it.
    #[test]
    fn a_join_across_the_set_answers_through_the_merged_default_graph_and_by_name() {
        let store = three_tenants();
        let bare = "SELECT ?item ?body WHERE { ?item <urn:next> ?note . ?note <urn:body> ?body }";
        assert_eq!(
            rows_over(&store, bare, Some(&pair())),
            ["item=<urn:item:1> body=\"joined\""],
            "the merged default graph joins across the set"
        );
        let named = format!(
            "SELECT ?item ?body WHERE {{ GRAPH <{G}> {{ ?item <urn:next> ?note }} \
             GRAPH <{OTHER}> {{ ?note <urn:body> ?body }} }}"
        );
        assert_eq!(
            rows_over(&store, &named, Some(&pair())),
            ["item=<urn:item:1> body=\"joined\""],
            "GRAPH blocks join across the set"
        );
        // The control, and the bug: confined to one graph the same query is empty.
        assert!(rows_over(&store, &named, Some(&[NamedNode::new(G).unwrap()])).is_empty());
    }

    #[test]
    fn a_variable_graph_binds_only_members_of_the_set() {
        let store = three_tenants();
        let query = "SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } }";
        assert_eq!(
            rows_over(&store, query, Some(&pair())),
            [format!("g=<{G}>"), format!("g=<{OTHER}>")]
        );
        assert_eq!(
            rows_over(&store, query, None).len(),
            3,
            "the control: unconfined, the third tenant is enumerable"
        );
        assert_eq!(
            rows_over(&store, "SELECT ?g WHERE { GRAPH ?g {} }", Some(&pair())),
            [format!("g=<{G}>"), format!("g=<{OTHER}>")],
            "enumerating registered graph names is confined to the set too"
        );
    }

    /// ★ Rule 3, multi-graph: every escape shape the one-graph tests pin, against a graph
    /// OUTSIDE a two-graph set, each beside a positive case so an empty answer means
    /// "confined" and not "broken".
    #[test]
    fn nothing_reaches_a_graph_outside_the_set() {
        let store = three_tenants();
        let set = pair();
        let outside = [
            format!("SELECT ?s WHERE {{ GRAPH <{THIRD}> {{ ?s ?p ?o }} }}"),
            format!(
                "SELECT ?s WHERE {{ {{ SELECT ?s WHERE {{ GRAPH <{THIRD}> {{ ?s ?p ?o }} }} }} }}"
            ),
            format!(
                "SELECT ?s WHERE {{ ?s ?p ?o FILTER EXISTS {{ GRAPH <{THIRD}> {{ ?a ?b ?c }} }} }}"
            ),
            "SELECT ?s WHERE { ?s <urn:body> \"third\" }".to_string(),
            "SELECT ?s WHERE { ?s <urn:p> \"host\" }".to_string(),
        ];
        for query in &outside {
            assert!(
                rows_over(&store, query, Some(&set)).is_empty(),
                "`{query}` reached outside the set"
            );
        }
        // Controls: unconfined, the named-graph shapes really do reach the third tenant,
        // and the bare `host` pattern really does reach the store's default graph.
        assert_eq!(rows_over(&store, &outside[0], None), ["s=<urn:secret:1>"]);
        assert_eq!(
            rows_over(&store, &outside[4], None),
            ["s=<urn:example:host:1>"]
        );
        // …and the positive case in the same set: `GRAPH <OTHER>` still answers.
        assert_eq!(
            rows_over(
                &store,
                &format!("SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s <urn:body> ?b }} }}"),
                Some(&set)
            ),
            ["s=<urn:note:1>"]
        );
    }

    /// ★ The brief asked for this one by name: `a_property_path_cannot_walk_out_of_the_scope`
    /// extended to a set. The path walks G → OTHER (both members, so it must traverse) and
    /// stops at the edge into THIRD; over the union of all graphs it continues.
    #[test]
    fn a_property_path_crosses_members_of_the_set_and_cannot_walk_out_of_it() {
        let store = three_tenants();
        let query = "SELECT ?end WHERE { <urn:item:1> <urn:next>+ ?end }";
        assert_eq!(
            rows_over(&store, query, Some(&pair())),
            ["end=<urn:note:1>", "end=<urn:secret:1>"],
            "the path traverses inside the set — the edge INTO urn:secret:1 lives in OTHER"
        );
        // One step further is the escape: urn:secret:1's own quads live in THIRD, so a path
        // that needs them must stop.
        let deeper = "SELECT ?body WHERE { <urn:item:1> <urn:next>+/<urn:body> ?body }";
        assert_eq!(
            rows_over(&store, deeper, Some(&pair())),
            ["body=\"joined\""],
            "a path reached a body stored in a graph outside the set"
        );
        let mut prepared = SparqlEvaluator::new().parse_query(deeper).unwrap();
        prepared.dataset_mut().set_default_graph_as_union();
        let QueryResults::Solutions(solutions) = prepared.on_store(&store).execute().unwrap()
        else {
            panic!("not a solution set");
        };
        assert_eq!(
            solutions.count(),
            2,
            "the control: over the union the path really does reach THIRD's body"
        );
    }

    #[test]
    fn a_service_clause_is_refused_over_a_set_too() {
        let store = three_tenants();
        let mut prepared = crate::service::evaluator()
            .parse_query("SELECT ?s WHERE { SERVICE <http://example.invalid/sparql> { ?s ?p ?o } }")
            .unwrap();
        confine(&mut prepared, &pair());
        let failed = match prepared.on_store(&store).execute() {
            Err(_) => true,
            Ok(QueryResults::Solutions(solutions)) => solutions.into_iter().any(|s| s.is_err()),
            Ok(_) => panic!("not a solution set"),
        };
        assert!(failed, "a SERVICE clause was answered under a graph set");
    }

    /// ⚠ Pins upstream, and it is not the spec's RDF merge: oxigraph evaluates a
    /// several-graph default graph as a BAG, so a triple in two members matches once per
    /// member. Written as found (2), so the day it becomes a set this fails and the docs
    /// that tell callers to use `DISTINCT` get revisited.
    #[test]
    fn a_triple_in_two_members_matches_once_per_member_in_the_default_graph() {
        let store = three_tenants();
        let mut prepared = SparqlEvaluator::new()
            .parse_query("SELECT ?o WHERE { <urn:shared> <urn:p> ?o }")
            .unwrap();
        confine(&mut prepared, &pair());
        let QueryResults::Solutions(solutions) = prepared.on_store(&store).execute().unwrap()
        else {
            panic!("not a solution set");
        };
        assert_eq!(
            solutions.count(),
            2,
            "oxigraph's several-graph default graph changed from a bag — revisit the docs"
        );
        assert_eq!(
            rows_over(
                &store,
                "SELECT ?g WHERE { GRAPH ?g { <urn:shared> <urn:p> ?o } }",
                Some(&pair())
            ),
            [format!("g=<{G}>"), format!("g=<{OTHER}>")]
        );
    }
}
