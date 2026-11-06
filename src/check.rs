use crate::install::{self, Result};
use serde::Deserialize;
use std::{
    cmp::Ordering,
    env, fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

const LATEST_URL: &str = "https://update.code.visualstudio.com/api/update/linux-x64/stable/latest";

#[derive(Debug)]
struct Release {
    version: String,
    commit: String,
}

#[derive(Deserialize)]
struct LatestMetadata {
    #[serde(rename = "productVersion")]
    product_version: String,
    version: String,
    url: String,
    sha256hash: String,
}

#[derive(Deserialize)]
struct PackageMetadata {
    version: String,
}

#[derive(Deserialize)]
struct ProductMetadata {
    commit: String,
    #[serde(rename = "applicationName")]
    application_name: String,
    version: Option<String>,
}

pub async fn run() -> Result<()> {
    if env::consts::OS != "linux" || env::consts::ARCH != "x86_64" {
        return Err("Only Linux x64 is supported".into());
    }
    let (root, _) = install::configured_install_paths()?;
    println!("{}", check_at(&root, LATEST_URL).await?);
    Ok(())
}

async fn check_at(root: &Path, lookup_url: &str) -> Result<String> {
    let installed = installed_release(root)?;
    let latest = fetch_latest(lookup_url).await?;
    describe(installed.as_ref(), &latest)
}

fn installed_release(root: &Path) -> Result<Option<Release>> {
    let current = root.join("current");
    match fs::symlink_metadata(&current) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("Cannot inspect {}: {error}", current.display()).into());
        }
        Ok(metadata) if !metadata.is_symlink() => {
            return Err(format!("{} must be a managed release symlink", current.display()).into());
        }
        Ok(_) => (),
    }

    let versions = root.join("versions");
    if !fs::symlink_metadata(&versions)?.is_dir() {
        return Err("Managed versions must be a directory, not a symlink".into());
    }
    let versions = fs::canonicalize(versions)?;
    let application = fs::canonicalize(&current).map_err(|error| {
        format!(
            "Cannot resolve managed installation {}: {error}",
            current.display()
        )
    })?;
    if application.parent() != Some(versions.as_path()) || !application.is_dir() {
        return Err(
            "The current symlink must select a release inside the managed versions directory"
                .into(),
        );
    }

    let package: PackageMetadata =
        serde_json::from_slice(&read_metadata(&application, "resources/app/package.json")?)?;
    let product: ProductMetadata =
        serde_json::from_slice(&read_metadata(&application, "resources/app/product.json")?)?;
    version_key(&package.version)?;
    validate_commit(&product.commit)?;
    let identity = format!("{}-{}", package.version, product.commit);
    if application.file_name() != Some(std::ffi::OsStr::new(&identity))
        || product.application_name != "code"
        || product
            .version
            .as_ref()
            .is_some_and(|version| version != &package.version)
    {
        return Err("Managed version/build metadata does not match its release directory".into());
    }
    Ok(Some(Release {
        version: package.version,
        commit: product.commit,
    }))
}

fn read_metadata(application: &Path, relative: &str) -> Result<Vec<u8>> {
    let path = application.join(relative);
    let resolved: PathBuf = fs::canonicalize(&path)
        .map_err(|error| format!("Cannot read managed metadata {}: {error}", path.display()))?;
    if !resolved.starts_with(application) || !resolved.is_file() {
        return Err(format!(
            "Managed metadata {} must be a file inside its release",
            path.display()
        )
        .into());
    }
    Ok(fs::read(resolved)?)
}

async fn fetch_latest(lookup_url: &str) -> Result<Release> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    let response = client
        .get(lookup_url)
        .send()
        .await
        .map_err(|error| format!("Latest Stable lookup failed: {error}"))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!("Latest Stable lookup returned {}", response.status()).into());
    }
    let metadata: LatestMetadata = response
        .json()
        .await
        .map_err(|error| format!("Invalid latest Stable metadata: {error}"))?;
    version_key(&metadata.product_version)?;
    validate_commit(&metadata.version)?;
    install::validate_checksum(&metadata.sha256hash)?;
    let url = reqwest::Url::parse(&metadata.url)?;
    let commit = install::release_commit(&url)?;
    if !commit.eq_ignore_ascii_case(&metadata.version) {
        return Err("Latest Stable metadata and download URL identify different builds".into());
    }
    Ok(Release {
        version: metadata.product_version,
        commit: metadata.version,
    })
}

