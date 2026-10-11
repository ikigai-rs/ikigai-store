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
//! 1. Copy the quads of graph `G` that the update can observe — and nothing outside `G` —
//!    into a private in-memory store (see "What it costs" for which quads, ledger #1132).
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
//! # What it costs (ledger #1132)
//!
//! Step 1 copies only the part of `G` the update can OBSERVE OR REMOVE — its [`Footprint`]:
//! the quads of `G` matching some triple pattern of a `WHERE` (in `EXISTS`, sub-selects and
//! paths included), some `DELETE` template, or some `DELETE DATA` quad, by the constants in
//! that pattern. A ledger append names its counter by subject and predicate and its key by
//! predicate and object, so it copies a quad or two whatever the ledger holds. Until ledger
//! #1132 the whole of `G` was copied on every write, so each append cost in proportion to
//! everything appended before it: measured on the RocksDB backing, a median append took 23,
//! 144 and 614 ms in debug at 100, 750 and 3,000 items (3, 25 and 122 ms in release), and
//! now about 1 ms in debug and 0.15 ms in release at all three
//! (`tests/scoped_append_cost.rs`).
//!
//! ★ **The boundary does not depend on the footprint.** Whatever is copied, the private store
//! holds quads of `G` and nothing else, so no other graph can be read, and step 3 checks the
//! effects exactly as before. What the footprint has to get right is FIDELITY — that the
//! update computes on the part of `G` what it would have computed on all of it — and it does
//! by an argument on SPARQL's semantics, pinned by a differential test in this module
//! (`the_footprint_computes_what_the_whole_graph_computes`):
//!
//! - A triple pattern's matches depend only on the quads that agree with its constants, and
//!   every operator above a pattern (join, `OPTIONAL`, `MINUS`, `FILTER`, aggregates,
//!   `EXISTS`) is a function of its operands' solutions. A quad of `G` that matches no pattern
//!   of the update changes no solution, so leaving it out changes no answer.
//! - A `DELETE` removes only quads that instantiate one of its templates, and every such quad
//!   matches that template's pattern, so it was copied: no removal is lost.
//! - An `INSERT` of a quad that was in `G` but not copied looks like an addition to the
//!   private store, so step 4 checks the real store before counting it: `+0` stays `+0`.
//! - Three shapes observe the graph as a whole, and copy all of it: a path that can be
//!   zero-length (`p*`, `p?`, which match every node of the graph when both ends are
//!   variables) or is a negated property set; a pattern with no constant at all; and
//!   `CLEAR`/`DROP`, which remove everything. So does anything this walk does not recognize
//!   (`SERVICE`, refused anyway, and `LOAD`). Copying too much is a cost; copying too little
//!   would be a wrong answer, so every doubt resolves to the whole graph.
//!
//! ⚠ What the footprint cannot preserve is an order SPARQL leaves undefined: a `LIMIT` with no
//! `ORDER BY` picks among its solutions by the private store's insertion order, which a
//! smaller copy can change. That was never the same as the broad door's choice either.
//!
//! The unscoped `urn:iki:store:update` is unchanged and copies nothing.
//!
//! Blank-node labels survive the round trip (oxigraph stores them by label), but this
//! ecosystem skolemizes and a blank node in a scoped graph is outside what these tests
//! pin.

use std::collections::HashSet;

use ikigai_core::{Error, Result};
use oxigraph::model::{GraphName, NamedNode, NamedOrBlankNode, Quad, Term};
use spargebra::algebra::{
    AggregateExpression, Expression, GraphPattern, OrderExpression, PropertyPathExpression,
};
use spargebra::term::{GroundTerm, GroundTermPattern, NamedNodePattern, TermPattern};
use spargebra::{GraphUpdateOperation, Update};

