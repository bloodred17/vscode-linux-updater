//! Never execute VS Code to discover running releases.
use crate::install::Result;
use std::{
    collections::HashSet,
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub type Scanner = fn(&Path) -> Result<HashSet<PathBuf>>;

pub fn running(root: &Path) -> Result<HashSet<PathBuf>> {
    let versions = fs::canonicalize(root.join("versions"))?;
    scan(Path::new("/proc"), &versions)
}

fn scan(proc: &Path, versions: &Path) -> Result<HashSet<PathBuf>> {
    let mut releases = HashSet::new();
    // Inspect the installing user's processes. Cross-user launches are outside this user-local lifecycle.
    let uid = fs::metadata(proc.join("self"))?.uid();
    for entry in fs::read_dir(proc)? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if vanished(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        if metadata.uid() != uid {
            continue;
        }
        let executable = match fs::read_link(path.join("exe")) {
            Ok(path) => path,
            Err(e) if vanished(&e) => {
                // Zombie/kernel threads legitimately have no executable. A live process that cannot be inspected is uncertain.
                match fs::read_to_string(path.join("status")) {
                    Ok(status)
                        if status.lines().any(|l| {
                            l.starts_with("State:")
                                && (l.contains("Z (zombie)") || l.contains("X (dead)"))
                        }) || status.lines().any(|l| l == "Kthread:\t1") =>
                    {
                        continue;
                    }
                    Err(e) if vanished(&e) => continue,
                    _ => {
                        return Err(format!(
                            "Cannot determine executable for {}; activation/cleanup blocked",
                            path.display()
                        )
                        .into());
                    }
                }
            }
            Err(e) => {
                return Err(format!(
                    "Cannot inspect {}: {e}; activation/cleanup blocked",
                    path.display()
                )
                .into());
            }
        };
        if let Ok(relative) = executable.strip_prefix(versions)
            && let Some(name) = relative.components().next()
        {
            releases.insert(versions.join(name.as_os_str()));
        }
    }
    Ok(releases)
}

fn vanished(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn process_scan_identifies_release_paths_and_ignores_other_executables() {
        let home = tempfile::tempdir().unwrap();
        let proc = home.path().join("proc");
        fs::create_dir_all(proc.join("self")).unwrap();
        let versions = home.path().join("versions");
        for (pid, target) in [
            ("10", versions.join("build/code")),
            ("11", PathBuf::from("/usr/bin/sleep")),
            ("12", versions.join("old/code (deleted)")),
        ] {
            fs::create_dir(proc.join(pid)).unwrap();
            symlink(target, proc.join(pid).join("exe")).unwrap();
        }
        assert_eq!(
            scan(&proc, &versions).unwrap(),
            HashSet::from([versions.join("build"), versions.join("old")])
        );
    }

    #[test]
    fn uninspectable_live_processes_block_but_zombies_do_not() {
        let home = tempfile::tempdir().unwrap();
        let proc = home.path();
        fs::create_dir(proc.join("self")).unwrap();
        fs::create_dir(proc.join("10")).unwrap();
        fs::write(proc.join("10/status"), "State:\tS (sleeping)\n").unwrap();
        assert!(scan(proc, Path::new("/versions")).is_err());
        fs::write(proc.join("10/status"), "State:\tZ (zombie)\n").unwrap();
        assert!(scan(proc, Path::new("/versions")).unwrap().is_empty());
    }
}
