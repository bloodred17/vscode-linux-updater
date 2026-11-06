mod cache;
mod check;
mod clean;
mod commands;
mod config;
mod desktop;
mod doctor;
mod install;
mod processes;
mod releases;
mod status;
#[cfg(test)]
mod test_support;
mod update;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use tokio::io::AsyncWriteExt;

#[derive(Parser, Debug)]
#[command(name = "vscu", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<commands::Command>,
}

#[tokio::main]
async fn main() -> install::Result<()> {
    let cli = Cli::parse();
    if std::env::consts::OS != "linux" || std::env::consts::ARCH != "x86_64" {
        return Err("Only Linux x64 is supported".into());
    }

    match cli.command {
        Some(commands::Command::Install { version, desktop }) => {
            install_vscode(&version, desktop).await?
        }
        Some(commands::Command::Update { activate }) => update::run(activate).await?,
        Some(commands::Command::Check) => check::run().await?,
        Some(commands::Command::Status) => status::run()?,
        Some(commands::Command::Rollback) => update::rollback()?,
        Some(commands::Command::Doctor) => doctor::run()?,
        Some(commands::Command::Clean { yes }) => clean::run(yes)?,
        None => {
            status::run()?;
            println!(
                "\nActions: install [--desktop], check, update [--activate], status, rollback, doctor, clean [--yes].\nUse vscu --help for details."
            );
        }
    }
    println!();
    Ok(())
}

struct ReleaseInfo {
    download_url: reqwest::Url,
    checksum: String,
    commit: String,
}

async fn fetch_download_link(version: &str) -> install::Result<ReleaseInfo> {
    println!("Searching {version} ...");

    let url = format!("https://update.code.visualstudio.com/{version}/linux-x64/stable");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let resp = client.head(&url).send().await?;
    if resp.status() != reqwest::StatusCode::FOUND {
        return Err(format!("Release lookup returned {} for {version}", resp.status()).into());
    }
    let checksum = resp
        .headers()
        .get("x-sha256")
        .ok_or("Release lookup is missing its SHA-256 checksum")?
        .to_str()?
        .to_owned();
    install::validate_checksum(&checksum)?;
    let location = resp
        .headers()
        .get("location")
        .ok_or("Release lookup is missing its download URL")?;
    let download_url = reqwest::Url::parse(location.to_str()?)?;
    let commit = install::release_commit(&download_url)?;

    Ok(ReleaseInfo {
        download_url,
        checksum,
        commit,
    })
}

async fn download_vscode(url: &reqwest::Url, destination: &Path) -> install::Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(30 * 60),
        download_with_retries(url, destination),
    )
    .await
    .map_err(|_| "Download exceeded the overall 30-minute timeout")?
}

async fn download_with_retries(url: &reqwest::Url, destination: &Path) -> install::Result<()> {
    println!("Downloading from: {url}");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30 * 60))
        .build()?;

    for attempt in 0..3 {
        match download_attempt(&client, url, destination).await {
            Ok(()) => return Ok(()),
            Err(failure) if failure.retry && attempt < 2 => {
                eprintln!(
                    "Download attempt {} failed: {}; retrying",
                    attempt + 1,
                    failure.message
                );
                tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
            }
            Err(failure) => return Err(failure.message.into()),
        }
    }
    unreachable!()
}

struct DownloadFailure {
    message: String,
    retry: bool,
}

impl From<reqwest::Error> for DownloadFailure {
    fn from(error: reqwest::Error) -> Self {
        Self {
            retry: error.is_timeout()
                || error.is_connect()
                || error.is_body()
                || error.is_decode()
                || error.is_request(),
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for DownloadFailure {
    fn from(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
            retry: false,
        }
    }
}

async fn download_attempt(
    client: &reqwest::Client,
    url: &reqwest::Url,
    destination: &Path,
) -> Result<(), DownloadFailure> {
    let mut download_resp = client.get(url.clone()).send().await?;
    let status = download_resp.status();
    if !status.is_success() {
        return Err(DownloadFailure {
            message: format!("Download returned {status}"),
            retry: status.is_server_error()
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT,
        });
    }

    let progress = match download_resp.content_length() {
        Some(total) => ProgressBar::new(total).with_style(
            ProgressStyle::with_template("{wide_bar:.cyan/blue} {bytes}/{total_bytes} ({eta})")
                .expect("static progress template"),
        ),
        None => ProgressBar::new_spinner().with_style(
            ProgressStyle::with_template("{spinner} Downloaded {bytes}")
                .expect("static progress template"),
        ),
    };

    let result: Result<(), DownloadFailure> = async {
        let mut file = tokio::fs::File::create(destination).await?;

        while let Some(chunk) = download_resp.chunk().await? {
            file.write_all(&chunk).await?;
            progress.inc(chunk.len() as u64);
        }

        file.flush().await?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            progress.finish_and_clear();
            println!("Downloaded {} MB", progress.position() / (1024 * 1024));
        }
        Err(error) => {
            progress.abandon_with_message("Download failed");
            return Err(error);
        }
    }
    Ok(())
}

async fn install_vscode(version: &str, desktop: bool) -> install::Result<()> {
    install::validate_version(version)?;
    let mut installer = install::Installer::prepare()?;
    if desktop {
        installer.enable_desktop()?;
    }
    let release = fetch_download_link(version).await?;
    if !installer.reuse_cached(version, &release.commit, &release.checksum)? {
        download_vscode(&release.download_url, &installer.download_path()).await?;
    }

    println!("Verifying and installing ...");
    let version = version.to_owned();
    tokio::task::spawn_blocking(move || {
        installer.finish(&release.checksum, &version, &release.commit)
    })
    .await??;
    Ok(())
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn server(responses: Vec<String>) -> (reqwest::Url, tokio::task::JoinHandle<usize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            let count = responses.len();
            for response in responses {
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
                assert!(request.starts_with(b"GET / HTTP/1.1"));
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
            count
        });
        (url, task)
    }

    fn response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn retries_transient_responses_and_stops_after_three_attempts() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("archive");
        let (url, task) = server(vec![
            response("503 Unavailable", ""),
            response("429 Too Many Requests", ""),
            response("200 OK", "verified later"),
        ])
        .await;
        download_vscode(&url, &path).await.unwrap();
        assert_eq!(task.await.unwrap(), 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "verified later");
        let (url, task) = server(vec![response("503 Unavailable", ""); 3]).await;
        assert!(download_vscode(&url, &path).await.is_err());
        assert_eq!(task.await.unwrap(), 3);
    }

    #[tokio::test]
    async fn permanent_http_errors_are_not_retried() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("archive");
        let (url, task) = server(vec![response("404 Not Found", "")]).await;
        let error = download_vscode(&url, &path).await.unwrap_err();
        assert!(error.to_string().contains("404"));
        assert_eq!(task.await.unwrap(), 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn retries_truncated_response_from_the_start_instead_of_appending() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("archive");
        let (url, task) = server(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 99\r\nConnection: close\r\n\r\npartial".into(),
            response("200 OK", "complete"),
        ])
        .await;
        download_vscode(&url, &path).await.unwrap();
        assert_eq!(task.await.unwrap(), 2);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "complete");
    }
}
