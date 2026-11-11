# Upstream issue draft: COPY/MOVE/ADD lose empty destination registration during lowering

Draft for Brian to post. No upstream issue has been filed.

## Versions and reproduction

Reproduced on 2026-11-11 against the current published releases:
[oxigraph 0.5.13](https://crates.io/crates/oxigraph/0.5.13) and
[spargebra 0.4.8](https://crates.io/crates/spargebra/0.4.8).
The standalone reproduction depends only on those two crates, with RocksDB disabled.

Cargo.toml:

```toml
[package]
name = "empty-transfer-reproduction"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
oxigraph = { version = "=0.5.13", default-features = false }
spargebra = "=0.4.8"
```

src/main.rs:

```rust
use oxigraph::model::NamedNode;
use oxigraph::sparql::SparqlEvaluator;
use oxigraph::store::Store;
use spargebra::SparqlParser;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let destination = NamedNode::new("urn:destination")?;
    let mut missing = Vec::new();
    for operation in ["COPY", "MOVE", "ADD"] {
        let store = Store::new()?;
        store.insert_named_graph(NamedNode::new("urn:empty")?.as_ref())?;
        let text = format!("{operation} GRAPH <urn:empty> TO GRAPH <urn:destination>");
        let parsed = SparqlParser::new().parse_update(&text)?;
        println!("{text}\n{}", parsed.to_sse());
        SparqlEvaluator::new()
            .for_update(parsed)
            .on_store(&store)
            .execute()?;
        let exists = store.contains_named_graph(destination.as_ref())?;
        println!("destination exists: {exists}");
        if !exists {
            missing.push(operation);
        }
    }
    assert!(missing.is_empty(), "transfers left no destination: {missing:?}");
    Ok(())
}
```

The checked-in copy can be run from this repository with:

```sh
cargo run --manifest-path reproductions/empty-transfer/Cargo.toml --target-dir target
```

It prints `destination exists: false` for COPY, MOVE, and ADD, then exits with code 101
because all three destinations are missing. This failure is intentional: it reproduces the
upstream defect independently of ikigai-store's workaround.

## Expected and actual behavior

After each successful transfer, `contains_named_graph(urn:destination)` should return true,
even though the source is empty. SPARQL 1.1 Update sections
[COPY](https://www.w3.org/TR/sparql11-update/#copy),
[MOVE](https://www.w3.org/TR/sparql11-update/#move), and
[ADD](https://www.w3.org/TR/sparql11-update/#add) require destination creation.
Oxigraph records empty named graphs, so registration is observable separately from triples.

In the reproduction the source is registered explicitly, so this is unrelated to the
specification's optional failure for a nonexistent source. The same failure occurs with an
empty default source. If the destination already contains a triple, COPY and MOVE delete
its registration as well as its contents; ADD retains that existing graph and its contents.
These additional cases are covered by ikigai-store's public-door regression tests.

## Reduction to parser lowering

The printed algebra shows COPY becoming DROP SILENT followed by INSERT/WHERE. MOVE adds
DROP of the source. ADD becomes INSERT/WHERE alone. None creates the named destination when
the insertion has no solutions.

The lowered algebra is identical to an explicitly written primitive sequence. An explicit
INSERT/WHERE with no matches must keep its existing behavior, so creating graphs for all
such insertion templates would be incorrect. Preserving a transfer operation or equivalent
destination-creation intent through parsing would let the evaluator distinguish these cases.
Self-transfers must remain identity operations.

## Local workaround and regression coverage

ikigai-store identifies original transfer syntax using the existing bounded lexical scan and
spargebra validation, retains the original AST, and inserts CREATE SILENT after the lowered
insertion for nonidentity transfers to named destinations. It runs inside the existing
transaction and confinement checks. This is a local workaround, not a proposed upstream API.

The three formerly ignored regressions now run normally. They cover named/default empty
sources and absent/existing destinations. Additional tests cover exact contents, identity,
operation order, rollback, explicit primitive sequences, scoped escape rejection, prefix/base
resolution, lexical ambiguity, blank-node identity, and Unicode feature unification. This is
targeted regression evidence, not a claim of general SPARQL conformance.
