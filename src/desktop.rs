use crate::install::{self, Result};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub fn path(root: &Path) -> Result<PathBuf> {
    Ok(root
        .parent()
        .ok_or("Invalid data root")?
        .join("applications/vscu-code.desktop"))
}

fn value(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or("Desktop launcher paths must be UTF-8")?;
    if value.chars().any(char::is_control) {
        return Err("Desktop launcher paths cannot contain control characters".into());
    }
    Ok(value.replace('\\', "\\\\"))
}

fn exec(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .ok_or("Desktop executable path must be UTF-8")?;
    if text.contains('=') || text.chars().any(char::is_control) {
        return Err("Desktop executable path cannot contain '=' or control characters".into());
    }
    let mut quoted = String::from("\"");
    for character in text.chars() {
        match character {
            '\\' => quoted.push_str("\\\\\\\\"),
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

pub fn content(root: &Path) -> Result<String> {
    let executable = exec(&root.join("current/code"))?;
    let icon = value(&root.join("current/resources/app/resources/linux/code.png"))?;
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=Visual Studio Code (vscu)\nComment=Code Editing. Redefined.\nExec={executable} %F\nIcon={icon}\nTerminal=false\nCategories=Development;IDE;TextEditor;\nStartupNotify=true\nStartupWMClass=Code\nX-Vscu-Managed=true\n"
    ))
}

pub fn preflight(root: &Path) -> Result<()> {
    content(root)?;
    let launcher = path(root)?;
    let directory = launcher.parent().ok_or("Invalid desktop path")?;
    for name in [
        "vscu-code.desktop",
        "code.desktop",
        "visual-studio-code.desktop",
    ] {
        install::require_absent(
            &directory.join(name),
            "Existing desktop launcher; explicit integration choice is required",
        )?;
    }
    Ok(())
}

pub fn create(root: &Path) -> Result<()> {
    preflight(root)?;
    let launcher = path(root)?;
    let directory = launcher.parent().unwrap();
    install::ensure_directory(directory)?;
    let mut pending = tempfile::NamedTempFile::new_in(directory)?;
    pending.write_all(content(root)?.as_bytes())?;
    pending
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    pending.as_file().sync_all()?;
    pending.persist_noclobber(&launcher).map_err(|e| e.error)?;
    println!("Desktop launcher: {}", launcher.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn optional_launcher_follows_current_and_preserves_conflicting_entries() {
        for kind in ["none", "file", "broken", "official"] {
            let home = tempfile::tempdir().unwrap();
            let root = home.path().join("data/vscu");
            let path = path(&root).unwrap();
            if kind != "none" {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                match kind {
                    "file" => fs::write(&path, "keep").unwrap(),
                    "broken" => symlink("missing", &path).unwrap(),
                    "official" => {
                        fs::write(path.parent().unwrap().join("code.desktop"), "keep").unwrap()
                    }
                    _ => (),
                }
                let before = crate::test_support::snapshot(home.path());
                assert!(create(&root).is_err());
                assert_eq!(before, crate::test_support::snapshot(home.path()));
            } else {
                preflight(&root).unwrap();
                assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
                create(&root).unwrap();
                let actual = fs::read_to_string(&path).unwrap();
                assert!(actual.contains("/current/code\" %F"));
                assert!(actual.contains("/current/resources/app/resources/linux/code.png"));
                assert!(actual.contains("X-Vscu-Managed=true"));
            }
        }
    }

    #[test]
    fn desktop_paths_are_quoted_and_escaped() {
        let home = tempfile::tempdir().unwrap();
        let root = home
            .path()
            .join("data with spaces $money `tick` \\quote\" 100%/vscu");
        create(&root).unwrap();
        let launcher = path(&root).unwrap();
        let actual = fs::read_to_string(launcher).unwrap();
        assert!(actual.contains("100%%"));
        assert!(content(Path::new("/tmp/line\nbreak/vscu")).is_err());
        assert!(content(Path::new("/tmp/equal=sign/vscu")).is_err());
    }
}
