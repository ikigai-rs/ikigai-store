//! The refusal at the overdue cap says what the cap is, where it came from, and how a host
//! raises it (ledger #1500).
//!
//! The claim (from ikigai-rs/ikigai-shacl PR 34): on a 4-core CI runner the default cap,
//! `default_max_overdue()` (a quarter of the machine's available parallelism, at least 1), is
//! 1, so one runaway `sh:select` made every later validation in the process `Unavailable`. The
//! refusal said "the most it allows is 1" and nothing else: not that 1 was a default, not that
//! it came from the machine's core count, and not what a host does to raise it. A reader on a
//! large machine, where the default is higher, could not reproduce it and could not tell why.
//!
//! These pin the three facts in both refusals the cap produces — at once while the cap is
//! filled by OVERDUE evaluations, and after waiting while it is filled by running ones — for a
//! cap left at its default and one the host set. And they pin that a consumer's budget is its
//! own: an `EvaluationBudget::new` counts apart from every other, so a consumer can state its
//! own cap rather than share one.
//!
//! ★ Since ledger #1002 one CALLER cannot fill the cap (it holds at most its share, and a
//! slot is reserved for the host), so these fill it as the HOST, which root is: the store-wide
//! refusal is the one pinned here, and `tests/overdue_share.rs` pins the per-caller ones.

use ikigai_core::{Capability, Error};
use ikigai_store::budget::{default_max_overdue, EvaluationBudget, TimeBudget};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn reader() -> Capability {
    Capability::scoped(["urn:cap:space:read"])
}

/// This machine's available parallelism, as the refusal states it.
fn parallelism() -> String {
    std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Work that tells `started` it is running, then waits until `held` is released, so it can
/// only time out and then stays overdue until released.
fn held_work(
    started: mpsc::Sender<()>,
    held: mpsc::Receiver<()>,
) -> impl FnOnce(&oxigraph::sparql::CancellationToken) -> ikigai_core::Result<()> + Send + 'static {
    move |_| {
        let _ = started.send(());
        let _ = held.recv();
        Ok(())
    }
}

/// Fill `budget`'s cap with overdue evaluations; returns the senders that release them. As
/// the host (root), which alone may use every slot of the cap (ledger #1002).
fn fill_with_overdue(budget: &EvaluationBudget, n: usize) -> Vec<mpsc::Sender<()>> {
    (0..n)
        .map(|_| {
            let (release, held) = mpsc::channel::<()>();
            let (started, _seen) = mpsc::channel::<()>();
            let answer = budget.run(
                "ASK {}",
                &Capability::root(),
                None,
                held_work(started, held),
            );
            assert!(matches!(answer, Err(Error::Timeout(_))), "{answer:?}");
            release
        })
        .collect()
}

fn release_all(budget: &EvaluationBudget, releases: Vec<mpsc::Sender<()>>) {
    for release in releases {
        let _ = release.send(());
    }
    let start = Instant::now();
    while budget.overdue_evaluations() != 0 {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "workers never ended"
        );
        std::thread::sleep(ms(5));
    }
}

fn refused_at_once(budget: &EvaluationBudget) -> String {
    let start = Instant::now();
    let (_release, held) = mpsc::channel::<()>();
    let (started, _seen) = mpsc::channel::<()>();
    match budget.run("ASK {}", &reader(), None, held_work(started, held)) {
        Err(Error::Unavailable(message)) => {
            assert!(start.elapsed() < ms(50), "refused at once");
            message
        }
        other => panic!("expected Unavailable at the cap, got {other:?}"),
    }
}

