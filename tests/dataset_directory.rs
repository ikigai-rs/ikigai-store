//! The dataset directory: the crate's one ambient filesystem reach (Hypothesis H, ledger
//! #2425). `DurableStore::open` creates the directory a host names and opens that same
//! directory as the RocksDB dataset; these pin both halves and the sentence an operator
//! reads when the first half fails.
#![cfg(feature = "persistent")]

use ikigai_store::{Backing, DurableStore};
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ikigai-store-dir-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// A missing directory, parents included, is created and becomes the dataset.
#[test]
fn a_missing_nested_directory_is_created_and_opened_as_the_dataset() {
    let root = scratch("nested");
    let path = root.join("a").join("b").join("store");
    let store = DurableStore::open(&path).unwrap();
    assert!(path.is_dir(), "the dataset directory must exist after open");
    assert_eq!(store.backing(), &Backing::Durable(path.clone()));
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

/// A directory that cannot be created is refused with a sentence naming the path, before
/// RocksDB is asked anything.
#[test]
fn a_directory_that_cannot_be_created_is_refused_naming_the_path() {
    let root = scratch("blocked");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("not-a-dir");
    std::fs::write(&file, b"a file where a parent directory should be").unwrap();
    let path = file.join("store");

    let err = DurableStore::open(&path).unwrap_err();
    assert!(
        matches!(err, ikigai_core::Error::Endpoint(_)),
        "an uncreatable directory is a configuration error, not a transient one: {err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("cannot create the store directory"),
        "the refusal must say which step failed: {text}"
    );
    assert!(
        text.contains(&path.display().to_string()),
        "the refusal must name the path: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
