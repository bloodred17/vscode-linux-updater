mod cache;
mod check;
mod commands;
mod config;
mod install;
mod status;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use tokio::io::AsyncWriteExt;

// #[derive(Parser, Debug)]
// #[command(version, about, long_about = None)]
// struct ArgsDemo {
//     #[arg(short, long)]
//     name: String,

//     #[arg(short, long, default_value_t = 1)]
//     count: u8,
// }

// fn main() {
//     let args_demo = ArgsDemo::parse();

//     for _ in 0..args_demo.count {
//         print!("Hello {}!", args_demo.name);
//     }
// }

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: commands::Command,
}

#[tokio::main]
async fn main() -> install::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        commands::Command::Install { version } => {
            install_vscode(&version).await?;
        }
        commands::Command::Update => {
            install_vscode("latest").await?;
        }
        commands::Command::Check => check::run().await?,
        commands::Command::Status => status::run()?,
        _ => {
            print!("Others")
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
    println!("Downloading from: {url}");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30 * 60))
        .build()?;

    let mut download_resp = client.get(url.clone()).send().await?;
    if !download_resp.status().is_success() {
        return Err(format!("Download returned {}", download_resp.status()).into());
    }

    let progress = match download_resp.content_length() {
        Some(total) => ProgressBar::new(total).with_style(ProgressStyle::with_template(
            "{wide_bar:.cyan/blue} {bytes}/{total_bytes} ({eta})",
        )?),
        None => ProgressBar::new_spinner().with_style(ProgressStyle::with_template(
            "{spinner} Downloaded {bytes}",
        )?),
    };

    let result: install::Result<()> = async {
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

async fn install_vscode(version: &str) -> install::Result<()> {
    install::validate_version(version)?;
    let mut installer = install::Installer::prepare()?;
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
