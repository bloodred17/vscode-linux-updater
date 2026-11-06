use crate::{
    config::Config,
    install::{self, Result},
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const MANIFEST: &str = "vscu-package.json";
const ARCHIVE: &str = "archive.tar.gz";
const FORMAT: &str = "vscu-download-v1";

/// Serializes access to a shared cache and cleans incomplete downloads on drop.
pub struct DownloadCache {
    root: PathBuf,
    max_count: usize,
    pending: TempDir,
    _lock: File,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Package {
    format: String,
    version: String,
    commit: String,
    checksum: String,
}

impl Package {
    fn directory_name(&self) -> String {
        format!("vscu-{}-{}", self.version, self.commit)
    }

    fn read(path: &Path) -> Option<Self> {
        // Only recognize our named directories with a matching ownership manifest.
        if !fs::symlink_metadata(path).ok()?.is_dir()
            || !fs::symlink_metadata(path.join(MANIFEST)).ok()?.is_file()
            || !fs::symlink_metadata(path.join(ARCHIVE)).ok()?.is_file()
        {
            return None;
        }
        let package: Self = serde_json::from_reader(File::open(path.join(MANIFEST)).ok()?).ok()?;
        if package.format != FORMAT
            || version_key(&package.version).is_err()
            || package.commit.len() != 40
            || !package.commit.bytes().all(|b| b.is_ascii_hexdigit())
            || install::validate_checksum(&package.checksum).is_err()
            || path.file_name()?.to_str()? != package.directory_name()
        {
            return None;
        }
        Some(package)
    }
}

impl DownloadCache {
    pub fn prepare(config: &Config) -> Result<Self> {
        if !config.download_dir.is_absolute() || config.max_cache_count == 0 {
            return Err("Cache requires an absolute directory and max_cache_count >= 1".into());
        }
        install::ensure_directory(&config.download_dir)?;
        let lock_path = config.download_dir.join("cache.lock");
        if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| !metadata.is_file()) {
            return Err("The cache lock path is not a regular file".into());
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        FileExt::try_lock_exclusive(&lock).map_err(|error| {
            format!(
                "Cannot acquire download cache lock (another operation may be running): {error}"
            )
        })?;
        let pending = tempfile::Builder::new()
            .prefix(".download-")
            .tempdir_in(&config.download_dir)?;
        Ok(Self {
            root: config.download_dir.clone(),
            max_count: config.max_cache_count,
            pending,
            _lock: lock,
        })
    }

    pub fn download_path(&self) -> PathBuf {
        self.pending.path().join(ARCHIVE)
    }

    pub fn lookup(&self, requested: &str, commit: &str, checksum: &str) -> Result<Option<PathBuf>> {
        for (path, package) in self.packages()? {
            if package.commit.eq_ignore_ascii_case(commit)
                && package.checksum.eq_ignore_ascii_case(checksum)
                && (requested == "latest" || package.version == requested)
            {
                let archive = path.join(ARCHIVE);
                if install::verify_checksum(&archive, checksum).is_ok() {
                    return Ok(Some(archive));
                }
                eprintln!("Cached package failed verification; downloading a fresh copy");
            }
        }
        Ok(None)
    }

    /// Called only after checksum, extraction and application metadata validation.
    pub fn retain(
        &self,
        source: &Path,
        version: &str,
        commit: &str,
        checksum: &str,
    ) -> Result<PathBuf> {
        version_key(version)?;
        install::validate_checksum(checksum)?;
        if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("Invalid cache build commit".into());
        }
        let package = Package {
            format: FORMAT.into(),
            version: version.into(),
            commit: commit.to_ascii_lowercase(),
            checksum: checksum.to_ascii_lowercase(),
        };
        let destination = self.root.join(package.directory_name());
        let archive = destination.join(ARCHIVE);
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                let existing = Package::read(&destination).ok_or_else(|| {
                    format!(
                        "Refusing to replace an unrecognized cache entry: {}",
                        destination.display()
                    )
                })?;
                if !existing.checksum.eq_ignore_ascii_case(checksum) {
                    return Err("Cached build checksum differs from the resolved release".into());
                }
                // Replace a corrupt, owned archive only after its replacement was validated.
                if source != archive {
                    fs::rename(source, &archive)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let staging = tempfile::Builder::new()
                    .prefix(".package-")
                    .tempdir_in(&self.root)?;
                fs::write(
                    staging.path().join(MANIFEST),
                    serde_json::to_vec_pretty(&package)?,
                )?;
                fs::rename(source, staging.path().join(ARCHIVE))?;
                fs::rename(staging.path(), &destination)?;
            }
            Err(error) => return Err(error.into()),
        }
        self.prune(&destination, version)?;
        println!("Cached VS Code {version} at {}", archive.display());
        Ok(archive)
    }

    fn packages(&self) -> Result<Vec<(PathBuf, Package)>> {
        let mut packages = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("vscu-"))
                && let Some(package) = Package::read(&entry.path())
            {
                packages.push((entry.path(), package));
            }
        }
        Ok(packages)
    }

    fn prune(&self, protected: &Path, version: &str) -> Result<()> {
        let mut packages = self.packages()?;
        // Keep one build of each version, preferring the package just used.
        packages.sort_by(|(path_a, a), (path_b, b)| {
            version_key(&a.version)
                .unwrap()
                .cmp(&version_key(&b.version).unwrap())
                .then_with(|| (path_a == protected).cmp(&(path_b == protected)))
                .then_with(|| a.commit.cmp(&b.commit))
        });
        let mut retained_versions = std::collections::HashSet::new();
        retained_versions.insert(version_key(version)?);
        let mut remove = Vec::new();
        for (path, package) in packages.into_iter().rev() {
            if path == protected {
                continue;
            }
            if retained_versions.len() >= self.max_count
                || !retained_versions.insert(version_key(&package.version)?)
            {
                remove.push(path);
            }
        }
        for path in remove {
            // Remove only known files, never recursively delete unrelated directory contents.
            fs::remove_file(path.join(ARCHIVE))?;
            fs::remove_file(path.join(MANIFEST))?;
            match fs::remove_dir(&path) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => (),
                Err(error) => return Err(error.into()),
            }
            println!("Removed cached package {}", path.display());
        }
        Ok(())
    }
}

fn version_key(version: &str) -> Result<[u64; 3]> {
    install::validate_version(version)?;
    let parts = version
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<Vec<u64>, _>>()?;
    parts
        .try_into()
        .map_err(|_| "Cache version must be major.minor.patch".into())
}
