//! Running an **arbitrary** SPARQL UPDATE with an **exact** per-graph write scope.
//!
//! # ★ Why this is not a check on the update's text
//!
//! `urn:cap:store:write` is `DROP ALL`, so a module layered over this store has to make
//! its callers hold that authority to append one triple — which is why the `ikigai-cli`
//! binding arc refused to put the store on the served space or the HTTP door at all, and
//! why segmenting a ledger by client needs a real boundary rather than a tag.
//! `urn:cap:store:write:graph:<iri>` is that boundary, and it is only worth anything if
//! it is exact.
//!
//! The obvious implementation — parse the update and look at which graphs it names — is
//! **not available and would not be enough if it were.** Not available: oxigraph's
//! `Update` keeps its `spargebra` AST private and exposes only the `USING` clauses, and
//! taking a direct `spargebra` dependency would mean pinning `=0.4.7` beside oxigraph's
//! own exact pin, so a stranger's fresh `cargo add ikigai-store` would stop compiling the
//! day oxigraph bumps it. Not enough: `DELETE WHERE { GRAPH ?g { ?s ?p ?o } }` names its
//! graph with a *variable*, and `INSERT DATA { <s> <p> <o> }` names none at all and
//! writes the default graph.
//!
//! So the scope is enforced on **effects, not syntax**:
//!
//! 1. Copy graph `G` — and nothing else — into a private in-memory store.
//! 2. Run the caller's update against *that*.
//! 3. Refuse if anything landed outside `G`: a quad in another graph or in the default
//!    graph, or a named graph the update created.
//! 4. Otherwise apply the difference to the real store's `G`, in one transaction.
//!
//! Every shape that defeats a syntactic check is handled by construction, and each of
//! them is pinned by a test in `tests/graph_scope.rs`:
//!
//! | update | what happens | why |
//! | --- | --- | --- |
//! | `INSERT DATA { GRAPH <G> { … } }` | applied | in scope |
//! | `INSERT DATA { <s> <p> <o> }` | **refused** | the default graph is not `G` |
//! | `INSERT DATA { GRAPH <other> { … } }` | **refused** | named another graph |
//! | `WITH <other> INSERT … WHERE …` | **refused** | same, by another spelling |
//! | `COPY`/`MOVE`/`ADD … TO <other>` | **refused** | the destination quads escape |
//! | `CREATE GRAPH <other>` | **refused** | creates no quad, but registers a graph |
//! | `DROP ALL`, `CLEAR ALL` | applied to `G` alone | the private store held only `G`, and emptying `G` is within a grant over `G` |
//! | `DELETE WHERE { GRAPH ?g { … } }` | applied to `G` alone | the variable can only bind `G` |
//!
//! # ⚠ The scoped door also cannot READ outside its graph, and that is deliberate
//!
//! The private store contains `G` and nothing else, so a `WHERE` clause that reads
//! another graph — or the default graph — matches nothing, and the same update run
//! through `urn:iki:store:update` would behave differently. That is the price of the
//! boundary and it is the right way round: a grant that let one client's agent *read*
//! another client's graph in order to decide what to write in its own would not be a
//! boundary at all. It is stated on the endpoint, in the README, and here, because a
//! silent difference in results is worse than a refusal.
//!
//! # What it costs
//!
//! Graph `G` is copied into memory on every scoped write, twice over (the before and
//! after sets), so this door is priced for a graph a module owns — a ledger, a layer, an
//! annotation set — and not for a materialized database. The unscoped
//! `urn:iki:store:update` is unchanged and copies nothing.
//!
//! Blank-node labels survive the round trip (oxigraph stores them by label), but this
//! ecosystem skolemizes and a blank node in a scoped graph is outside what these tests
//! pin.

use std::collections::HashSet;

use ikigai_core::{Error, Result};
use oxigraph::model::{GraphName, NamedNode, Quad};
use oxigraph::sparql::SparqlEvaluator;
use oxigraph::store::Store;

/// What a scoped update did to the real store.
pub(crate) struct Applied {
    pub added: usize,
    pub removed: usize,
}

