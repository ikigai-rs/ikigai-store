//! Where the durable dataset lives, stated in a file rather than in the environment.
//!
//! **Config home for the setting, data home for the bytes.** `store.toml` is a config
//! file and lives where config files live (`$XDG_CONFIG_HOME/ikigai`, else
//! `$HOME/.config/ikigai`); the dataset it names is data and defaults to
//! `~/.ikigai/store`. Both directories come from `ikigai_core::config`, which is the
//! one place in the ecosystem that spells them — eight local spellings is how the
//! previous divergence happened.
//!
//! **No environment variable names the path**, ever. An env var is the banned third
//! channel: it is invisible to `ikigai config`, it is not inherited by a launchd agent,
//! and two processes that disagree about it never meet.
//!
//! # The file
//!
//! ```toml
//! # ~/.config/ikigai/store.toml
//! path = "/Volumes/fast/ikigai-store"   # optional; default is ~/.ikigai/store
//! ```
//!
//! Layered the way every ikigai config is: `store.toml` states what every host shares,
//! and `<app>.store.toml` beside it overrides the keys one host differs on. Later wins,
//! key-wise. `deny_unknown_fields` on the layer type is what makes a misspelled key
//! loud instead of ignored.

use std::path::{Path, PathBuf};

use ikigai_core::config::{config_home, data_home, layered_paths_in};
use ikigai_core::{Error, Result};
use serde::Deserialize;

/// The config file stem. Not a path — [`layered_paths_in`] turns it into one.
pub const STEM: &str = "store.toml";

/// The dataset directory's name under the data home, when no config file names one.
pub const DEFAULT_DIR: &str = "store";

/// One layer of `store.toml`. Every field optional: a layer states only what it
/// overrides.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Layer {
    path: Option<PathBuf>,
}

/// Where this host's durable dataset lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    /// The dataset directory.
    pub path: PathBuf,
}

impl StoreConfig {
    /// Read the layered `store.toml` from the process's real config home, defaulting
    /// the path under the real data home.
    ///
    /// Fails loudly — never silently to a working-directory-relative guess — when
    /// neither home is known, which is the one case `ikigai_core::config` reports as
    /// `None` rather than guessing.
    pub fn load(app: Option<&str>) -> Result<StoreConfig> {
        let config = config_home().ok_or_else(|| {
            Error::Endpoint(
                "no ikigai config home: neither XDG_CONFIG_HOME nor HOME is set, so \
                 `store.toml` has no location to be read from"
                    .to_string(),
            )
        })?;
        let data = data_home().ok_or_else(|| {
            Error::Endpoint(
                "no ikigai data home: HOME is not set, so the durable store has no \
                 default location. Name one explicitly with `path` in store.toml"
                    .to_string(),
            )
        })?;
        Self::load_in(&config, &data, app)
    }

    /// [`load`](StoreConfig::load) rooted at explicit directories — the pure form, and
    /// the only testable one, since the process environment is global and `set_var`
    /// races the harness's own threads.
    pub fn load_in(config_home: &Path, data_home: &Path, app: Option<&str>) -> Result<StoreConfig> {
        let mut path: Option<PathBuf> = None;
        for layer in read_layers(config_home, app) {
            let (file, text) = layer?;
            let layer: Layer = toml::from_str(&text)
                .map_err(|e| Error::Endpoint(format!("parsing {}: {e}", file.display())))?;
            if let Some(p) = layer.path {
                if p.as_os_str().is_empty() {
                    return Err(Error::Endpoint(format!(
                        "{}: `path` is empty — a store directory of \"\" is the current \
                         working directory wearing a configuration's clothes",
                        file.display()
                    )));
                }
                // A relative path is resolved against the data home, not against the
                // working directory: a store that moves with `cd` is the failure this
                // module exists to refuse.
                path = Some(if p.is_absolute() {
                    p
                } else {
                    data_home.join(p)
                });
            }
        }
        Ok(StoreConfig {
            path: path.unwrap_or_else(|| data_home.join(DEFAULT_DIR)),
        })
    }
}