fn validate_commit(commit: &str) -> Result<()> {
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Invalid VS Code build commit".into());
    }
    Ok(())
}

fn version_key(version: &str) -> Result<[u64; 3]> {
    install::validate_version(version)?;
    let parts = version
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<Vec<u64>, _>>()?;
    parts
        .try_into()
        .map_err(|_| "VS Code version must be major.minor.patch".into())
}

fn describe(installed: Option<&Release>, latest: &Release) -> Result<String> {
    let latest_line = format!("Latest Stable: {} ({})", latest.version, latest.commit);
    let Some(installed) = installed else {
        return Ok(format!(
            "No vscu-managed installation found.\n{latest_line}\nRun vscu install to install it."
        ));
    };
    let outcome = match version_key(&installed.version)?.cmp(&version_key(&latest.version)?) {
        Ordering::Less => "Update available.",
        Ordering::Greater => "Installed version is ahead of upstream Stable.",
        Ordering::Equal if installed.commit.eq_ignore_ascii_case(&latest.commit) => "Up to date.",
        Ordering::Equal => "Installed and upstream builds differ; build order is unknown.",
    };
    Ok(format!(
        "Installed: {} ({})\n{latest_line}\n{outcome}",
        installed.version, installed.commit
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_COMMIT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn release(version: &str, commit: &str) -> Release {
        Release {
            version: version.to_owned(),
            commit: commit.to_owned(),
        }
    }

    fn installation(root: &Path, absolute: bool) -> PathBuf {
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
            serde_json::to_vec(&serde_json::json!({
                "commit": COMMIT, "applicationName": "code"
            }))
            .unwrap(),
        )
        .unwrap();
        symlink(
            if absolute { &application } else { &relative },
            root.join("current"),
        )
        .unwrap();
        application
    }

    fn latest_metadata() -> serde_json::Value {
        serde_json::json!({
            "productVersion": "1.101.0",
            "version": COMMIT,
            "url": format!("https://example.com/stable/{COMMIT}/code.tar.gz"),
            "sha256hash": "0".repeat(64)
        })
    }

    async fn server(status: &str, body: &str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(request.starts_with(b"GET / HTTP/1.1\r\n"));
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (url, task)
    }

    #[test]
    fn comparisons_cover_numeric_ordering_and_build_changes() {
        for (installed, latest, expected) in [
            (
                release("1.99.0", COMMIT),
                release("1.100.0", COMMIT),
                "Update available",
            ),
            (
                release("1.100.1", COMMIT),
                release("1.100.0", COMMIT),
                "ahead of upstream",
            ),
            (
                release("2.0.0", COMMIT),
                release("1.999.0", COMMIT),
                "ahead of upstream",
            ),
            (
                release("1.100.0", COMMIT),
                release("1.100.0", COMMIT),
                "Up to date",
            ),
            (
                release("1.100.0", COMMIT),
                release("1.100.0", OTHER_COMMIT),
                "build order is unknown",
            ),
            (
                release("1.100.0", COMMIT),
                release("1.100.0", &COMMIT.to_uppercase()),
                "Up to date",
            ),
        ] {
            assert!(
                describe(Some(&installed), &latest)
                    .unwrap()
                    .contains(expected)
            );
        }
        for invalid in ["latest", "1.2", "1.2.3-beta", "18446744073709551616.0.0"] {
            assert!(version_key(invalid).is_err());
        }
    }

    #[test]
    fn reads_absolute_and_relative_managed_targets() {
        for absolute in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let application = installation(home.path(), absolute);
            let installed = installed_release(home.path()).unwrap().unwrap();
            assert_eq!(installed.version, "1.100.0");
            assert_eq!(installed.commit, COMMIT);
            assert!(application.is_dir());
        }
    }

    #[test]
    fn rejects_broken_unmanaged_and_non_symlink_current_entries() {
        for kind in ["broken", "outside", "directory", "file", "versions-link"] {
            let home = tempfile::tempdir().unwrap();
            let root = home.path().join("vscu");
            let application = installation(&root, false);
            let current = root.join("current");
            fs::remove_file(&current).unwrap();
            match kind {
                "broken" => symlink("versions/missing", &current).unwrap(),
                "outside" => symlink(home.path(), &current).unwrap(),
                "directory" => fs::create_dir(&current).unwrap(),
                "file" => fs::write(&current, "keep me").unwrap(),
                "versions-link" => {
                    fs::rename(root.join("versions"), home.path().join("external")).unwrap();
                    symlink(home.path().join("external"), root.join("versions")).unwrap();
                    symlink(&application, &current).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(installed_release(&root).is_err(), "accepted {kind}");
            assert!(fs::symlink_metadata(current).is_ok());
        }
    }

    #[test]
    fn rejects_invalid_or_mismatched_local_metadata() {
        for invalid in [
            "json",
            "missing",
            "version",
            "commit",
            "application",
            "product-version",
            "escaping-metadata",
        ] {
            let home = tempfile::tempdir().unwrap();
            let application = installation(home.path(), true);
            let package = application.join("resources/app/package.json");
            let product = application.join("resources/app/product.json");
            match invalid {
                "json" => fs::write(&package, "broken JSON").unwrap(),
                "missing" => fs::remove_file(&product).unwrap(),
                "version" => fs::write(&package, r#"{"version":"1.101.0"}"#).unwrap(),
                "escaping-metadata" => {
                    fs::remove_file(&package).unwrap();
                    let external = home.path().join("external.json");
                    fs::write(&external, r#"{"version":"1.100.0"}"#).unwrap();
                    symlink(external, package).unwrap();
                }
                _ => {
                    let mut metadata =
                        serde_json::json!({"commit": COMMIT, "applicationName": "code"});
                    match invalid {
                        "commit" => metadata["commit"] = OTHER_COMMIT.into(),
                        "application" => metadata["applicationName"] = "code-insiders".into(),
                        "product-version" => metadata["version"] = "1.99.0".into(),
                        _ => unreachable!(),
                    }
                    fs::write(product, serde_json::to_vec(&metadata).unwrap()).unwrap();
                }
            }
            assert!(
                installed_release(home.path()).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[tokio::test]
    async fn checks_missing_and_existing_installations_without_writing_files() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("missing");
        let (url, task) = server("200 OK", &latest_metadata().to_string()).await;
        let output = check_at(&root, &url).await.unwrap();
        task.await.unwrap();
        assert!(output.contains("No vscu-managed installation"));
        assert!(output.contains("Latest Stable: 1.101.0"));
        assert!(output.contains("vscu install"));
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);

        let application = installation(&root, false);
        let before = fs::read(application.join("resources/app/product.json")).unwrap();
        let (url, task) = server("200 OK", &latest_metadata().to_string()).await;
        let output = check_at(&root, &url).await.unwrap();
        task.await.unwrap();
        assert!(output.contains("Installed: 1.100.0"));
        assert!(output.contains("Update available"));
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        assert_eq!(
            fs::read(application.join("resources/app/product.json")).unwrap(),
            before
        );
        assert_eq!(
            fs::read_link(root.join("current")).unwrap(),
            PathBuf::from(format!("versions/1.100.0-{COMMIT}"))
        );
    }

    #[tokio::test]
    async fn rejects_failed_lookup_and_invalid_upstream_metadata() {
        for invalid in [
            "http",
            "redirect",
            "json",
            "missing",
            "version",
            "commit",
            "checksum",
            "url",
            "build-mismatch",
        ] {
            let mut metadata = latest_metadata();
            let mut status = "200 OK";
            match invalid {
                "http" => status = "503 Service Unavailable",
                "redirect" => status = "302 Found",
                "missing" => {
                    metadata.as_object_mut().unwrap().remove("productVersion");
                }
                "version" => metadata["productVersion"] = "latest".into(),
                "commit" => metadata["version"] = "invalid".into(),
                "checksum" => metadata["sha256hash"] = "invalid".into(),
                "url" => {
                    metadata["url"] =
                        format!("http://example.com/stable/{COMMIT}/code.tar.gz").into()
                }
                "build-mismatch" => metadata["version"] = OTHER_COMMIT.into(),
                _ => (),
            }
            let body = if invalid == "json" {
                "invalid JSON".to_owned()
            } else {
                metadata.to_string()
            };
            let (url, task) = server(status, &body).await;
            assert!(fetch_latest(&url).await.is_err(), "accepted {invalid}");
            task.await.unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert!(fetch_latest(&url).await.is_err());
    }
}
