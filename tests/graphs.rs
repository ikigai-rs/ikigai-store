//! `urn:iki:store:graphs` — the enumeration a scoped read cannot perform.
//!
//! # What these tests are for
//!
//! The resource has two paths that must be indistinguishable from outside — a tenant's
//! answer comes from its own grants, root's comes from the store, because
//! `Capability::scopes()` is `None` for root and root therefore cannot be asked what it
//! holds. Everything that could go wrong is invisible to every automatic check:
//!
//! - **The oracle.** The one property that must never break is that a caller cannot learn
//!   that a graph it may not read *exists*. Nothing in `ikigai-conformance` can see that;
//!   it is [`a_tenant_is_never_told_another_tenants_graph_exists`] or it is nothing.
//! - **Empty is a legitimate answer**, exactly as in `tests/read_scope.rs`: a listing that
//!   has quietly stopped working is indistinguishable from a store with no named graphs.
//!   So every refusal is paired with the positive case that proves the path still works.
//! - **The two paths agreeing** is a property of a pair of answers and not of either one,
//!   so it takes a test that asks both.
//! - **Cacheability under a `SharerWrites` declaration** is per-candidate here rather than
//!   per-graph, which is a rule with no other home.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{
    cap_read_graph, cap_write_graph, space, DurableStore, SharerWrites, CAP_READ, CAP_READ_ANY,
    CAP_WRITE,
};
use std::sync::Arc;

const ACME: &str = "urn:example:acme";
const ZENITH: &str = "urn:example:zenith";
/// Granted below, and never written to: the case that separates "may read" from "exists".
const EMPTY: &str = "urn:example:empty";

const GRAPHS: &str = "urn:iki:store:graphs";

/// Two tenants' graphs and one triple in the store's own default graph — the third is what
/// makes "the default graph is never listed" testable rather than asserted.
fn kernel() -> Kernel {
    kernel_over(DurableStore::in_memory().unwrap())
}

fn kernel_over(store: DurableStore) -> Kernel {
    let kernel = Kernel::new(Arc::new(space(store)));
    let seed = format!(
        "INSERT DATA {{ \
           GRAPH <{ACME}> {{ <urn:example:acme:1> <urn:p> \"acme\" }} \
           GRAPH <{ZENITH}> {{ <urn:example:zenith:1> <urn:p> \"zenith\" }} \
           <urn:example:host:1> <urn:p> \"host\" \
         }}"
    );
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:update",
        &[("content", &seed)],
        &Capability::root(),
    )
    .expect("seeding");
    kernel
}

fn issue(
    kernel: &Kernel,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
    cap: &Capability,
) -> Result<String, Error> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(request, cap)).map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

/// The listing as lines, so an assertion is about names and not about whitespace.
fn list(kernel: &Kernel, cap: &Capability) -> Result<Vec<String>, Error> {
    issue(kernel, Verb::Source, GRAPHS, &[], cap)
        .map(|text| text.lines().map(str::to_string).collect())
}

// ------------------------------------------------------------------- the two paths

#[test]
fn root_lists_every_named_graph() {
    // Root's `scopes()` is `None`, so this answer can only have come from the store.
    assert_eq!(
        list(&kernel(), &Capability::root()).unwrap(),
        vec![ACME.to_string(), ZENITH.to_string()]
    );
}

