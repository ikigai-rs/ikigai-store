//! A query only the CLOCK can stop, for the tests that assert a time-budget refusal through
//! the store's read doors (ledger #1685).
//!
//! ★ **A read through a door is stopped by whichever bound fires FIRST.** Those tests used a
//! four-pattern cross product over the seeded graph (120^4 rows) and relied on the time budget
//! answering it. But the answer's SIZE is bounded too (16 MiB, 100,000 rows; ledger #970), and
//! that bound is reached as fast as the machine can serialize: measured on plasma (M-series,
//! 2026-10-16), 89 ms in a release build and 923 ms in a debug one. So under `cargo test
//! --release` every 300 ms and 400 ms budget test was refused for size instead, and in a debug
//! build the 400 ms one passed by a factor of two, which is a machine's speed, not a proof.
//!
//! [`slow`] is built so that no other bound CAN fire: its rows are few, at most [`SLOW_ROWS`],
//! each one small, so its whole answer is under both size bounds by construction (the const
//! assertion below), and each row is made expensive instead. A machine fast enough to finish it
//! inside a budget answers with rows, and every test refuses an answer: it fails as "not slow
//! enough", never as a different bound standing in for the clock. The same shape fixed
//! ikigai-cms-web's door tests (ikigai-rs/ikigai-cms-web PR 110, ledger #1673).
//!
//! It is NOT for the update doors (an update's answer is not size-bounded, so the plain cross
//! product still meets only the clock there) nor for an aggregate (one row).

use ikigai_store::budget::{DEFAULT_MAX_BYTES, DEFAULT_MAX_ROWS};

/// The seeded graph: 60 nodes with two `<urn:p>` edges each, so `?s <urn:p> ?o` matches
/// [`SEED_TRIPLES`] in any graph it is inserted into.
pub fn seed_triples() -> String {
    let mut triples = String::new();
    for i in 0..60usize {
        for j in [i + 1, i * 7 + 3] {
            triples.push_str(&format!("<urn:n{i}> <urn:p> <urn:n{}> . ", j % 60));
        }
    }
    triples
}

/// How many triples [`seed_triples`] writes.
pub const SEED_TRIPLES: u64 = 120;

/// The most graphs a test inserts [`seed_triples`] into. A read sees at most all of them, so
/// one `<urn:p>` pattern matches at most `SEED_TRIPLES * SEED_GRAPHS` triples whatever the
/// dataset a door gives it.
pub const SEED_GRAPHS: u64 = 2;

/// The most rows [`slow`] can answer: two patterns, each matching at most
/// `SEED_TRIPLES * SEED_GRAPHS` triples (14,400 at the doors these tests use, which read one
/// graph).
pub const SLOW_ROWS: u64 = (SEED_TRIPLES * SEED_GRAPHS) * (SEED_TRIPLES * SEED_GRAPHS);

/// An upper bound on one of [`slow`]'s rows serialized: one IRI and one integer, as a SPARQL
/// JSON binding (`{"a":{"type":"uri","value":"urn:n59"},"n":{"type":"literal","datatype":
/// "http://www.w3.org/2001/XMLSchema#integer","value":"1000014"}},` is 134 bytes) or as a
/// constructed triple in any syntax. Checked against both doors by
/// `tests/sparql_time_budget.rs`'s `the_slow_query_cannot_reach_a_size_bound`.
pub const SLOW_ROW_BYTES: u64 = 192;

// The slow query cannot reach a size bound, so a refusal of it is the clock's. The head and
// tail of an answer are a few dozen bytes; one row's worth is ample room for them.
const _: () = {
    assert!(SLOW_ROWS < DEFAULT_MAX_ROWS);
    assert!((SLOW_ROWS + 1) * SLOW_ROW_BYTES < DEFAULT_MAX_BYTES);
};

/// `head` (a `SELECT` or `CONSTRUCT` clause naming at most `?a` and `?n`) over every pair of
/// `<urn:p>` triples, binding `?n` to the length of a string grown from the pair: ten `x`s and
/// the two subjects, every `x` replaced by ten five times over, so a million characters a row,
/// of which only the length is kept. The string depends on `?a` and `?b`, so the planner cannot
/// fold it into one constant.
///
/// Measured on plasma (2026-10-16, through `urn:iki:store:select`): about 1.2 ms a row in a
/// release build and 20 ms in a debug one, so the 14,400 rows of the seeded graph take about
/// 17 s in release and 5 minutes in debug. Finishing inside the largest budget these tests set
/// for it ([`MAX_BUDGET_MS`]) would take a machine about sixty times faster than plasma's
/// release build. The serializer checks the cancellation token between rows, so a timed-out
/// read stops within one row: a sixth round would make a row ten times dearer, and a slow
/// machine's cancelled read ten times later to end, for a margin no real machine needs.
pub fn slow(head: &str) -> String {
    let mut grown = r#"CONCAT(STR(?a), STR(?b), "xxxxxxxxxx")"#.to_string();
    for _ in 0..5 {
        grown = format!(r#"REPLACE({grown}, "x", "xxxxxxxxxx")"#);
    }
    format!("{head} WHERE {{ ?a <urn:p> ?x . ?b <urn:p> ?y . BIND(STRLEN({grown}) AS ?n) }}")
}

/// The largest budget a test may give a read of [`slow`] and still expect the clock to stop it.
/// See [`slow`] for the margin it leaves.
pub const MAX_BUDGET_MS: u64 = 300;

/// A budget of `ms` for a store or grant that a read of [`slow`] runs under, refused past
/// [`MAX_BUDGET_MS`], so the margin [`slow`] states cannot be spent by accident.
pub fn slow_budget(ms: u64) -> std::time::Duration {
    assert!(
        ms <= MAX_BUDGET_MS,
        "{ms} ms is past the {MAX_BUDGET_MS} ms that `common::slow` is sized for"
    );
    std::time::Duration::from_millis(ms)
}

/// What a read of [`slow`] answered, said briefly: a finished answer is a test failure whose
/// cause is this machine's speed, and its body is megabytes nobody wants in a panic message.
pub fn not_slow_enough(bytes: usize) -> String {
    format!(
        "an ANSWER of {bytes} bytes: the slow query finished inside its budget, so this \
         machine is fast enough that `common::slow` needs more rounds (ledger #1685)"
    )
}
