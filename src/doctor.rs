use crate::{
    check,
    config::Config,
    desktop,
    install::{self, Result},
    processes, releases, status,
};
use std::{
    env, fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::Command,
};

pub fn run() -> Result<()> {
    let (root, command) = install::configured_install_paths()?;
    let paths: Vec<_> = env::var_os("PATH")
        .map(|p| env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut problems = Vec::new();
    match status::status_at(&root, &command, &paths) {
        Ok(report) => {
            println!("{report}");
            if report.contains("PATH conflict")
                || report.contains("not found (no executable")
                || report.contains("missing from PATH")
            {
                problems.push("Terminal integration needs attention".to_owned());
            }
        }
        Err(e) => problems.push(format!("Installation/status: {e}")),
    }
    match Config::load() {
        Ok(c) => println!(
            "Download cache: {} (limit {})",
            c.download_dir.display(),
            c.max_cache_count
        ),
        Err(e) => problems.push(format!("Configuration: {e}")),
    }
    if releases::has_journal(&root) {
        problems.push(
            "Interrupted activation: next update/rollback will recover bookkeeping under lock"
                .into(),
        );
    }
    for name in ["previous", "staged"] {
        match releases::selected(&root, name) {
            Ok(Some(path)) => println!("{name}: {}", path.display()),
            Ok(None) => println!("{name}: none"),
            Err(e) => problems.push(format!("{name}: {e}")),
        }
    }
    match check::installed_release(&root) {
        Ok(Some(installed)) => {
            if let Err(e) = releases::validate_target(&root, &installed.application) {
                problems.push(format!("Application files: {e}"));
            }
            if !releases::removable(&installed.application).unwrap_or(false) {
                println!(
                    "Ownership: no complete matching manifest; clean will preserve this release."
                );
            }
            if fs::symlink_metadata(installed.application.join("data")).is_ok() {
                problems.push("Portable-mode data exists in the managed release; migration requires an explicit decision".into());
            }
            let sandbox = installed.application.join("chrome-sandbox");
            match fs::metadata(&sandbox) {
                Ok(m) => println!(
                    "Sandbox helper: uid {}, mode {:o}. User-namespace/AppArmor policy and actual launch are not verified.",
                    m.uid(),
                    m.permissions().mode() & 0o7777
                ),
                Err(e) => problems.push(format!(
                    "Sandbox helper unavailable: {e}; sandbox policy remains unverified"
                )),
            }
            dependencies(&installed.application, &mut problems);
            for setting in [
                "/proc/sys/kernel/unprivileged_userns_clone",
                "/proc/sys/user/max_user_namespaces",
                "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
            ] {
                match fs::read_to_string(setting) {
                    Ok(value) => println!(
                        "Sandbox policy {setting}: {} (actual launch still unverified)",
                        value.trim()
                    ),
                    Err(_) => println!("Sandbox policy {setting}: unavailable; readiness unknown"),
                }
            }
            match processes::running(&root) {
                Ok(running) => {
                    println!("Running managed releases for this user: {}", running.len())
                }
                Err(e) => problems.push(format!("Process inspection: {e}")),
            }
        }
        Ok(None) => problems.push("No active managed installation; run vscu install".into()),
        Err(e) => problems.push(format!("Managed release: {e}")),
    }
    match desktop::path(&root) {
        Ok(path) => match fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => match (fs::read_to_string(&path), desktop::content(&root)) {
                (Ok(actual), Ok(expected)) if actual == expected => {
                    println!("Desktop launcher: {} (managed)", path.display())
                }
                _ => problems.push(format!(
                    "Desktop launcher differs from the managed template: {}",
                    path.display()
                )),
            },
            Ok(_) => problems.push(format!(
                "Desktop launcher is not a regular file: {}",
                path.display()
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!(
                "Desktop launcher: not installed (optional; install --desktop enables it)."
            ),
            Err(e) => problems.push(format!("Desktop launcher: {e}")),
        },
        Err(e) => problems.push(format!("Desktop path: {e}")),
    }
    for path in [&root, command.parent().unwrap_or(Path::new("/"))] {
        if let Ok(m) = fs::metadata(path) {
            if m.permissions().readonly() {
                problems.push(format!("No write permission bits on {}", path.display()));
            }
            println!(
                "Permissions {}: uid {}, mode {:o}; effective ACL access is not tested by writing.",
                path.display(),
                m.uid(),
                m.permissions().mode() & 0o7777
            );
        }
    }
    if problems.is_empty() {
        println!(
            "Local checks passed. Actual COSMIC launch, display integration, and sandbox/dependency readiness remain unverified."
        );
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("Issue: {problem}");
        }
        Err(format!(
            "Doctor found {} issue(s); no files were changed",
            problems.len()
        )
        .into())
    }
}

/// Inspect ELF metadata with a host tool, never execute the downloaded application.
fn dependencies(application: &Path, problems: &mut Vec<String>) {
    let output = match Command::new("/usr/bin/readelf")
        .args(["--dynamic", "--wide"])
        .arg(application.join("code"))
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => {
            println!(
                "Dependencies: ELF inspection unavailable; install binutils/readelf or inspect the actual loader error. No launch was attempted."
            );
            return;
        }
    };
    let mut directories = vec![
        application.to_owned(),
        application.join("lib"),
        Path::new("/lib/x86_64-linux-gnu").into(),
        Path::new("/usr/lib/x86_64-linux-gnu").into(),
        Path::new("/lib64").into(),
        Path::new("/usr/lib64").into(),
    ];
    if let Some(paths) = env::var_os("LD_LIBRARY_PATH") {
        directories.extend(env::split_paths(&paths));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut needed = 0;
    for line in text.lines().filter(|line| line.contains("(NEEDED)")) {
        if let Some(name) = line.split('[').nth(1).and_then(|v| v.split(']').next()) {
            needed += 1;
            if directories
                .iter()
                .any(|directory| directory.join(name).is_file())
            {
                println!(
                    "Dependency {name}: found in a standard, application, or LD_LIBRARY_PATH directory"
                );
            } else {
                problems.push(format!("Dependency {name}: not found in checked directories; install the matching Linux x64 library or inspect loader configuration/RPATH"));
            }
        }
    }
    println!(
        "Inspected {needed} direct ELF dependencies. Transitive libraries, custom loader paths, ABI compatibility, and actual launch remain unverified."
    );
}
