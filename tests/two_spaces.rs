//! Two spaces over one dataset: neither may cache a read the other's writes can change
//! (ledger #751, probe p3).
//!
//! The crate example binds `space(store.clone())` into two kernels. Each kernel keeps its
//! own cache and cuts a golden thread only for a write IT served, so before the fix both
//! reported `covered: true`, both cached, and a write through one left the other serving a
//! stale answer indefinitely. Now a second LIVE space forfeits coverage for every space
//! over that dataset — the same cost as handing out the raw handle, and said the same way.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;

fn kernel(store: DurableStore) -> Kernel {
    Kernel::new(Arc::new(space(store)))
}

fn issue(kernel: &Kernel, verb: Verb, iri: &str, args: &[(&str, &str)]) -> String {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    String::from_utf8(
        block_on(kernel.issue(request, &Capability::root()))
            .expect(iri)
            .bytes,
    )
    .unwrap()
}

fn ask(kernel: &Kernel) -> String {
    issue(
        kernel,
        Verb::Source,
        "urn:iki:store:ask",
        &[("query", "ASK { <urn:s> <urn:p> <urn:o> }")],
    )
}

fn write(kernel: &Kernel, update: &str) {
    issue(
        kernel,
        Verb::Sink,
        "urn:iki:store:update",
        &[("content", update)],
    );
}

fn info(kernel: &Kernel) -> String {
    issue(kernel, Verb::Source, "urn:iki:store:info", &[])
}

#[test]
fn a_write_through_one_space_is_seen_by_a_kernel_over_the_other() {
    let store = DurableStore::in_memory().unwrap();
    let k1 = kernel(store.clone());
    let k2 = kernel(store.clone());
    assert_eq!(store.spaces_bound(), 2);
    assert!(
        ask(&k2).contains("false"),
        "k2 reads (and would have cached) the empty store"
    );
    write(&k1, "INSERT DATA { <urn:s> <urn:p> <urn:o> }");
    assert!(ask(&k1).contains("true"), "control: k1 sees its own write");
    assert!(
        ask(&k2).contains("true"),
        "k2 served a stale cached answer after a write through k1"
    );
    // And the other direction, so the fix is not an accident of which kernel wrote.
    write(&k2, "DELETE DATA { <urn:s> <urn:p> <urn:o> }");
    assert!(
        ask(&k1).contains("false"),
        "k1 served a stale answer after a write through k2"
    );
    // Both say so, through the line an operator reads.
    for k in [&k1, &k2] {
        assert!(info(k).contains("covered: false\n"), "{}", info(k));
    }
    assert!(!store.is_sole_writer());
    assert!(!store.read_is_covered(None));
    assert!(!store.read_is_covered(Some("urn:example:g")));
}

/// The count is of LIVE spaces: dropping the second space gives coverage back — a kernel
/// that is gone writes nothing.
#[test]
fn coverage_follows_the_number_of_live_spaces() {
    let store = DurableStore::in_memory().unwrap();
    assert_eq!(store.spaces_bound(), 0);
    let k1 = kernel(store.clone());
    let k2 = kernel(store.clone());
    assert_eq!(store.spaces_bound(), 2);
    assert!(!store.read_is_covered(None));
    drop(k2);
    assert_eq!(store.spaces_bound(), 1);
    assert!(store.is_sole_writer());
    assert!(store.read_is_covered(None));
    assert!(info(&k1).contains("covered: true\n"), "{}", info(&k1));
    // A clone that is never bound is not a writer: it has no door.
    let _unbound = store.clone();
    assert_eq!(store.spaces_bound(), 1);
}

