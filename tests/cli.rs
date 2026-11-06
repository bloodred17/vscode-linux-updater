use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::{Command, Output},
};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vscu"))
        .args(args)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("PATH", home.join(".local/bin"))
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .current_dir(home)
        .output()
        .unwrap()
}

fn output(result: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    )
}

fn release(home: &Path, version: &str) -> std::path::PathBuf {
    let root = home.join("data/vscu");
    let commit = "a".repeat(40);
    let app = root.join("versions").join(format!("{version}-{commit}"));
    fs::create_dir_all(app.join("resources/app")).unwrap();
    fs::create_dir(app.join("bin")).unwrap();
    fs::write(
        app.join("resources/app/package.json"),
        serde_json::json!({"version":version}).to_string(),
    )
    .unwrap();
    fs::write(
        app.join("resources/app/product.json"),
        serde_json::json!({"applicationName":"code","commit":commit}).to_string(),
    )
    .unwrap();
    for path in [app.join("code"), app.join("bin/code")] {
        fs::write(
            &path,
            "#!/bin/sh\n/usr/bin/touch command-was-executed\nexit 99\n",
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    app
}

#[test]
fn default_status_and_clean_preview_work_offline_without_creating_files() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec![],
        vec!["status"],
        vec!["clean"],
        vec!["clean", "--yes"],
    ] {
        let result = run(home.path(), &args);
        assert!(result.status.success(), "{}", output(&result));
        assert!(!output(&result).contains("Others"));
        if args.is_empty() {
            assert!(output(&result).contains("Actions:"));
        }
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    }
    let help = run(home.path(), &["--help"]);
    assert!(help.status.success());
    assert!(output(&help).contains("Usage: vscu"));
    assert!(output(&run(home.path(), &["update", "--help"])).contains("--activate"));
    assert!(output(&run(home.path(), &["install", "--help"])).contains("--desktop"));
    assert!(output(&run(home.path(), &["clean", "--help"])).contains("--yes"));
}

#[test]
fn missing_installation_commands_fail_with_useful_errors_without_network() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["update"],
        vec!["update", "--activate"],
        vec!["rollback"],
        vec!["doctor"],
    ] {
        let result = run(home.path(), &args);
        assert_eq!(result.status.code(), Some(1), "{}", output(&result));
        assert!(output(&result).contains("vscu install"));
        assert!(!output(&result).contains("Others"));
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    }
}

#[test]
fn status_doctor_and_missing_staged_state_never_execute_application() {
    let home = tempfile::tempdir().unwrap();
    let app = release(home.path(), "1.100.0");
    let root = home.path().join("data/vscu");
    symlink(&app, root.join("current")).unwrap();
    let command = home.path().join(".local/bin/code");
    fs::create_dir_all(command.parent().unwrap()).unwrap();
    symlink(root.join("current/bin/code"), &command).unwrap();
    let status = run(home.path(), &["status"]);
    assert!(status.status.success(), "{}", output(&status));
    assert!(output(&status).contains("code selects the active managed release"));
    let doctor = run(home.path(), &["doctor"]);
    assert_eq!(doctor.status.code(), Some(1));
    assert!(output(&doctor).contains("Sandbox helper unavailable"));
    assert!(!home.path().join("command-was-executed").exists());
    assert!(!root.join("install.lock").exists());
    let activation = run(home.path(), &["update", "--activate"]);
    assert_eq!(activation.status.code(), Some(1));
    assert!(output(&activation).contains("No staged update"));
    assert_eq!(fs::canonicalize(root.join("current")).unwrap(), app);
    assert!(!home.path().join("command-was-executed").exists());
}

#[test]
fn malformed_config_and_desktop_conflicts_are_reported() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("config/vscu")).unwrap();
    fs::write(
        home.path().join("config/vscu/config.toml"),
        "max_cache_count = 0",
    )
    .unwrap();
    let clean = run(home.path(), &["clean"]);
    assert_eq!(clean.status.code(), Some(1));
    assert!(output(&clean).contains("max_cache_count"));
    let status = run(home.path(), &["status"]);
    assert!(status.status.success());
    fs::remove_file(home.path().join("config/vscu/config.toml")).unwrap();
    let launcher = home.path().join("data/applications/code.desktop");
    fs::create_dir_all(launcher.parent().unwrap()).unwrap();
    fs::write(&launcher, "existing launcher").unwrap();
    let install = run(home.path(), &["install", "--desktop"]);
    assert_eq!(install.status.code(), Some(1));
    assert!(output(&install).contains("Existing desktop launcher"));
    assert_eq!(fs::read_to_string(launcher).unwrap(), "existing launcher");
    assert!(!home.path().join("data/vscu/current").exists());
    assert!(
        !home
            .path()
            .join("data/applications/vscu-code.desktop")
            .exists()
    );
}