#[test]
fn the_default_graph_is_never_listed() {
    // The seed puts a triple in the default graph. It has no IRI, so no grant can name it
    // and no scoped read can reach it — and an empty listing therefore does NOT mean an
    // empty store, which is the thing a consumer would otherwise get wrong.
    let names = list(&kernel(), &Capability::root()).unwrap();
    assert!(
        names.iter().all(|n| n.starts_with("urn:example:")),
        "{names:?}"
    );
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn a_tenant_sees_the_graphs_its_own_grants_name() {
    let cap = Capability::scoped([cap_read_graph(ACME)]);
    assert_eq!(list(&kernel(), &cap).unwrap(), vec![ACME.to_string()]);
}

#[test]
fn a_broad_reader_gets_exactly_what_root_gets() {
    // ★ The two ends of the read family answer identically, which is what makes the
    // `urn:cap:store:read*` declaration honest: a broad reader may read every graph, so
    // listing every graph is its answer too — and a consumer that resolves this resource
    // does not have to know which kind of caller it is running as.
    let kernel = kernel();
    assert_eq!(
        list(&kernel, &Capability::scoped([CAP_READ])).unwrap(),
        list(&kernel, &Capability::root()).unwrap()
    );
}

#[test]
fn the_two_paths_produce_the_same_shape() {
    // Same bytes, same order, for the part of the store both callers can see. The tenant
    // holds grants for both seeded graphs here, so the answers must be equal — a
    // difference would mean a consumer has to branch on who it is.
    let kernel = kernel();
    let tenant = Capability::scoped([cap_read_graph(ACME), cap_read_graph(ZENITH)]);
    assert_eq!(
        issue(&kernel, Verb::Source, GRAPHS, &[], &tenant).unwrap(),
        issue(&kernel, Verb::Source, GRAPHS, &[], &Capability::root()).unwrap()
    );
}

// --------------------------------------------------------- may-read versus exists

#[test]
fn a_grant_for_a_graph_that_does_not_exist_lists_nothing() {
    // ★ The intersection decision, as an observable fact: `EMPTY` is granted and has never
    // been written to, so it is not listed. A consumer asking "which ledgers exist" gets
    // the ones that do, not the ones its operator meant to create.
    let cap = Capability::scoped([cap_read_graph(ACME), cap_read_graph(EMPTY)]);
    assert_eq!(list(&kernel(), &cap).unwrap(), vec![ACME.to_string()]);
}

#[test]
fn a_graph_appears_as_soon_as_something_is_written_to_it() {
    // The other half of the pair: the absence above is about the store's contents and not
    // about a filter that drops everything. ★ It is also the tenant path's thread test —
    // the first listing is cached (this store is covered), so the second one can only be
    // right if `urn:iki:store:graph-update` cut the thread it depends on.
    let kernel = kernel();
    let cap = Capability::scoped([cap_read_graph(EMPTY), cap_write_graph(EMPTY)]);
    assert!(list(&kernel, &cap).unwrap().is_empty());
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            (
                "content",
                &format!("INSERT DATA {{ GRAPH <{EMPTY}> {{ <urn:s> <urn:p> \"v\" }} }}") as &str,
            ),
            ("graph", EMPTY),
        ],
        &cap,
    )
    .expect("the scoped write");
    assert_eq!(list(&kernel, &cap).unwrap(), vec![EMPTY.to_string()]);
}

#[test]
fn a_tenant_is_never_told_another_tenants_graph_exists() {
    // ⚠ THE property. `ZENITH` exists and is not granted, so nothing about it may appear
    // — not the name, not a count, not a placeholder. The listing is a subset of what the
    // caller's own grants already name, so existence is disclosed only where the caller
    // could have probed it anyway (`urn:iki:store:graph-ask` over its own graph).
    let cap = Capability::scoped([cap_read_graph(ACME)]);
    let text = issue(&kernel(), Verb::Source, GRAPHS, &[], &cap).unwrap();
    // Exactly the granted name and nothing else: no other name, and no count or summary
    // line that would say how much of the store is being withheld.
    assert_eq!(text, format!("{ACME}\n"));
}

// --------------------------------------------------------------- declared = enforced

#[test]
fn a_capability_holding_nothing_is_denied() {
    let err = list(&kernel(), &Capability::scoped(Vec::<String>::new())).unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
}

#[test]
fn a_write_only_capability_is_denied_rather_than_told_it_may_read_nothing() {
    // ⚠ Why the declared scope is `urn:cap:store:read*` and not `urn:cap:store:*`: the
    // wider spelling would let this caller past the kernel's pre-check and hand it an
    // empty listing, which reads like an answer. It is a denial.
    let cap = Capability::scoped([CAP_WRITE.to_string(), cap_write_graph(ACME)]);
    let err = list(&kernel(), &cap).unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
}

#[test]
fn both_halves_of_the_read_family_satisfy_the_declared_scope() {
    // The declaration is one token because `requires` is all-of and the requirement is
    // any-of. Both held forms must get in — that is the whole point of the spelling, and
    // this is what would fail if `CAP_READ_ANY` were narrowed to either half.
    let kernel = kernel();
    assert!(list(&kernel, &Capability::scoped([CAP_READ])).is_ok());
    assert!(list(&kernel, &Capability::scoped([cap_read_graph(ACME)])).is_ok());
    assert_eq!(CAP_READ_ANY, "urn:cap:store:read*");
}

#[test]
fn a_held_family_wildcard_enumerates_nothing() {
    // A *held* `urn:cap:store:read:graph:*` is a declaration form, not a grant: it says
    // what may be reached and names no graph. It satisfies the pre-check (it is under the
    // family prefix) and then lists nothing, which is the honest answer — guessing names
    // from a wildcard is not a thing this can do.
    let cap = Capability::scoped(["urn:cap:store:read:graph:*"]);
    assert!(list(&kernel(), &cap).unwrap().is_empty());
}

// ------------------------------------------------------------------- cacheability

