//! A wrapper the planner can erase does not reset the join count (ledger #1429).
//!
//! The claim, reproduced here before it was fixed: the fix for `GRAPH` blocks (ledger #1426)
//! counted one erased wrapper, but sparopt's `normalize_pattern` erases more before
//! `reorder_joins` flattens a join. `FILTER(true)`, `OPTIONAL {}` and `BIND(?unbound AS ?x)`
//! closing six groups of 31 patterns each passed `budget::check_query` as joins of 31 and
//! planned as ONE join of 186: seconds of planning that no cancellation token reaches, after
//! the caller had been answered `Timeout`, and on the store's one writer thread when the shape
//! was an update's `WHERE`.
//!
//! The counting rule (the same in `ikigai-sparql`, ledger #970) is the doc comment on the
//! walk's `Cost::join` in `src/budget.rs`: every wrapper normalization CAN erase is counted as
//! erased, so its patterns count toward the enclosing join.
//!
//! The door probes run in a CHILD process (this binary re-executed with one probe named in its
//! environment), so a regression's planner, which keeps its core long after the caller is
//! answered, is ended with the child instead of running on beside the rest of this binary;
//! the parent kills a child still running after [`HARD`].

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::budget::{check_query, check_update, TimeBudget};
use ikigai_store::{cap_read_graph, cap_write_graph, space, DurableStore, CAP_READ};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROBE: &str = "IKIGAI_STORE_WRAPPER_PROBE";

/// How long the parent lets a child run before killing it.
const HARD: Duration = Duration::from_secs(60);

/// The budget the door probes run under, in milliseconds.
const BUDGET_MS: u64 = 400;

/// The three shapes the audit measured: each wrapper is erased by normalization, so six blocks
/// plan as one join of 186.
const SHAPES: [(&str, &str); 3] = [
    ("filter-true", "FILTER(true)"),
    ("optional-empty", "OPTIONAL {}"),
    ("bind-unbound", "BIND(?unbound{b} AS ?x{b})"),
];

/// `blocks` groups of 31 patterns side by side, each closed by `wrap` (`{b}` is the block's
/// number).
fn wrapped_body(blocks: usize, wrap: &str) -> String {
    (0..blocks)
        .map(|b| {
            let inner: String = (0..31)
                .map(|i| format!("?s <urn:p> ?o{b}_{i} . "))
                .collect();
            format!("{{ {inner} {} }} ", wrap.replace("{b}", &b.to_string()))
        })
        .collect()
}

fn wrapped_blocks(blocks: usize, wrap: &str) -> String {
    format!("SELECT * WHERE {{ {} }}", wrapped_body(blocks, wrap))
}

fn wrap_named(which: &str) -> &'static str {
    SHAPES
        .iter()
        .find(|(name, _)| *name == which)
        .map(|(_, wrap)| *wrap)
        .unwrap_or_else(|| panic!("no shape named {which}"))
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

#[test]
fn erasable_wrappers_count_toward_the_enclosing_join() {
    for (which, wrap) in SHAPES {
        let parsed = spargebra::SparqlParser::new()
            .parse_query(&wrapped_blocks(6, wrap))
            .unwrap();
        assert_join_refused(check_query(&parsed, "query"), "query", 186);
        let update = format!(
            "INSERT {{ GRAPH <urn:t:a> {{ <urn:y> <urn:p> 2 }} }} WHERE {{ {} }}",
            wrapped_body(6, wrap)
        );
        let parsed = spargebra::SparqlParser::new()
            .parse_update(&update)
            .unwrap_or_else(|e| panic!("{which}: {e}"));
        assert_join_refused(check_update(&parsed, "content"), "content", 186);
    }
}

/// The child's half: inert unless a parent named a probe.
#[test]
fn probe_child() {
    let Ok(spec) = std::env::var(PROBE) else {
        return;
    };
    let (door, which) = spec.split_once(':').unwrap();
    let wrap = wrap_named(which);
    let store = DurableStore::in_memory()
        .unwrap()
        .with_time_budget(TimeBudget::new(Duration::from_millis(BUDGET_MS)));
    let kernel = Kernel::new(Arc::new(space(store.clone())));
    let request = match door {
        "select" => Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
            .with_arg(
                "query",
                ArgRef::Inline(wrapped_blocks(6, wrap).into_bytes()),
            ),
        "graph-update" => Request::new(
            Verb::Sink,
            Iri::parse("urn:iki:store:graph-update").unwrap(),
        )
        .with_arg("graph", ArgRef::Inline(b"urn:t:a".to_vec()))
        .with_arg(
            "content",
            ArgRef::Inline(
                format!(
                    "INSERT {{ GRAPH <urn:t:a> {{ <urn:y> <urn:p> 2 }} }} WHERE {{ {} }}",
                    wrapped_body(6, wrap)
                )
                .into_bytes(),
            ),
        ),
        other => panic!("no door named {other}"),
    };
    let cap = match door {
        "select" => Capability::scoped([CAP_READ]),
        _ => Capability::scoped([cap_write_graph("urn:t:a"), cap_read_graph("urn:t:a")]),
    };
    let started = Instant::now();
    let outcome = block_on(kernel.issue(request, &cap));
    let took = started.elapsed();
    let line = match outcome {
        Ok(rep) => format!("ok {}", rep.bytes.len()),
        Err(Error::Timeout(m)) => format!("timeout {m}"),
        Err(e) => format!("err {e}"),
    };
    println!("\nTOOK {}", took.as_millis());
    println!("OUTCOME {}", line.replace('\n', " "));
    println!("OVERDUE {}", store.overdue_evaluations());
    // Ends a regression's planner with this process.
    std::process::exit(0);
}

struct Probe {
    took: Duration,
    outcome: String,
    overdue: usize,
}

fn probe(door: &str, which: &str) -> Probe {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "probe_child", "--nocapture", "--test-threads=1"])
        .env(PROBE, format!("{door}:{which}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
        s
    });
    let deadline = Instant::now() + HARD;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the {which} probe through {door} was still running after {HARD:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Reaped, not just at EOF (field guide 9h).
    let status = child.wait().unwrap();
    let out = reader.join().unwrap();
    assert!(status.success(), "the probe died: {status}\n{out}");
    let field = |key: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("the probe reported no {key}: {out}"))
            .to_string()
    };
    Probe {
        took: Duration::from_millis(field("TOOK").parse().unwrap()),
        outcome: field("OUTCOME"),
        overdue: field("OVERDUE").parse().unwrap(),
    }
}

/// Through the doors each shape is refused at once, before planning, so no planner is left
/// running on overdue. Before the fix each was answered `Timeout` at the budget and its
/// planner ran on for seconds.
#[test]
fn wrappers_the_planner_erases_are_refused_at_the_doors_before_planning() {
    for door in ["select", "graph-update"] {
        let arg = if door == "select" { "query" } else { "content" };
        for (which, _) in SHAPES {
            let p = probe(door, which);
            assert!(
                p.outcome
                    .starts_with(&format!("err invalid argument `{arg}`"))
                    && p.outcome.contains("MAX_JOIN_OPERANDS")
                    && p.outcome.contains("joins 186 patterns"),
                "{door} {which}: {}",
                p.outcome
            );
            assert!(
                p.took < Duration::from_secs(2),
                "{door} {which}: took {:?}",
                p.took
            );
            assert_eq!(p.overdue, 0, "{door} {which}: a planner was left running");
        }
    }
}