/// Evaluate `update` confined to `graph`, applying it only if nothing escaped.
///
/// The caller must already hold the per-graph capability and the store's write lock —
/// this function is the mechanism, not the gate.
pub(crate) fn scoped_update(store: &Store, graph: &NamedNode, update: &str) -> Result<Applied> {
    let scope = GraphName::from(graph.clone());
    let scratch = Store::new().map_err(storage)?;

    // 1. The private dataset: graph G, and nothing else. Its REGISTRATION is copied too,
    //    so `DROP GRAPH <G>` on an existing-but-empty graph is the same operation here
    //    that it would be against the real store.
    let registered = store
        .contains_named_graph(graph.as_ref())
        .map_err(storage)?;
    if registered {
        scratch
            .insert_named_graph(graph.as_ref())
            .map_err(storage)?;
    }
    for quad in store.quads_for_pattern(None, None, None, Some(graph.as_ref().into())) {
        scratch.insert(&quad.map_err(storage)?).map_err(storage)?;
    }
    let before: HashSet<Quad> = scratch
        .iter()
        .collect::<std::result::Result<_, _>>()
        .map_err(storage)?;

    // 2. The caller's update, against the private dataset.
    SparqlEvaluator::new()
        .parse_update(update)
        .map_err(|e| Error::InvalidArgument {
            name: "content".to_string(),
            detail: format!("not a SPARQL update: {e}"),
        })?
        .on_store(&scratch)
        .execute()
        .map_err(|e| Error::Endpoint(format!("update: {e}")))?;

    // 3. Did anything land outside G? Nothing has touched the real store yet, so a
    //    refusal here is total — which is also what keeps a `;`-separated request
    //    all-or-nothing through this door.
    let after: HashSet<Quad> = scratch
        .iter()
        .collect::<std::result::Result<_, _>>()
        .map_err(storage)?;
    if let Some(escaped) = after.iter().find(|q| q.graph_name != scope) {
        return Err(escaped_quad(graph, escaped));
    }
    for name in scratch.named_graphs() {
        let name = name.map_err(storage)?;
        if GraphName::from(name.clone()) != scope {
            return Err(escaped_graph(graph, &name.to_string()));
        }
    }

    // 4. Apply the difference, in one transaction.
    let added: Vec<&Quad> = after.difference(&before).collect();
    let removed: Vec<&Quad> = before.difference(&after).collect();
    let now_registered = scratch
        .contains_named_graph(graph.as_ref())
        .map_err(storage)?;
    let mut txn = store.start_transaction().map_err(storage)?;
    for quad in &removed {
        txn.remove(quad.as_ref());
    }
    for quad in &added {
        txn.insert(quad.as_ref());
    }
    if registered && !now_registered {
        txn.remove_named_graph(graph.as_ref()).map_err(storage)?;
    } else if !registered && now_registered {
        txn.insert_named_graph(graph.as_ref());
    }
    txn.commit().map_err(storage)?;

    Ok(Applied {
        added: added.len(),
        removed: removed.len(),
    })
}

/// ⚠ `Denied`, not `InvalidArgument`: the update is well-formed and the caller simply
/// lacks authority over the graph it reached for. That is the typed shape the kernel's
/// denial observability and the conformance suite both read.
fn escaped_quad(graph: &NamedNode, quad: &Quad) -> Error {
    let where_it_went = match &quad.graph_name {
        GraphName::DefaultGraph => "the DEFAULT graph".to_string(),
        other => format!("graph {other}"),
    };
    Error::Denied(format!(
        "this write is scoped to graph <{}> and the update would have written {where_it_went} \
         (`{} {} {}`). Nothing was applied. A statement with no `GRAPH` block goes to the \
         default graph, which is not the scoped graph — wrap it in `GRAPH <{}> {{ … }}`, or \
         hold `urn:cap:store:write` and use `urn:iki:store:update`",
        graph.as_str(),
        quad.subject,
        quad.predicate,
        quad.object,
        graph.as_str(),
    ))
}

fn escaped_graph(graph: &NamedNode, created: &str) -> Error {
    Error::Denied(format!(
        "this write is scoped to graph <{}> and the update created graph {created}. Nothing was \
         applied. Creating or dropping another graph needs `urn:cap:store:write`",
        graph.as_str(),
    ))
}

fn storage(e: impl std::fmt::Display) -> Error {
    Error::Endpoint(format!("store: {e}"))
}
