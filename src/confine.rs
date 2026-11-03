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
//! | `WITH <other> INSERT … WHERE …` | **refused** | reads another graph (ledger #1606); with `USING <G>`, writes it |
//! | `USING <other>`, `USING NAMED <other>`, `WITH <other> DELETE …` | **refused** by name, before evaluation | reads another graph, so it would match nothing (ledger #1606, [`reads_outside`]) |
//! | `… WHERE { GRAPH <other> { … } }`, `COPY`/`ADD <other> TO <G>` | **refused** by name, before evaluation | same: a constant graph other than `G` anywhere in a `WHERE`, `EXISTS` included |
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
//! ★ **And an update that NAMES another graph to read is refused** (ledger #1606), the way a
//! scoped read refuses `GRAPH <other>` (ledger #388). Until then `WITH <other>`, `USING
//! <other>`, `USING NAMED <other>` and `GRAPH <other>` in a `WHERE` were applied against the
//! private copy and answered `+0 -0`, or worse: `FILTER NOT EXISTS` over the other graph held
//! for every row, and `COPY <other> TO <G>` emptied `G`. [`reads_outside`] refuses each by
//! name, on the text, before anything is evaluated. Only constant names: `GRAPH ?g` still
//! binds `G` alone, and a pattern outside any `GRAPH` block still reads the private copy's
//! empty default graph, unrefused.
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
//! - ★ **A `VALUES` block lists constants** (ledger #2940). `VALUES ?s { <a> <b> } ?s ?p ?o`
//!   has no constant in its pattern, but the join keeps only the matches whose `?s` is `<a>` or
//!   `<b>`, so it copies the quads of `<a>` and `<b>`: one pattern for any number of subjects,
//!   where a `UNION` branch per subject spends the algebra budget (`ikigai-ledger` deletes an
//!   item's dependents this way). The same list narrows a `DELETE` template whose variable the
//!   `WHERE` can only bind to a listed IRI, if at all. It narrows only what it binds for
//!   CERTAIN: a pattern in the same group (or joined to it) as a `VALUES` whose every row binds
//!   the variable to an IRI. An `UNDEF` row, a literal in the list, a `VALUES` outside the
//!   `GRAPH` block or in another branch, an `OPTIONAL`, a `MINUS` or an `EXISTS` leave the
//!   pattern as it was, and a pattern with no constant then copies the whole graph. A property
//!   test pins it over random graphs
//!   (`an_update_over_its_footprint_computes_what_it_computes_over_the_graph`).
//!
//! ⚠ What the footprint cannot preserve is an order SPARQL leaves undefined: a `LIMIT` with no
//! `ORDER BY` picks among its solutions by the private store's insertion order, which a
//! smaller copy can change. That was never the same as the broad door's choice either.
//!
//! The unscoped `urn:iki:store:update` is unchanged and copies nothing.
//!
//! ★ **And the copy is bounded** (ledger #146). A copy of the whole graph is held three times
//! (the private store, the set step 4 diffs against, the set the update leaves) at about 2 KB
//! a quad, so a host pointing this door at a large graph used to get a silent cliff: 3.7 s and
//! half a gigabyte at 250,000 quads, 40 s and 5 GB at 3,000,000. Past
//! [`DurableStore::max_scoped_copy_quads`](crate::DurableStore::max_scoped_copy_quads)
//! ([`crate::limits::MAX_SCOPED_COPY_QUADS`] unless the host sets another) the copy stops and
//! the update is refused with nothing applied, naming the broad door, which copies nothing.
//! ⚠ The bound applies only to a caller who may READ `G`: for any other caller, refused-or-not
//! would say whether `G` holds more than the bound, which is the one-bit oracle ledger #761
//! closed for `DROP`/`CLEAR`/`CREATE`. Such a caller's whole-graph updates are exactly `CLEAR`
//! and `DROP` (anything with a `WHERE` needs the read grant), and the time budget is what
//! bounds those: the copy checks it between quads.
//!
//! Blank-node labels survive the round trip (oxigraph stores them by label), but this
//! ecosystem skolemizes and a blank node in a scoped graph is outside what these tests
//! pin.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use ikigai_core::{Error, Result};
use oxigraph::model::{GraphName, NamedNode, NamedOrBlankNode, Quad, Term};
use spargebra::algebra::{
    AggregateExpression, Expression, GraphPattern, OrderExpression, PropertyPathExpression,
};
use spargebra::term::{
    GraphNamePattern, GroundTerm, GroundTermPattern, NamedNodePattern, TermPattern, Variable,
};
use spargebra::{GraphUpdateOperation, Update};

use crate::budget::Deadline;
use oxigraph::sparql::PreparedSparqlUpdate;
use oxigraph::store::{Store, Transaction};

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
    let inserts = insert_probes(&parsed);
    let reads = graphs_read(&parsed);
    Ok(Parsed {
        prepared: crate::service::evaluator()
            .with_cancellation_token(deadline.token())
            .for_update(parsed),
        footprint,
        inserts,
        reads,
    })
}

/// A parsed, bounded update, what of a scoped graph it can observe, where its `INSERT`
/// templates can put a triple term, and which graphs it names to read.
pub(crate) struct Parsed {
    pub prepared: PreparedSparqlUpdate,
    pub footprint: Footprint,
    pub inserts: Vec<InsertProbe>,
    pub reads: GraphsRead,
}

