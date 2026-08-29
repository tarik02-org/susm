use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use susm_domain::ids::WorkloadId;
use thiserror::Error;

use crate::canonical::config_generation;
use crate::model::{CandidateConfig, WorkloadDefinition};
use crate::raw::{DefinitionError, RawDefinition};

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_WORKLOAD_FILES: usize = 4096;

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("configuration directory does not exist: {0}")]
    MissingDirectory(PathBuf),
    #[error("configuration entry is not a regular file: {0}")]
    NotRegularFile(PathBuf),
    #[error("configuration filename is not valid Unicode: {0}")]
    NonUnicodeFilename(PathBuf),
    #[error("invalid workload ID from {path}: {source}")]
    InvalidWorkloadId {
        path: PathBuf,
        #[source]
        source: susm_domain::ids::InvalidWorkloadId,
    },
    #[error("configuration file exceeds 1 MiB: {0}")]
    FileTooLarge(PathBuf),
    #[error("configuration contains more than 4,096 workload files")]
    TooManyFiles,
    #[error("configuration contains more than 16 MiB of TOML")]
    TooManyBytes,
    #[error("configuration file is not UTF-8: {path}: {source}")]
    InvalidUtf8 {
        path: PathBuf,
        #[source]
        source: std::str::Utf8Error,
    },
    #[error("invalid TOML in {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid workload definition in {path}: {source}")]
    Definition {
        path: PathBuf,
        #[source]
        source: DefinitionError,
    },
    #[error("duplicate workload ID {id:?} from {first} and {second}")]
    DuplicateId {
        id: String,
        first: PathBuf,
        second: PathBuf,
    },
    #[error("configuration changed while reload was reading it")]
    ChangedDuringRead,
}

pub fn ensure_directory(path: &Path) -> Result<(), LoadError> {
    fs::create_dir_all(path).map_err(|source| LoadError::Io {
        path: path.to_owned(),
        source,
    })
}

pub fn load_directory(path: &Path) -> Result<CandidateConfig, LoadError> {
    if !path.try_exists().map_err(|source| LoadError::Io {
        path: path.to_owned(),
        source,
    })? {
        return Err(LoadError::MissingDirectory(path.to_owned()));
    }

    let snapshots = enumerate(path)?;
    if snapshots.len() > MAX_WORKLOAD_FILES {
        return Err(LoadError::TooManyFiles);
    }
    let total_bytes = snapshots
        .values()
        .try_fold(0_u64, |total, snapshot| total.checked_add(snapshot.size))
        .ok_or(LoadError::TooManyBytes)?;
    if total_bytes > MAX_TOTAL_BYTES {
        return Err(LoadError::TooManyBytes);
    }

    let mut definitions = BTreeMap::<WorkloadId, WorkloadDefinition>::new();
    let mut id_sources = BTreeMap::<String, PathBuf>::new();
    for snapshot in snapshots.values() {
        let stem = snapshot
            .path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| LoadError::NonUnicodeFilename(snapshot.path.clone()))?;
        let id = WorkloadId::parse(stem).map_err(|source| LoadError::InvalidWorkloadId {
            path: snapshot.path.clone(),
            source,
        })?;
        let normalized_id = id.as_str().to_ascii_lowercase();
        if let Some(first) = id_sources.insert(normalized_id.clone(), snapshot.path.clone()) {
            return Err(LoadError::DuplicateId {
                id: normalized_id,
                first,
                second: snapshot.path.clone(),
            });
        }

        let bytes = fs::read(&snapshot.path).map_err(|source| LoadError::Io {
            path: snapshot.path.clone(),
            source,
        })?;
        if u64::try_from(bytes.len()).ok() != Some(snapshot.size) {
            return Err(LoadError::ChangedDuringRead);
        }
        let source = std::str::from_utf8(&bytes).map_err(|source| LoadError::InvalidUtf8 {
            path: snapshot.path.clone(),
            source,
        })?;
        let raw = RawDefinition::parse(source).map_err(|source| LoadError::Toml {
            path: snapshot.path.clone(),
            source,
        })?;
        let definition =
            raw.into_definition(id.clone())
                .map_err(|source| LoadError::Definition {
                    path: snapshot.path.clone(),
                    source,
                })?;
        definitions.insert(id, definition);
    }

    if snapshots != enumerate(path)? {
        return Err(LoadError::ChangedDuringRead);
    }

    let generation = config_generation(&definitions);
    Ok(CandidateConfig::new(generation, definitions))
}

fn enumerate(root: &Path) -> Result<BTreeMap<String, FileSnapshot>, LoadError> {
    let entries = fs::read_dir(root).map_err(|source| LoadError::Io {
        path: root.to_owned(),
        source,
    })?;
    let mut snapshots = BTreeMap::new();
    let mut case_folded = BTreeSet::new();

    for entry in entries {
        let entry = entry.map_err(|source| LoadError::Io {
            path: root.to_owned(),
            source,
        })?;
        let path = entry.path();
        if !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("toml"))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|source| LoadError::Io {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(LoadError::NotRegularFile(path));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(LoadError::FileTooLarge(path));
        }
        let filename = entry
            .file_name()
            .into_string()
            .map_err(|_| LoadError::NonUnicodeFilename(path.clone()))?;
        let folded = filename.to_uppercase();
        if !case_folded.insert(folded) {
            return Err(LoadError::ChangedDuringRead);
        }
        let modified = metadata.modified().map_err(|source| LoadError::Io {
            path: path.clone(),
            source,
        })?;
        snapshots.insert(
            filename,
            FileSnapshot {
                path,
                size: metadata.len(),
                modified,
            },
        );
    }

    Ok(snapshots)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileSnapshot {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}
