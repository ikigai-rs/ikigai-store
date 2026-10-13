//! A `GRAPH` block is not one join operand (ledger #1426).
//!
//! The claim, reproduced here before it was fixed: `budget::check_query` bounds the operands
//! of ONE join at `MAX_JOIN_OPERANDS` (32) because the planner's greedy join reordering is
//! about cubic in them and cannot be cancelled. But `sparopt` ERASES a `GRAPH <g> { … }` (and
//! a `GRAPH ?g { … }`) wrapper, pushing the graph name down into each quad pattern, and then
//! flattens nested joins, while the walk counted a `GRAPH` block as ONE operand. So six
//! `GRAPH` blocks of 31 patterns passed as a join of 31 and were planned as one join of 186:
//! 3.8 s on an EMPTY store in a release build, 12 blocks 63 s, all uncancellable, and through
//! `urn:iki:store:graph-update` on the store's one writer thread, so a tenant holding a grant
//! for its own graph alone starved every other tenant's writes for seconds a request.
//!
//! The counting rule (the same in `ikigai-sparql`): a `GRAPH` block's inner patterns count
//! toward the ENCLOSING join's operands, as any nested join does. `UNION`, `OPTIONAL` and
//! `MINUS` branches stay joins of their own, each counted on its own.
//!
//! Every slow shape here is now refused before planning, so nothing in this file runs long
//! even when it regresses: a regression is answered at the 500 ms budget, and its planner is
//! left to the end of this test binary.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::budget::{check_query, check_update, TimeBudget, MAX_JOIN_OPERANDS};
use ikigai_store::{cap_read_graph, cap_write_graph, space, DurableStore, CAP_READ};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `per_block` patterns, all joined on `?s`, written as one `GRAPH {graph}` block.
fn block(graph: &str, b: usize, per_block: usize) -> String {
    let patterns: String = (0..per_block)
        .map(|i| format!("?s <urn:p> ?o{b}_{i} . "))
        .collect();
    format!("GRAPH {graph} {{ {patterns}}} ")
}

/// `blocks` `GRAPH` blocks side by side in one group: one join, once the wrapper is erased.
fn graph_blocks(graph: &str, blocks: usize, per_block: usize) -> String {
    let body: String = (0..blocks).map(|b| block(graph, b, per_block)).collect();
    format!("SELECT * WHERE {{ {body}}}")
}

fn parse_query(text: &str) -> spargebra::Query {
    spargebra::SparqlParser::new().parse_query(text).unwrap()
}

fn parse_update(text: &str) -> spargebra::Update {
    spargebra::SparqlParser::new().parse_update(text).unwrap()
}

/// The refusal names the argument, the operand count and the bound.
fn assert_join_refused(result: ikigai_core::Result<()>, arg: &str, operands: usize) {
    match result {
        Err(Error::InvalidArgument { name, detail }) => {
            assert_eq!(name, arg);
            assert!(
                detail.contains(&format!("joins {operands} patterns"))
                    && detail.contains("MAX_JOIN_OPERANDS"),
                "{detail}"
            );
        }
        other => panic!("expected a join of {operands} refused on `{arg}`, got {other:?}"),
    }
}

/// ★ The auditor's case: six `GRAPH` blocks of 31 patterns are ONE join of 186.
#[test]
fn graph_blocks_count_toward_the_enclosing_join() {
    assert_join_refused(
        check_query(&parse_query(&graph_blocks("<urn:g>", 6, 31)), "query"),
        "query",
        186,
    );
    // A variable graph name is erased the same way.
    assert_join_refused(
        check_query(&parse_query(&graph_blocks("?g", 6, 31)), "query"),
        "query",
        186,
    );
    // And the auditor's 30 blocks (930 patterns, 19 KB), which planned for minutes.
    assert_join_refused(
        check_query(&parse_query(&graph_blocks("<urn:g>", 30, 31)), "query"),
        "query",
        930,
    );
}

/// The bound is exact across blocks: 32 operands pass, 33 do not, however they are grouped.
#[test]
fn the_bound_is_the_flattened_total() {
    assert_eq!(MAX_JOIN_OPERANDS, 32, "the bound keeps its value");
    check_query(&parse_query(&graph_blocks("<urn:g>", 2, 16)), "query").unwrap();
    // 16 in a block beside 16 bare patterns: one join of 32.
    let mixed = format!(
        "SELECT * WHERE {{ {} {} }}",
        block("<urn:g>", 0, 16),
        (0..16)
            .map(|i| format!("?s <urn:q> ?x{i} . "))
            .collect::<String>()
    );
    check_query(&parse_query(&mixed), "query").unwrap();
    assert_join_refused(
        check_query(
            &parse_query(&mixed.replacen("?x0 .", "?x0 . ?s <urn:q> ?y .", 1)),
            "query",
        ),
        "query",
        33,
    );
    // A block nested in a block is erased twice: still one join.
    let nested = format!(
        "SELECT * WHERE {{ GRAPH <urn:a> {{ {} {} }} }}",
        block("<urn:b>", 0, 17),
        block("<urn:c>", 1, 17)
    );
    assert_join_refused(check_query(&parse_query(&nested), "query"), "query", 34);
    // A lone block of 31 is a join of 31, as before.
    check_query(&parse_query(&graph_blocks("<urn:g>", 1, 31)), "query").unwrap();
}