use crate::budget::Deadline;
use oxigraph::sparql::PreparedSparqlUpdate;
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
/// with a `LOAD` in it is refused here, before evaluation (ledger #992, [`crate::service::refuse_load`]), and
/// so is one with a `SERVICE` in a `WHERE` (ledger #1083, [`crate::service`]).
///
/// `unreadable` names the graphs the caller may write and may NOT read: graph-management
/// operations on them run as `SILENT` (ledger #761, [`Unreadable`]).
///
/// The answer carries the update's [`Footprint`] as well, read off the same parse, for
/// [`scoped_update`]; the broad door ignores it.
pub(crate) fn parse(
    update: &str,
    deadline: &Deadline,
    unreadable: Unreadable<'_>,
) -> Result<Parsed> {
    crate::limits::check_sparql(update, "content")?;
    // ★ Parsed by the parser oxigraph uses, and measured before oxigraph plans it: the
    // planner cannot be cancelled (ledger #964, `src/budget.rs`).
    let mut parsed = spargebra::SparqlParser::new()
        .parse_update(update)
        .map_err(|e| Error::InvalidArgument {
            name: "content".to_string(),
            detail: format!("not a SPARQL update: {e}"),
        })?;
    crate::service::refuse_load(&parsed, "content")?;
    silence_graph_management(&mut parsed, unreadable);
    crate::budget::check_update(&parsed, "content")?;
    // ★ No `SERVICE` in a `WHERE`, refused by name, on an evaluator that refuses one itself in
    // every build (ledger #1083, `src/service.rs`).
    crate::service::refuse_service_in_update(&parsed, "content")?;
    let footprint = footprint(&parsed);
    Ok(Parsed {
        prepared: crate::service::evaluator()
            .with_cancellation_token(deadline.token())
            .for_update(parsed),
        footprint,
    })
}

/// A parsed, bounded update, and what of a scoped graph it can observe.
pub(crate) struct Parsed {
    pub prepared: PreparedSparqlUpdate,
    pub footprint: Footprint,
}

/// Which quads of the scoped graph an update can OBSERVE OR REMOVE (ledger #1132): what
/// [`scoped_update`] copies into its private store, and all it needs to. The module docs
/// ("What it costs") give the argument that this is enough.
///
/// ★ Read off the update's TEXT, like the read rule: which quads are copied depends on what
/// `G` holds, but nothing a caller sees does — the answer is the same either way, and the
/// counts in it go only to a caller who may read `G`.
#[derive(Debug, PartialEq)]
pub(crate) enum Footprint {
    /// Every quad of the graph: `CLEAR`, `DROP`, a pattern with no constant in it, a path
    /// that can be zero-length or is negated, and anything this walk does not recognize.
    Whole,
    /// The quads matching at least one of these. Empty for an update that reads and removes
    /// nothing (`INSERT DATA`, `CREATE`).
    Matching(Vec<Probe>),
}

/// One pattern's constants: a quad of the graph matches when it agrees with each one given.
#[derive(Debug, PartialEq)]
pub(crate) struct Probe {
    subject: Option<NamedOrBlankNode>,
    predicate: Option<NamedNode>,
    object: Option<Term>,
}

impl Probe {
    fn is_everything(&self) -> bool {
        self.subject.is_none() && self.predicate.is_none() && self.object.is_none()
    }
}

/// The [`Footprint`] of a parsed update: every triple pattern of every `WHERE`, every
/// `DELETE` template and every `DELETE DATA` quad, by their constants.
pub(crate) fn footprint(update: &Update) -> Footprint {
    let mut probes = Vec::new();
    for operation in &update.operations {
        let bounded = match operation {
            // Adds quads and reads none, so it observes nothing of the graph: whether an added
            // quad was already there is settled against the real store (`scoped_update`).
            GraphUpdateOperation::InsertData { .. } | GraphUpdateOperation::Create { .. } => true,
            GraphUpdateOperation::DeleteData { data } => {
                probes.extend(data.iter().map(|quad| Probe {
                    subject: Some(quad.subject.clone().into()),
                    predicate: Some(quad.predicate.clone()),
                    object: ground_term(&quad.object),
                }));
                true
            }
            GraphUpdateOperation::DeleteInsert {
                delete, pattern, ..
            } => {
                probes.extend(delete.iter().map(|template| Probe {
                    subject: ground_term_pattern(&template.subject).and_then(subject),
                    predicate: named_node(&template.predicate),
                    object: ground_term_pattern(&template.object),
                }));
                pattern_probes(pattern, &mut probes)
            }
            // `CLEAR` and `DROP` remove the whole graph; `LOAD` is refused before this runs.
            GraphUpdateOperation::Clear { .. }
            | GraphUpdateOperation::Drop { .. }
            | GraphUpdateOperation::Load { .. } => false,
        };
        if !bounded {
            return Footprint::Whole;
        }
    }
    if probes.iter().any(Probe::is_everything) {
        return Footprint::Whole;
    }
    Footprint::Matching(probes)
}

