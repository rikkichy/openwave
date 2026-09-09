use indexmap::IndexMap;
use openwave_core::{
    model::*,
    scenes::{self, Scene, SceneId},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    fs,
    io::Write,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

fn prepare_atomic(path: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(OperationError::new(
                ErrorCode::Io,
                format!("Refusing nonregular state file: {}", path.display()),
            ));
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let parent = path
        .parent()
        .ok_or_else(|| OperationError::invalid("State path has no parent"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".openwave-")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    Ok(temporary)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    prepare_atomic(path, bytes)?
        .persist(path)
        .map_err(|error| OperationError::from(error.error))?;
    Ok(())
}

pub(crate) struct Prepared<T> {
    temporary: tempfile::NamedTempFile,
    value: T,
}

fn encoded(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}
fn serde_value<T: Serialize>(value: &T) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

pub struct Stored<T> {
    path: PathBuf,
    value: T,
    unreadable: Option<OperationError>,
    decode: fn(Value) -> Result<T>,
    encode: fn(&T) -> Result<Value>,
}
impl<T> Stored<T> {
    fn load(
        path: PathBuf,
        fallback: T,
        decode: fn(Value) -> Result<T>,
        encode: fn(&T) -> Result<Value>,
        seed_missing: bool,
    ) -> Self {
        let mut store = Self {
            path,
            value: fallback,
            unreadable: None,
            decode,
            encode,
        };
        let loaded = match fs::read(&store.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(OperationError::from)
                .and_then(decode)
                .map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        };
        match loaded {
            Ok(Some(value)) => store.value = value,
            Ok(None) if seed_missing => {
                let saved = (store.encode)(&store.value)
                    .and_then(|value| encoded(&value))
                    .and_then(|bytes| atomic_write(&store.path, &bytes));
                if let Err(error) = saved {
                    store.unreadable = Some(store.read_error(error));
                }
            }
            Ok(None) => {}
            Err(error) => store.unreadable = Some(store.read_error(error)),
        }
        store
    }
    fn read_error(&self, error: OperationError) -> OperationError {
        OperationError::new(
            ErrorCode::CorruptStore,
            format!("{} is read-only: {error}", self.path.display()),
        )
    }
    pub fn value(&self) -> &T {
        &self.value
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn error(&self) -> Option<&OperationError> {
        self.unreadable.as_ref()
    }
    pub fn writable(&self) -> bool {
        self.unreadable.is_none()
    }
    pub fn require_writable(&self) -> Result<()> {
        match &self.unreadable {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
    pub(crate) fn prepare(&self, value: T) -> Result<Prepared<T>> {
        self.require_writable()?;
        let value = (self.decode)((self.encode)(&value)?)?;
        let bytes = encoded(&(self.encode)(&value)?)?;
        Ok(Prepared {
            temporary: prepare_atomic(&self.path, &bytes)?,
            value,
        })
    }
    pub(crate) fn publish(&mut self, prepared: Prepared<T>) -> Result<()> {
        prepared
            .temporary
            .persist(&self.path)
            .map_err(|error| OperationError::from(error.error))?;
        self.value = prepared.value;
        Ok(())
    }
    pub fn replace(&mut self, value: T) -> Result<()> {
        let prepared = self.prepare(value)?;
        self.publish(prepared)
    }
}

pub struct ConfigStore {
    pub sources: Stored<Sources>,
    pub mixes: Stored<Mixes>,
    pub matrix: Stored<MatrixState>,
    pub preferences: Stored<Preferences>,
    pub scenes: Stored<IndexMap<SceneId, Scene>>,
}
impl ConfigStore {
    pub fn load(root: &Path) -> Self {
        Self {
            sources: Stored::load(
                root.join("sources.json"),
                Sources::new(),
                normalize_sources,
                serde_value,
                false,
            ),
            mixes: Stored::load(
                root.join("mixdefs.json"),
                default_mixes(),
                normalize_mixes,
                serde_value,
                true,
            ),
            matrix: Stored::load(
                root.join("mixes.json"),
                MatrixState::default(),
                MatrixState::from_value,
                MatrixState::to_value,
                false,
            ),
            preferences: Stored::load(
                root.join("ui-state.json"),
                Preferences::default(),
                Preferences::from_value,
                serde_value,
                false,
            ),
            scenes: Stored::load(
                root.join("scenes.json"),
                IndexMap::new(),
                scenes::decode_store,
                scenes::encode_store,
                false,
            ),
        }
    }
    pub fn routing_available(&self) -> bool {
        self.sources.writable() && self.mixes.writable() && self.matrix.writable()
    }
    pub fn desired(&self) -> DesiredState {
        DesiredState {
            sources: self.sources.value().clone(),
            mixes: if self.mixes.writable() {
                self.mixes.value().clone()
            } else {
                Mixes::new()
            },
            matrix: self.matrix.value().clone(),
            scenes: self.scenes.value().clone(),
        }
    }
    pub fn issues(&self) -> Vec<OperationIssue> {
        [
            self.sources.error(),
            self.mixes.error(),
            self.matrix.error(),
            self.preferences.error(),
            self.scenes.error(),
        ]
        .into_iter()
        .flatten()
        .map(|error| OperationIssue {
            target: "store".into(),
            message: error.to_string(),
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_store_edits_preserve_volume_coordinates_bindings_and_extensions() {
        let root = tempfile::tempdir().unwrap();
        let inputs = [
            (
                "sources.json",
                json!({"music":{
                    "kind":"app","name":"Music","match_app_name":" Player, Classical ",
                    "level":0.5,"muted":false,"custom":{"keep":true}
                }}),
            ),
            (
                "mixdefs.json",
                json!({"personal":{
                    "name":"Personal","sink":"openwave_personal_mix","custom":[1,2]
                }}),
            ),
            (
                "mixes.json",
                json!({
                    "music.personal":{"volume":0.4,"muted":false,"annotation":"send"},
                    "output":"headphones",
                    "volumes":{"personal":{"volume":0.5,"muted":false,"annotation":"master"}},
                    "custom":{"keep":true}
                }),
            ),
            (
                "scenes.json",
                json!({"scenes":{"quiet":{
                    "name":"Quiet",
                    "sources":{"music":{"level":0.5,"muted":false}},
                    "cells":{"music.personal":{"volume":0.4,"muted":false}},
                    "volumes":{"personal":{"volume":0.5,"muted":false}},
                    "outputs":{"personal":"headphones"}
                }}}),
            ),
        ];
        for (name, value) in &inputs {
            fs::write(root.path().join(name), serde_json::to_vec(value).unwrap()).unwrap();
        }
        let mut store = ConfigStore::load(root.path());
        assert!(store.routing_available());
        assert!(store.issues().is_empty());
        // Admission normalizes in memory; it does not rewrite any existing file.
        for (name, value) in &inputs {
            assert_eq!(
                fs::read(root.path().join(name)).unwrap(),
                serde_json::to_vec(value).unwrap()
            );
        }
        let mut sources = store.sources.value().clone();
        sources.get_mut("music").unwrap().name = "Renamed".into();
        store.sources.replace(sources).unwrap();
        let mut mixes = store.mixes.value().clone();
        mixes.get_mut("personal").unwrap().subtitle = "Updated".into();
        store.mixes.replace(mixes).unwrap();
        let mut matrix = store.matrix.value().clone();
        matrix.cells.get_mut("music.personal").unwrap().muted = true;
        store.matrix.replace(matrix).unwrap();
        let mut scenes = store.scenes.value().clone();
        scenes
            .get_mut(&SceneId::new("quiet").unwrap())
            .unwrap()
            .name = "Renamed scene".into();
        store.scenes.replace(scenes).unwrap();

        let saved = |name: &str| -> Value {
            serde_json::from_slice(&fs::read(root.path().join(name)).unwrap()).unwrap()
        };
        let sources = saved("sources.json");
        assert_eq!(sources["music"]["name"], "Renamed");
        assert_eq!(sources["music"]["level"], 0.5);
        assert_eq!(
            sources["music"]["match_app_names"],
            json!(["Player, Classical"])
        );
        assert!(sources["music"].get("match_app_name").is_none());
        assert_eq!(sources["music"]["custom"], json!({"keep":true}));
        let mixes = saved("mixdefs.json");
        assert_eq!(mixes["personal"]["subtitle"], "Updated");
        assert_eq!(mixes["personal"]["custom"], json!([1, 2]));
        let matrix = saved("mixes.json");
        assert_eq!(
            matrix["music.personal"],
            json!({"volume":0.4,"muted":true,"annotation":"send"})
        );
        assert_eq!(
            matrix["volumes"]["personal"],
            json!({"volume":0.5,"muted":false,"annotation":"master"})
        );
        assert_eq!(matrix["outputs"]["personal"], "headphones");
        assert!(matrix.get("output").is_none());
        assert_eq!(matrix["custom"], json!({"keep":true}));
        let mut expected_scene = inputs[3].1.clone();
        expected_scene["scenes"]["quiet"]["name"] = json!("Renamed scene");
        assert_eq!(saved("scenes.json"), expected_scene);
        assert!(ConfigStore::load(root.path()).routing_available());
    }

    #[test]
    fn missing_and_intentionally_empty_topology_remain_distinct() {
        let missing = tempfile::tempdir().unwrap();
        let seeded = ConfigStore::load(missing.path());
        assert_eq!(seeded.mixes.value(), &default_mixes());
        assert!(missing.path().join("mixdefs.json").exists());
        assert!(!missing.path().join("sources.json").exists());
        assert!(!missing.path().join("mixes.json").exists());

        let empty = tempfile::tempdir().unwrap();
        for name in ["sources.json", "mixdefs.json", "mixes.json"] {
            fs::write(empty.path().join(name), b"{}").unwrap();
        }
        let store = ConfigStore::load(empty.path());
        assert!(store.routing_available());
        assert!(store.desired().mixes.is_empty());
        for name in ["sources.json", "mixdefs.json", "mixes.json"] {
            assert_eq!(fs::read(empty.path().join(name)).unwrap(), b"{}");
        }
    }

    #[test]
    fn unreadable_routing_stores_refuse_replacement_and_preserve_original_bytes() {
        // Blank/malformed JSON, unsupported boolean coercion, ambiguous topology,
        // and the legacy output validation boundary are distinct refusal paths.
        for (name, bytes) in [
            ("sources.json", " \n"),
            ("mixes.json", "{broken"),
            ("sources.json", r#"{"music":{"muted":1}}"#),
            (
                "mixdefs.json",
                r#"{"one":{"sink":"openwave_shared"},"two":{"sink":"openwave_shared"}}"#,
            ),
            ("mixes.json", r#"{"output":""}"#),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join(name);
            fs::write(&path, bytes).unwrap();
            let mut store = ConfigStore::load(root.path());
            assert!(!store.routing_available(), "{name}: {bytes}");
            let error = match name {
                "sources.json" => store.sources.replace(Sources::new()).unwrap_err(),
                "mixdefs.json" => store.mixes.replace(default_mixes()).unwrap_err(),
                "mixes.json" => store.matrix.replace(MatrixState::default()).unwrap_err(),
                _ => unreachable!(),
            };
            assert_eq!(error.code, ErrorCode::CorruptStore);
            assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
        }
    }
}
