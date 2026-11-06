use crate::{
    cache::DownloadCache,
    check,
    config::Config,
    install::{self, Result},
    processes, releases,
};
use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
};

pub async fn run(activate_only: bool) -> Result<()> {
    let (root, command) = install::configured_install_paths()?;
    run_at(
        &root,
        &command,
        activate_only,
        check::LATEST_URL,
        processes::running,
        None,
    )
    .await
}

async fn run_at(
    root: &Path,
    command: &Path,
    activate_only: bool,
    lookup_url: &str,
    running: processes::Scanner,
    config: Option<Config>,
) -> Result<()> {
    check::installed_release(root)?.ok_or("No managed installation; run vscu install")?;
    let _lock = releases::lock(root)?;
    releases::recover(root)?;
    let current =
        check::installed_release(root)?.ok_or("No managed installation; run vscu install")?;
    releases::validate_target(root, &current.application)?;
    require_command(root, command)?;
    if activate_only {
        let target = releases::selected(root, "staged")?.ok_or("No staged update to activate")?;
        if !releases::removable(&target)? {
            return Err(
                "Staged release has no matching ownership manifest; refusing activation".into(),
            );
        }
        ensure_stopped_with(root, running)?;
        releases::activate(root, &target)?;
        println!("Activated {}", target.display());
        return Ok(());
    }

    releases::selected(root, "staged")?;
    let latest = check::fetch_verified(lookup_url).await?;
    if !should_update(&current.release, &latest.release)? {
        println!("Already current; no update needed.");
        if let Some(staged) = releases::selected(root, "staged")? {
            println!(
                "Staged release: {}. Use vscu update --activate after closing VS Code.",
                staged.display()
            );
        }
        return Ok(());
    }

    let config = match config {
        Some(config) => config,
        None => Config::load()?,
    };
    let cache = DownloadCache::prepare(&config)?;
    let archive = match cache.lookup(
        &latest.release.version,
        &latest.release.commit,
        &latest.checksum,
    )? {
        Some(path) => path,
        None => {
            let path = cache.download_path();
            crate::download_vscode(&latest.url, &path).await?;
            path
        }
    };
    let root_for_stage = root.to_owned();
    let target = tokio::task::spawn_blocking(move || {
        stage(
            &root_for_stage,
            &cache,
            &archive,
            &latest.release.version,
            &latest.release.commit,
            &latest.checksum,
        )
    })
    .await??;
    releases::atomic_link(root, "staged", &target)?;
    match running(root) {
        Ok(running) if running.is_empty() => {
            releases::activate(root, &target)?;
            println!("Updated to {}", target.display());
        }
        Ok(_) => println!("Update staged at {}. Close all managed VS Code processes, then run vscu update --activate.", target.display()),
        Err(error) => return Err(format!("Update staged at {}, but activation was blocked: {error}. Retry vscu update --activate after resolving process inspection.", target.display()).into()),
    }
    Ok(())
}

pub fn should_update(current: &check::Release, latest: &check::Release) -> Result<bool> {
    match check::version_key(&current.version)?.cmp(&check::version_key(&latest.version)?) {
        Ordering::Less => Ok(true),
        Ordering::Greater => Err(
            "Installed version is ahead of upstream Stable; refusing an automatic downgrade".into(),
        ),
        Ordering::Equal if current.commit.eq_ignore_ascii_case(&latest.commit) => Ok(false),
        Ordering::Equal => Err(
            "Equal versions have different builds; refusing an update with unknown build order"
                .into(),
        ),
    }
}

fn stage(
    root: &Path,
    cache: &DownloadCache,
    archive: &Path,
    version: &str,
    commit: &str,
    checksum: &str,
) -> Result<PathBuf> {
    install::verify_checksum(archive, checksum)?;
    let staging = tempfile::Builder::new()
        .prefix(".update-")
        .tempdir_in(root)?;
    install::extract_archive(archive, staging.path())?;
    let application = staging.path().join("VSCode-linux-x64");
    install::validate_application(&application, version, commit)?;
    releases::write_manifest(&application, version, commit)?;
    cache.retain(archive, version, commit, checksum)?;
    let destination = root.join("versions").join(format!("{version}-{commit}"));
    match fs::symlink_metadata(&destination) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(&application, &destination)?
        }
        Err(e) => return Err(e.into()),
        Ok(_) => {
            let resolved = fs::canonicalize(&destination)?;
            releases::validate_target(root, &resolved)?;
            if !releases::removable(&resolved)? {
                return Err("Existing target is unowned or modified; refusing to overwrite or activate it automatically".into());
            }
            // A matching manifest establishes ownership, not agreement with this download.
            if install::file_checksum(&destination.join(releases::MANIFEST))?
                != install::file_checksum(&application.join(releases::MANIFEST))?
            {
                return Err("Existing release differs from the verified download".into());
            }
        }
    }
    Ok(fs::canonicalize(destination)?)
}

