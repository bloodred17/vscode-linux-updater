use crate::{
    check,
    install::{self, Result},
};
use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub fn run() -> Result<()> {
    if env::consts::OS != "linux" || env::consts::ARCH != "x86_64" {
        return Err("Only Linux x64 is supported".into());
    }
    let (root, command) = install::configured_install_paths()?;
    // An unset PATH has no search entries; an explicitly empty entry means cwd.
    let paths: Vec<_> = env::var_os("PATH")
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default();
    println!("{}", status_at(&root, &command, &paths)?);
    Ok(())
}

pub(crate) fn status_at(root: &Path, command: &Path, paths: &[PathBuf]) -> Result<String> {
    let installed = check::installed_release(root)?;
    let mut lines = vec![format!("Managed root: {}", root.display())];
    if let Some(installed) = &installed {
        lines.push(format!(
            "Managed Stable: {} ({})",
            installed.release.version, installed.release.commit
        ));
        lines.push(format!("Installation: {}", installed.application.display()));
    } else {
        lines.push("No vscu-managed installation found.".into());
        lines.push("Run vscu install to install it.".into());
    }
    for name in ["previous", "staged"] {
        if let Some(target) = crate::releases::selected(root, name)? {
            lines.push(format!("{name}: {}", target.display()));
        }
    }
    if crate::releases::has_journal(root) {
        lines.push("Interrupted activation bookkeeping: run update --activate or rollback to recover under lock.".into());
    }

    match code_on_path(paths)? {
        Some((candidate, resolved)) => {
            lines.push(format!("code on PATH: {}", candidate.display()));
            lines.push(format!("Resolved command: {}", resolved.display()));
            if let Some(installed) = &installed {
                let managed = installed.application.join("bin/code");
                let selects_managed = match fs::canonicalize(&managed) {
                    Ok(target) => target.starts_with(&installed.application) && target == resolved,
                    Err(error) if missing(&error) => false,
                    Err(error) => {
                        return Err(format!("Cannot resolve {}: {error}", managed.display()).into());
                    }
                };
                lines.push(if selects_managed {
                    "code selects the active managed release.".into()
                } else {
                    "PATH conflict: code does not select the active managed release.".into()
                });
            } else {
                lines.push(
                    "The PATH command is outside an active vscu-managed installation.".into(),
                );
            }
        }
        None => lines.push("code on PATH: not found (no executable command).".into()),
    }

    if let Some(bin) = command.parent() {
        // Canonicalize existing directory aliases so equivalent PATH entries count.
        let bin_resolved = fs::canonicalize(bin).ok();
        let on_path = paths.iter().any(|path| {
            path == bin
                || bin_resolved.as_ref().is_some_and(|bin| {
                    fs::canonicalize(if path.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        path
                    })
                    .is_ok_and(|path| &path == bin)
                })
        });
        if !on_path {
            lines.push(format!("{} is missing from PATH.", bin.display()));
        }
    }
    Ok(lines.join("\n"))
}

