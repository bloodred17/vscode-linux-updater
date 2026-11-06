use crate::{cache::DownloadCache, config::Config};
use flate2::read::GzDecoder;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::{fs::PermissionsExt, fs::symlink},
    path::{Component, Path, PathBuf},
};
use tempfile::TempDir;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Keeps the install lock and private staging directory alive for the whole operation.
pub struct Installer {
    root: PathBuf,
    command: PathBuf,
    staging: TempDir,
    cache: DownloadCache,
    archive: PathBuf,
    _lock: File,
}

impl Installer {
    pub fn prepare() -> Result<Self> {
        if env::consts::OS != "linux" || env::consts::ARCH != "x86_64" {
            return Err("Only Linux x64 is supported".into());
        }
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or("HOME must be set to an absolute directory")?;
        let data = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty());
        let (root, command) = install_paths(Path::new(&home), data.as_deref())?;
        let search_path = env::var_os("PATH").unwrap_or_default();
        let paths: Vec<_> = env::split_paths(&search_path).collect();
        let config = Config::load()?;
        Self::prepare_at(root, command, &paths, &config)
    }

    fn prepare_at(
        root: PathBuf,
        command: PathBuf,
        paths: &[PathBuf],
        config: &Config,
    ) -> Result<Self> {
        // Check conflicts before creating staging files or downloading anything.
        require_absent(
            &root.join("current"),
            "An active or broken managed installation exists; use the future update/doctor flow",
        )?;
        require_absent(
            &command,
            "The terminal command already exists; explicit replacement is required",
        )?;
        for dir in paths {
            let candidate = dir.join("code");
            if let Ok(metadata) = fs::metadata(&candidate)
                && metadata.is_file()
                && metadata.permissions().mode() & 0o111 != 0
            {
                return Err(format!(
                    "Existing code command on PATH: {}. Explicit integration choice is required",
                    candidate.display()
                )
                .into());
            }
        }
        ensure_directory(&root)?;
        let lock_path = root.join("install.lock");
        if fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.is_file()) {
            return Err("The install lock path is not a regular file".into());
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        FileExt::try_lock_exclusive(&lock).map_err(|e| {
            format!("Cannot acquire install lock (another operation may be running): {e}")
        })?;
        require_absent(
            &root.join("current"),
            "A managed installation already exists",
        )?;
        ensure_directory(&root.join("versions"))?;
        let staging = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(&root)?;
        let cache = DownloadCache::prepare(config)?;
        let archive = cache.download_path();
        Ok(Self {
            root,
            command,
            staging,
            cache,
            archive,
            _lock: lock,
        })
    }

    pub fn download_path(&self) -> PathBuf {
        self.archive.clone()
    }

    pub fn reuse_cached(&mut self, requested: &str, commit: &str, checksum: &str) -> Result<bool> {
        if let Some(archive) = self.cache.lookup(requested, commit, checksum)? {
            println!("Using cached package {}", archive.display());
            self.archive = archive;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn finish(self, checksum: &str, requested: &str, commit: &str) -> Result<PathBuf> {
        verify_checksum(&self.download_path(), checksum)?;
        let unpacked = self.staging.path().join("unpacked");
        fs::create_dir(&unpacked)?;
        extract_archive(&self.download_path(), &unpacked)?;
        let application = unpacked.join("VSCode-linux-x64");
        let version = validate_application(&application, requested, commit)?;
        self.cache
            .retain(&self.archive, &version, commit, checksum)?;
        // The commit distinguishes different builds of a release without using `latest` as an identity.
        let destination = self
            .root
            .join("versions")
            .join(format!("{version}-{commit}"));
        require_absent(
            &destination,
            "The release directory already exists; it will not be overwritten",
        )?;
        require_absent(
            &self.root.join("current"),
            "A managed installation already exists",
        )?;
        require_absent(
            &self.command,
            "The terminal command now exists; it will not be overwritten",
        )?;
        let bin = self
            .command
            .parent()
            .ok_or("Invalid terminal command path")?;
        ensure_directory(bin)?;
        fs::rename(&application, &destination)?;

        // Each symlink is created exclusively. On failure remove only links created here.
        let current = self.root.join("current");
        if let Err(error) = symlink(&destination, &current) {
            fs::remove_dir_all(&destination)?;
            return Err(error.into());
        }
        if let Err(error) = symlink(current.join("bin/code"), &self.command) {
            if fs::read_link(&current).is_ok_and(|target| target == destination) {
                fs::remove_file(&current)?;
                fs::remove_dir_all(&destination)?;
            }
            return Err(error.into());
        }
        println!("Installed VS Code {version} at {}", destination.display());
        println!("Terminal command: {}", self.command.display());
        let search_path = env::var_os("PATH").unwrap_or_default();
        if !env::split_paths(&search_path).any(|path| path == bin) {
            println!("Add {} to PATH to launch it as code", bin.display());
        }
        println!(
            "Desktop launcher setup is not implemented yet; settings and extensions were preserved."
        );
        Ok(destination)
    }
}

fn install_paths(home: &Path, data: Option<&OsStr>) -> Result<(PathBuf, PathBuf)> {
    if !home.is_absolute() {
        return Err("HOME must be an absolute directory".into());
    }
    let data = data
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    if !data.is_absolute() {
        return Err("XDG_DATA_HOME must be an absolute directory".into());
    }
    Ok((data.join("vscu"), home.join(".local/bin/code")))
}

pub fn validate_version(version: &str) -> Result<()> {
    let parts: Vec<_> = version.split('.').collect();
    if version == "latest"
        || (parts.len() == 3
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())))
    {
        Ok(())
    } else {
        Err("Version must be latest or major.minor.patch".into())
    }
}

