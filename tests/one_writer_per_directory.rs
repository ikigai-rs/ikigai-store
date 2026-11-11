//! One writer per dataset directory, however the directory is spelled (ledger #3431).
//!
//! RocksDB refuses a second read-write open of a directory this process already holds by
//! looking the lock file's path STRING up in a registry of its own; the cross-process POSIX
//! lock cannot help inside one process. So a second open has to reach RocksDB under the same
//! string as the first, or it succeeds: before the fix every spelling below opened a second
//! writer over the held dataset, and a write through it did not show in the first handle's reads
//! (measured: the first counted 0 quads after the second wrote 1).
#![cfg(all(feature = "persistent", not(target_family = "wasm")))]

use ikigai_core::Error;
use ikigai_store::DurableStore;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ikigai-store-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// The spellings of `base/db` that are not the string `base/db`.
fn other_spellings(base: &std::path::Path) -> Vec<PathBuf> {
    let dir = base.join("db");
    let alias = base.join("alias");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dir, &alias).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&dir, &alias).unwrap();
    vec![
        PathBuf::from(format!("{}/", dir.display())),
        dir.join("."),
        base.join("db").join("..").join("db"),
        alias,
    ]
}

#[test]
fn a_held_directory_refuses_a_second_open_under_every_spelling() {
    let base = scratch("spellings");
    let dir = base.join("db");
    let first = DurableStore::open(&dir).unwrap();
    for spelling in other_spellings(&base) {
        match DurableStore::open(&spelling) {
            Err(Error::Unavailable(text)) => assert!(
                text.contains("one writer per directory"),
                "{}: the refusal must be the legible one: {text}",
                spelling.display()
            ),
            Err(other) => panic!(
                "{}: refused, but not as held: {other:?}",
                spelling.display()
            ),
            Ok(_) => panic!(
                "{} opened a SECOND writer over {}, which this process already holds",
                spelling.display(),
                dir.display()
            ),
        }
    }
    drop(first);
    // The positive half: once the holder is dropped, every spelling opens.
    for spelling in other_spellings_again(&base) {
        let store = DurableStore::open(&spelling)
            .unwrap_or_else(|e| panic!("{} after the holder dropped: {e:?}", spelling.display()));
        drop(store);
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// The same spellings, the symlink already in place.
fn other_spellings_again(base: &std::path::Path) -> Vec<PathBuf> {
    let dir = base.join("db");
    vec![
        dir.clone(),
        PathBuf::from(format!("{}/", dir.display())),
        dir.join("."),
        base.join("alias"),
    ]
}

/// The spelling is the host's: the backing reports the path it was given, not the canonical
/// one this crate opened.
#[test]
fn the_backing_names_the_path_the_host_gave() {
    let base = scratch("backing");
    let spelled = PathBuf::from(format!("{}/", base.join("db").display()));
    let store = DurableStore::open(&spelled).unwrap();
    assert_eq!(
        store.backing(),
        &ikigai_store::Backing::Durable(spelled.clone())
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&base);
}