/// Add `pattern`'s probes, or answer `false` when it can observe the graph as a whole.
///
/// ⚠ Exhaustive over `spargebra`'s algebra on purpose, for the reason `service.rs` gives: a
/// new variant upstream is a compile error here, not a pattern this walk silently skips —
/// which here would be a wrong answer, not a missed refusal. Recursive and bounded: it runs
/// after `budget::check_update`, on the sized SPARQL thread.
fn pattern_probes(pattern: &GraphPattern, probes: &mut Vec<Probe>) -> bool {
    match pattern {
        GraphPattern::Bgp { patterns } => {
            probes.extend(patterns.iter().map(|triple| Probe {
                subject: term_pattern(&triple.subject).and_then(subject),
                predicate: named_node(&triple.predicate),
                object: term_pattern(&triple.object),
            }));
            true
        }
        // The path's ends do not narrow it: a path reaches through intermediate nodes.
        GraphPattern::Path { path, .. } => path_probes(path, probes),
        GraphPattern::Values { .. } => true,
        // Refused before this runs (`service::refuse_service_in_update`).
        GraphPattern::Service { .. } => false,
        GraphPattern::Join { left, right }
        | GraphPattern::Lateral { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => {
            pattern_probes(left, probes) && pattern_probes(right, probes)
        }
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => {
            pattern_probes(left, probes)
                && pattern_probes(right, probes)
                && expression
                    .as_ref()
                    .is_none_or(|e| expression_probes(e, probes))
        }
        GraphPattern::Filter { expr, inner } => {
            expression_probes(expr, probes) && pattern_probes(inner, probes)
        }
        GraphPattern::Extend {
            inner, expression, ..
        } => expression_probes(expression, probes) && pattern_probes(inner, probes),
        GraphPattern::OrderBy { inner, expression } => {
            expression.iter().all(|order| match order {
                OrderExpression::Asc(e) | OrderExpression::Desc(e) => expression_probes(e, probes),
            }) && pattern_probes(inner, probes)
        }
        GraphPattern::Group {
            inner, aggregates, ..
        } => {
            aggregates.iter().all(|(_, aggregate)| match aggregate {
                AggregateExpression::FunctionCall { expr, .. } => expression_probes(expr, probes),
                AggregateExpression::CountSolutions { .. } => true,
            }) && pattern_probes(inner, probes)
        }
        // A `GRAPH` name does not narrow the probe: only `G` is copied, whatever it names.
        GraphPattern::Graph { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. } => pattern_probes(inner, probes),
    }
}

/// The probes of the patterns inside an expression: an `EXISTS` reads the graph.
fn expression_probes(expression: &Expression, probes: &mut Vec<Probe>) -> bool {
    match expression {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => true,
        Expression::Exists(pattern) => pattern_probes(pattern, probes),
        Expression::In(e, members) => {
            expression_probes(e, probes) && members.iter().all(|m| expression_probes(m, probes))
        }
        Expression::Coalesce(args) | Expression::FunctionCall(_, args) => {
            args.iter().all(|a| expression_probes(a, probes))
        }
        Expression::If(a, b, c) => {
            expression_probes(a, probes)
                && expression_probes(b, probes)
                && expression_probes(c, probes)
        }
        Expression::UnaryPlus(e) | Expression::UnaryMinus(e) | Expression::Not(e) => {
            expression_probes(e, probes)
        }
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
        | Expression::Divide(a, b) => expression_probes(a, probes) && expression_probes(b, probes),
    }
}

/// A path's probes: one per predicate it can traverse, or `false` when it can match without
/// traversing any (`p*`, `p?`: a zero-length path matches every node of the graph when both
/// ends are variables) or can traverse any predicate but some (`!p`).
fn path_probes(path: &PropertyPathExpression, probes: &mut Vec<Probe>) -> bool {
    match path {
        PropertyPathExpression::NamedNode(predicate) => {
            probes.push(Probe {
                subject: None,
                predicate: Some(predicate.clone()),
                object: None,
            });
            true
        }
        PropertyPathExpression::Reverse(inner) | PropertyPathExpression::OneOrMore(inner) => {
            path_probes(inner, probes)
        }
        PropertyPathExpression::Sequence(a, b) | PropertyPathExpression::Alternative(a, b) => {
            path_probes(a, probes) && path_probes(b, probes)
        }
        PropertyPathExpression::ZeroOrMore(_)
        | PropertyPathExpression::ZeroOrOne(_)
        | PropertyPathExpression::NegatedPropertySet(_) => false,
    }
}

// ⚠ The three term conversions below end in a catch-all, unlike the walks above, and that is
// the safe direction: `TermPattern`, `GroundTermPattern` and `GroundTerm` grow a quoted-triple
// variant when ANY crate in the build enables RDF 1.2 (feature unification; see
// `sparql::bindings_json`), so no match can be exhaustive in every build — and an unknown term
// becomes "no constant", which copies MORE of the graph, never less.

fn term_pattern(term: &TermPattern) -> Option<Term> {
    #[allow(unreachable_patterns)] // reachable only when a sibling crate enables RDF 1.2
    match term {
        TermPattern::NamedNode(node) => Some(node.clone().into()),
        TermPattern::Literal(literal) => Some(literal.clone().into()),
        // A blank node in a pattern is a variable.
        _ => None,
    }
}

fn ground_term_pattern(term: &GroundTermPattern) -> Option<Term> {
    #[allow(unreachable_patterns)] // reachable only when a sibling crate enables RDF 1.2
    match term {
        GroundTermPattern::NamedNode(node) => Some(node.clone().into()),
        GroundTermPattern::Literal(literal) => Some(literal.clone().into()),
        _ => None,
    }
}

fn ground_term(term: &GroundTerm) -> Option<Term> {
    #[allow(unreachable_patterns)] // reachable only when a sibling crate enables RDF 1.2
    match term {
        GroundTerm::NamedNode(node) => Some(node.clone().into()),
        GroundTerm::Literal(literal) => Some(literal.clone().into()),
        _ => None,
    }
}

fn named_node(pattern: &NamedNodePattern) -> Option<NamedNode> {
    match pattern {
        NamedNodePattern::NamedNode(node) => Some(node.clone()),
        NamedNodePattern::Variable(_) => None,
    }
}

/// A constant in subject position as a subject. A literal there matches no quad at all;
/// treating it as no constant copies more than needed, which is the safe direction.
fn subject(term: Term) -> Option<NamedOrBlankNode> {
    match term {
        Term::NamedNode(node) => Some(node.into()),
        _ => None,
    }
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
    footprint: &Footprint,
    deadline: &Deadline,
) -> Result<Applied> {
    let scope = GraphName::from(graph.clone());
    let scratch = Store::new().map_err(storage)?;

    // 1. The private dataset: the part of G the update can observe, and nothing outside G.
    //    G's REGISTRATION is copied too, so `DROP GRAPH <G>` on an existing-but-empty graph
    //    is the same operation here that it would be against the real store.
    let registered = store
        .contains_named_graph(graph.as_ref())
        .map_err(storage)?;
    if registered {
        scratch
            .insert_named_graph(graph.as_ref())
            .map_err(storage)?;
    }
    let before = copy(store, graph, footprint, &scratch, deadline)?;

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
    let removed: Vec<&Quad> = before.difference(&after).collect();
    let mut added: Vec<&Quad> = Vec::new();
    for quad in after.difference(&before) {
        deadline.check()?;
        // ★ A quad the footprint did not copy may be in G already, re-inserted by the update:
        // a store is a set, so that adds nothing, and counting it would make `+0` read `+1`.
        // With the whole graph copied, nothing new to the copy can be in G.
        if *footprint == Footprint::Whole || !store.contains(quad).map_err(storage)? {
            added.push(quad);
        }
    }
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

/// Step 1 of [`scoped_update`]: copy `footprint`'s quads of `graph` into `scratch`, and
/// answer the set copied — the "before" that step 4 diffs against.
fn copy(
    store: &Store,
    graph: &NamedNode,
    footprint: &Footprint,
    scratch: &Store,
    deadline: &Deadline,
) -> Result<HashSet<Quad>> {
    let in_graph = Some(graph.as_ref().into());
    let mut copied = HashSet::new();
    let mut take = |quads: oxigraph::store::QuadIter<'_>| -> Result<()> {
        for quad in quads {
            deadline.check()?;
            let quad = quad.map_err(storage)?;
            if !copied.contains(&quad) {
                scratch.insert(&quad).map_err(storage)?;
                copied.insert(quad);
            }
        }
        Ok(())
    };
    match footprint {
        Footprint::Whole => take(store.quads_for_pattern(None, None, None, in_graph))?,
        Footprint::Matching(probes) => {
            for probe in probes {
                take(store.quads_for_pattern(
                    probe.subject.as_ref().map(NamedOrBlankNode::as_ref),
                    probe.predicate.as_ref().map(NamedNode::as_ref),
                    probe.object.as_ref().map(Term::as_ref),
                    in_graph,
                ))?;
            }
        }
    }
    Ok(copied)
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

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::io::RdfFormat;
    use oxigraph::model::Literal;

    const G: &str = "urn:x:g";

    /// Two graphs and a default graph, with something for every pattern below to find —
    /// and the same predicates in the graphs a scoped update must not see.
    const DATA: &str = r#"
        <urn:x:s1> <urn:x:p> <urn:x:o1> .
        <urn:x:d1> <urn:x:n> 8 .
        <urn:x:g> {
            <urn:x:s1> <urn:x:p> <urn:x:s2> .
            <urn:x:s2> <urn:x:p> <urn:x:s3> .
            <urn:x:s3> <urn:x:q> <urn:x:s4> .
            <urn:x:s1> <urn:x:n> 1 .
            <urn:x:s2> <urn:x:n> 2 .
            <urn:x:s3> <urn:x:n> 3 .
            <urn:x:s1> <urn:x:type> <urn:x:T> .
            <urn:x:s2> <urn:x:type> <urn:x:T> .
            <urn:x:s1> <urn:x:label> "one" .
            <urn:x:counter> <urn:x:last> 3 .
            <urn:x:s4> <urn:x:key> "k4" .
        }
        <urn:x:other> {
            <urn:x:s1> <urn:x:p> <urn:x:o1> .
            <urn:x:o1> <urn:x:n> 9 .
            <urn:x:s9> <urn:x:key> "k9" .
        }
    "#;

    fn store() -> Store {
        let store = Store::new().unwrap();
        store.load_from_slice(RdfFormat::TriG, DATA).unwrap();
        // A blank node by a FIXED label: a parser mints a fresh one per load, and the two
        // stores compared below must start identical.
        store
            .insert(&Quad::new(
                oxigraph::model::BlankNode::new("b1").unwrap(),
                NamedNode::new("urn:x:p").unwrap(),
                NamedNode::new("urn:x:s1").unwrap(),
                graph(),
            ))
            .unwrap();
        store
    }

    fn graph() -> NamedNode {
        NamedNode::new(G).unwrap()
    }

    fn parse_one(update: &str) -> Parsed {
        parse(update, &Deadline::never(), Unreadable::None)
            .unwrap_or_else(|e| panic!("`{update}` does not parse: {e}"))
    }

    /// Run `update` on `store` through the scoped door's mechanism, copying `footprint`.
    fn run(store: &Store, update: &str, whole: bool) -> String {
        let parsed = parse_one(update);
        let footprint = if whole {
            Footprint::Whole
        } else {
            parsed.footprint
        };
        match scoped_update(
            store,
            &graph(),
            parsed.prepared,
            &footprint,
            &Deadline::never(),
        ) {
            Ok(applied) => format!("+{} -{}", applied.added, applied.removed),
            Err(e) => format!("refused: {e}"),
        }
    }

    fn dump(store: &Store) -> Vec<String> {
        let mut quads: Vec<String> = store.iter().map(|q| q.unwrap().to_string()).collect();
        quads.sort();
        quads
    }

    /// Each update, and whether its footprint is the whole graph.
    ///
    /// ★ Every shape the walk distinguishes is here: each kind of path, `EXISTS` in a filter
    /// and in a `BIND`, `MINUS`, `OPTIONAL`, aggregates, sub-selects, `VALUES`, a blank node
    /// and a literal as a pattern's subject, `GRAPH ?g`, `USING`, `WITH`, the escapes (which
    /// must be refused identically), and `;` chains whose second operation reads what the
    /// first wrote or removes what it re-inserted.
    const CORPUS: &[(&str, bool)] = &[
        ("INSERT DATA { GRAPH <urn:x:g> { <urn:x:s5> <urn:x:p> <urn:x:s1> } }", false),
        // Already there: `+0`, though the footprint copied nothing to say so.
        ("INSERT DATA { GRAPH <urn:x:g> { <urn:x:s1> <urn:x:p> <urn:x:s2> } }", false),
        ("DELETE DATA { GRAPH <urn:x:g> { <urn:x:s1> <urn:x:p> <urn:x:s2> } }", false),
        ("DELETE DATA { GRAPH <urn:x:g> { <urn:x:s1> <urn:x:p> <urn:x:s9> } }", false),
        ("DELETE DATA { GRAPH <urn:x:other> { <urn:x:s1> <urn:x:p> <urn:x:o1> } }", false),
        // `ikigai-ledger`'s keyed append, with the key free and then taken.
        (
            "DELETE { GRAPH <urn:x:g> { <urn:x:counter> <urn:x:last> ?old } }
             INSERT { GRAPH <urn:x:g> { <urn:x:counter> <urn:x:last> ?new .
                                         <urn:x:s6> <urn:x:n> ?new ; <urn:x:key> \"k6\" } }
             WHERE {
               { { SELECT ?last WHERE { GRAPH <urn:x:g> { <urn:x:counter> <urn:x:last> ?last } }
                   ORDER BY DESC(?last) LIMIT 1 }
                 UNION
                 { BIND(0 AS ?last)
                   FILTER NOT EXISTS { GRAPH <urn:x:g> { <urn:x:counter> <urn:x:last> ?any } } } }
               OPTIONAL { GRAPH <urn:x:g> { <urn:x:counter> <urn:x:last> ?old } }
               BIND(?last + 1 AS ?new)
               FILTER NOT EXISTS { GRAPH <urn:x:g> { ?taken <urn:x:key> \"k6\" } }
             }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { <urn:x:s7> <urn:x:n> 7 } }
             WHERE { FILTER NOT EXISTS { GRAPH <urn:x:g> { ?taken <urn:x:key> \"k4\" } } }",
            false,
        ),
        (
            "DELETE { GRAPH <urn:x:g> { ?s <urn:x:n> ?v } } INSERT { GRAPH <urn:x:g> { ?s <urn:x:n> ?w } }
             WHERE { GRAPH <urn:x:g> { ?s <urn:x:n> ?v } BIND(?v + 10 AS ?w) }",
            false,
        ),
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:reach> ?o } } WHERE { GRAPH <urn:x:g> { ?s <urn:x:p>+ ?o } }", false),
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:reach> ?o } } WHERE { GRAPH <urn:x:g> { ?s <urn:x:p>/<urn:x:q> ?o } }", false),
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:reach> ?o } } WHERE { GRAPH <urn:x:g> { ?s (<urn:x:p>|<urn:x:q>) ?o } }", false),
        ("INSERT { GRAPH <urn:x:g> { ?o <urn:x:rev> ?s } } WHERE { GRAPH <urn:x:g> { ?s ^<urn:x:p> ?o } }", false),
        // Zero-length and negated paths see the graph as a whole.
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:reach> ?o } } WHERE { GRAPH <urn:x:g> { ?s <urn:x:p>* ?o } }", true),
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:reach> ?o } } WHERE { GRAPH <urn:x:g> { ?s <urn:x:p>? ?o } }", true),
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:np> ?o } } WHERE { GRAPH <urn:x:g> { ?s !<urn:x:p> ?o } }", true),
        (
            "INSERT { GRAPH <urn:x:g> { <urn:x:s1> <urn:x:count> ?c } }
             WHERE { SELECT (COUNT(*) AS ?c) WHERE { GRAPH <urn:x:g> { ?s <urn:x:type> <urn:x:T> } } }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { <urn:x:sum> <urn:x:is> ?t } }
             WHERE { SELECT (SUM(?v) AS ?t) WHERE { GRAPH <urn:x:g> { ?s <urn:x:n> ?v } } }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { <urn:x:max> <urn:x:is> ?s } }
             WHERE { SELECT ?s WHERE { GRAPH <urn:x:g> { ?s <urn:x:n> ?v } } ORDER BY DESC(?v) LIMIT 1 }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { ?s <urn:x:untyped> true } }
             WHERE { GRAPH <urn:x:g> { ?s <urn:x:n> ?v FILTER NOT EXISTS { ?s <urn:x:type> <urn:x:T> } } }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { ?s <urn:x:minus> true } }
             WHERE { GRAPH <urn:x:g> { ?s <urn:x:n> ?v MINUS { ?s <urn:x:type> <urn:x:T> } } }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { ?s <urn:x:opt> ?l } }
             WHERE { GRAPH <urn:x:g> { ?s <urn:x:type> <urn:x:T> OPTIONAL { ?s <urn:x:label> ?l } } }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { <urn:x:s1> <urn:x:exists> ?e } }
             WHERE { BIND(EXISTS { GRAPH <urn:x:g> { <urn:x:s3> <urn:x:q> <urn:x:s4> } } AS ?e) }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { ?s <urn:x:vals> ?v } }
             WHERE { VALUES ?s { <urn:x:s1> <urn:x:s2> } GRAPH <urn:x:g> { ?s <urn:x:n> ?v } }",
            false,
        ),
        // A blank node in a pattern is a variable; a literal subject matches nothing.
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:blank> true } } WHERE { GRAPH <urn:x:g> { _:x <urn:x:p> ?s } }", false),
        ("INSERT { GRAPH <urn:x:g> { <urn:x:lit> <urn:x:is> true } } WHERE { GRAPH <urn:x:g> { \"one\" <urn:x:p> ?o } }", false),
        // `GRAPH ?g { }` reads the graph's registration, which is always copied.
        ("INSERT { GRAPH <urn:x:g> { <urn:x:graphs> <urn:x:has> ?g } } WHERE { GRAPH ?g { } }", false),
        // A quad only a `DELETE` template names, never the `WHERE`.
        (
            "DELETE { GRAPH <urn:x:g> { ?s <urn:x:label> \"one\" } }
             WHERE { GRAPH <urn:x:g> { ?s <urn:x:type> <urn:x:T> } }",
            false,
        ),
        ("DELETE WHERE { GRAPH <urn:x:g> { <urn:x:s1> ?p ?o } }", false),
        ("DELETE WHERE { GRAPH ?g { ?s <urn:x:p> ?o } }", false),
        ("DELETE WHERE { GRAPH ?g { ?s ?p ?o } }", true),
        (
            "INSERT { GRAPH ?g { ?s <urn:x:copy> ?o } } WHERE { GRAPH ?g { ?s <urn:x:q> ?o } }",
            false,
        ),
        // `USING <G>` makes G the default graph; `WITH <G>` names it for both halves.
        (
            "INSERT { GRAPH <urn:x:g> { ?s <urn:x:via> ?o } } USING <urn:x:g> WHERE { ?s <urn:x:p> ?o }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { ?s <urn:x:via> ?o } } USING NAMED <urn:x:g> WHERE { GRAPH ?h { ?s <urn:x:q> ?o } }",
            false,
        ),
        (
            "WITH <urn:x:g> DELETE { ?s <urn:x:label> ?l } INSERT { ?s <urn:x:label> \"renamed\" } WHERE { ?s <urn:x:label> ?l }",
            false,
        ),
        // Reading another graph, or the default graph, matches nothing.
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:leak> ?o } } WHERE { GRAPH <urn:x:other> { ?s <urn:x:p> ?o } }", false),
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:leak> ?o } } WHERE { ?s <urn:x:p> ?o }", false),
        ("INSERT { GRAPH <urn:x:g> { ?s <urn:x:leak> ?o } } USING <urn:x:other> WHERE { ?s <urn:x:p> ?o }", false),
        // The escapes, refused the same way whatever was copied.
        ("WITH <urn:x:other> INSERT { <urn:x:x> <urn:x:p> \"no\" } WHERE { }", false),
        ("INSERT { GRAPH <urn:x:other> { ?s <urn:x:leak> ?o } } WHERE { GRAPH <urn:x:g> { ?s <urn:x:q> ?o } }", false),
        ("INSERT { ?s <urn:x:leak> ?o } WHERE { GRAPH <urn:x:g> { ?s <urn:x:q> ?o } }", false),
        ("INSERT { GRAPH ?o { ?s <urn:x:leak> true } } WHERE { GRAPH <urn:x:g> { ?s <urn:x:q> ?o } }", false),
        ("INSERT DATA { GRAPH <urn:x:other> { <urn:x:x> <urn:x:p> \"no\" } }", false),
        ("CREATE GRAPH <urn:x:other>", false),
        ("CREATE GRAPH <urn:x:g>", false),
        ("COPY <urn:x:g> TO <urn:x:other>", true),
        ("ADD <urn:x:other> TO <urn:x:g>", true),
        ("CLEAR GRAPH <urn:x:g>", true),
        ("DROP ALL", true),
        // `;` chains: the second operation reads what the first wrote, or removes a quad the
        // first re-inserted.
        (
            "INSERT DATA { GRAPH <urn:x:g> { <urn:x:s8> <urn:x:p> <urn:x:s1> } } ;
             INSERT { GRAPH <urn:x:g> { ?s <urn:x:saw> ?o } } WHERE { GRAPH <urn:x:g> { ?s <urn:x:p> ?o } }",
            false,
        ),
        (
            "INSERT DATA { GRAPH <urn:x:g> { <urn:x:s1> <urn:x:label> \"one\" } } ;
             DELETE DATA { GRAPH <urn:x:g> { <urn:x:s1> <urn:x:label> \"one\" } }",
            false,
        ),
        (
            "INSERT DATA { GRAPH <urn:x:g> { <urn:x:s1> <urn:x:label> \"one\" } } ;
             DELETE WHERE { GRAPH <urn:x:g> { ?s <urn:x:label> ?l } }",
            false,
        ),
    ];

    /// ★ FIDELITY (ledger #1132): for every update in the corpus, copying only its footprint
    /// gives the same answer — the same counts, or the same refusal — and leaves the store in
    /// the same state as copying the whole graph did. The boundary itself does not rest on
    /// this (the private store never holds anything outside `G`); the update's meaning does.
    #[test]
    fn the_footprint_computes_what_the_whole_graph_computes() {
        let mut narrowed = 0;
        for (update, whole) in CORPUS {
            let footprint = parse_one(update).footprint;
            assert_eq!(
                footprint == Footprint::Whole,
                *whole,
                "`{update}`: footprint {footprint:?}"
            );
            if !whole {
                narrowed += 1;
            }
            let (by_whole, by_footprint) = (store(), store());
            let answer_whole = run(&by_whole, update, true);
            let answer_footprint = run(&by_footprint, update, false);
            assert_eq!(
                answer_footprint, answer_whole,
                "`{update}`: the answers differ"
            );
            assert_eq!(
                dump(&by_footprint),
                dump(&by_whole),
                "`{update}` ({answer_whole}): the stores differ afterwards"
            );
        }
        assert!(
            narrowed >= 40,
            "only {narrowed} updates exercised a footprint"
        );
    }

    /// ★ THE MECHANISM (ledger #1132): a ledger append copies the counter, whatever the ledger
    /// holds — not the ledger. `tests/scoped_append_cost.rs` measures what this saves.
    #[test]
    fn an_append_copies_its_footprint_and_not_the_graph() {
        let store = Store::new().unwrap();
        let g = graph();
        let node = |local: String| NamedNode::new(format!("urn:x:{local}")).unwrap();
        for i in 0..1_000 {
            for (p, o) in [
                ("type", Term::from(node("Item".to_string()))),
                ("n", Literal::from(i).into()),
                (
                    "key",
                    Literal::new_simple_literal(format!("key-{i}")).into(),
                ),
            ] {
                store
                    .insert(&Quad::new(
                        node(format!("item:{i}")),
                        node(p.to_string()),
                        o,
                        g.clone(),
                    ))
                    .unwrap();
            }
        }
        store
            .insert(&Quad::new(
                node("counter".to_string()),
                node("last".to_string()),
                Literal::from(1_000),
                g.clone(),
            ))
            .unwrap();
        let append = CORPUS[5].0;
        let parsed = parse_one(append);
        let scratch = Store::new().unwrap();
        let copied = copy(&store, &g, &parsed.footprint, &scratch, &Deadline::never()).unwrap();
        assert_eq!(
            copied.len(),
            1,
            "the append copied {} of the graph's {} quads",
            copied.len(),
            store.len().unwrap()
        );
        let applied = scoped_update(
            &store,
            &g,
            parsed.prepared,
            &parsed.footprint,
            &Deadline::never(),
        )
        .unwrap();
        // The counter's new value, the item's number and key in; the old value out.
        assert_eq!((applied.added, applied.removed), (3, 1));
        assert!(store
            .contains(&Quad::new(
                node("counter".to_string()),
                node("last".to_string()),
                Literal::from(1_001),
                g.clone(),
            ))
            .unwrap());
    }
}