/// `UNION`, `OPTIONAL` and `MINUS` branches are joins of their own: the same 186 patterns
/// split across branches are accepted when each branch is within the bound.
#[test]
fn union_optional_and_minus_branches_stay_separate_joins() {
    let union = (0..6)
        .map(|b| format!("{{ {} }}", block("<urn:g>", b, 31)))
        .collect::<Vec<_>>()
        .join(" UNION ");
    check_query(
        &parse_query(&format!("SELECT * WHERE {{ {union} }}")),
        "query",
    )
    .unwrap();

    let optional: String = (1..6)
        .map(|b| format!("OPTIONAL {{ {} }} ", block("<urn:g>", b, 31)))
        .collect();
    check_query(
        &parse_query(&format!(
            "SELECT * WHERE {{ {} {optional}}}",
            block("<urn:g>", 0, 31)
        )),
        "query",
    )
    .unwrap();

    let minus: String = (1..6)
        .map(|b| format!("MINUS {{ {} }} ", block("<urn:g>", b, 31)))
        .collect();
    check_query(
        &parse_query(&format!(
            "SELECT * WHERE {{ {} {minus}}}",
            block("<urn:g>", 0, 31)
        )),
        "query",
    )
    .unwrap();

    // But a branch is bounded too: two blocks of 17 inside ONE union branch are 34.
    let wide = format!(
        "SELECT * WHERE {{ {{ {} {} }} UNION {{ ?s ?p ?o }} }}",
        block("<urn:g>", 0, 17),
        block("<urn:g>", 1, 17)
    );
    assert_join_refused(check_query(&parse_query(&wide), "query"), "query", 34);
}

/// The same in an update's `WHERE`.
#[test]
fn an_updates_where_is_counted_the_same_way() {
    let body: String = (0..6).map(|b| block("<urn:t:a>", b, 31)).collect();
    let slow = format!("INSERT {{ GRAPH <urn:t:a> {{ <urn:y> <urn:p> 2 }} }} WHERE {{ {body}}}");
    assert_join_refused(
        check_update(&parse_update(&slow), "content"),
        "content",
        186,
    );
}

fn store(budget_ms: u64) -> (DurableStore, Arc<Kernel>) {
    let store = DurableStore::in_memory()
        .unwrap()
        .with_time_budget(TimeBudget::new(Duration::from_millis(budget_ms)));
    let kernel = Arc::new(Kernel::new(Arc::new(space(store.clone()))));
    (store, kernel)
}

/// Through the door the query is refused at once, so no planner runs on overdue. Before the
/// fix the caller was answered `Timeout` at 500 ms and the planner ran on for seconds.
#[test]
fn through_the_door_the_query_is_refused_before_planning() {
    let (store, kernel) = store(500);
    let start = Instant::now();
    let result = block_on(kernel.issue(
        Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap()).with_arg(
            "query",
            ArgRef::Inline(graph_blocks("<urn:g>", 6, 31).into_bytes()),
        ),
        &Capability::scoped([CAP_READ]),
    ));
    match result {
        Err(Error::InvalidArgument { name, detail }) => {
            assert_eq!(name, "query");
            assert!(detail.contains("joins 186 patterns"), "{detail}");
        }
        other => panic!(
            "expected the query refused, got {:?} after {:?}",
            other.map(|r| r.bytes.len()),
            start.elapsed()
        ),
    }
    assert_eq!(store.overdue_evaluations(), 0, "nothing is left running");
}

fn graph_update(
    kernel: &Kernel,
    cap: &Capability,
    graph: &str,
    content: String,
) -> (Duration, ikigai_core::Result<String>) {
    let start = Instant::now();
    let r = block_on(
        kernel.issue(
            Request::new(
                Verb::Sink,
                Iri::parse("urn:iki:store:graph-update").unwrap(),
            )
            .with_arg("graph", ArgRef::Inline(graph.as_bytes().to_vec()))
            .with_arg("content", ArgRef::Inline(content.into_bytes())),
            cap,
        ),
    );
    (
        start.elapsed(),
        r.map(|rep| String::from_utf8_lossy(&rep.bytes).trim().to_string()),
    )
}

/// ★ The tenancy consequence. Tenant A, holding write and read on its own graph only, sends
/// the six-block update; tenant B, holding write on its own graph only, sends one quad 100 ms
/// later. Before the fix A's planner held the store's one writer thread, so B's trivial write
/// timed out too. Now A is refused by the cost check, which runs before the write lock is
/// taken, and B is answered.
#[test]
fn one_tenants_update_does_not_block_another_tenants_write() {
    let (store, kernel) = store(500);
    let a = Capability::scoped([cap_write_graph("urn:t:a"), cap_read_graph("urn:t:a")]);
    let b = Capability::scoped([cap_write_graph("urn:t:b")]);
    let body: String = (0..6).map(|blk| block("<urn:t:a>", blk, 31)).collect();
    let slow = format!("INSERT {{ GRAPH <urn:t:a> {{ <urn:y> <urn:p> 2 }} }} WHERE {{ {body}}}");

    let ka = Arc::clone(&kernel);
    let ta = std::thread::spawn(move || graph_update(&ka, &a, "urn:t:a", slow));
    std::thread::sleep(Duration::from_millis(100));
    let (b_took, b_result) = graph_update(
        &kernel,
        &b,
        "urn:t:b",
        "INSERT DATA { GRAPH <urn:t:b> { <urn:x> <urn:p> 3 } }".into(),
    );
    let (a_took, a_result) = ta.join().unwrap();
    match a_result {
        Err(Error::InvalidArgument { name, detail }) => {
            assert_eq!(name, "content");
            assert!(detail.contains("joins 186 patterns"), "{detail}");
        }
        other => panic!("expected A's update refused, got {other:?} after {a_took:?}"),
    }
    assert_eq!(
        b_result.as_deref(),
        Ok("updated <urn:t:b>"),
        "B's one-quad write to its own graph, after {b_took:?}"
    );
    assert_eq!(store.overdue_evaluations(), 0, "nothing is left running");
}