/// ★ The order hole, closed: once a space has CACHED a read, a second space bound later
/// could write under it and nothing could evict the cached answer — so the late space
/// refuses every request, is not counted, and the first stays covered and correct.
#[test]
fn a_space_bound_after_a_cached_read_refuses_rather_than_go_stale_under_it() {
    let store = DurableStore::in_memory().unwrap();
    let k1 = kernel(store.clone());
    assert!(ask(&k1).contains("false"), "k1 caches the empty answer");
    assert!(info(&k1).contains("covered: true\n"));

    let late = kernel(store.clone());
    assert_eq!(store.spaces_bound(), 1, "the late space is not counted");
    let mut request = Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap());
    request = request.with_arg(
        "content",
        ArgRef::Inline(b"INSERT DATA { <urn:s> <urn:p> <urn:o> }".to_vec()),
    );
    let err = block_on(late.issue(request, &Capability::root()))
        .expect_err("a write through the late space");
    assert!(err.to_string().contains("before the first read"), "{err}");
    let read = Request::new(Verb::Source, Iri::parse("urn:iki:store:info").unwrap());
    assert!(block_on(late.issue(read, &Capability::root())).is_err());
    // k1 is untouched: still covered, still right.
    assert!(ask(&k1).contains("false"));
    assert!(store.is_sole_writer());

    // Once every space is gone, every cache that could have gone stale went with it, so a
    // fresh pair binds normally.
    drop(late);
    drop(k1);
    assert_eq!(store.spaces_bound(), 0);
    let k3 = kernel(store.clone());
    write(&k3, "INSERT DATA { <urn:s> <urn:p> <urn:o> }");
    assert!(ask(&k3).contains("true"));
}

/// ⚠ **The shape this crate cannot see** (ledger #761): ONE space, `Arc`-cloned into two
/// kernels. It is one binding to the count above and two caches to the kernels, so neither
/// rule applies — `info` says `covered: true`, `k2` caches, and a write through `k1` leaves
/// `k2` serving the stale answer with no bound. `ikigai-embedded` memoized its store space
/// exactly this way.
///
/// ★ This test PINS THE HAZARD rather than a fix, because no fix is reachable from this
/// crate: an endpoint is not told which kernel invoked it, and nothing here holds a kernel's
/// cache (`DurableStore::spaces_bound` has the argument). If it starts failing, something
/// upstream changed what an endpoint can see — read why before deleting it, because the
/// store may now be able to close this properly.
#[test]
fn one_space_shared_by_two_kernels_is_the_hazard_this_crate_cannot_see() {
    let store = DurableStore::in_memory().unwrap();
    let shared: Arc<dyn ikigai_core::Space> = Arc::new(space(store.clone()));
    let k1 = Kernel::new(Arc::clone(&shared));
    let k2 = Kernel::new(Arc::clone(&shared));
    assert_eq!(
        store.spaces_bound(),
        1,
        "one space, however many kernels hold it"
    );
    assert!(info(&k2).contains("covered: true\n"), "{}", info(&k2));

    assert!(ask(&k2).contains("false"), "k2 caches the empty answer");
    write(&k1, "INSERT DATA { <urn:s> <urn:p> <urn:o> }");
    assert!(ask(&k1).contains("true"), "control: k1 sees its own write");
    assert!(
        ask(&k2).contains("false"),
        "k2 is no longer stale: something upstream now lets this crate see a shared space — \
         see this test's doc comment before changing it"
    );
}

/// ★ **The supported shape for several kernels**, and the one `ikigai-embedded` should
/// memoize: ONE `DurableStore` (one open, so RocksDB's one-writer rule holds), a space PER
/// kernel. Counted, so coverage is forfeit while both live and neither goes stale.
#[test]
fn one_store_with_a_space_per_kernel_is_never_stale() {
    let memoized = DurableStore::in_memory().unwrap();
    let k1 = Kernel::new(Arc::new(space(memoized.clone())));
    let k2 = Kernel::new(Arc::new(space(memoized.clone())));
    assert_eq!(memoized.spaces_bound(), 2);
    assert!(ask(&k2).contains("false"));
    write(&k1, "INSERT DATA { <urn:s> <urn:p> <urn:o> }");
    assert!(ask(&k2).contains("true"), "k2 sees k1's write");
}