pub fn release_commit(url: &reqwest::Url) -> Result<String> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Release lookup did not return a valid HTTPS download URL".into());
    }
    let segments: Vec<_> = url
        .path_segments()
        .ok_or("Download URL has no release identity")?
        .collect();
    let commit = segments
        .windows(2)
        .find(|pair| pair[0] == "stable")
        .map(|pair| pair[1])
        .ok_or("Download URL does not identify an exact Stable build")?;
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Download URL has an invalid build commit".into());
    }
    Ok(commit.to_owned())
}

pub fn validate_checksum(checksum: &str) -> Result<()> {
    if checksum.len() != 64 || !checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Release lookup returned an invalid SHA-256 checksum".into());
    }
    Ok(())
}

pub(crate) fn verify_checksum(path: &Path, expected: &str) -> Result<()> {
    validate_checksum(expected)?;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected) {
        return Err("Archive SHA-256 mismatch; nothing was activated".into());
    }
    Ok(())
}

fn require_absent(path: &Path, message: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(format!("{message}: {}", path.display()).into()),
    }
}

pub(crate) fn ensure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(format!("Expected a real directory: {}", path.display()).into());
    }
    Ok(())
}

fn extract_archive(path: &Path, destination: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(path)?));
    archive.set_overwrite(false);
    // Preserve executable bits, but never restore special permission bits.
    archive.set_mask(0o7000);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let components: Vec<_> = path.components().collect();
        if components.first() != Some(&Component::Normal(OsStr::new("VSCode-linux-x64")))
            || components
                .iter()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(format!("Unsafe or unexpected archive path: {}", path.display()).into());
        }
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            let target = entry.link_name()?.ok_or("Archive link has no target")?;
            // Conservative containment policy: refuse absolute or parent-relative links.
            if target.components().next().is_none()
                || target
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
            {
                return Err(format!("Unsafe archive link: {}", path.display()).into());
            }
            if kind.is_hard_link() && !target.starts_with("VSCode-linux-x64") {
                return Err("Archive hard link must stay in the application directory".into());
            }
        } else if !kind.is_file() && !kind.is_dir() {
            return Err(format!("Unsupported archive entry: {}", path.display()).into());
        }
        if !entry.unpack_in(destination)? {
            return Err(format!("Archive entry escaped staging: {}", path.display()).into());
        }
    }
    // Consume the gzip trailer too, detecting truncated/corrupt gzip members.
    io::copy(&mut archive.into_inner(), &mut io::sink())?;
    Ok(())
}

