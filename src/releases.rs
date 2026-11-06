//! Local release ownership, serialized mutations and recoverable activation.
use crate::{
    check,
    install::{self, Result},
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::symlink,
    path::{Component, Path, PathBuf},
};

pub const MANIFEST: &str = "vscu-release.json";
const JOURNAL: &str = "activation.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    version: String,
    commit: String,
    entries: BTreeMap<PathBuf, Entry>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Entry {
    Directory,
    File(String),
    Link(PathBuf),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    from: PathBuf,
    to: PathBuf,
}

pub fn lock(root: &Path) -> Result<File> {
    install::ensure_directory(root)?;
    let path = root.join("install.lock");
    if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
        return Err("The install lock path is not a regular file".into());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    FileExt::try_lock_exclusive(&lock).map_err(|e| {
        format!("Cannot acquire install lock (another operation may be running): {e}")
    })?;
    Ok(lock)
}

fn inventory(root: &Path, directory: &Path, entries: &mut BTreeMap<PathBuf, Entry>) -> Result<()> {
    for item in fs::read_dir(directory)? {
        let path = item?.path();
        let relative = path.strip_prefix(root)?.to_owned();
        if relative == Path::new(MANIFEST) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        let entry = if metadata.is_symlink() {
            Entry::Link(fs::read_link(&path)?)
        } else if metadata.is_dir() {
            inventory(root, &path, entries)?;
            Entry::Directory
        } else if metadata.is_file() {
            Entry::File(install::file_checksum(&path)?)
        } else {
            return Err(format!("Unsupported release entry: {}", path.display()).into());
        };
        entries.insert(relative, entry);
    }
    Ok(())
}

pub fn write_manifest(application: &Path, version: &str, commit: &str) -> Result<()> {
    install::require_absent(
        &application.join(MANIFEST),
        "Archive must not supply a vscu ownership manifest",
    )?;
    let mut entries = BTreeMap::new();
    inventory(application, application, &mut entries)?;
    let manifest = Manifest {
        format: "vscu-release-v1".into(),
        version: version.into(),
        commit: commit.into(),
        entries,
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(application.join(MANIFEST))?;
    file.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    file.sync_all()?;
    Ok(())
}

/// Unknown/modified directory layouts remain untouched, including portable data.
pub fn removable(application: &Path) -> Result<bool> {
    if !fs::symlink_metadata(application)?.is_dir() {
        return Ok(false);
    }
    let manifest_path = application.join(MANIFEST);
    match fs::symlink_metadata(&manifest_path) {
        Ok(m) if m.is_file() => (),
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    let manifest: Manifest = match serde_json::from_reader(File::open(manifest_path)?) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    let release = match check::release_at(application) {
        Ok(r) => r,
        Err(_) => return Ok(false),
    };
    if manifest.format != "vscu-release-v1"
        || manifest.version != release.release.version
        || manifest.commit != release.release.commit
        || manifest.entries.keys().any(|p| {
            p.as_os_str().is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_)))
        })
        || fs::symlink_metadata(application.join("data")).is_ok()
    {
        return Ok(false);
    }
    let mut actual = BTreeMap::new();
    inventory(application, application, &mut actual)?;
    Ok(actual == manifest.entries)
}

pub fn selected(root: &Path, name: &str) -> Result<Option<PathBuf>> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(m) if !m.is_symlink() => {
            return Err(format!("{} must be a release symlink", path.display()).into());
        }
        Ok(_) => (),
    }
    let target =
        fs::canonicalize(&path).map_err(|e| format!("Cannot resolve {}: {e}", path.display()))?;
    validate_target(root, &target)?;
    Ok(Some(target))
}

pub fn validate_target(root: &Path, target: &Path) -> Result<()> {
    if !fs::symlink_metadata(root.join("versions"))?.is_dir()
        || !fs::symlink_metadata(target)?.is_dir()
        || target.parent() != Some(fs::canonicalize(root.join("versions"))?.as_path())
    {
        return Err("Release must be a real directory inside managed versions".into());
    }
    let installed = check::release_at(target)?;
    install::validate_application(
        target,
        &installed.release.version,
        &installed.release.commit,
    )?;
    Ok(())
}

