//! The PEAK MEMORY of one `urn:iki:store:dump` over a SYNTHETIC RocksDB store (ledger #1732).
//!
//! ```text
//! cargo run --release --features persistent --example dump-memory -- seed <store-dir> <quads>
//! /usr/bin/time -l target/release/examples/dump-memory dump <store-dir> [<out-file>]
//! /usr/bin/time -l target/release/examples/dump-memory baseline <store-dir>
//! /usr/bin/time -l target/release/examples/dump-memory scan <store-dir>
//! ```
//!
//! `seed` builds the store: `<quads>` quads shaped like `ikigai-gonk`'s live dataset (about 93%
//! in one named graph beside a second, about 225 bytes of N-Quads each, literals that do not all
//! compress alike), loaded in pieces under the store's own load cap, and then FLUSHED, so the
//! measurement that follows does not also pay for replaying a write-ahead log. `dump` opens the
//! store owned, binds it into a kernel and asks for one dump as root (the answer ceiling: 10M
//! quads, 1 GiB), writing it to `<out-file>` when given so two builds' dumps can be compared with
//! `cmp`; `baseline` does everything `dump` does except the dump, so the difference between
//! their peaks is the dump's; `scan` reads every quad through the store's `SnapshotReader` and
//! keeps none, so the difference between `scan` and `dump` is what the dump HOLDS, and between
//! `baseline` and `scan` what RocksDB itself takes to read the dataset once. Nothing here opens a
//! real store.
//!
//! It is the same shape as `ikigai-gonk`'s `examples/backup-memory.rs` (gonk PR 178), so the two
//! measurements are comparable.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};

const USAGE: &str = "usage: dump-memory seed <store-dir> <quads> | dump <store-dir> [<out-file>] \
                     | baseline <store-dir> | scan <store-dir>";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["seed", dir, quads] => seed(Path::new(dir), quads.parse().expect("a quad count")),
        ["dump", dir] => run(Path::new(dir), true, None),
        ["dump", dir, out] => run(Path::new(dir), true, Some(Path::new(out))),
        ["baseline", dir] => run(Path::new(dir), false, None),
        ["scan", dir] => scan(Path::new(dir)),
        _ => panic!("{USAGE}"),
    }
}

/// One synthetic quad, the `n`th: deterministic, so two seeds of one size are one dataset.
fn quad(n: u64, out: &mut String) {
    let graph = if n.is_multiple_of(14) {
        "urn:iki:ledger:graph:default"
    } else {
        "urn:iki:browse:graph:default"
    };
    let mixed = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    out.push_str(&format!(
        "<urn:item:{}> <urn:ik:p{}> \"{n} {mixed:016x}{:016x} {}\" <{graph}> .\n",
        n / 8,
        n % 8,
        mixed.rotate_left(17) ^ n,
        "x".repeat(120)
    ));
}

fn issue(kernel: &Kernel, verb: Verb, iri: &str, args: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.clone())),
    );
    block_on(kernel.issue(request, &Capability::root()))
        .unwrap_or_else(|e| panic!("{iri}: {e}"))
        .bytes
}

fn seed(dir: &Path, quads: u64) {
    // Shared only so the seed can flush the store's write-ahead log when it is done.
    let (store, handle) = DurableStore::open_shared(dir).expect("the scratch store opens");
    let kernel = Kernel::new(Arc::new(space(store)));
    let started = Instant::now();
    let mut n = 0;
    while n < quads {
        let mut chunk = String::new();
        for i in n..(n + 200_000).min(quads) {
            quad(i, &mut chunk);
        }
        n = (n + 200_000).min(quads);
        issue(
            &kernel,
            Verb::Sink,
            "urn:iki:store:load",
            &[
                ("content", chunk.into_bytes()),
                ("format", b"application/n-quads".to_vec()),
            ],
        );
    }
    handle.flush().expect("the store flushes");
    println!("seeded {quads} quads in {:.2?}", started.elapsed());
}

fn run(dir: &Path, take: bool, out: Option<&Path>) {
    let store = DurableStore::open(dir).expect("the scratch store opens");
    let kernel = Kernel::new(Arc::new(space(store)));
    let started = Instant::now();
    if !take {
        let info = issue(&kernel, Verb::Source, "urn:iki:store:graphs", &[]);
        println!(
            "baseline in {:.2?}: {} graph(s)",
            started.elapsed(),
            String::from_utf8_lossy(&info).lines().count()
        );
        return;
    }
    let dump = issue(&kernel, Verb::Source, "urn:iki:store:dump", &[]);
    let elapsed = started.elapsed();
    let lines = dump.iter().filter(|b| **b == b'\n').count();
    println!("dump in {elapsed:.2?}: {lines} quads, {} bytes", dump.len());
    if let Some(out) = out {
        std::fs::write(out, &dump).expect("the dump is written");
    }
}

fn scan(dir: &Path) {
    let store = DurableStore::open(dir).expect("the scratch store opens");
    let reader = store.reader();
    let kernel = Kernel::new(Arc::new(space(store)));
    let started = Instant::now();
    let mut quads = 0u64;
    for quad in reader.snapshot_iter() {
        quad.expect("a quad");
        quads += 1;
    }
    println!("scan in {:.2?}: {quads} quads", started.elapsed());
    drop(kernel);
}