fn validate_application(application: &Path, requested: &str, commit: &str) -> Result<String> {
    if !fs::symlink_metadata(application)?.is_dir() {
        return Err("Expected an application directory, not a link".into());
    }
    for executable in ["code", "bin/code"] {
        let path = application.join(executable);
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("Missing executable: {}", path.display()).into());
        }
        if !fs::canonicalize(&path)?.starts_with(fs::canonicalize(application)?) {
            return Err("Application executable points outside the installation".into());
        }
    }
    require_absent(
        &application.join("data"),
        "Portable-mode data directory is not allowed in a fresh managed install",
    )?;
    let product: serde_json::Value =
        serde_json::from_reader(File::open(application.join("resources/app/product.json"))?)?;
    let package: serde_json::Value =
        serde_json::from_reader(File::open(application.join("resources/app/package.json"))?)?;
    let version = package["version"]
        .as_str()
        .ok_or("Missing application version")?;
    validate_version(version)?;
    if version == "latest" || (requested != "latest" && requested != version) {
        return Err("Extracted version does not match the requested release".into());
    }
    if product["commit"].as_str() != Some(commit)
        || product["applicationName"].as_str() != Some("code")
        || product
            .get("version")
            .is_some_and(|value| value.as_str() != Some(version))
    {
        return Err(
            "Extracted application metadata does not match the resolved Stable build".into(),
        );
    }
    Ok(version.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn setup(home: &Path) -> Result<Installer> {
        let (root, command) = install_paths(home, None)?;
        Installer::prepare_at(root, command, &[], &cache_config(home))
    }

    fn cache_config(home: &Path) -> Config {
        Config {
            download_dir: home.join(".vscu"),
            max_cache_count: 3,
        }
    }

    fn append(builder: &mut tar::Builder<GzEncoder<File>>, name: &str, content: &[u8], mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        builder.append_data(&mut header, name, content).unwrap();
    }

    fn fixture(path: &Path, extra: Option<(&str, &str, tar::EntryType)>) -> String {
        fixture_version(path, "1.100.0", extra)
    }

    fn fixture_version(
        path: &Path,
        version: &str,
        extra: Option<(&str, &str, tar::EntryType)>,
    ) -> String {
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(path).unwrap(),
            Compression::fast(),
        ));
        append(
            &mut builder,
            "VSCode-linux-x64/code",
            b"fake executable",
            0o755,
        );
        append(
            &mut builder,
            "VSCode-linux-x64/bin/code",
            b"fake launcher",
            0o755,
        );
        append(
            &mut builder,
            "VSCode-linux-x64/resources/app/package.json",
            format!(r#"{{"version":"{version}"}}"#).as_bytes(),
            0o644,
        );
        let product =
            format!(r#"{{"version":"{version}","commit":"{COMMIT}","applicationName":"code"}}"#);
        append(
            &mut builder,
            "VSCode-linux-x64/resources/app/product.json",
            product.as_bytes(),
            0o644,
        );
        if let Some((name, target, kind)) = extra {
            let mut header = tar::Header::new_gnu();
            // Write raw names so the fixture can include names the builder normally rejects.
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mode(0o755);
            if kind.is_symlink() || kind.is_hard_link() {
                header.set_link_name(target).unwrap();
            }
            header.set_cksum();
            builder.append(&header, io::empty()).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        digest(path)
    }

    fn digest(path: &Path) -> String {
        Sha256::digest(fs::read(path).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    // Independent fresh installations share a download cache, without enabling updates.
    fn cached_setup(home: &Path, name: &str, max_count: usize) -> Installer {
        let root = home.join(name);
        let mut config = cache_config(home);
        config.max_cache_count = max_count;
        Installer::prepare_at(root.clone(), root.join("bin/code"), &[], &config).unwrap()
    }

    fn cached_archive(home: &Path, version: &str) -> PathBuf {
        home.join(".vscu")
            .join(format!("vscu-{version}-{COMMIT}/archive.tar.gz"))
    }

    fn cache_versions(home: &Path) -> Vec<String> {
        let mut versions = Vec::new();
        for entry in fs::read_dir(home.join(".vscu")).unwrap() {
            let path = entry.unwrap().path();
            if path.join("archive.tar.gz").is_file()
                && let Ok(file) = File::open(path.join("vscu-package.json"))
            {
                let metadata: serde_json::Value = serde_json::from_reader(file).unwrap();
                versions.push(metadata["version"].as_str().unwrap().to_owned());
            }
        }
        versions.sort();
        versions
    }

    fn populate_cache(home: &Path) {
        for version in ["1.99.0", "1.100.0", "2.0.0"] {
            let installer = cached_setup(home, version, 3);
            let checksum = fixture_version(&installer.download_path(), version, None);
            installer.finish(&checksum, version, COMMIT).unwrap();
        }
    }

    #[test]
    fn cache_retention_follows_all_three_examples_and_preserves_installed_releases() {
        let home = tempfile::tempdir().unwrap();
        let mut installed = Vec::new();
        for (index, (version, expected)) in [
            ("1.0.0", vec!["1.0.0"]),
            ("2.0.0", vec!["1.0.0", "2.0.0"]),
            ("3.0.0", vec!["1.0.0", "2.0.0", "3.0.0"]),
            ("4.0.0", vec!["2.0.0", "3.0.0", "4.0.0"]),
            ("1.0.0", vec!["1.0.0", "3.0.0", "4.0.0"]),
            ("2.0.0", vec!["2.0.0", "3.0.0", "4.0.0"]),
        ]
        .into_iter()
        .enumerate()
        {
            let installer = cached_setup(home.path(), &format!("install-{index}"), 3);
            let checksum = fixture_version(&installer.download_path(), version, None);
            installed.push(installer.finish(&checksum, version, COMMIT).unwrap());
            assert_eq!(cache_versions(home.path()), expected);
        }
        for release in installed {
            assert_eq!(
                fs::read(release.join("bin/code")).unwrap(),
                b"fake launcher"
            );
        }
    }

    #[test]
    fn reused_package_resolves_latest_and_applies_a_reduced_limit() {
        let home = tempfile::tempdir().unwrap();
        populate_cache(home.path());
        let archive = cached_archive(home.path(), "1.99.0");
        let checksum = digest(&archive);
        let mut installer = cached_setup(home.path(), "reuse", 1);
        assert!(
            !installer
                .reuse_cached("1.99.0", &"b".repeat(40), &checksum)
                .unwrap()
        );
        assert!(
            !installer
                .reuse_cached("1.100.0", COMMIT, &checksum)
                .unwrap()
        );
        assert!(installer.reuse_cached("latest", COMMIT, &checksum).unwrap());
        assert_eq!(installer.download_path(), archive);
        assert!(!installer.cache.download_path().exists());
        // No eviction until this reused package has passed full validation again.
        assert_eq!(cache_versions(home.path()).len(), 3);
        installer.finish(&checksum, "latest", COMMIT).unwrap();
        assert_eq!(cache_versions(home.path()), ["1.99.0"]);
        assert_eq!(digest(&archive), checksum);
    }

    #[test]
    fn eviction_compares_versions_numerically() {
        let home = tempfile::tempdir().unwrap();
        populate_cache(home.path());
        let installer = cached_setup(home.path(), "numeric", 3);
        let checksum = fixture_version(&installer.download_path(), "2.1.0", None);
        installer.finish(&checksum, "2.1.0", COMMIT).unwrap();
        assert_eq!(cache_versions(home.path()), ["1.100.0", "2.0.0", "2.1.0"]);
    }

    #[test]
    fn failed_download_or_package_validation_preserves_existing_cache() {
        let home = tempfile::tempdir().unwrap();
        populate_cache(home.path());
        let expected = cache_versions(home.path());
        let abandoned = cached_setup(home.path(), "abandoned", 1);
        let partial = abandoned.download_path();
        fs::write(&partial, "partial download").unwrap();
        drop(abandoned);
        assert!(!partial.exists());
        assert_eq!(cache_versions(home.path()), expected);
        for failure in ["checksum", "gzip", "version", "commit", "unsafe"] {
            let installer = cached_setup(home.path(), failure, 1);
            let archive = installer.download_path();
            let extra = (failure == "unsafe").then_some(("../escape", "", tar::EntryType::Regular));
            let mut checksum = fixture_version(&archive, "3.0.0", extra);
            let mut version = "3.0.0";
            let mut commit = COMMIT.to_owned();
            match failure {
                "checksum" => checksum = "0".repeat(64),
                "gzip" => {
                    fs::write(&archive, "invalid gzip").unwrap();
                    checksum = digest(&archive);
                }
                "version" => version = "4.0.0",
                "commit" => commit = "b".repeat(40),
                _ => (),
            }
            assert!(installer.finish(&checksum, version, &commit).is_err());
            assert!(!archive.exists());
            assert_eq!(
                cache_versions(home.path()),
                expected,
                "cache changed after {failure}"
            );
        }
    }

    #[test]
    fn corrupt_cached_package_is_replaced_only_after_a_valid_download() {
        let home = tempfile::tempdir().unwrap();
        populate_cache(home.path());
        let archive = cached_archive(home.path(), "1.99.0");
        let checksum = digest(&archive);
        fs::write(&archive, "corrupt cached download").unwrap();
        let mut installer = cached_setup(home.path(), "replacement", 3);
        assert!(!installer.reuse_cached("1.99.0", COMMIT, &checksum).unwrap());
        assert_eq!(fs::read(&archive).unwrap(), b"corrupt cached download");
        let replacement_checksum = fixture_version(&installer.download_path(), "1.99.0", None);
        assert_eq!(replacement_checksum, checksum);
        installer.finish(&checksum, "1.99.0", COMMIT).unwrap();
        assert_eq!(digest(&archive), checksum);
        assert_eq!(cache_versions(home.path()).len(), 3);
    }

    #[test]
    fn cache_cleanup_leaves_unrelated_files_and_symlinks_untouched() {
        let home = tempfile::tempdir().unwrap();
        populate_cache(home.path());
        let cache = home.path().join(".vscu");
        let oldest = cached_archive(home.path(), "1.99.0")
            .parent()
            .unwrap()
            .to_owned();
        fs::write(cache.join("notes.txt"), "keep notes").unwrap();
        fs::write(oldest.join("user-file"), "keep inside package directory").unwrap();
        let unowned = cache.join(format!("vscu-0.1.0-{COMMIT}"));
        fs::create_dir(&unowned).unwrap();
        fs::write(unowned.join("archive.tar.gz"), "unmanaged archive").unwrap();
        fs::write(unowned.join("vscu-package.json"), "invalid manifest").unwrap();
        symlink(home.path().join("1.100.0"), cache.join("vscu-link")).unwrap();
        let installer = cached_setup(home.path(), "cleanup", 1);
        let checksum = fixture_version(&installer.download_path(), "3.0.0", None);
        installer.finish(&checksum, "3.0.0", COMMIT).unwrap();
        assert_eq!(
            fs::read_to_string(cache.join("notes.txt")).unwrap(),
            "keep notes"
        );
        assert_eq!(
            fs::read_to_string(oldest.join("user-file")).unwrap(),
            "keep inside package directory"
        );
        assert_eq!(
            fs::read_to_string(unowned.join("archive.tar.gz")).unwrap(),
            "unmanaged archive"
        );
        assert!(
            fs::symlink_metadata(cache.join("vscu-link"))
                .unwrap()
                .is_symlink()
        );
        assert!(!oldest.join("archive.tar.gz").exists());
    }

    #[test]
    fn shared_cache_is_locked_across_distinct_installation_roots() {
        let home = tempfile::tempdir().unwrap();
        let first = cached_setup(home.path(), "first", 3);
        let other_root = home.path().join("second");
        let error = Installer::prepare_at(
            other_root.clone(),
            other_root.join("bin/code"),
            &[],
            &cache_config(home.path()),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("download cache lock"));
        drop(first);
        drop(cached_setup(home.path(), "second", 3));
    }

    #[test]
    fn fresh_install_activates_exact_build_and_preserves_permissions_and_user_data() {
        let home = tempfile::tempdir().unwrap();
        let settings = home.path().join(".config/Code/User/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "user settings").unwrap();
        let installer = setup(home.path()).unwrap();
        let root = installer.root.clone();
        let staging = installer.staging.path().to_owned();
        let command = installer.command.clone();
        let checksum = fixture(&installer.download_path(), None);
        let destination = installer.finish(&checksum, "latest", COMMIT).unwrap();
        assert_eq!(
            destination.file_name().unwrap(),
            format!("1.100.0-{COMMIT}").as_str()
        );
        assert_eq!(fs::read_link(root.join("current")).unwrap(), destination);
        assert_eq!(fs::read(&command).unwrap(), b"fake launcher");
        assert_ne!(
            fs::metadata(command).unwrap().permissions().mode() & 0o111,
            0
        );
        assert_eq!(fs::read_to_string(settings).unwrap(), "user settings");
        assert!(!staging.exists());
        assert!(setup(home.path()).is_err());
    }

    #[test]
    fn checksum_mismatch_leaves_no_active_release_and_cleans_staging() {
        let home = tempfile::tempdir().unwrap();
        let installer = setup(home.path()).unwrap();
        let root = installer.root.clone();
        let staging = installer.staging.path().to_owned();
        fixture(&installer.download_path(), None);
        assert!(installer.finish(&"0".repeat(64), "latest", COMMIT).is_err());
        assert!(!root.join("current").exists());
        assert_eq!(fs::read_dir(root.join("versions")).unwrap().count(), 0);
        assert!(!staging.exists());
    }

    #[test]
    fn refuses_mismatched_version_or_commit_without_activation() {
        for (version, commit) in [
            ("1.99.0", COMMIT),
            ("latest", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ] {
            let home = tempfile::tempdir().unwrap();
            let installer = setup(home.path()).unwrap();
            let root = installer.root.clone();
            let checksum = fixture(&installer.download_path(), None);
            assert!(installer.finish(&checksum, version, commit).is_err());
            assert!(fs::symlink_metadata(root.join("current")).is_err());
        }
    }

    #[test]
    fn refuses_escaping_paths_links_and_special_files() {
        for (name, target, kind) in [
            ("../escape", "", tar::EntryType::Regular),
            ("/tmp/vscu-escape", "", tar::EntryType::Regular),
            ("VSCode-linux-x64/../../escape", "", tar::EntryType::Regular),
            (
                "VSCode-linux-x64/link",
                "../../escape",
                tar::EntryType::Symlink,
            ),
            (
                "VSCode-linux-x64/link",
                "/tmp/escape",
                tar::EntryType::Symlink,
            ),
            (
                "VSCode-linux-x64/link",
                "../../escape",
                tar::EntryType::Link,
            ),
            ("VSCode-linux-x64/device", "", tar::EntryType::Char),
            ("VSCode-linux-x64/data", "", tar::EntryType::Directory),
        ] {
            let home = tempfile::tempdir().unwrap();
            let installer = setup(home.path()).unwrap();
            let root = installer.root.clone();
            let checksum = fixture(&installer.download_path(), Some((name, target, kind)));
            assert!(
                installer.finish(&checksum, "latest", COMMIT).is_err(),
                "accepted {name} -> {target}"
            );
            assert!(fs::symlink_metadata(root.join("current")).is_err());
        }
    }

    #[test]
    fn existing_release_is_not_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let installer = setup(home.path()).unwrap();
        let destination = installer
            .root
            .join("versions")
            .join(format!("1.100.0-{COMMIT}"));
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), "existing content").unwrap();
        let checksum = fixture(&installer.download_path(), None);
        assert!(installer.finish(&checksum, "latest", COMMIT).is_err());
        // A validated download remains reusable even when activation has a conflict.
        verify_checksum(&cached_archive(home.path(), "1.100.0"), &checksum).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("keep")).unwrap(),
            "existing content"
        );
    }

    #[test]
    fn rejects_existing_command_including_dangling_symlink() {
        for dangling in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let (root, command) = install_paths(home.path(), None).unwrap();
            fs::create_dir_all(command.parent().unwrap()).unwrap();
            if dangling {
                symlink("missing", &command).unwrap();
            } else {
                fs::write(&command, "existing command").unwrap();
            }
            assert!(
                Installer::prepare_at(
                    root.clone(),
                    command.clone(),
                    &[],
                    &cache_config(home.path())
                )
                .is_err()
            );
            assert!(fs::symlink_metadata(command).is_ok());
            assert!(!root.exists());
        }
    }

    #[test]
    fn rejects_path_conflicts_and_concurrent_operations() {
        let home = tempfile::tempdir().unwrap();
        let existing_bin = home.path().join("other-bin");
        fs::create_dir(&existing_bin).unwrap();
        fs::write(existing_bin.join("code"), "existing").unwrap();
        fs::set_permissions(existing_bin.join("code"), fs::Permissions::from_mode(0o755)).unwrap();
        let (root, command) = install_paths(home.path(), None).unwrap();
        assert!(
            Installer::prepare_at(root, command, &[existing_bin], &cache_config(home.path()))
                .is_err()
        );
        let installer = setup(home.path()).unwrap();
        assert!(setup(home.path()).is_err());
        drop(installer);
        assert!(setup(home.path()).is_ok());
    }

    #[test]
    fn conflicts_appearing_during_download_do_not_get_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let installer = setup(home.path()).unwrap();
        let command = installer.command.clone();
        let current = installer.root.join("current");
        let checksum = fixture(&installer.download_path(), None);
        fs::create_dir_all(command.parent().unwrap()).unwrap();
        fs::write(&command, "keep me").unwrap();
        assert!(installer.finish(&checksum, "latest", COMMIT).is_err());
        assert_eq!(fs::read_to_string(command).unwrap(), "keep me");
        assert!(fs::symlink_metadata(current).is_err());
    }

    #[test]
    fn rejects_corrupt_archive_with_matching_checksum() {
        let home = tempfile::tempdir().unwrap();
        let installer = setup(home.path()).unwrap();
        fs::write(installer.download_path(), "not a gzip archive").unwrap();
        let checksum = digest(&installer.download_path());
        assert!(installer.finish(&checksum, "latest", COMMIT).is_err());
    }

    #[test]
    fn respects_xdg_paths_and_rejects_relative_roots() {
        let (root, command) =
            install_paths(Path::new("/home/test"), Some(OsStr::new("/data"))).unwrap();
        assert_eq!(root, Path::new("/data/vscu"));
        assert_eq!(command, Path::new("/home/test/.local/bin/code"));
        assert!(install_paths(Path::new("relative"), None).is_err());
        assert!(install_paths(Path::new("/home/test"), Some(OsStr::new("relative"))).is_err());
    }

    #[test]
    fn release_lookup_requires_a_pinned_https_build_and_checksum() {
        let url = reqwest::Url::parse(&format!(
            "https://example.com/download/stable/{COMMIT}/code.tar.gz"
        ))
        .unwrap();
        assert_eq!(release_commit(&url).unwrap(), COMMIT);
        assert!(
            release_commit(
                &reqwest::Url::parse("http://example.com/stable/latest/code.tar.gz").unwrap()
            )
            .is_err()
        );
        assert!(
            release_commit(&reqwest::Url::parse("https://example.com/latest/code.tar.gz").unwrap())
                .is_err()
        );
        assert!(validate_checksum("missing").is_err());
        assert!(validate_version("1.100.0/../../other").is_err());
    }

    #[test]
    fn rejects_broken_current_link_without_changing_it() {
        let home = tempfile::tempdir().unwrap();
        let (root, command) = install_paths(home.path(), None).unwrap();
        fs::create_dir_all(&root).unwrap();
        symlink("versions/missing", root.join("current")).unwrap();
        assert!(
            Installer::prepare_at(root.clone(), command, &[], &cache_config(home.path())).is_err()
        );
        assert_eq!(
            fs::read_link(root.join("current")).unwrap(),
            Path::new("versions/missing")
        );
    }

    #[test]
    fn safe_relative_links_remain_valid_after_activation() {
        let home = tempfile::tempdir().unwrap();
        let installer = setup(home.path()).unwrap();
        let checksum = fixture(
            &installer.download_path(),
            Some((
                "VSCode-linux-x64/code-alias",
                "code",
                tar::EntryType::Symlink,
            )),
        );
        let destination = installer.finish(&checksum, "1.100.0", COMMIT).unwrap();
        assert_eq!(
            fs::read(destination.join("code-alias")).unwrap(),
            b"fake executable"
        );
    }

    #[test]
    #[ignore = "requires an existing official archive and matching release lookup metadata"]
    fn official_archive_installs_only_in_temporary_home() {
        let archive = env::var_os("VSCU_TEST_ARCHIVE").expect("set VSCU_TEST_ARCHIVE");
        let checksum = env::var("VSCU_TEST_SHA256").expect("set VSCU_TEST_SHA256");
        let commit = env::var("VSCU_TEST_COMMIT").expect("set VSCU_TEST_COMMIT");
        let version = env::var("VSCU_TEST_VERSION").expect("set VSCU_TEST_VERSION");
        let home = tempfile::tempdir().unwrap();
        let installer = setup(home.path()).unwrap();
        let command = installer.command.clone();
        fs::copy(archive, installer.download_path()).unwrap();
        let destination = installer.finish(&checksum, &version, &commit).unwrap();
        assert_eq!(
            fs::canonicalize(command).unwrap(),
            destination.join("bin/code")
        );
    }
}