pub fn atomic_link(root: &Path, name: &str, target: &Path) -> Result<()> {
    let destination = root.join(name);
    if fs::symlink_metadata(&destination).is_ok_and(|m| !m.is_symlink()) {
        return Err(format!("Refusing to replace non-symlink {}", destination.display()).into());
    }
    let staging = tempfile::Builder::new().prefix(".link-").tempdir_in(root)?;
    let pending = staging.path().join("link");
    symlink(target, &pending)?;
    fs::rename(pending, destination)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

pub fn has_journal(root: &Path) -> bool {
    fs::symlink_metadata(root.join(JOURNAL)).is_ok()
}

pub fn recover(root: &Path) -> Result<()> {
    let path = root.join(JOURNAL);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(m) if !m.is_file() => return Err("Activation journal must be a regular file".into()),
        Ok(_) => (),
    }
    let transaction: Activation = serde_json::from_reader(File::open(&path)?)?;
    validate_target(root, &transaction.from)?;
    validate_target(root, &transaction.to)?;
    let current = selected(root, "current")?.ok_or("Activation journal has no current release")?;
    if current == transaction.to {
        // Finish bookkeeping after a completed switch. Never switch a running editor during recovery.
        atomic_link(root, "previous", &transaction.from)?;
        if selected(root, "staged")?.as_ref() == Some(&transaction.to) {
            fs::remove_file(root.join("staged"))?;
        }
    } else if current != transaction.from {
        return Err("Activation journal does not match current; inspect with doctor".into());
    }
    fs::remove_file(path)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

pub fn activate(root: &Path, target: &Path) -> Result<()> {
    recover(root)?;
    validate_target(root, target)?;
    if fs::symlink_metadata(target.join(MANIFEST)).is_ok() && !removable(target)? {
        return Err(
            "Release contents differ from the ownership manifest; refusing activation".into(),
        );
    }
    let current = selected(root, "current")?.ok_or("No managed installation; run vscu install")?;
    if current == target {
        if selected(root, "staged")?.as_deref() == Some(target) {
            fs::remove_file(root.join("staged"))?;
        }
        return Ok(());
    }
    selected(root, "previous")?;
    let mut journal = tempfile::NamedTempFile::new_in(root)?;
    journal.write_all(&serde_json::to_vec(&Activation {
        from: current,
        to: target.into(),
    })?)?;
    journal.as_file().sync_all()?;
    journal
        .persist_noclobber(root.join(JOURNAL))
        .map_err(|e| e.error)?;
    File::open(root)?.sync_all()?;
    atomic_link(root, "current", target)?;
    recover(root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self as fixture, A, B};

    #[test]
    fn modified_staged_application_is_not_activated() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let old = fixture::release(root, "1.99.0", A, false);
        let target = fixture::release(root, "1.100.0", B, true);
        fixture::select(root, "current", &old);
        fs::write(target.join("code"), "changed").unwrap();
        assert!(activate(root, &target).is_err());
        assert_eq!(selected(root, "current").unwrap(), Some(old));
        assert!(!has_journal(root));
    }

    #[test]
    fn activation_and_rollback_preserve_both_releases() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let first = fixture::release(root, "1.99.0", A, false);
        let second = fixture::release(root, "1.100.0", B, true);
        fixture::select(root, "current", &first);
        fixture::select(root, "staged", &second);
        activate(root, &second).unwrap();
        assert_eq!(selected(root, "current").unwrap(), Some(second.clone()));
        assert_eq!(selected(root, "previous").unwrap(), Some(first.clone()));
        assert!(selected(root, "staged").unwrap().is_none());
        assert!(!has_journal(root));
        activate(root, &first).unwrap();
        assert_eq!(selected(root, "current").unwrap(), Some(first.clone()));
        assert_eq!(selected(root, "previous").unwrap(), Some(second.clone()));
        assert!(first.is_dir() && second.is_dir());
    }

    #[test]
    fn interrupted_activation_recovers_before_and_after_switch() {
        for switched in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let root = home.path();
            let first = fixture::release(root, "1.99.0", A, true);
            let second = fixture::release(root, "1.100.0", B, true);
            fixture::select(root, "current", if switched { &second } else { &first });
            fixture::select(root, "staged", &second);
            fs::write(
                root.join(JOURNAL),
                serde_json::to_vec(&Activation {
                    from: first.clone(),
                    to: second.clone(),
                })
                .unwrap(),
            )
            .unwrap();
            recover(root).unwrap();
            assert!(!has_journal(root));
            if switched {
                assert_eq!(selected(root, "previous").unwrap(), Some(first));
                assert!(selected(root, "staged").unwrap().is_none());
            } else {
                assert_eq!(selected(root, "current").unwrap(), Some(first));
                assert!(selected(root, "previous").unwrap().is_none());
                assert_eq!(selected(root, "staged").unwrap(), Some(second));
            }
        }
    }

    #[test]
    fn invalid_targets_and_conflicting_entries_never_change_current() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let first = fixture::release(root, "1.99.0", A, true);
        let second = fixture::release(root, "1.100.0", B, true);
        fixture::select(root, "current", &first);
        assert!(activate(root, home.path()).is_err());
        fs::write(root.join("previous"), "keep").unwrap();
        assert!(activate(root, &second).is_err());
        assert_eq!(selected(root, "current").unwrap(), Some(first));
        assert_eq!(fs::read_to_string(root.join("previous")).unwrap(), "keep");
        assert!(!has_journal(root));
    }

    #[test]
    fn manifests_preserve_unowned_added_changed_and_portable_content() {
        for modification in ["none", "unowned", "added", "changed", "portable", "link"] {
            let home = tempfile::tempdir().unwrap();
            let app = fixture::release(home.path(), "1.100.0", A, modification != "unowned");
            match modification {
                "added" => fs::write(app.join("personal"), "keep").unwrap(),
                "changed" => fs::write(app.join("bin/code"), "changed").unwrap(),
                "portable" => fs::create_dir(app.join("data")).unwrap(),
                "link" => {
                    fs::remove_file(app.join("bin/code")).unwrap();
                    symlink("/bin/sh", app.join("bin/code")).unwrap();
                }
                _ => (),
            }
            assert_eq!(
                removable(&app).unwrap(),
                modification == "none",
                "{modification}"
            );
        }
    }

    #[test]
    fn shared_install_lock_and_invalid_journal_are_rejected() {
        let home = tempfile::tempdir().unwrap();
        let guard = lock(home.path()).unwrap();
        assert!(lock(home.path()).is_err());
        drop(guard);
        assert!(lock(home.path()).is_ok());
        fs::write(home.path().join(JOURNAL), "bad json").unwrap();
        assert!(recover(home.path()).is_err());
        assert!(has_journal(home.path()));
    }
}