fn require_command(root: &Path, command: &Path) -> Result<()> {
    if !fs::symlink_metadata(command)?.is_symlink()
        || fs::read_link(command)? != root.join("current/bin/code")
    {
        return Err(
            "Terminal command is not the vscu current/bin/code symlink; inspect with vscu doctor"
                .into(),
        );
    }
    Ok(())
}

fn ensure_stopped_with(root: &Path, running: processes::Scanner) -> Result<()> {
    if !running(root)?.is_empty() {
        return Err("Close all managed VS Code processes before activation or rollback".into());
    }
    Ok(())
}

pub fn rollback() -> Result<()> {
    let (root, command) = install::configured_install_paths()?;
    rollback_at(&root, &command, processes::running)
}

fn rollback_at(root: &Path, command: &Path, running: processes::Scanner) -> Result<()> {
    check::installed_release(root)?.ok_or("No managed installation; run vscu install")?;
    let _lock = releases::lock(root)?;
    releases::recover(root)?;
    require_command(root, command)?;
    let current = releases::selected(root, "current")?.ok_or("No active managed installation")?;
    let previous = releases::selected(root, "previous")?
        .ok_or("No previous release recorded; rollback is unavailable")?;
    if current == previous {
        return Err("Previous release matches current; no rollback available".into());
    }
    ensure_stopped_with(root, running)?;
    releases::activate(root, &previous)?;
    println!(
        "Rolled back to {}. Settings and extensions remain at their current versions.",
        previous.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self as fixture, A, B};

    fn config(root: &Path) -> Config {
        Config {
            download_dir: root.join("cache"),
            max_cache_count: 3,
        }
    }

    fn stopped(_: &Path) -> Result<std::collections::HashSet<PathBuf>> {
        Ok(std::collections::HashSet::new())
    }
    fn running_editor(root: &Path) -> Result<std::collections::HashSet<PathBuf>> {
        Ok(std::collections::HashSet::from([
            root.join("versions/running")
        ]))
    }
    fn uncertain(_: &Path) -> Result<std::collections::HashSet<PathBuf>> {
        Err("process inspection denied".into())
    }

    async fn upstream(
        version: &str,
        commit: &str,
        checksum: &str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let body = serde_json::json!({"productVersion":version,"version":commit,"url":format!("https://example.com/stable/{commit}/code.tar.gz"),"sha256hash":checksum}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let n = stream.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buffer[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (url, task)
    }

    #[tokio::test]
    async fn full_cached_update_can_activate_or_defer_and_then_rollback_offline() {
        for scanner in [stopped as processes::Scanner, running_editor, uncertain] {
            let home = tempfile::tempdir().unwrap();
            let root = home.path();
            let old = fixture::release(root, "1.99.0", A, false);
            fixture::select(root, "current", &old);
            let command = root.join("command");
            std::os::unix::fs::symlink(root.join("current/bin/code"), &command).unwrap();
            let checksum = {
                let cache = DownloadCache::prepare(&config(root)).unwrap();
                let archive = cache.download_path();
                let checksum = fixture::archive(&archive, "1.100.0", B);
                cache.retain(&archive, "1.100.0", B, &checksum).unwrap();
                checksum
            };
            let (url, task) = upstream("1.100.0", B, &checksum).await;
            let result = run_at(root, &command, false, &url, scanner, Some(config(root))).await;
            task.await.unwrap();
            let target = root.join("versions").join(format!("1.100.0-{B}"));
            if let Err(error) = result {
                assert!(error.to_string().contains("process inspection denied"));
            }
            if releases::selected(root, "staged").unwrap().is_some() {
                assert_eq!(
                    releases::selected(root, "current").unwrap(),
                    Some(old.clone())
                );
                assert!(
                    run_at(
                        root,
                        &command,
                        true,
                        "unusable-offline-url",
                        running_editor,
                        None
                    )
                    .await
                    .is_err()
                );
                run_at(root, &command, true, "unusable-offline-url", stopped, None)
                    .await
                    .unwrap();
            }
            assert_eq!(
                releases::selected(root, "current").unwrap(),
                Some(target.clone())
            );
            assert_eq!(
                releases::selected(root, "previous").unwrap(),
                Some(old.clone())
            );
            assert!(releases::selected(root, "staged").unwrap().is_none());
            assert!(rollback_at(root, &command, running_editor).is_err());
            rollback_at(root, &command, stopped).unwrap();
            assert_eq!(releases::selected(root, "current").unwrap(), Some(old));
            assert_eq!(releases::selected(root, "previous").unwrap(), Some(target));
            assert!(root.join("current/bin/code").is_file());
        }
    }

    #[tokio::test]
    async fn current_build_does_not_download_or_load_cache_configuration() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let current = fixture::release(root, "1.100.0", A, true);
        fixture::select(root, "current", &current);
        let command = root.join("command");
        std::os::unix::fs::symlink(root.join("current/bin/code"), &command).unwrap();
        let (url, task) = upstream("1.100.0", A, &"0".repeat(64)).await;
        run_at(
            root,
            &command,
            false,
            &url,
            uncertain,
            Some(Config {
                download_dir: root.join("never-created"),
                max_cache_count: 0,
            }),
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert!(!root.join("never-created").exists());
        assert_eq!(releases::selected(root, "current").unwrap(), Some(current));
    }

    #[test]
    fn update_comparison_refuses_downgrades_and_unknown_build_order() {
        let release = |version: &str, commit: &str| check::Release {
            version: version.into(),
            commit: commit.into(),
        };
        assert!(should_update(&release("1.99.0", A), &release("1.100.0", B)).unwrap());
        assert!(!should_update(&release("1.100.0", A), &release("1.100.0", A)).unwrap());
        assert!(should_update(&release("1.100.0", A), &release("1.99.0", B)).is_err());
        assert!(should_update(&release("1.100.0", A), &release("1.100.0", B)).is_err());
    }

    #[test]
    fn stage_verifies_archives_preserves_current_and_reuses_matching_release() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let old = fixture::release(root, "1.99.0", A, false);
        fixture::select(root, "current", &old);
        let cache = DownloadCache::prepare(&config(root)).unwrap();
        let archive = cache.download_path();
        let checksum = fixture::archive(&archive, "1.100.0", B);
        let destination = stage(root, &cache, &archive, "1.100.0", B, &checksum).unwrap();
        assert_eq!(releases::selected(root, "current").unwrap(), Some(old));
        assert!(releases::removable(&destination).unwrap());
        let archive = cache.lookup("1.100.0", B, &checksum).unwrap().unwrap();
        assert_eq!(
            stage(root, &cache, &archive, "1.100.0", B, &checksum).unwrap(),
            destination
        );
    }

    #[test]
    fn failed_stage_does_not_change_current_or_cached_packages() {
        for invalid in ["checksum", "metadata", "archive"] {
            let home = tempfile::tempdir().unwrap();
            let root = home.path();
            let old = fixture::release(root, "1.99.0", A, true);
            fixture::select(root, "current", &old);
            let cache = DownloadCache::prepare(&config(root)).unwrap();
            let archive = cache.download_path();
            let mut checksum = fixture::archive(&archive, "1.100.0", B);
            if invalid == "checksum" {
                checksum = "0".repeat(64);
            }
            if invalid == "archive" {
                fs::write(&archive, "not gzip").unwrap();
                checksum = install::file_checksum(&archive).unwrap();
            }
            let before = fixture::snapshot(root);
            assert!(
                stage(
                    root,
                    &cache,
                    &archive,
                    if invalid == "metadata" {
                        "1.101.0"
                    } else {
                        "1.100.0"
                    },
                    B,
                    &checksum
                )
                .is_err()
            );
            assert_eq!(before, fixture::snapshot(root), "{invalid}");
        }
    }

    #[test]
    fn command_takeover_and_modified_existing_releases_are_preserved() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let old = fixture::release(root, "1.99.0", A, false);
        fixture::select(root, "current", &old);
        let command = root.join("command");
        fs::write(&command, "keep").unwrap();
        assert!(require_command(root, &command).is_err());
        let destination = fixture::release(root, "1.100.0", B, true);
        fs::write(destination.join("personal"), "keep").unwrap();
        let cache = DownloadCache::prepare(&config(root)).unwrap();
        let archive = cache.download_path();
        let checksum = fixture::archive(&archive, "1.100.0", B);
        assert!(stage(root, &cache, &archive, "1.100.0", B, &checksum).is_err());
        assert_eq!(
            fs::read_to_string(destination.join("personal")).unwrap(),
            "keep"
        );
        assert_eq!(releases::selected(root, "current").unwrap(), Some(old));
    }
}
