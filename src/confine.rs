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
//! day oxigraph bumps it. (Since ledger #964 this crate does depend on `spargebra`, at a
//! caret that unifies with oxigraph's pin, to bound a query's algebra before planning — see
//! the note in `Cargo.toml`. The argument below, that a syntactic check would not be enough,
//! stands either way.) Not enough: `DELETE WHERE { GRAPH ?g { ?s ?p ?o } }` names its
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
//! Every row whose update has a `WHERE` — the `WITH`, `COPY`/`MOVE`/`ADD` and
//! `DELETE WHERE` rows — is reached only by a caller that may also READ `G`; a write-only
//! caller is refused on the grant before any of this runs. See below.
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
//! # ★ …and a WRITE grant alone cannot read even `G` (ledger #751)
//!
//! A `WHERE` clause READS the graph it matches against, so an update with one is a read
//! of `G` as well as a write. Until ledger #751 (2026-10-05) a caller holding only
//! `urn:cap:store:write:graph:<G>` could read `G` two ways without changing anything:
//! copy its quads into an escaping pattern (`INSERT { GRAPH <elsewhere> { ?s ?p ?o } }
//! WHERE { GRAPH <G> { ?s ?p ?o } }`) and read them back out of the refusal, which quoted
//! the first escaping quad; or, with the refusal redacted, guess a value and watch whether
//! the update was refused — a boolean oracle over `G`'s contents, one guess per call.
//!
//! So the rule is **an update with a `WHERE` needs the read grant on every graph its
//! `WHERE` reads** — here exactly `G`, so `urn:cap:store:read:graph:<G>` (or the broad
//! `urn:cap:store:read`, which reads `G` anyway). Three properties make it hold:
//!
//! - **It is decided on the grant, before anything is evaluated.** Whether the update has
//!   a `WHERE` is a property of its TEXT ([`reads_the_dataset`]), never of `G`'s contents,
//!   so refused-or-not says nothing about the data.
//! - **A refusal never quotes data.** It names the scoped graph, where the escape went,
//!   and the grant — not the quad.
//! - **A write-only caller is not told the counts.** `+N -M quads` is an oracle of its
//!   own (an `INSERT DATA` of a quad already there adds 0), so the success line carries
//!   them only for a caller that may read `G`.
//!
//! `INSERT DATA`, `DELETE DATA`, `CLEAR`, `DROP` and `CREATE` have no `WHERE` and stay
//! write-only — but for a caller who may not read `G`, `DROP`, `CLEAR` and `CREATE` of `G`
//! run as `SILENT`, because their non-`SILENT` failure says whether `G` exists (ledger #761,
//! `silence_graph_management`). (`LOAD` has none either, and is refused outright: see [`parse`].) `COPY`, `MOVE` and `ADD` are rewritten by the parser into
//! `DELETE/INSERT … WHERE` over the source graph, so they read it, and need the grant.
//!
//! //! # What it costs
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

use crate::budget::Deadline;
use oxigraph::sparql::{PreparedSparqlUpdate, SparqlEvaluator};
use oxigraph::store::Store;

/// What a scoped update did to the real store.
pub(crate) struct Applied {
    pub added: usize,
    pub removed: usize,
}

/// Parse a SPARQL update, refusing one that does not parse by the argument's name.
///
/// ★ The one place an update reaches the parser, so the one place it is bounded first
/// (ledger #915): [`crate::limits::check_sparql`] refuses text past the byte or nesting
/// bound. Call it inside [`crate::DurableStore::evaluate`], which runs the parse and the
/// evaluation that follows on a stack sized for them (both are recursive) and within the
/// caller's time budget.
///
/// The update is evaluated under `deadline`'s cancellation token (ledger #964). An update
/// with a `LOAD` in it is refused here, before evaluation (ledger #992, [`refuse_load`]).
///
/// `unreadable` names the graphs the caller may write and may NOT read: graph-management
/// operations on them run as `SILENT` (ledger #761, [`Unreadable`]).
pub(crate) fn parse(
    update: &str,
    deadline: &Deadline,
    unreadable: Unreadable<'_>,
) -> Result<PreparedSparqlUpdate> {
    crate::limits::check_sparql(update, "content")?;
    // ★ Parsed by the parser oxigraph uses, and measured before oxigraph plans it: the
    // planner cannot be cancelled (ledger #964, `src/budget.rs`).
    let mut parsed = spargebra::SparqlParser::new()
        .parse_update(update)
        .map_err(|e| Error::InvalidArgument {
            name: "content".to_string(),
            detail: format!("not a SPARQL update: {e}"),
        })?;
    refuse_load(&parsed)?;
    silence_graph_management(&mut parsed, unreadable);
    crate::budget::check_update(&parsed, "content")?;
    Ok(SparqlEvaluator::new()
        .with_cancellation_token(deadline.token())
        .for_update(parsed))
}

