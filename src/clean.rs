use crate::{
    cache,
    config::Config,
    install::{self, Result},
    processes, releases,
};
use std::{collections::HashSet, fs, io, path::Path};

pub fn run(apply: bool) -> Result<()> {
    let (root, _) = install::configured_install_paths()?;
    let config = Config::load()?;
    clean_at(&root, &config, apply)
}

pub(crate) fn clean_at(root: &Path, config: &Config, apply: bool) -> Result<()> {
    clean_with(root, config, apply, processes::running)
}

fn clean_with(
    root: &Path,
    config: &Config,
    apply: bool,
    running: processes::Scanner,
) -> Result<()> {
    let exists = match fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() => true,
        Ok(_) => return Err("Managed root must be a real directory".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    let _lock = if exists && apply {
        Some(releases::lock(root)?)
    } else {
        None
    };
    if exists {
        if apply {
            releases::recover(root)?;
        } else if releases::has_journal(root) {
            return Err("Interrupted activation needs recovery before cleanup; run vscu update --activate or vscu rollback".into());
        }
        let mut protected = HashSet::new();
        for name in ["current", "previous", "staged"] {
            if let Some(target) = releases::selected(root, name)? {
                protected.insert(target);
            }
        }
        let versions = root.join("versions");
        match fs::symlink_metadata(&versions) {
            Ok(m) if m.is_dir() => {
                protected.extend(running(root)?);
                for entry in fs::read_dir(&versions)? {
                    let path = entry?.path();
                    if !fs::symlink_metadata(&path)?.is_dir() {
                        println!("Preserving unrecognized entry {}", path.display());
                        continue;
                    }
                    let path = fs::canonicalize(path)?;
                    if protected.contains(&path) {
                        println!("Keeping protected release {}", path.display());
                        continue;
                    }
                    if !releases::removable(&path)? {
                        println!("Preserving unowned or modified release {}", path.display());
                        continue;
                    }
                    println!(
                        "{} release {}",
                        if apply { "Removing" } else { "Would remove" },
                        path.display()
                    );
                    if apply {
                        if running(root)?.contains(&path) {
                            println!("Keeping newly running release {}", path.display());
                            continue;
                        }
                        if !releases::removable(&path)? {
                            return Err("Release changed during cleanup; retry preview".into());
                        }
                        fs::remove_dir_all(path)?;
                    }
                }
            }
            Ok(_) => return Err("Managed versions must be a real directory".into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    cache::clean(config, apply)?;
    if !apply {
        println!("Preview only. Run vscu clean --yes to remove these candidates.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cache::DownloadCache,
        test_support::{self as fixture, A, B},
    };

    #[test]
    fn running_releases_are_protected_and_uncertain_scans_prevent_cleanup() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let old = fixture::release(root, "1.99.0", A, true);
        let config = Config {
            download_dir: root.join("cache"),
            max_cache_count: 1,
        };
        let before = fixture::snapshot(root);
        assert!(clean_with(root, &config, false, |_| Err("uncertain".into())).is_err());
        assert_eq!(before, fixture::snapshot(root));
        clean_with(root, &config, true, |root| {
            Ok(HashSet::from([root
                .join("versions")
                .join(format!("1.99.0-{A}"))]))
        })
        .unwrap();
        assert!(old.is_dir());
    }

    #[test]
    fn preview_is_read_only_and_apply_retains_protected_unowned_and_modified_releases() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("vscu");
        let current = fixture::release(&root, "1.100.0", A, true);
        let previous = fixture::release(&root, "1.99.0", A, true);
        let staged = fixture::release(&root, "1.101.0", B, true);
        let old = fixture::release(&root, "1.98.0", A, true);
        let unowned = fixture::release(&root, "1.97.0", A, false);
        let modified = fixture::release(&root, "1.96.0", A, true);
        fs::write(modified.join("personal"), "keep").unwrap();
        fixture::select(&root, "current", &current);
        fixture::select(&root, "previous", &previous);
        fixture::select(&root, "staged", &staged);
        let config = Config {
            download_dir: home.path().join("cache"),
            max_cache_count: 1,
        };
        let before = fixture::snapshot(home.path());
        clean_with(&root, &config, false, |_| Ok(HashSet::new())).unwrap();
        assert_eq!(before, fixture::snapshot(home.path()));
        clean_with(&root, &config, true, |_| Ok(HashSet::new())).unwrap();
        assert!(!old.exists());
        for kept in [current, previous, staged, unowned, modified] {
            assert!(kept.is_dir());
        }
        assert!(!config.download_dir.exists());
    }

    #[test]
    fn cache_cleanup_respects_numeric_limit_and_preserves_unrelated_files() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("absent");
        let mut config = Config {
            download_dir: home.path().join("cache"),
            max_cache_count: 3,
        };
        {
            let cache = DownloadCache::prepare(&config).unwrap();
            for version in ["1.9.0", "1.10.0", "1.100.0"] {
                let archive = cache.download_path();
                let checksum = fixture::archive(&archive, version, A);
                cache.retain(&archive, version, A, &checksum).unwrap();
            }
        }
        let old = config.download_dir.join(format!("vscu-1.9.0-{A}"));
        fs::write(old.join("personal"), "keep").unwrap();
        fs::write(config.download_dir.join("unrelated"), "keep").unwrap();
        config.max_cache_count = 1;
        let before = fixture::snapshot(home.path());
        clean_with(&root, &config, false, |_| Ok(HashSet::new())).unwrap();
        assert_eq!(before, fixture::snapshot(home.path()));
        clean_with(&root, &config, true, |_| Ok(HashSet::new())).unwrap();
        assert_eq!(fs::read_to_string(old.join("personal")).unwrap(), "keep");
        assert_eq!(
            fs::read_to_string(config.download_dir.join("unrelated")).unwrap(),
            "keep"
        );
        assert!(
            config
                .download_dir
                .join(format!("vscu-1.100.0-{A}/archive.tar.gz"))
                .is_file()
        );
        assert!(
            !config
                .download_dir
                .join(format!("vscu-1.10.0-{A}"))
                .exists()
        );
    }

    #[test]
    fn dangling_protected_links_and_symlink_roots_block_deletion() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("vscu");
        let old = fixture::release(&root, "1.98.0", A, true);
        fixture::select(&root, "previous", &root.join("versions/missing"));
        let config = Config {
            download_dir: home.path().join("cache"),
            max_cache_count: 1,
        };
        assert!(clean_at(&root, &config, true).is_err());
        assert!(old.is_dir());
        let alias = home.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(clean_at(&alias, &config, true).is_err());
    }
}