fn missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Resolve filesystem commands only; shell aliases, functions and wrappers are not executed.
pub(crate) fn code_on_path(paths: &[PathBuf]) -> Result<Option<(PathBuf, PathBuf)>> {
    for directory in paths {
        let candidate = directory.join("code");
        let metadata = match fs::metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if missing(&error) => continue,
            Err(error) => {
                return Err(format!("Cannot inspect {}: {error}", candidate.display()).into());
            }
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            let resolved = fs::canonicalize(&candidate)
                .map_err(|error| format!("Cannot resolve {}: {error}", candidate.display()))?;
            return Ok(Some((candidate, resolved)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn executable(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Any accidental execution fails; status must only inspect files.
        fs::write(path, "#!/bin/sh\nexit 99\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn installation(home: &Path, absolute: bool) -> (PathBuf, PathBuf, PathBuf) {
        let root = home.join("data/vscu");
        let relative = PathBuf::from(format!("versions/1.100.0-{COMMIT}"));
        let application = root.join(&relative);
        fs::create_dir_all(application.join("resources/app")).unwrap();
        fs::write(
            application.join("resources/app/package.json"),
            r#"{"version":"1.100.0"}"#,
        )
        .unwrap();
        fs::write(
            application.join("resources/app/product.json"),
            serde_json::json!({"commit": COMMIT, "applicationName": "code"}).to_string(),
        )
        .unwrap();
        executable(&application.join("bin/code"));
        symlink(
            if absolute { &application } else { &relative },
            root.join("current"),
        )
        .unwrap();
        let command = home.join(".local/bin/code");
        fs::create_dir_all(command.parent().unwrap()).unwrap();
        symlink(root.join("current/bin/code"), &command).unwrap();
        (root, command, application)
    }

    #[test]
    fn reports_managed_version_location_and_command_without_changes() {
        for absolute in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let (root, command, application) = installation(home.path(), absolute);
            let current = fs::read_link(root.join("current")).unwrap();
            let link = fs::read_link(&command).unwrap();
            let output = status_at(&root, &command, &[command.parent().unwrap().into()]).unwrap();
            assert!(output.contains(&format!("Managed Stable: 1.100.0 ({COMMIT})")));
            assert!(output.contains(&format!("Installation: {}", application.display())));
            assert!(output.contains(&format!("code on PATH: {}", command.display())));
            assert!(output.contains(&format!(
                "Resolved command: {}",
                application.join("bin/code").display()
            )));
            assert!(output.contains("code selects the active managed release."));
            assert!(!output.contains("missing from PATH"));
            assert_eq!(fs::read_link(root.join("current")).unwrap(), current);
            assert_eq!(fs::read_link(&command).unwrap(), link);
            assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        }
    }

    #[test]
    fn missing_installation_and_path_are_normal_without_creating_files() {
        let home = tempfile::tempdir().unwrap();
        let output = status_at(
            &home.path().join("data/vscu"),
            &home.path().join(".local/bin/code"),
            &[],
        )
        .unwrap();
        assert!(output.contains("No vscu-managed installation found."));
        assert!(output.contains("vscu install"));
        assert!(output.contains("code on PATH: not found"));
        assert!(output.contains("missing from PATH"));
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    }

    #[test]
    fn first_executable_wins_and_external_wrappers_are_only_reported() {
        let home = tempfile::tempdir().unwrap();
        let (root, command, _) = installation(home.path(), false);
        let external = home.path().join("external/code");
        executable(&external);
        let bin = command.parent().unwrap().to_owned();
        let external_bin = external.parent().unwrap().to_owned();
        let output = status_at(&root, &command, &[external_bin.clone(), bin.clone()]).unwrap();
        assert!(output.contains("PATH conflict"));
        assert!(output.contains(&format!("Resolved command: {}", external.display())));
        let output = status_at(&root, &command, &[bin, external_bin.clone()]).unwrap();
        assert!(output.contains("code selects the active managed release."));
        let output = status_at(&home.path().join("absent"), &command, &[external_bin]).unwrap();
        assert!(output.contains("outside an active vscu-managed installation"));
    }

    #[test]
    fn skips_nonexecutables_directories_and_dangling_links() {
        let home = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for kind in ["missing", "file", "directory", "broken", "executable"] {
            let bin = home.path().join(kind);
            fs::create_dir(&bin).unwrap();
            let candidate = bin.join("code");
            match kind {
                "file" => fs::write(&candidate, "not executable").unwrap(),
                "directory" => fs::create_dir(&candidate).unwrap(),
                "broken" => symlink("absent", &candidate).unwrap(),
                "executable" => executable(&candidate),
                _ => (),
            }
            paths.push(bin);
        }
        assert!(code_on_path(&paths[..4]).unwrap().is_none());
        assert_eq!(
            code_on_path(&paths).unwrap().unwrap().0,
            paths[4].join("code")
        );
    }

    #[test]
    fn managed_installation_can_exist_without_an_executable_on_path() {
        let home = tempfile::tempdir().unwrap();
        let (root, command, _) = installation(home.path(), false);
        let output = status_at(&root, &command, &[]).unwrap();
        assert!(output.contains("Managed Stable: 1.100.0"));
        assert!(output.contains("code on PATH: not found"));
        assert!(output.contains("missing from PATH"));
    }

    #[test]
    fn path_directory_aliases_and_relative_command_symlinks_resolve() {
        let home = tempfile::tempdir().unwrap();
        let (root, command, application) = installation(home.path(), false);
        let alias = home.path().join("bin-alias");
        symlink(command.parent().unwrap(), &alias).unwrap();
        fs::remove_file(&command).unwrap();
        symlink("../../data/vscu/current/bin/code", &command).unwrap();
        let output = status_at(&root, &command, &[alias]).unwrap();
        assert!(output.contains("code selects the active managed release."));
        assert!(output.contains(&application.join("bin/code").display().to_string()));
        assert!(!output.contains("missing from PATH"));
    }

    #[test]
    fn rejects_invalid_managed_state_and_path_inspection_errors() {
        for kind in ["broken", "non-link", "outside", "metadata"] {
            let home = tempfile::tempdir().unwrap();
            let (root, command, application) = installation(home.path(), false);
            if kind == "metadata" {
                fs::write(
                    application.join("resources/app/package.json"),
                    "invalid JSON",
                )
                .unwrap();
            } else {
                fs::remove_file(root.join("current")).unwrap();
                match kind {
                    "broken" => symlink("versions/missing", root.join("current")).unwrap(),
                    "non-link" => fs::create_dir(root.join("current")).unwrap(),
                    "outside" => symlink(home.path(), root.join("current")).unwrap(),
                    _ => unreachable!(),
                }
            }
            assert!(status_at(&root, &command, &[]).is_err(), "accepted {kind}");
        }
        let home = tempfile::tempdir().unwrap();
        symlink("code", home.path().join("code")).unwrap();
        let error = code_on_path(&[home.path().into()]).unwrap_err().to_string();
        assert!(error.contains("Cannot inspect"));
    }
}