/// Refuse an update with a `LOAD <url>` in it, before anything is evaluated (ledger #992).
///
/// `LOAD` fetches a document and parses it INSIDE oxigraph, with nothing between the fetch and
/// the parse where `urn:iki:store:load`'s depth scan ([`crate::depth`]) could stand: in a
/// build with RDF 1.2 on, a fetched document nesting ~50,000 triple terms aborted the host on
/// the `ikigai-store-sparql` thread (reproduced with `oxigraph/http-client` on). And the fetch
/// is oxigraph's own, so in a host whose graph enables `oxigraph/http-client` (`ikigai-cli`
/// does, through rudof) it is an outbound request no `urn:cap:net:*` gates (ledger #145).
/// Without that feature `LOAD` already failed, at evaluation; it now fails at the door, by
/// name, in every build. To bring a remote graph in, source it through the kernel (where the
/// net capability applies) and sink it into `urn:iki:store:load`.
fn refuse_load(update: &spargebra::Update) -> Result<()> {
    let loads = update
        .operations
        .iter()
        .any(|op| matches!(op, spargebra::GraphUpdateOperation::Load { .. }));
    if loads {
        return Err(Error::InvalidArgument {
            name: "content".to_string(),
            detail: "`LOAD` is not available through this store: it would fetch and parse a \
                     document inside the SPARQL engine, unscanned for depth and ungated by any \
                     network capability. Nothing was evaluated. Source the document through the \
                     kernel and sink it into `urn:iki:store:load`"
                .to_string(),
        });
    }
    Ok(())
}

/// Which graphs the caller may write but may not read, for [`parse`] (ledger #761).
#[derive(Clone, Copy)]
pub(crate) enum Unreadable<'a> {
    /// The caller may read every graph this update can name; standard SPARQL semantics.
    None,
    /// The scoped door: the caller may not read this graph.
    Graph(&'a NamedNode),
    /// The broad door: the caller may read no graph at all.
    EveryGraph,
}

impl Unreadable<'_> {
    fn covers(self, graph: &str) -> bool {
        match self {
            Unreadable::None => false,
            Unreadable::Graph(g) => g.as_str() == graph,
            Unreadable::EveryGraph => true,
        }
    }
}

/// Run `DROP`, `CLEAR` and `CREATE` of a graph the caller may not read as if the update had
/// said `SILENT` (ledger #761).
///
/// ★ A non-`SILENT` `DROP GRAPH <G>` or `CLEAR GRAPH <G>` FAILS when `G` does not exist, and
/// `CREATE GRAPH <G>` fails when it does, so through a door that runs them for a caller with
/// the write grant alone each was one bit of `G`'s state per call — the last read channel the
/// ledger #751 arc reproduced and left open. `SILENT` is the uniform answer because it is the
/// only one that keeps the operation meaningful: the caller gets the same success line
/// whatever `G` held, and the effect is still what it asked for (after `DROP`, `G` is gone;
/// after `CREATE`, it exists). A uniform REFUSAL would have broken every legitimate one.
///
/// Decided on the caller's GRANT and the update's TEXT, never on the data, like the rest of
/// the read rule. A caller who may read the graph keeps the standard error: it could have
/// asked `graph-ask`, and the error tells it its update named nothing. `DEFAULT`, `NAMED` and
/// `ALL` targets are untouched — they cannot fail, so they were never an oracle.
fn silence_graph_management(update: &mut spargebra::Update, unreadable: Unreadable<'_>) {
    use spargebra::algebra::GraphTarget;
    use spargebra::GraphUpdateOperation::{Clear, Create, Drop};
    for operation in &mut update.operations {
        match operation {
            Drop {
                silent,
                graph: GraphTarget::NamedNode(graph),
            }
            | Clear {
                silent,
                graph: GraphTarget::NamedNode(graph),
            }
            | Create { silent, graph }
                if unreadable.covers(graph.as_str()) =>
            {
                *silent = true;
            }
            _ => {}
        }
    }
}