/// Where an update names a graph it READS (ledger #1606), in the order a refusal lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReadClause {
    /// `WITH <g>` or `USING <g>`: the graph a `WHERE` reads as its default graph. The parser
    /// folds `WITH` into the dataset when there is no `USING`, so the two cannot be told apart.
    Dataset,
    /// `USING NAMED <g>`.
    Named,
    /// `GRAPH <g> { … }` in a `WHERE` (an `EXISTS` included), or the source of `COPY`, `MOVE`
    /// or `ADD`, which the parser rewrites into one.
    Block,
}

impl ReadClause {
    fn describe(self) -> &'static str {
        match self {
            ReadClause::Dataset => "`WITH` or `USING`",
            ReadClause::Named => "`USING NAMED`",
            ReadClause::Block => "a `GRAPH` block, or the source of `COPY`, `MOVE` or `ADD`",
        }
    }
}

/// Every graph an update names by a constant IRI to READ, and where it names it.
pub(crate) type GraphsRead = BTreeMap<NamedNode, BTreeSet<ReadClause>>;

/// The [`GraphsRead`] of a parsed update: its dataset clauses (`WITH`, `USING`, `USING NAMED`)
/// and the constant `GRAPH <iri>` names in every `WHERE` (ledger #1606).
///
/// ★ Only CONSTANT names. `GRAPH ?g` names nothing until evaluation, and the private copy is
/// what keeps it inside the scoped graph. A pattern outside any `GRAPH` block reads the
/// default graph, which has no IRI, so it is not here either.
///
/// What an update WRITES is not here: `INSERT` and `DELETE` templates are checked on effects
/// by [`scoped_update`], and `WITH` reaches a template only as its graph name.
pub(crate) fn graphs_read(update: &Update) -> GraphsRead {
    let mut reads = GraphsRead::new();
    for operation in &update.operations {
        let GraphUpdateOperation::DeleteInsert { using, pattern, .. } = operation else {
            continue;
        };
        if let Some(dataset) = using {
            for graph in &dataset.default {
                reads
                    .entry(graph.clone())
                    .or_default()
                    .insert(ReadClause::Dataset);
            }
            for graph in dataset.named.iter().flatten() {
                reads
                    .entry(graph.clone())
                    .or_default()
                    .insert(ReadClause::Named);
            }
        }
        let mut blocks = BTreeSet::new();
        crate::scope::pattern_graphs(pattern, &mut blocks);
        for graph in blocks {
            reads.entry(graph).or_default().insert(ReadClause::Block);
        }
    }
    reads
}

/// Refuse a scoped update that names a graph other than `graph` to read (ledger #1606).
///
/// ★ **The boundary does not depend on this; telling the caller does.** The private copy
/// holds only `graph`, so a `WHERE` reading another graph already matched nothing — and the
/// update answered `+0 -0`, or worse: `FILTER NOT EXISTS` over the other graph held for every
/// row, and `COPY <other> TO <graph>` emptied `graph`. Each looked like success. This is the
/// write side of the read doors' refusal (ledger #388, [`crate::scope::graphs_outside`]):
/// refused by name before anything is evaluated, decided on the update's TEXT and `graph=`,
/// never on the data.
///
/// `InvalidArgument` on `content`, not `Denied`, for the reason ledger #388 gives: a grant
/// on the other graph would not change the answer, because this door reads `graph=` alone.
pub(crate) fn reads_outside(reads: &GraphsRead, graph: &NamedNode) -> Result<()> {
    let outside: Vec<String> = reads
        .iter()
        .filter(|(name, _)| *name != graph)
        .map(|(name, clauses)| {
            let places: Vec<&str> = clauses.iter().map(|c| c.describe()).collect();
            format!("<{}> (in {})", name.as_str(), places.join("; in "))
        })
        .collect();
    if outside.is_empty() {
        return Ok(());
    }
    let g = graph.as_str();
    Err(Error::InvalidArgument {
        name: "content".to_string(),
        detail: format!(
            "this update reads {}, which {} not this write's graph, `graph={g}`. \
             `urn:iki:store:graph-update` evaluates an update against a private copy of that \
             one graph and nothing else, so {} would match nothing; the update is refused \
             rather than applied, because `+0 -0` from a mistyped or forgotten graph looks \
             exactly like an update with nothing to do. Nothing was applied: the update was \
             not evaluated. Correct the IRI, or, \
             holding `urn:cap:store:write` and `urn:cap:store:read`, use \
             `urn:iki:store:update`, which reads the whole dataset",
            outside.join(", "),
            if outside.len() == 1 { "is" } else { "are" },
            if outside.len() == 1 {
                "reading it"
            } else {
                "reading them"
            },
        ),
    })
}

