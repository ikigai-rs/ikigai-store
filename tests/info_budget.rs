//! Whole-store info counts must obey the caller's time budget, including on shared stores.
use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{budget::TimeBudget, space, DurableStore, CAP_READ};
use std::{sync::Arc, time::Duration};

#[test]
fn info_refuses_a_count_past_the_callers_budget() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let data: String = (0..10_000)
        .map(|i| format!("<urn:s:{i}> <urn:p> <urn:o> .\n"))
        .collect();
    handle
        .load_from_slice(oxigraph::io::RdfFormat::NTriples, data.as_bytes())
        .unwrap();
    // A deliberately tiny host budget makes expiration deterministic at a small size.
    let store = store.with_time_budget(TimeBudget::new(Duration::from_nanos(1)));
    let kernel = Kernel::new(Arc::new(space(store)));
    let result = block_on(kernel.issue(
        Request::new(Verb::Source, Iri::parse("urn:iki:store:info").unwrap()),
        &Capability::scoped([CAP_READ.to_string()]),
    ));
    let Err(Error::Timeout(why)) = result else {
        panic!("expected a timeout, got {result:?}");
    };
    assert!(why.contains("counting quads"), "{why}");
    assert!(why.contains("with nothing answered"), "{why}");
    assert!(why.contains("urn:cap:store:budget:<ms>"), "{why}");
}

#[test]
fn info_validates_the_requests_budget() {
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    for value in ["0", "not a budget"] {
        let result = block_on(
            kernel.issue(
                Request::new(Verb::Source, Iri::parse("urn:iki:store:info").unwrap())
                    .with_arg("budget", ArgRef::Inline(value.as_bytes().to_vec())),
                &Capability::root(),
            ),
        );
        assert!(
            matches!(&result, Err(Error::InvalidArgument { name, .. }) if name == "budget"),
            "{result:?}"
        );
    }
}

#[test]
fn info_counts_default_and_named_quads_and_observes_shared_writes() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let kernel = Kernel::new(Arc::new(space(store)));
    for count in 0..=2 {
        let rep = block_on(
            kernel.issue(
                Request::new(Verb::Source, Iri::parse("urn:iki:store:info").unwrap())
                    .with_arg("budget", ArgRef::Inline(b"1000".to_vec())),
                &Capability::scoped([CAP_READ.to_string()]),
            ),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(rep.bytes).unwrap(),
            format!("backing: memory\nquads: {count}\ncovered: false\n")
        );
        let quad = if count == 0 {
            "<urn:s> <urn:p> <urn:o> ."
        } else {
            "<urn:s> <urn:p> <urn:o> <urn:g> ."
        };
        handle
            .load_from_slice(oxigraph::io::RdfFormat::NQuads, quad.as_bytes())
            .unwrap();
    }
}