/// Whether the update has a `WHERE` — i.e. READS the dataset it runs against.
///
/// ★ Decided on the update's TEXT, never on the data, which is what keeps the read-grant
/// refusal from being an oracle. oxigraph keeps the parsed AST private, but it exposes one
/// query dataset specification per `DELETE/INSERT … WHERE` operation (absent a `USING`
/// clause, the default one), and none for any other operation: so "has at least one" is
/// exactly "some operation in this request evaluates a graph pattern". That covers
/// `DELETE WHERE`, `WITH … WHERE` and the `COPY`/`MOVE`/`ADD` rewrites, and excludes
/// `INSERT DATA`, `DELETE DATA`, `CLEAR`, `DROP`, `CREATE` and `LOAD` (which [`parse`] refuses).
/// `the_read_grant_rule_classifies_every_operation` pins the table, so an upstream change
/// to that mapping fails a test rather than quietly reopening the oracle.
pub(crate) fn reads_the_dataset(prepared: &PreparedSparqlUpdate) -> bool {
    prepared.using_datasets().next().is_some()
}

/// Evaluate `update` confined to `graph`, applying it only if nothing escaped.
///
/// The caller must already hold the per-graph capability — the write grant always, and the
/// read grant when [`reads_the_dataset`] — and the store's write lock: this function is
/// the mechanism, not the gate.
///
/// ★ The real store is written only at `deadline`'s commit point (ledger #964): the scratch
/// copy is private, so an update that runs out of time before step 4 has written nothing
/// anywhere, and one that runs out at step 4 is not committed.
pub(crate) fn scoped_update(
    store: &Store,
    graph: &NamedNode,
    update: PreparedSparqlUpdate,
    deadline: &Deadline,
) -> Result<Applied> {
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
        deadline.check()?;
        scratch.insert(&quad.map_err(storage)?).map_err(storage)?;
    }
    let before: HashSet<Quad> = scratch
        .iter()
        .collect::<std::result::Result<_, _>>()
        .map_err(storage)?;

    // 2. The caller's update, against the private dataset.
    update
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
    deadline.settle(|| txn.commit().map_err(storage))?;

    Ok(Applied {
        added: added.len(),
        removed: removed.len(),
    })
}

/// ⚠ `Denied`, not `InvalidArgument`: the update is well-formed and the caller simply
/// lacks authority over the graph it reached for. That is the typed shape the kernel's
/// denial observability and the conformance suite both read.
///
/// ★ **It never quotes the quad** (ledger #751). Until ledger #751 it did, and the quad could
/// be one the update had copied out of `G` by a `WHERE` — so the refusal was a read
/// channel. Where the escape went is named (the default graph, or a graph the caller's own
/// text or readable data supplied); the data is not.
fn escaped_quad(graph: &NamedNode, quad: &Quad) -> Error {
    let where_it_went = match &quad.graph_name {
        GraphName::DefaultGraph => "the DEFAULT graph".to_string(),
        other => format!("graph {other}"),
    };
    Error::Denied(format!(
        "this write is scoped to graph <{}> and the update would have written {where_it_went}. \
         Nothing was applied. A statement with no `GRAPH` block goes to the default graph, \
         which is not the scoped graph — wrap it in `GRAPH <{}> {{ … }}`, or hold \
         `urn:cap:store:write` and use `urn:iki:store:update`",
        graph.as_str(),
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