/// One `INSERT` template that can put a triple term in the store, by its constants: every quad
/// it adds matches each one given (ledger #1333).
///
/// A template whose object is an IRI, a literal or a blank node adds no triple term, and
/// `INSERT DATA` adds only what its text spells, which [`crate::limits::check_sparql`] has
/// already bounded, so neither has a probe. What is left is a template whose object is a
/// variable (bound, perhaps, to a stored term the `WHERE` wrapped in `TRIPLE(…)`) or a
/// `<<( … )>>` pattern around one.
#[derive(Debug, PartialEq)]
pub(crate) struct InsertProbe {
    subject: Option<NamedOrBlankNode>,
    predicate: Option<NamedNode>,
    /// `None` for a `GRAPH ?g` template. `WITH` is already in the template as its graph
    /// (spargebra injects it), so the default graph here is the default graph.
    graph: Option<GraphName>,
}

/// The [`InsertProbe`]s of a parsed update.
pub(crate) fn insert_probes(update: &Update) -> Vec<InsertProbe> {
    let mut probes = Vec::new();
    for operation in &update.operations {
        let GraphUpdateOperation::DeleteInsert { insert, .. } = operation else {
            continue;
        };
        for template in insert {
            if matches!(
                template.object,
                TermPattern::NamedNode(_) | TermPattern::Literal(_) | TermPattern::BlankNode(_)
            ) {
                continue;
            }
            let probe = InsertProbe {
                subject: term_pattern(&template.subject).and_then(subject),
                predicate: named_node(&template.predicate),
                graph: match &template.graph_name {
                    GraphNamePattern::NamedNode(node) => Some(node.clone().into()),
                    GraphNamePattern::DefaultGraph => Some(GraphName::DefaultGraph),
                    GraphNamePattern::Variable(_) => None,
                },
            };
            if !probes.contains(&probe) {
                probes.push(probe);
            }
        }
    }
    probes
}

/// Refuse an update, before it commits, that has left a quad in `transaction` whose object
/// nests triple terms deeper than [`crate::depth::MAX_RDF_NESTING`] and that `store` did not
/// already hold (ledger #1333).
///
/// Reads, as the transaction sees them, the quads each of `probes` matches: every quad the
/// update's templates added is among them. A quad the store held before this update is not
/// this update's doing, and a store written before this check may hold one; it is passed
/// over (the read that decoded it already survived it).
pub(crate) fn check_inserted(
    transaction: &Transaction<'_>,
    store: &Store,
    probes: &[InsertProbe],
    deadline: &Deadline,
) -> Result<()> {
    for probe in probes {
        let quads = transaction.quads_for_pattern(
            probe.subject.as_ref().map(NamedOrBlankNode::as_ref),
            probe.predicate.as_ref().map(NamedNode::as_ref),
            None,
            probe.graph.as_ref().map(GraphName::as_ref),
        );
        for quad in quads {
            deadline.check()?;
            let quad = quad.map_err(storage)?;
            if crate::depth::nests_deeper_than_the_bound(quad.object.as_ref())
                && !store.contains(&quad).map_err(storage)?
            {
                return Err(crate::depth::grown_too_deep("content"));
            }
        }
    }
    Ok(())
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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
/// `DELETE` template and every `DELETE DATA` quad, by their constants, and by the IRIs a
/// `VALUES` block lists for one of their variables (ledger #2940, [`bindings`]).
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
                // ★ A template variable the `WHERE` confines to listed IRIs (ledger #2940) can
                // only be instantiated as one of them: an unbound one instantiates nothing.
                let listed = if delete.is_empty() {
                    Listed::new()
                } else {
                    bindings(pattern).confined
                };
                for template in delete {
                    push_listed(
                        Probe {
                            subject: ground_term_pattern(&template.subject).and_then(subject),
                            predicate: named_node(&template.predicate),
                            object: ground_term_pattern(&template.object),
                        },
                        ground_variable(&template.subject),
                        ground_variable(&template.object),
                        &listed,
                        &mut probes,
                    );
                }
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
    // A listed set is probed once per pattern that names it, and a ledger names the same set in
    // its `WHERE` and its template: each distinct probe is read once.
    let mut seen = HashSet::new();
    probes.retain(|probe| seen.insert(probe.clone()));
    Footprint::Matching(probes)
}

/// For each variable, the IRIs a `VALUES` block lists for it (ledger #2940).
type Listed = BTreeMap<Variable, BTreeSet<NamedNode>>;

/// What the `VALUES` blocks in a pattern say about its solutions (ledger #2940).
///
/// ★ Both halves are claims about EVERY solution, so each rule below may only drop an entry
/// it cannot prove, never invent one: a missing entry copies more, a wrong one copies less.
#[derive(Default)]
struct Bindings {
    /// Every solution binds the variable to one of these IRIs: what narrows a triple pattern
    /// joined to this one, whose matches bind the variable too.
    certain: Listed,
    /// Every solution binds it to one of these or leaves it unbound: what narrows a `DELETE`
    /// template, which an unbound variable leaves uninstantiated.
    confined: Listed,
}

