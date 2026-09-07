use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use windows::{
    Win32::Storage::FileSystem::{
        MOVE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    },
    core::PCWSTR,
};

const REQUIRED_FILES: [&str; 3] = ["susm.exe", "susmd.exe", "susm-supervisor.exe"];
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: u64 = 768 * 1024 * 1024;
const MAX_ZIP_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid bundle manifest: {0}")]
    Manifest(#[from] toml::de::Error),
    #[error("invalid bundle version: {0}")]
    Version(#[from] semver::Error),
    #[error("unsupported bundle: {0}")]
    Invalid(String),
    #[error("cannot open ZIP bundle: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("atomic selection replacement failed: {0}")]
    Windows(#[from] windows::core::Error),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    bundle_format: u32,
    version: String,
    target: String,
    protocol_major: u32,
    controller_schema_read_min: u32,
    controller_schema_read_max: u32,
    controller_schema_write: u32,
    supervisor_runtime_formats: Vec<u32>,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    bundle_format: u32,
    version: String,
    manifest_sha256: String,
}

pub struct InstalledVersion {
    pub version: String,
    pub identity: String,
    pub path: PathBuf,
    pub current: bool,
    pub pin_count: usize,
}

pub fn install(source: &Path) -> Result<InstalledVersion, InstallError> {
    let source_metadata = metadata(source)?;
    if source_metadata.file_type().is_symlink() {
        return Err(InstallError::Invalid(
            "bundle source must not be a symbolic link or junction".to_owned(),
        ));
    }
    let extracted;
    let root = if source.is_dir() {
        source
    } else if source
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        if metadata(source)?.len() > MAX_ZIP_BYTES {
            return Err(InstallError::Invalid("ZIP exceeds 1 GiB".to_owned()));
        }
        extracted = tempfile::tempdir().map_err(|source_error| io_error(source, source_error))?;
        extract_zip(source, extracted.path())?;
        extracted.path()
    } else {
        return Err(InstallError::Invalid(
            "bundle must be a directory or .zip file".to_owned(),
        ));
    };
    validate_and_install(root)
}

pub fn list_versions() -> Result<Vec<InstalledVersion>, InstallError> {
    let installation = installation_root()?;
    let root = installation.join("versions");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut versions = Vec::new();
    for entry in fs::read_dir(&root).map_err(|source| io_error(&root, source))? {
        let entry = entry.map_err(|source| io_error(&root, source))?;
        if !entry
            .file_type()
            .map_err(|source| io_error(&entry.path(), source))?
            .is_dir()
        {
            continue;
        }
        if let Some((version, identity)) = entry.file_name().to_string_lossy().rsplit_once('-') {
            let directory_name = entry.file_name();
            let pins = installation.join("pins").join(&directory_name);
            versions.push(InstalledVersion {
                version: version.to_owned(),
                identity: identity.to_owned(),
                path: entry.path(),
                current: current_selection(&installation)?
                    .is_some_and(|selection| selection == directory_name),
                pin_count: directory_entry_count(&pins)?,
            });
        }
    }
    versions.sort_by(|left, right| left.version.cmp(&right.version));
    Ok(versions)
}

pub fn uninstall_user() -> Result<(), InstallError> {
    let bin = installation_root()?.join("bin");
    if !bin.is_dir() {
        return Ok(());
    }
    for name in REQUIRED_FILES {
        let path = bin.join(name);
        if path
            .try_exists()
            .map_err(|source| io_error(&path, source))?
        {
            fs::remove_file(&path).map_err(|source| io_error(&path, source))?;
        }
    }
    Ok(())
}

pub fn rollback(prefix: &str) -> Result<InstalledVersion, InstallError> {
    let matches = list_versions()?
        .into_iter()
        .filter(|version| version.version == prefix || version.identity.starts_with(prefix))
        .collect::<Vec<_>>();
    let [selected] = matches.as_slice() else {
        return Err(InstallError::Invalid(if matches.is_empty() {
            format!("no installed version matches {prefix}")
        } else {
            format!("installed version prefix {prefix} is ambiguous")
        }));
    };
    validate_and_install(&selected.path)
}

pub fn garbage_collect() -> Result<usize, InstallError> {
    let installation = installation_root()?;
    let versions = installation.join("versions");
    if !versions.is_dir() {
        return Ok(0);
    }
    let mut removed = 0;
    let current = current_selection(&installation)?;
    for entry in fs::read_dir(&versions).map_err(|source| io_error(&versions, source))? {
        let entry = entry.map_err(|source| io_error(&versions, source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(&entry.path(), source))?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".staging-") {
            fs::remove_dir_all(entry.path()).map_err(|source| io_error(&entry.path(), source))?;
            removed += 1;
            continue;
        }
        if current.as_ref() == Some(&name)
            || directory_entry_count(&installation.join("pins").join(&name))? != 0
        {
            continue;
        }
        validate_installed_directory(&entry.path())?;
        fs::remove_dir_all(entry.path()).map_err(|source| io_error(&entry.path(), source))?;
        let pins = installation.join("pins").join(&name);
        if pins.is_dir() {
            fs::remove_dir(&pins).map_err(|source| io_error(&pins, source))?;
        }
        removed += 1;
    }
    Ok(removed)
}

fn validate_and_install(root: &Path) -> Result<InstalledVersion, InstallError> {
    validate_source_root(root)?;
    let manifest_path = root.join("manifest.toml");
    let manifest_bytes = bounded_read(&manifest_path, MAX_MANIFEST_BYTES)?;
    let manifest: Manifest = toml::from_str(
        std::str::from_utf8(&manifest_bytes)
            .map_err(|_| InstallError::Invalid("manifest is not UTF-8".to_owned()))?,
    )?;
    validate_manifest(&manifest)?;
    let version = manifest.version.parse::<semver::Version>()?;
    if !version.build.is_empty() {
        return Err(InstallError::Invalid(
            "bundle version must not contain build metadata".to_owned(),
        ));
    }
    let identity = hex(&Sha256::digest(&manifest_bytes));
    let files = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let mut total = 0_u64;
    for name in REQUIRED_FILES {
        let declared = files
            .get(name)
            .ok_or_else(|| InstallError::Invalid(format!("manifest does not declare {name}")))?;
        if declared.size > MAX_FILE_BYTES {
            return Err(InstallError::Invalid(format!("{name} exceeds 256 MiB")));
        }
        total = total
            .checked_add(declared.size)
            .ok_or_else(|| InstallError::Invalid("payload size overflowed".to_owned()))?;
        let path = root.join(name);
        let actual = metadata(&path)?.len();
        if actual != declared.size {
            return Err(InstallError::Invalid(format!(
                "{name} size is {actual}, expected {}",
                declared.size
            )));
        }
        if digest_file(&path)? != declared.sha256 {
            return Err(InstallError::Invalid(format!(
                "{name} SHA-256 does not match"
            )));
        }
    }
    if total > MAX_PAYLOAD_BYTES {
        return Err(InstallError::Invalid("payload exceeds 768 MiB".to_owned()));
    }

    let installation = installation_root()?;
    let versions = installation.join("versions");
    fs::create_dir_all(&versions).map_err(|source| io_error(&versions, source))?;
    let final_path = versions.join(format!("{}-{identity}", manifest.version));
    if final_path.is_dir() {
        validate_installed_directory(&final_path)?;
    } else {
        let staging = versions.join(format!(".staging-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&staging).map_err(|source| io_error(&staging, source))?;
        fs::write(staging.join("manifest.toml"), &manifest_bytes)
            .map_err(|source| io_error(&staging.join("manifest.toml"), source))?;
        for name in REQUIRED_FILES {
            fs::copy(root.join(name), staging.join(name))
                .map_err(|source| io_error(&staging.join(name), source))?;
        }
        fs::rename(&staging, &final_path).map_err(|source| io_error(&final_path, source))?;
    }
    let selection = format!(
        "bundle_format = 1\nversion = {:?}\nmanifest_sha256 = {:?}\n",
        manifest.version, identity
    );
    atomic_write(
        &installation.join("current.susm-install"),
        selection.as_bytes(),
    )?;
    let bin = installation.join("bin");
    fs::create_dir_all(&bin).map_err(|source| io_error(&bin, source))?;
    let cli = "susm.exe";
    let bytes = bounded_read(&final_path.join(cli), MAX_FILE_BYTES)?;
    atomic_write(&bin.join(cli), &bytes)?;
    Ok(InstalledVersion {
        version: manifest.version,
        identity,
        path: final_path,
        current: true,
        pin_count: 0,
    })
}

fn validate_source_root(root: &Path) -> Result<(), InstallError> {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(root).map_err(|source| io_error(root, source))? {
        let entry = entry.map_err(|source| io_error(root, source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(&entry.path(), source))?;
        if !file_type.is_file() || file_type.is_symlink() {
            return Err(InstallError::Invalid(format!(
                "bundle entry {} is not a regular file",
                entry.path().display()
            )));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| InstallError::Invalid("bundle filename is not Unicode".to_owned()))?;
        if name != "manifest.toml" && !REQUIRED_FILES.contains(&name.as_str()) {
            return Err(InstallError::Invalid(format!(
                "bundle contains unexpected file {name}"
            )));
        }
        names.insert(name.to_uppercase());
    }
    if names.len() != REQUIRED_FILES.len() + 1
        || !names.contains("MANIFEST.TOML")
        || REQUIRED_FILES
            .iter()
            .any(|name| !names.contains(&name.to_uppercase()))
    {
        return Err(InstallError::Invalid(
            "bundle must contain exactly the manifest and three executables".to_owned(),
        ));
    }
    Ok(())
}

fn validate_installed_directory(path: &Path) -> Result<(), InstallError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| InstallError::Invalid("installed directory name is invalid".to_owned()))?;
    let (_, identity) = name.rsplit_once('-').ok_or_else(|| {
        InstallError::Invalid("installed directory has no manifest identity".to_owned())
    })?;
    let manifest_path = path.join("manifest.toml");
    let manifest_bytes = bounded_read(&manifest_path, MAX_MANIFEST_BYTES)?;
    if hex(&Sha256::digest(&manifest_bytes)) != identity {
        return Err(InstallError::Invalid(format!(
            "installed manifest identity does not match {name}"
        )));
    }
    let manifest: Manifest = toml::from_str(
        std::str::from_utf8(&manifest_bytes)
            .map_err(|_| InstallError::Invalid("manifest is not UTF-8".to_owned()))?,
    )?;
    validate_manifest(&manifest)?;
    let files = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    for name in REQUIRED_FILES {
        let declared = files
            .get(name)
            .ok_or_else(|| InstallError::Invalid(format!("manifest does not declare {name}")))?;
        let file = path.join(name);
        let metadata = metadata(&file)?;
        if !metadata.is_file()
            || metadata.len() != declared.size
            || digest_file(&file)? != declared.sha256
        {
            return Err(InstallError::Invalid(format!(
                "installed {name} does not match its manifest"
            )));
        }
    }
    Ok(())
}

fn current_selection(installation: &Path) -> Result<Option<OsString>, InstallError> {
    let path = installation.join("current.susm-install");
    if !path
        .try_exists()
        .map_err(|source| io_error(&path, source))?
    {
        return Ok(None);
    }
    let bytes = bounded_read(&path, MAX_MANIFEST_BYTES)?;
    let selection: Selection = toml::from_str(
        std::str::from_utf8(&bytes)
            .map_err(|_| InstallError::Invalid("selection is not UTF-8".to_owned()))?,
    )?;
    if selection.bundle_format != 1
        || selection.version.parse::<semver::Version>().is_err()
        || selection.manifest_sha256.len() != 64
        || !selection
            .manifest_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(InstallError::Invalid(
            "current selection is invalid".to_owned(),
        ));
    }
    Ok(Some(OsString::from(format!(
        "{}-{}",
        selection.version, selection.manifest_sha256
    ))))
}

fn directory_entry_count(path: &Path) -> Result<usize, InstallError> {
    if !path.try_exists().map_err(|source| io_error(path, source))? {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(InstallError::Invalid(format!(
            "pin path {} is not a regular directory",
            path.display()
        )));
    }
    fs::read_dir(path)
        .map_err(|source| io_error(path, source))?
        .try_fold(0_usize, |count, entry| {
            entry
                .map(|_| count.saturating_add(1))
                .map_err(|source| io_error(path, source))
        })
}

fn validate_manifest(manifest: &Manifest) -> Result<(), InstallError> {
    if manifest.bundle_format != 1 || manifest.protocol_major != 1 {
        return Err(InstallError::Invalid(
            "bundle_format and protocol_major must be 1".to_owned(),
        ));
    }
    if manifest.controller_schema_read_min > manifest.controller_schema_read_max
        || !(manifest.controller_schema_read_min..=manifest.controller_schema_read_max)
            .contains(&manifest.controller_schema_write)
    {
        return Err(InstallError::Invalid(
            "controller schema compatibility range is invalid".to_owned(),
        ));
    }
    if !manifest.supervisor_runtime_formats.contains(&1) {
        return Err(InstallError::Invalid(
            "supervisor runtime format 1 is required".to_owned(),
        ));
    }
    let expected_target = match std::env::consts::ARCH {
        "x86_64" => "x86_64-pc-windows-msvc",
        "aarch64" => "aarch64-pc-windows-msvc",
        architecture => {
            return Err(InstallError::Invalid(format!(
                "unsupported native architecture {architecture}"
            )));
        }
    };
    if manifest.target != expected_target {
        return Err(InstallError::Invalid(format!(
            "bundle target {} does not match {expected_target}",
            manifest.target
        )));
    }
    if manifest.files.len() != REQUIRED_FILES.len() {
        return Err(InstallError::Invalid(
            "manifest must declare exactly the three SUSM executables".to_owned(),
        ));
    }
    let mut names = manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    let mut required = REQUIRED_FILES;
    required.sort_unstable();
    if names != required {
        return Err(InstallError::Invalid(
            "manifest contains an unexpected file set".to_owned(),
        ));
    }
    for file in &manifest.files {
        if file.sha256.len() != 64
            || !file
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(InstallError::Invalid(format!(
                "{} has an invalid SHA-256",
                file.path
            )));
        }
    }
    Ok(())
}

