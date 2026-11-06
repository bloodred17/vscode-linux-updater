use crate::{install, releases};
use flate2::{Compression, write::GzEncoder};
use std::{
    fs::{self, File},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};

pub const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub fn release(root: &Path, version: &str, commit: &str, owned: bool) -> PathBuf {
    let application = root.join("versions").join(format!("{version}-{commit}"));
    fs::create_dir_all(application.join("resources/app")).unwrap();
    fs::create_dir(application.join("bin")).unwrap();
    fs::write(
        application.join("resources/app/package.json"),
        serde_json::json!({"version":version}).to_string(),
    )
    .unwrap();
    fs::write(
        application.join("resources/app/product.json"),
        serde_json::json!({"commit":commit,"applicationName":"code"}).to_string(),
    )
    .unwrap();
    for path in [application.join("code"), application.join("bin/code")] {
        fs::write(&path, "#!/bin/sh\nexit 99\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    if owned {
        releases::write_manifest(&application, version, commit).unwrap();
    }
    application
}

pub fn select(root: &Path, name: &str, target: &Path) {
    symlink(target, root.join(name)).unwrap();
}

pub fn archive(path: &Path, version: &str, commit: &str) -> String {
    let home = tempfile::tempdir().unwrap();
    let app = release(home.path(), version, commit, false);
    let mut builder = tar::Builder::new(GzEncoder::new(
        File::create(path).unwrap(),
        Compression::fast(),
    ));
    builder.append_dir_all("VSCode-linux-x64", &app).unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    install::file_checksum(path).unwrap()
}

pub fn snapshot(root: &Path) -> Vec<(PathBuf, String)> {
    fn visit(root: &Path, dir: &Path, result: &mut Vec<(PathBuf, String)>) {
        if !dir.exists() {
            return;
        }
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let value = if metadata.is_symlink() {
                format!("link:{}", fs::read_link(&path).unwrap().display())
            } else if metadata.is_dir() {
                visit(root, &path, result);
                "dir".into()
            } else {
                install::file_checksum(&path).unwrap()
            };
            result.push((path.strip_prefix(root).unwrap().into(), value));
        }
    }
    let mut result = Vec::new();
    visit(root, root, &mut result);
    result.sort();
    result
}