/// The [`Bindings`] of `pattern`.
///
/// Only a `VALUES` block whose every row binds the variable to an IRI lists it: an `UNDEF` row
/// joins with any value, and a literal is not a subject. Everything this does not reason about
/// (`GROUP`, `LATERAL`, `SERVICE`, a pattern or path) lists nothing.
fn bindings(pattern: &GraphPattern) -> Bindings {
    match pattern {
        GraphPattern::Values {
            variables,
            bindings,
        } => {
            let mut listed = Listed::new();
            for (i, variable) in variables.iter().enumerate() {
                let iris: Option<BTreeSet<NamedNode>> = bindings
                    .iter()
                    .map(|row| match row.get(i) {
                        Some(Some(GroundTerm::NamedNode(node))) => Some(node.clone()),
                        _ => None,
                    })
                    .collect();
                if let Some(iris) = iris {
                    listed.insert(variable.clone(), iris);
                }
            }
            Bindings {
                certain: listed.clone(),
                confined: listed,
            }
        }
        // A joined solution binds a variable when either side does, to the value both agree
        // on: so certain on either side is certain, within both sides' lists.
        GraphPattern::Join { left, right } => {
            let (l, r) = (bindings(left), bindings(right));
            let mut certain = l.certain.clone();
            for (variable, iris) in &r.certain {
                certain
                    .entry(variable.clone())
                    .and_modify(|mine| *mine = mine.intersection(iris).cloned().collect())
                    .or_insert_with(|| iris.clone());
            }
            let mut confined = either(left, &l, right, &r);
            confined.extend(certain.clone());
            Bindings { certain, confined }
        }
        // Every solution extends one of the left's, so what the left certainly binds stays.
        GraphPattern::LeftJoin { left, right, .. } => {
            let (l, r) = (bindings(left), bindings(right));
            let mut confined = either(left, &l, right, &r);
            confined.extend(l.certain.clone());
            Bindings {
                certain: l.certain,
                confined,
            }
        }
        GraphPattern::Union { left, right } => {
            let (l, r) = (bindings(left), bindings(right));
            let mut certain = Listed::new();
            for (variable, iris) in &l.certain {
                if let Some(theirs) = r.certain.get(variable) {
                    certain.insert(variable.clone(), iris.union(theirs).cloned().collect());
                }
            }
            Bindings {
                certain,
                confined: either(left, &l, right, &r),
            }
        }
        // Solutions of the left only, some removed.
        GraphPattern::Minus { left, .. } => bindings(left),
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Graph { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. } => bindings(inner),
        GraphPattern::Project { inner, variables } => {
            let mut inner = bindings(inner);
            inner.certain.retain(|v, _| variables.contains(v));
            inner.confined.retain(|v, _| variables.contains(v));
            inner
        }
        // `BIND` gives its variable any value at all.
        GraphPattern::Extend {
            inner, variable, ..
        } => {
            let mut inner = bindings(inner);
            inner.certain.remove(variable);
            inner.confined.remove(variable);
            inner
        }
        GraphPattern::Bgp { .. }
        | GraphPattern::Path { .. }
        | GraphPattern::Group { .. }
        | GraphPattern::Lateral { .. }
        | GraphPattern::Service { .. } => Bindings::default(),
    }
}

/// The variables a solution of one of two patterns binds only to listed IRIs, if at all: those
/// each side confines, where a side that cannot bind the variable (it is not in scope there)
/// confines it to nothing.
fn either(left: &GraphPattern, l: &Bindings, right: &GraphPattern, r: &Bindings) -> Listed {
    let in_scope = |pattern: &GraphPattern| {
        let mut variables = BTreeSet::new();
        pattern.on_in_scope_variable(|v| {
            variables.insert(v.clone());
        });
        variables
    };
    let (left_scope, right_scope) = (in_scope(left), in_scope(right));
    let side = |b: &Bindings, scope: &BTreeSet<Variable>, variable: &Variable| match b
        .confined
        .get(variable)
    {
        Some(iris) => Some(iris.clone()),
        None if !scope.contains(variable) => Some(BTreeSet::new()),
        None => None,
    };
    let mut confined = Listed::new();
    for variable in l.confined.keys().chain(r.confined.keys()) {
        if let (Some(a), Some(b)) = (
            side(l, &left_scope, variable),
            side(r, &right_scope, variable),
        ) {
            confined.insert(variable.clone(), a.union(&b).cloned().collect());
        }
    }
    confined
}

/// Push `base`, or, when its subject (else its object) is a variable `listed` names and the
/// position is not already a constant, one probe per listed IRI in that position.
///
/// Only one position is narrowed: a probe with fewer constants is a superset, so that is never
/// less than the pattern can match, and narrowing both would multiply the probes.
fn push_listed(
    base: Probe,
    subject_variable: Option<&Variable>,
    object_variable: Option<&Variable>,
    listed: &Listed,
    probes: &mut Vec<Probe>,
) {
    if base.subject.is_none() {
        if let Some(iris) = subject_variable.and_then(|v| listed.get(v)) {
            probes.extend(iris.iter().map(|iri| Probe {
                subject: Some(iri.clone().into()),
                ..base.clone()
            }));
            return;
        }
        if base.object.is_none() {
            if let Some(iris) = object_variable.and_then(|v| listed.get(v)) {
                probes.extend(iris.iter().map(|iri| Probe {
                    object: Some(iri.clone().into()),
                    ..base.clone()
                }));
                return;
            }
        }
    }
    probes.push(base);
}

