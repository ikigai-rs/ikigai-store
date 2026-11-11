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
    assert!(
        missing.is_empty(),
        "transfers left no destination: {missing:?}"
    );
    Ok(())
}
