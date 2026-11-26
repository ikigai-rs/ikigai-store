use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use oxigraph::store::Store;
use std::{env, fs, path::Path, time::Instant};
fn size(p: &Path) -> u64 {
    fs::read_dir(p)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum()
}
fn main() {
    let path = env::args().nth(1).unwrap();
    let n: usize = env::args().nth(2).unwrap().parse().unwrap();
    fs::create_dir_all(&path).unwrap();
    let path = Path::new(&path);
    let store = Store::open(path).unwrap();
    let p = NamedNode::new("urn:x:bulk-servable:p").unwrap();
    let start = Instant::now();
    let q = (0..n).map(|i| {
        Quad::new(
            NamedNode::new(format!(
                "urn:x:bulk-servable:s:{:020}",
                (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ))
            .unwrap(),
            p.clone(),
            Literal::from(i as i64),
            GraphName::DefaultGraph,
        )
    });
    let mut loader = store
        .bulk_loader()
        .with_num_threads(1)
        .with_max_memory_size_in_megabytes(10);
    loader.load_quads(q).unwrap();
    loader.commit().unwrap();
    println!(
        "load_seconds {:.3} return_bytes {}",
        start.elapsed().as_secs_f64(),
        size(path)
    );
    let start = Instant::now();
    store.optimize().unwrap();
    let optimize_seconds = start.elapsed().as_secs_f64();
    let bytes = size(path);
    let count = store.len().unwrap();
    assert_eq!(count, n, "the bulk load lost quads");
    println!("optimize_seconds {optimize_seconds:.3} bytes {bytes} count {count}");
}