/// A `Join` operand's probes. ★ A basic graph pattern joined to a sibling that certainly binds
/// one of its variables to listed IRIs (a `VALUES` in the same group, ledger #2940) contributes
/// only matches binding it to one of them, so it is probed by those IRIs. Its own solutions
/// over the copy are then fewer than over the graph, but the JOIN's are the same: every match
/// the join keeps was copied, and a copy holds no quad the graph does not.
fn join_operand_probes(
    operand: &GraphPattern,
    sibling: &GraphPattern,
    probes: &mut Vec<Probe>,
) -> bool {
    if let GraphPattern::Bgp { patterns } = operand {
        let listed = bindings(sibling).certain;
        if !listed.is_empty() {
            for triple in patterns {
                push_listed(
                    Probe {
                        subject: term_pattern(&triple.subject).and_then(subject),
                        predicate: named_node(&triple.predicate),
                        object: term_pattern(&triple.object),
                    },
                    variable(&triple.subject),
                    variable(&triple.object),
                    &listed,
                    probes,
                );
            }
            return true;
        }
    }
    pattern_probes(operand, probes)
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
        GraphPattern::Join { left, right } => {
            join_operand_probes(left, right, probes) && join_operand_probes(right, left, probes)
        }
        GraphPattern::Lateral { left, right }
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

fn variable(term: &TermPattern) -> Option<&Variable> {
    match term {
        TermPattern::Variable(variable) => Some(variable),
        _ => None,
    }
}

fn ground_variable(term: &GroundTermPattern) -> Option<&Variable> {
    match term {
        GroundTermPattern::Variable(variable) => Some(variable),
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
///
/// ★ **And the copy is bounded in quads** (ledger #146): `max_copy` is the most quads step 1
/// may copy, and one more refuses the update with nothing applied ([`copy_too_large`]). It is
/// `None` for a caller who may not READ the graph, because a refusal that depends on how many
/// quads `G` holds would tell that caller something about `G` (the ledger #761 rule: whether
/// an update is refused says nothing about what `G` holds). The only updates such a caller can
/// send that copy the whole graph are `CLEAR` and `DROP` (every other one needs the read
/// grant), and those are bounded by the time budget alone. See the module docs.
pub(crate) fn scoped_update(
    store: &Store,
    graph: &NamedNode,
    update: PreparedSparqlUpdate,
    footprint: &Footprint,
    max_copy: Option<usize>,
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
    let before = copy(store, graph, footprint, max_copy, &scratch, deadline)?;

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
    // ★ No added quad may nest triple terms past the bound (ledger #1333): an update can wrap
    // a stored term in `TRIPLE(…)` and store it a few dozen levels deeper every request, until
    // a request that decodes it aborts the host. `added` is exactly what is new, so checking it
    // costs nothing for an update that builds no triple term.
    if added
        .iter()
        .any(|quad| crate::depth::nests_deeper_than_the_bound(quad.object.as_ref()))
    {
        return Err(crate::depth::grown_too_deep("content"));
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
/// answer the set copied — the "before" that step 4 diffs against. Refused, with nothing
/// written anywhere, at the first quad past `max_copy` (ledger #146), so what it holds never
/// exceeds the bound by more than that one quad.
fn copy(
    store: &Store,
    graph: &NamedNode,
    footprint: &Footprint,
    max_copy: Option<usize>,
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
                if let Some(bound) = max_copy.filter(|bound| copied.len() >= *bound) {
                    return Err(copy_too_large(graph, bound, footprint));
                }
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

/// The refusal of a scoped update whose footprint holds more than `bound` quads of `graph`
/// (ledger #146).
///
/// ★ `InvalidArgument` on `content`, the shape of the store's other size bounds: the update is
/// well-formed and the caller holds the grant, but this door evaluates against a private copy
/// and will not make one that large. It names what to do instead: narrow the update, or use the
/// broad door, which evaluates in place and copies nothing. It is reached only by a caller who
/// may read `graph` (see [`scoped_update`]), so naming the bound discloses nothing that caller
/// could not count for itself through `urn:iki:store:graph-info`.
fn copy_too_large(graph: &NamedNode, bound: usize, footprint: &Footprint) -> Error {
    let why = if *footprint == Footprint::Whole {
        "observes the graph as a whole (a `CLEAR` or `DROP`, a pattern with no IRI or literal \
         in it, or a path that can be zero-length or is negated), so the door copies every quad \
         of it"
    } else {
        "has patterns that match that many of the graph's quads"
    };
    Error::InvalidArgument {
        name: "content".to_string(),
        detail: format!(
            "this update {why}, and graph <{g}> holds more than {bound} of them: \
             `urn:iki:store:graph-update` evaluates an update against a private copy of what it \
             can observe and copies at most {bound} quads (the host's bound, ledger #146). \
             Nothing was applied. Name a subject, predicate or object in each pattern so the \
             update observes less, or, holding `urn:cap:store:write` (and `urn:cap:store:read` \
             for a `WHERE`), use `urn:iki:store:update`, which evaluates in place and copies \
             nothing. `urn:iki:store:graph-info graph={g}` says how many quads the graph holds",
            g = graph.as_str(),
        ),
    }
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
            None,
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
        let copied = copy(
            &store,
            &g,
            &parsed.footprint,
            None,
            &scratch,
            &Deadline::never(),
        )
        .unwrap();
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
            None,
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

    /// Each update that lists its subjects (or objects) in a `VALUES` block, and whether its
    /// footprint must be the whole graph (ledger #2940).
    ///
    /// ★ The narrowed rows are the shapes `ikigai-ledger` deletes and purges with: one pattern
    /// `?s ?p ?o` over a listed set, in a `WHERE` and in a `DELETE` template, alone and as one
    /// branch of a `UNION`. The `true` rows are every way the listed set fails to bind the
    /// pattern's variable for certain, each of which must stay whole: never under-copy.
    const VALUES_CORPUS: &[(&str, bool)] = &[
        // The listed subjects, read.
        (
            "INSERT { GRAPH <urn:x:g> { ?s <urn:x:seen> ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s1> <urn:x:s3> } ?s ?p ?o } }",
            false,
        ),
        // …and removed, by a template with no constant in it.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s1> <urn:x:s9> } ?s ?p ?o } }",
            false,
        ),
        // `VALUES` after the pattern, in the same group.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { ?s ?p ?o VALUES ?s { <urn:x:s2> } } }",
            false,
        ),
        // The listed objects.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?o { <urn:x:s2> <urn:x:s4> } ?s ?p ?o } }",
            false,
        ),
        // Two variables in one block.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES (?s ?o) { (<urn:x:s1> <urn:x:s2>) (<urn:x:s2> <urn:x:s3>) } ?s ?p ?o } }",
            false,
        ),
        // `ikigai-ledger`'s removal: the item's own quads, the edges pointing at it, and its
        // dependents listed once, each a branch of a `UNION`.
        (
            "DELETE { GRAPH <urn:x:g> { <urn:x:s1> ?p0 ?o0 . ?s1 ?p1 <urn:x:s1> . ?d ?dp ?do } }
             WHERE { GRAPH <urn:x:g> {
               { <urn:x:s1> ?p0 ?o0 }
               UNION { ?s1 ?p1 <urn:x:s1> FILTER(?p1 != <urn:x:type>) }
               UNION { VALUES ?d { <urn:x:s2> <urn:x:s3> } ?d ?dp ?do } } }",
            false,
        ),
        // `ikigai-ledger`'s read of the same set, one pattern binding `?s ?p ?o`.
        (
            "INSERT { GRAPH <urn:x:g> { ?s <urn:x:gone> ?p } }
             WHERE { GRAPH <urn:x:g> {
               { <urn:x:s1> ?p ?o BIND(<urn:x:s1> AS ?s) }
               UNION { VALUES ?s { <urn:x:s2> <urn:x:s3> } ?s ?p ?o } } }",
            false,
        ),
        // Narrowed under a filter, in a sub-select, beside an `OPTIONAL`, and in an `EXISTS`.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s1> <urn:x:s2> } ?s ?p ?o FILTER(?p != <urn:x:n>) } }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { ?s <urn:x:count> ?c } }
             WHERE { SELECT ?s (COUNT(*) AS ?c) WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s1> <urn:x:s2> } ?s ?p ?o } } GROUP BY ?s }",
            false,
        ),
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s1> <urn:x:s3> } ?s ?p ?o OPTIONAL { ?s <urn:x:label> ?l } } }",
            false,
        ),
        (
            "INSERT { GRAPH <urn:x:g> { <urn:x:s1> <urn:x:any> ?e } }
             WHERE { BIND(EXISTS { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s3> } ?s ?p ?o } } AS ?e) }",
            false,
        ),
        // A listed set that matches nothing.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:nobody> } ?s ?p ?o } }",
            false,
        ),
        // ── Stays whole. ──
        // An `UNDEF` row binds nothing, so the pattern is not confined to the list.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s1> UNDEF } ?s ?p ?o } }",
            true,
        ),
        // A literal in the list, for a subject.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s1> \"one\" } ?s ?p ?o } }",
            true,
        ),
        // A `VALUES` outside the `GRAPH` block's group.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { VALUES ?s { <urn:x:s1> } GRAPH <urn:x:g> { ?s ?p ?o } }",
            true,
        ),
        // A `VALUES` in another branch of a `UNION`.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { { VALUES ?s { <urn:x:s1> } } UNION { ?s ?p ?o } } }",
            true,
        ),
        // A `VALUES` inside an `OPTIONAL`, `MINUS` or `FILTER NOT EXISTS`, the pattern outside.
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { ?s ?p ?o OPTIONAL { VALUES ?s { <urn:x:s1> } } } }",
            true,
        ),
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { ?s ?p ?o MINUS { VALUES ?s { <urn:x:s1> } } } }",
            true,
        ),
        (
            "DELETE { GRAPH <urn:x:g> { ?s ?p ?o } }
             WHERE { GRAPH <urn:x:g> { ?s ?p ?o FILTER NOT EXISTS { VALUES ?s { <urn:x:s1> } } } }",
            true,
        ),
        // The pattern's subject is listed and its object reaches on: the second hop is free.
        (
            "DELETE { GRAPH <urn:x:g> { ?m ?q ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s1> } ?s <urn:x:p> ?m . ?m ?q ?o } }",
            true,
        ),
        // A template variable the list confines in one branch and a free pattern binds in another.
        (
            "DELETE { GRAPH <urn:x:g> { ?d ?p ?o } }
             WHERE { GRAPH <urn:x:g> { { VALUES ?d { <urn:x:s1> } ?d <urn:x:n> ?o } UNION { ?d <urn:x:type> ?o } } }",
            true,
        ),
        // A `BIND` over the listed variable's name.
        (
            "DELETE { GRAPH <urn:x:g> { ?t ?p ?o } }
             WHERE { GRAPH <urn:x:g> { VALUES ?s { <urn:x:s1> } ?s <urn:x:p> ?o BIND(?o AS ?t) ?t ?p ?x } }",
            true,
        ),
    ];

    /// ★ REPRODUCTION, then FIDELITY (ledger #2940): a `VALUES` in the same group as a pattern
    /// copies the quads of the IRIs it lists, and nothing else when the rest of the update is
    /// anchored too; every other shape copies the whole graph; and either way the update
    /// computes what it computes over the whole graph.
    #[test]
    fn a_values_block_names_the_quads_its_group_can_match() {
        for (update, whole) in VALUES_CORPUS {
            let footprint = parse_one(update).footprint;
            assert_eq!(
                footprint == Footprint::Whole,
                *whole,
                "`{update}`: footprint {footprint:?}"
            );
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
    }

    /// A graph of `items` items, each the subject of three quads, plus quads in another graph.
    fn items(items: usize) -> Store {
        let store = Store::new().unwrap();
        let g = graph();
        let other = NamedNode::new("urn:x:other").unwrap();
        for i in 0..items {
            let s = NamedNode::new(format!("urn:x:item:{i}")).unwrap();
            for (p, o) in [
                ("type", Term::from(NamedNode::new("urn:x:Item").unwrap())),
                ("n", Literal::from(i as i64).into()),
                (
                    "next",
                    NamedNode::new(format!("urn:x:item:{}", i + 1))
                        .unwrap()
                        .into(),
                ),
            ] {
                let p = NamedNode::new(format!("urn:x:{p}")).unwrap();
                store
                    .insert(&Quad::new(s.clone(), p.clone(), o.clone(), g.clone()))
                    .unwrap();
                store
                    .insert(&Quad::new(s.clone(), p, o, other.clone()))
                    .unwrap();
            }
        }
        store
    }

    /// ★ THE MECHANISM (ledger #2940): removing the quads of 1, 250 and 2,000 listed subjects by
    /// ONE pattern copies three quads per listed subject, under a copy bound the whole graph
    /// exceeds, and removes exactly what the whole-graph copy removes.
    ///
    /// One pattern for any number of subjects is what keeps an `ikigai-ledger` delete inside the
    /// store's algebra budget (`budget::MAX_ALGEBRA_NODES`): a `UNION` branch per subject costs
    /// about three operators each, and ran out past 250.
    #[test]
    fn a_listed_set_copies_its_own_quads_at_any_size() {
        const ITEMS: usize = 3_000;
        for listed in [1, 250, 2_000] {
            let list: Vec<String> = (0..listed)
                .map(|i| format!("<urn:x:item:{}>", i * ITEMS / listed))
                .collect();
            let update = format!(
                "DELETE {{ GRAPH <urn:x:g> {{ ?s ?p ?o }} }} \
                 WHERE {{ GRAPH <urn:x:g> {{ VALUES ?s {{ {} }} ?s ?p ?o }} }}",
                list.join(" ")
            );
            let parsed = parse_one(&update);
            assert!(
                matches!(parsed.footprint, Footprint::Matching(_)),
                "{listed} listed: the footprint is the whole graph"
            );
            let store = items(ITEMS);
            let copied = copy(
                &store,
                &graph(),
                &parsed.footprint,
                None,
                &Store::new().unwrap(),
                &Deadline::never(),
            )
            .unwrap();
            assert_eq!(copied.len(), 3 * listed, "{listed} listed");

            // A bound the graph's 9,000 quads exceed and the listed set's do not.
            let bound = Some(3 * listed);
            let applied = scoped_update(
                &store,
                &graph(),
                parsed.prepared,
                &parsed.footprint,
                bound,
                &Deadline::never(),
            )
            .unwrap_or_else(|e| panic!("{listed} listed: {e}"));
            assert_eq!((applied.added, applied.removed), (0, 3 * listed));

            let by_whole = items(ITEMS);
            assert_eq!(run(&by_whole, &update, true), format!("+0 -{}", 3 * listed));
            assert_eq!(dump(&store), dump(&by_whole), "{listed} listed");
        }
    }

    /// A small deterministic generator (xorshift64*), so the property test needs no dependency
    /// and a failure names the seed that reproduces it.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.next() % 100 < percent
        }
    }

    /// ★ PROPERTY (ledger #2940): over random small graphs and random updates whose patterns
    /// draw on a listed set, an update run over its footprint gives the same answer and leaves
    /// the same store as the same update run over the whole graph. The lists sometimes hold an
    /// `UNDEF` or a literal, so the fallback is exercised beside the narrowing.
    #[test]
    fn an_update_over_its_footprint_computes_what_it_computes_over_the_graph() {
        const CASES: u64 = 400;
        let node = |i: usize| format!("<urn:x:n{i}>");
        let mut narrowed = 0;
        for seed in 1..=CASES {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let store = Store::new().unwrap();
            let mut trig = String::new();
            for _ in 0..rng.below(30) + 5 {
                let s = node(rng.below(8));
                let p = format!("<urn:x:p{}>", rng.below(3));
                let o = if rng.chance(70) {
                    node(rng.below(8))
                } else {
                    format!("\"v{}\"", rng.below(4))
                };
                let g = if rng.chance(80) {
                    "urn:x:g"
                } else {
                    "urn:x:other"
                };
                trig.push_str(&format!("<{g}> {{ {s} {p} {o} }}\n"));
            }
            store.load_from_slice(RdfFormat::TriG, &trig).unwrap();

            let mut list: Vec<String> =
                (0..rng.below(4) + 1).map(|_| node(rng.below(10))).collect();
            if rng.chance(10) {
                list.push("UNDEF".to_string());
            }
            if rng.chance(10) {
                list.push("\"v1\"".to_string());
            }
            let list = list.join(" ");
            let values = |var: &str| format!("VALUES ?{var} {{ {list} }}");
            let p = format!("<urn:x:p{}>", rng.below(3));
            let shapes = [
                format!("DELETE {{ GRAPH <urn:x:g> {{ ?s ?p ?o }} }} WHERE {{ GRAPH <urn:x:g> {{ {} ?s ?p ?o }} }}", values("s")),
                format!("DELETE {{ GRAPH <urn:x:g> {{ ?s ?p ?o }} }} INSERT {{ GRAPH <urn:x:g> {{ ?o <urn:x:back> ?s }} }} WHERE {{ GRAPH <urn:x:g> {{ ?s ?p ?o {} }} }}", values("o")),
                format!("DELETE {{ GRAPH <urn:x:g> {{ <urn:x:n0> ?p0 ?o0 . ?d ?dp ?do }} }} WHERE {{ GRAPH <urn:x:g> {{ {{ <urn:x:n0> ?p0 ?o0 }} UNION {{ {} ?d ?dp ?do }} }} }}", values("d")),
                format!("INSERT {{ GRAPH <urn:x:g> {{ ?s <urn:x:count> ?c }} }} WHERE {{ SELECT ?s (COUNT(*) AS ?c) WHERE {{ GRAPH <urn:x:g> {{ {} ?s ?p ?o }} }} GROUP BY ?s }}", values("s")),
                format!("DELETE {{ GRAPH <urn:x:g> {{ ?x ?p ?o }} }} WHERE {{ GRAPH <urn:x:g> {{ {} ?s {p} ?x OPTIONAL {{ ?x ?p ?o }} }} }}", values("s")),
                format!("DELETE {{ GRAPH <urn:x:g> {{ ?s ?p ?o }} }} WHERE {{ GRAPH <urn:x:g> {{ {} ?s ?p ?o FILTER NOT EXISTS {{ ?o {p} ?any }} }} }}", values("s")),
                format!("DELETE {{ GRAPH <urn:x:g> {{ ?s ?p ?o }} }} WHERE {{ GRAPH <urn:x:g> {{ {} ?s ?p ?o MINUS {{ ?s {p} ?o }} }} }}", values("s")),
                format!("INSERT {{ GRAPH <urn:x:g> {{ ?s <urn:x:hop> ?o }} }} WHERE {{ GRAPH <urn:x:g> {{ {} ?s ?p ?m . ?m ?q ?o }} }}", values("s")),
                format!("DELETE {{ GRAPH <urn:x:g> {{ ?s ?p ?o }} }} WHERE {{ GRAPH <urn:x:g> {{ {{ {} ?s ?p ?o }} UNION {{ ?s {p} ?o }} }} }}", values("s")),
                format!("DELETE {{ GRAPH <urn:x:g> {{ ?d <urn:x:p0> ?o }} }} WHERE {{ GRAPH <urn:x:g> {{ {{ {} ?d <urn:x:p0> ?o }} UNION {{ ?d {p} ?o }} }} }}", values("d")),
                format!("DELETE {{ GRAPH <urn:x:g> {{ ?s ?p ?o }} }} WHERE {{ {} GRAPH <urn:x:g> {{ ?s ?p ?o }} }}", values("s")),
                format!("INSERT {{ GRAPH <urn:x:g> {{ ?s <urn:x:has> ?n }} }} WHERE {{ GRAPH <urn:x:g> {{ {} OPTIONAL {{ ?s ?p ?n }} }} }}", values("s")),
            ];
            let update = &shapes[rng.below(shapes.len())];
            if parse_one(update).footprint != Footprint::Whole {
                narrowed += 1;
            }
            let by_whole = Store::new().unwrap();
            by_whole.load_from_slice(RdfFormat::TriG, &trig).unwrap();
            let answer_whole = run(&by_whole, update, true);
            let answer_footprint = run(&store, update, false);
            assert_eq!(
                answer_footprint, answer_whole,
                "seed {seed}: `{update}` over\n{trig}"
            );
            assert_eq!(
                dump(&store),
                dump(&by_whole),
                "seed {seed}: `{update}` ({answer_whole}) over\n{trig}"
            );
        }
        assert!(
            narrowed >= CASES / 3,
            "only {narrowed} of {CASES} updates exercised a narrowed footprint"
        );
    }
}
