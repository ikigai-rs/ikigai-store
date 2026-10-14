//! A consumer's own `.depends_on(UPDATE_THREAD)` is decorative (ledger #150).
//!
//! The store's cacheable reads already declare all three write threads, and the kernel
//! carries a sub-request's threads up to whatever issued it. So a consumer endpoint that reads
//! through the store and answers `.cacheable()` is invalidated by a store write WITHOUT naming
//! any thread itself. `ikigai-ledger` found this by ablation (removing its own `depends_on`
//! calls left every freshness test green); this pins the store's half of that, so the doc
//! paragraph on [`ikigai_store::UPDATE_THREAD`] stays true.
//!
//! The composite counts its own invocations, so "cached" and "recomputed after a write" are
//! both observed, not inferred from a fresh answer that a never-cached read would also give.

use async_trait::async_trait;
use futures::executor::block_on;
use ikigai_core::{
    ArgRef, Capability, Description, Endpoint, Exact, Invocation, Iri, Kernel, ReprType,
    Representation, Request, Result, Verb,
};
use ikigai_store::{space, DurableStore};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const COMPOSITE: &str = "urn:example:composite";

/// Reads one ASK through the store and answers it, cacheable, declaring NO thread of its own.
struct Composite {
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Endpoint for Composite {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let answer = inv
            .issue(
                Request::new(Verb::Source, Iri::parse("urn:iki:store:ask").unwrap()).with_arg(
                    "query",
                    ArgRef::Inline(b"ASK { <urn:s> <urn:p> <urn:o> }".to_vec()),
                ),
            )
            .await?;
        // ★ The point: `.cacheable()` and nothing else. No `depends_on(UPDATE_THREAD)`.
        Ok(Representation::new(ReprType::new("text/plain"), answer.bytes).cacheable())
    }

    fn name(&self) -> &str {
        "composite"
    }

    fn describe(&self) -> Description {
        Description::new("composite").verb(Verb::Source)
    }
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

#[test]
fn a_store_write_invalidates_a_consumer_that_declares_no_thread_of_its_own() {
    let runs = Arc::new(AtomicUsize::new(0));
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap()).bind(
        Exact::new(COMPOSITE),
        Composite {
            runs: Arc::clone(&runs),
        },
    )));
    let read = || issue(&kernel, Verb::Source, COMPOSITE, &[]);

    assert!(read().contains("false"));
    assert!(read().contains("false"));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the second read was not served from the cache, so this test proves nothing"
    );

    // One write through each of the three doors, each followed by a read that must see it:
    // every door's thread reaches the consumer through the store's own declaration.
    for (door, args, expect, runs_after) in [
        (
            "urn:iki:store:update",
            vec![("content", "INSERT DATA { <urn:s> <urn:p> <urn:o> }")],
            "true",
            2,
        ),
        (
            "urn:iki:store:load",
            vec![
                ("content", "<urn:x> <urn:y> <urn:z> ."),
                ("format", "application/n-triples"),
            ],
            "true",
            3,
        ),
        (
            "urn:iki:store:update",
            vec![("content", "DELETE DATA { <urn:s> <urn:p> <urn:o> }")],
            "false",
            4,
        ),
    ] {
        issue(&kernel, Verb::Sink, door, &args);
        assert!(
            read().contains(expect),
            "stale after a write through {door}"
        );
        assert!(read().contains(expect));
        assert_eq!(
            runs.load(Ordering::SeqCst),
            runs_after,
            "after {door}: recomputed once, then cached again"
        );
    }
}
