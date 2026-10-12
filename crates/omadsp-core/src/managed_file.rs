//! Writing files outside the repo safely (PROMPT §2).
//!
//! Every file omadsp writes carries a marker: [`HASH_HEADER`] as the first line of config files,
//! or the [`JSON_MARKER`] key as the first member of JSON files (JSON has no comments). Before
//! replacing a file, [`write`] refuses symlinks, non-regular files, and files without the marker,
//! so omadsp never overwrites something it didn't create. Writes are atomic: a temp file in the
//! same directory, fsync, rename.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt};
use std::path::Path;

pub use crate::graph::MANAGED_HEADER as HASH_HEADER;

/// The managed marker as a JSON member.
pub const JSON_MARKER: &str = "\"//\": \"Managed by omadsp — do not edit\"";
const JSON_MARKER_KEY: &str = "Managed by omadsp — do not edit";

/// Whether `text` carries omadsp's marker: the exact first line, or a `"//"` member with the
/// marker text near the top of a JSON object.
pub fn is_managed(text: &str) -> bool {
    if text.lines().next() == Some(HASH_HEADER) {
        return true;
    }
    let head: String = text.chars().take(256).collect();
    let t = head.trim_start();
    t.starts_with('{') && head.contains("\"//\"") && head.contains(JSON_MARKER_KEY)
}

fn refuse(path: &Path, why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("refusing to write {}: {why}", path.display()))
}

/// Checks that `path` is absent, or a regular file omadsp owns.
pub fn check_writable(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.file_type().is_symlink() => Err(refuse(path, "it is a symlink")),
        Ok(m) if !m.file_type().is_file() => Err(refuse(path, "not a regular file")),
        Ok(_) => {
            let text = fs::read_to_string(path).map_err(|_| refuse(path, "it isn't a text file omadsp wrote"))?;
            if is_managed(&text) { Ok(()) } else { Err(refuse(path, "it has no \"Managed by omadsp\" header")) }
        }
    }
}

/// Atomically writes `contents` (which must carry the marker) to `path`, with `mode` for a new
/// file. Creates missing parent directories with mode 0700.
pub fn write(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    if !is_managed(contents) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: contents lack the managed marker", path.display()),
        ));
    }
    check_writable(path)?;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if !dir.exists() {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let tmp = dir.join(format!(".{}.omadsp-tmp-{}", name.to_string_lossy(), std::process::id()));
    let _ = fs::remove_file(&tmp);
    let result = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        // Re-check right before the rename: the target may have changed meanwhile.
        check_writable(path)?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Puts the JSON marker first in a pretty-printed JSON object.
pub fn json_with_marker(value: &serde_json::Value) -> String {
    let body = serde_json::to_string_pretty(value).expect("JSON values serialize");
    match body.strip_prefix('{') {
        Some(rest) if rest.trim() == "}" => format!("{{\n  {JSON_MARKER}\n}}\n"),
        Some(rest) => format!("{{\n  {JSON_MARKER},{rest}\n"),
        None => body,
    }
}

/// Whether `path` is a Unix socket (for clearing a stale `omadsp.sock`).
pub fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path).map(|m| m.file_type().is_socket()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        // Inside the workspace's target dir (git-ignored), not /tmp.
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("mf-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn markers() {
        assert!(is_managed(&format!("{HASH_HEADER}\nx = 1\n")));
        assert!(!is_managed(&format!("# hello\n{HASH_HEADER}\n")));
        assert!(is_managed(&json_with_marker(&serde_json::json!({"a": 1}))));
        assert!(is_managed(&json_with_marker(&serde_json::json!({}))));
        assert!(!is_managed("{ \"a\": 1 }"));
    }

    #[test]
    fn json_marker_comes_first_and_parses() {
        let t = json_with_marker(&serde_json::json!({"schema": 1, "name": "X"}));
        assert!(t.starts_with("{\n  \"//\": \"Managed by omadsp"));
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v["name"], "X");
    }

    #[test]
    fn writes_new_and_replaces_own_files() {
        let d = tmpdir("own");
        let p = d.join("sub/host.conf");
        write(&p, &format!("{HASH_HEADER}\na\n"), 0o600).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(d.join("sub")).unwrap().permissions().mode() & 0o777, 0o700);
        write(&p, &format!("{HASH_HEADER}\nb\n"), 0o600).unwrap();
        assert!(fs::read_to_string(&p).unwrap().ends_with("b\n"));
        assert_eq!(fs::read_dir(d.join("sub")).unwrap().count(), 1, "no temp file left behind");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn refuses_foreign_files_and_symlinks() {
        let d = tmpdir("foreign");
        let user = d.join("user.conf");
        fs::write(&user, "# the user's own file\n").unwrap();
        let e = write(&user, &format!("{HASH_HEADER}\n"), 0o644).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&user).unwrap(), "# the user's own file\n");

        let target = d.join("target.conf");
        fs::write(&target, format!("{HASH_HEADER}\n")).unwrap();
        let link = d.join("link.conf");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write(&link, &format!("{HASH_HEADER}\nnew\n"), 0o644).is_err(), "even a link to our own file");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());

        assert!(write(&d.join("x.conf"), "no marker\n", 0o644).is_err());
        fs::create_dir(d.join("dir.conf")).unwrap();
        assert!(write(&d.join("dir.conf"), &format!("{HASH_HEADER}\n"), 0o644).is_err());
        fs::remove_dir_all(&d).unwrap();
    }
}