#[test]
fn at_the_default_cap_the_refusal_says_it_is_the_default_and_names_the_parallelism() {
    let budget = EvaluationBudget::new(TimeBudget::new(ms(50)));
    let cap = default_max_overdue();
    assert_eq!(budget.time_budget().max_overdue(), cap);
    // Where the number came from changes the wording, never what a budget allows: setting
    // the default's own number explicitly is still the same budget.
    assert_eq!(
        TimeBudget::new(ms(50)).with_max_overdue(cap),
        budget.time_budget()
    );
    let releases = fill_with_overdue(&budget, cap);
    let message = refused_at_once(&budget);
    release_all(&budget, releases);

    // The old contract still holds …
    assert!(
        message.contains(&format!("the most it allows is {cap}")),
        "{message}"
    );
    // … and the three new facts: where the number came from, the machine it came from, and
    // what raises it.
    assert!(
        message.contains(&format!(
            "The cap of {cap} is the default: a quarter of this machine's available \
             parallelism ({}), and at least 2",
            parallelism()
        )),
        "{message}"
    );
    assert!(
        message.contains("TimeBudget::with_max_overdue"),
        "{message}"
    );
    assert!(message.contains("EvaluationBudget"), "{message}");
}

#[test]
fn at_a_cap_the_host_set_the_refusal_says_so_and_names_the_parallelism() {
    let budget = EvaluationBudget::new(TimeBudget::new(ms(50)).with_max_overdue(1));
    let releases = fill_with_overdue(&budget, 1);
    let message = refused_at_once(&budget);
    release_all(&budget, releases);

    assert!(message.contains("the most it allows is 1"), "{message}");
    assert!(
        message.contains(&format!(
            "The cap of 1 was set by the host (`TimeBudget::with_max_overdue`); this machine's \
             available parallelism is {}",
            parallelism()
        )),
        "{message}"
    );
}

#[test]
fn a_read_that_waited_for_a_slot_names_the_cap_and_where_it_came_from_too() {
    // A cap of 1, held by an evaluation still INSIDE its budget: the next read waits for the
    // slot within its own 50 ms, then is refused with the other sentence.
    let holder_budget =
        EvaluationBudget::new(TimeBudget::new(Duration::from_secs(20)).with_max_overdue(1));
    let (release, held) = mpsc::channel::<()>();
    let (started, running) = mpsc::channel::<()>();
    let holder = {
        let budget = holder_budget.clone();
        std::thread::spawn(move || budget.run("ASK {}", &reader(), None, held_work(started, held)))
    };
    running.recv().unwrap();

    let refused = holder_budget.run("ASK {}", &reader(), Some("50"), |_| Ok(()));
    let _ = release.send(());
    holder.join().unwrap().unwrap();

    let message = match refused {
        Err(Error::Unavailable(message)) => message,
        other => panic!("expected Unavailable after waiting, got {other:?}"),
    };
    assert!(
        message.contains("the most it runs at once is 1"),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "The cap of 1 was set by the host (`TimeBudget::with_max_overdue`); this machine's \
             available parallelism is {}",
            parallelism()
        )),
        "{message}"
    );
}

#[test]
fn a_consumer_budget_counts_apart_from_every_other_and_states_its_own_cap() {
    // Two `EvaluationBudget::new`s, as two consumers (say, shacl's and intray's) would make
    // them: one at its cap refuses; the other, with a cap of its own, does not notice.
    let full = EvaluationBudget::new(TimeBudget::new(ms(50)).with_max_overdue(1));
    let own = EvaluationBudget::new(TimeBudget::new(ms(50)).with_max_overdue(3));
    assert_eq!(own.time_budget().max_overdue(), 3);
    let releases = fill_with_overdue(&full, 1);
    let _ = refused_at_once(&full);

    assert_eq!(own.overdue_evaluations(), 0, "the count is per budget");
    assert_eq!(own.run("ASK {}", &reader(), None, |_| Ok(7)), Ok(7));
    // And its own cap holds: two more overdue are allowed before it refuses at three.
    let more = fill_with_overdue(&own, 3);
    let message = refused_at_once(&own);
    assert!(message.contains("the most it allows is 3"), "{message}");

    release_all(&full, releases);
    release_all(&own, more);
}