fn extract_zip(source: &Path, destination: &Path) -> Result<(), InstallError> {
    let file = File::open(source).map_err(|source_error| io_error(source, source_error))?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut total = 0_u64;
    let mut names = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| InstallError::Invalid("ZIP contains an unsafe path".to_owned()))?;
        if enclosed.components().count() != 1
            || enclosed
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(InstallError::Invalid("ZIP paths must be flat".to_owned()));
        }
        let name = enclosed
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| InstallError::Invalid("ZIP filename is not Unicode".to_owned()))?;
        if name != "manifest.toml" && !REQUIRED_FILES.contains(&name) {
            return Err(InstallError::Invalid(format!(
                "ZIP contains undeclared file {name}"
            )));
        }
        if !names.insert(name.to_uppercase()) {
            return Err(InstallError::Invalid(format!(
                "ZIP contains duplicate path {name}"
            )));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| InstallError::Invalid("ZIP payload size overflowed".to_owned()))?;
        if total > MAX_PAYLOAD_BYTES + MAX_MANIFEST_BYTES {
            return Err(InstallError::Invalid("ZIP payload is too large".to_owned()));
        }
        let output = destination.join(name);
        let mut target =
            File::create(&output).map_err(|source_error| io_error(&output, source_error))?;
        io::copy(&mut entry, &mut target)
            .map_err(|source_error| io_error(&output, source_error))?;
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), InstallError> {
    let parent = path
        .parent()
        .ok_or_else(|| InstallError::Invalid("installation path has no parent".to_owned()))?;
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::now_v7()
    ));
    let mut file = File::create(&temporary).map_err(|source| io_error(&temporary, source))?;
    file.write_all(bytes)
        .map_err(|source| io_error(&temporary, source))?;
    file.sync_all()
        .map_err(|source| io_error(&temporary, source))?;
    drop(file);
    if path.try_exists().map_err(|source| io_error(path, source))? {
        let source = wide(&temporary);
        let destination = wide(path);
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(destination.as_ptr()),
                MOVE_FILE_FLAGS(MOVEFILE_REPLACE_EXISTING.0 | MOVEFILE_WRITE_THROUGH.0),
            )?;
        }
    } else {
        fs::rename(&temporary, path).map_err(|source| io_error(path, source))?;
    }
    Ok(())
}

fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>, InstallError> {
    let size = metadata(path)?.len();
    if size > maximum {
        return Err(InstallError::Invalid(format!(
            "{} exceeds its size limit",
            path.display()
        )));
    }
    fs::read(path).map_err(|source| io_error(path, source))
}

fn digest_file(path: &Path) -> Result<String, InstallError> {
    let mut file = File::open(path).map_err(|source| io_error(path, source))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| io_error(path, source))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex(&digest.finalize()))
}

fn metadata(path: &Path) -> Result<fs::Metadata, InstallError> {
    fs::symlink_metadata(path).map_err(|source| io_error(path, source))
}

fn installation_root() -> Result<PathBuf, InstallError> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("Programs").join("susm"))
        .ok_or_else(|| InstallError::Invalid("LOCALAPPDATA is missing".to_owned()))
}

fn io_error(path: &Path, source: io::Error) -> InstallError {
    InstallError::Io {
        path: path.to_owned(),
        source,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain([0]).collect()
}