/// Read, lazily and lowest precedence first, the `store.toml` layers that exist under
/// `config_home`:
/// the crate's only ambient read of its configuration.
///
/// Confined by construction: it takes a config HOME, never a file path, and reads only the
/// [`layered_paths_in`] candidates for [`STEM`] inside it, so no caller can point it at any
/// other file. Only a host reaches it, through [`StoreConfig::load`] or
/// [`StoreConfig::load_in`], before it opens the store; no endpoint does. A configuration
/// read until a config-home resource exists (ledger #2400).
#[allow(clippy::disallowed_methods)] // ambient: fs: reads only the store.toml layer candidates inside the config home a host names, before it opens the store; no endpoint calls it (config-home resource: ledger #2400)
fn read_layers<'a>(
    config_home: &'a Path,
    app: Option<&'a str>,
) -> impl Iterator<Item = Result<(PathBuf, String)>> + 'a {
    layered_paths_in(config_home, STEM, app)
        .into_iter()
        .filter_map(|file| match std::fs::read_to_string(&file) {
            Ok(text) => Some(Ok((file, text))),
            // Absent is the ordinary case: the layers are candidates, not
            // requirements. Any OTHER io error is loud — a `store.toml` that exists
            // and cannot be read is a configuration the operator believes is in
            // effect.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some(Err(Error::Endpoint(format!(
                "reading {}: {e}",
                file.display()
            )))),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "ikigai-store-cfg-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn with_no_config_file_the_store_sits_under_the_data_home() {
        let config = scratch("none-c");
        let data = scratch("none-d");
        let got = StoreConfig::load_in(&config, &data, None).unwrap();
        assert_eq!(got.path, data.join("store"));
    }

    #[test]
    fn an_absolute_path_is_taken_as_written_and_a_relative_one_is_rooted_at_the_data_home() {
        let config = scratch("abs-c");
        let data = scratch("abs-d");
        std::fs::write(config.join(STEM), "path = \"/Volumes/fast/rdf\"\n").unwrap();
        assert_eq!(
            StoreConfig::load_in(&config, &data, None).unwrap().path,
            PathBuf::from("/Volumes/fast/rdf")
        );
        std::fs::write(config.join(STEM), "path = \"big\"\n").unwrap();
        assert_eq!(
            StoreConfig::load_in(&config, &data, None).unwrap().path,
            data.join("big")
        );
    }

    #[test]
    fn the_app_layer_overrides_the_shared_one() {
        let config = scratch("layer-c");
        let data = scratch("layer-d");
        std::fs::write(config.join(STEM), "path = \"/shared\"\n").unwrap();
        std::fs::write(config.join(format!("cms.{STEM}")), "path = \"/cms\"\n").unwrap();
        assert_eq!(
            StoreConfig::load_in(&config, &data, None).unwrap().path,
            PathBuf::from("/shared"),
            "an unnamed app reads only the shared layer"
        );
        assert_eq!(
            StoreConfig::load_in(&config, &data, Some("cms"))
                .unwrap()
                .path,
            PathBuf::from("/cms")
        );
    }

    #[test]
    fn a_misspelled_key_is_loud() {
        let config = scratch("typo-c");
        let data = scratch("typo-d");
        std::fs::write(config.join(STEM), "paths = \"/x\"\n").unwrap();
        let err = StoreConfig::load_in(&config, &data, None).unwrap_err();
        assert!(
            err.to_string().contains("paths"),
            "a key nobody reads must name itself: {err}"
        );
    }

    #[test]
    fn an_empty_path_is_refused_rather_than_resolved() {
        let config = scratch("empty-c");
        let data = scratch("empty-d");
        std::fs::write(config.join(STEM), "path = \"\"\n").unwrap();
        let err = StoreConfig::load_in(&config, &data, None).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    /// A `store.toml` that exists and cannot be read is loud and names the file: only
    /// ABSENCE is skipped. A directory at the file's name is the portable way to make a
    /// read fail with something other than NotFound.
    #[test]
    fn an_unreadable_layer_is_loud_and_names_the_file() {
        let config = scratch("unread-c");
        let data = scratch("unread-d");
        std::fs::create_dir_all(config.join(STEM)).unwrap();
        let err = StoreConfig::load_in(&config, &data, None).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("reading"), "{text}");
        assert!(text.contains(STEM), "the error must name the file: {text}");
    }

    /// Layers are read lazily, lowest precedence first, so the first broken layer is the
    /// one reported: a parse error in `store.toml` wins over an unreadable app layer.
    #[test]
    fn the_first_broken_layer_is_the_one_reported() {
        let config = scratch("order-c");
        let data = scratch("order-d");
        std::fs::write(config.join(STEM), "paths = \"/x\"\n").unwrap();
        std::fs::create_dir_all(config.join("cms.store.toml")).unwrap();
        let err = StoreConfig::load_in(&config, &data, Some("cms")).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("parsing") && text.contains("paths"),
            "the shared layer's error comes first: {text}"
        );
    }

    /// Only the `store.toml` candidates are read: another file in the config home, even
    /// one that would not parse, is never opened.
    #[test]
    fn only_store_toml_layers_are_read() {
        let config = scratch("only-c");
        let data = scratch("only-d");
        std::fs::write(config.join("other.toml"), "not toml at all [[[").unwrap();
        std::fs::create_dir_all(config.join("a11y.toml")).unwrap();
        let got = StoreConfig::load_in(&config, &data, Some("cms")).unwrap();
        assert_eq!(got.path, data.join(DEFAULT_DIR));
    }
}