#[test]
fn a_listing_is_cached_and_a_write_that_creates_a_graph_cuts_it() {
    // The root path's answer depends on which graphs exist, and a graph can only come into
    // existence through one of this crate's three write doors on a covered store. So the
    // answer is cacheable under exactly those three threads — no new thread is needed, and
    // this is the test that says the threads are really wired.
    let kernel = kernel();
    let cap = Capability::root();
    let request = Request::new(Verb::Source, Iri::parse(GRAPHS).unwrap());
    assert!(!kernel.is_cached(&request, &cap));
    assert_eq!(list(&kernel, &cap).unwrap().len(), 2);
    assert!(kernel.is_cached(&request, &cap), "the listing is cacheable");

    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:load",
        &[
            ("content", "<urn:s> <urn:p> \"v\" ."),
            ("graph", "urn:example:third"),
        ],
        &cap,
    )
    .expect("the load");
    assert!(
        !kernel.is_cached(&request, &cap),
        "`urn:iki:store:load` cut the thread"
    );
    assert_eq!(list(&kernel, &cap).unwrap().len(), 3);
}

#[test]
fn a_shared_store_forfeits_the_listing_the_way_it_forfeits_every_broad_read() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    drop(handle);
    let kernel = kernel_over(store);
    let cap = Capability::root();
    let request = Request::new(Verb::Source, Iri::parse(GRAPHS).unwrap());
    assert_eq!(list(&kernel, &cap).unwrap().len(), 2);
    assert!(
        !kernel.is_cached(&request, &cap),
        "an invisible writer can create a graph"
    );
}

#[test]
fn a_declaration_makes_a_tenants_listing_cacheable_and_leaves_roots_live() {
    // ★ The per-candidate rule, which exists nowhere else. A tenant's answer is a function
    // of exactly its own candidates' existence, so a sharer that cannot write any of them
    // cannot change it — even though root's answer over the same store is live, because a
    // sharer can always create a graph root would have to list.
    let (store, handle) =
        DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph()).unwrap();
    drop(handle);
    let kernel = kernel_over(store);
    let request = Request::new(Verb::Source, Iri::parse(GRAPHS).unwrap());

    let tenant = Capability::scoped([cap_read_graph(ACME)]);
    assert_eq!(list(&kernel, &tenant).unwrap(), vec![ACME.to_string()]);
    assert!(
        kernel.is_cached(&request, &tenant),
        "no sharer can write it"
    );

    let root = Capability::root();
    assert_eq!(list(&kernel, &root).unwrap().len(), 2);
    assert!(!kernel.is_cached(&request, &root));
}

#[test]
fn a_candidate_the_sharer_may_write_forfeits_the_whole_tenant_listing() {
    // ⚠ EVERY candidate, not any: one writable graph in the candidate set is enough for an
    // invisible write to change the answer, so the listing is live. This is the assertion
    // that would fail if the per-candidate `all` ever became an `any` — or a check of only
    // the first candidate.
    let (store, handle) = DurableStore::in_memory_shared_declaring(
        SharerWrites::only_the_default_graph().and_named_graph(ZENITH),
    )
    .unwrap();
    drop(handle);
    let kernel = kernel_over(store);
    let request = Request::new(Verb::Source, Iri::parse(GRAPHS).unwrap());

    let safe = Capability::scoped([cap_read_graph(ACME)]);
    assert!(list(&kernel, &safe).is_ok());
    assert!(kernel.is_cached(&request, &safe));

    let exposed = Capability::scoped([cap_read_graph(ACME), cap_read_graph(ZENITH)]);
    assert_eq!(list(&kernel, &exposed).unwrap().len(), 2);
    assert!(
        !kernel.is_cached(&request, &exposed),
        "one writable candidate makes the whole listing live"
    );
}

#[test]
fn the_cache_does_not_serve_one_capabilitys_listing_to_another() {
    // The kernel keys its cache on the capability fingerprint, which is what makes a
    // capability-dependent answer safe to cache at all. Asserted here rather than assumed:
    // this endpoint is the first in this crate whose BYTES differ by caller.
    let kernel = kernel();
    assert_eq!(list(&kernel, &Capability::root()).unwrap().len(), 2);
    let tenant = Capability::scoped([cap_read_graph(ACME)]);
    assert_eq!(list(&kernel, &tenant).unwrap(), vec![ACME.to_string()]);
}

// -------------------------------------------------------------------------- verbs

#[test]
fn only_source_is_answered() {
    let err = issue(
        &kernel(),
        Verb::Sink,
        GRAPHS,
        &[("content", "anything")],
        &Capability::root(),
    )
    .unwrap_err();
    assert!(!matches!(err, Error::Denied(_)), "{err:?}");
}
