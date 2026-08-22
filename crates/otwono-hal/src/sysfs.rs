//! Small, forgiving readers for `/proc` and `/sys`.
//!
//! Everything here returns `Option` rather than `Result`: for hardware probing, "the file
//! is not there" is the normal case on most machines, not an error worth propagating.

use std::fs;
use std::path::Path;

/// Read a file and trim trailing whitespace and NUL bytes.
///
/// Device-tree property files are NUL-terminated, which trips up naive readers and is the
/// single most common source of "why does my model string have a garbage character".
pub fn read_trimmed(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let s = String::from_utf8_lossy(&bytes);
    Some(
        s.trim_matches(|c: char| c.is_whitespace() || c == '\0')
            .to_string(),
    )
}

/// Read a file as an unsigned integer, tolerating `0x` prefixes and trailing newlines.
pub fn read_u64(path: &Path) -> Option<u64> {
    let s = read_trimmed(path)?;
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

/// List directory entry names, sorted for deterministic output.
pub fn list_dir(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// Parse a `KEY=value` file (uevent, os-release) into pairs.
pub fn read_keyvals(path: &Path) -> Vec<(String, String)> {
    let Some(text) = read_trimmed(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Look up one key in a `KEY=value` file.
pub fn keyval(path: &Path, key: &str) -> Option<String> {
    read_keyvals(path)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// Does a path exist? Used for device-node presence checks.
pub fn exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("otwono-sysfs-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(content).unwrap();
        p
    }

    #[test]
    fn trims_nul_terminated_device_tree_strings() {
        let d = tmpdir("nul");
        let p = write(&d, "model", b"Raspberry Pi 5 Model B Rev 1.0\0");
        assert_eq!(
            read_trimmed(&p).as_deref(),
            Some("Raspberry Pi 5 Model B Rev 1.0")
        );
    }

    #[test]
    fn reads_decimal_and_hex() {
        let d = tmpdir("nums");
        assert_eq!(read_u64(&write(&d, "a", b"4096\n")), Some(4096));
        assert_eq!(read_u64(&write(&d, "b", b"0x10de\n")), Some(0x10de));
        assert_eq!(read_u64(&write(&d, "c", b"not-a-number")), None);
    }

    #[test]
    fn missing_files_are_none_not_errors() {
        assert_eq!(read_trimmed(Path::new("/nonexistent/xyz")), None);
        assert_eq!(read_u64(Path::new("/nonexistent/xyz")), None);
        assert!(list_dir(Path::new("/nonexistent/xyz")).is_empty());
    }

    #[test]
    fn parses_uevent_style_files() {
        let d = tmpdir("uevent");
        let p = write(&d, "uevent", b"DRIVER=amdgpu\nPCI_ID=1002:73FF\n");
        assert_eq!(keyval(&p, "DRIVER").as_deref(), Some("amdgpu"));
        assert_eq!(keyval(&p, "PCI_ID").as_deref(), Some("1002:73FF"));
        assert_eq!(keyval(&p, "NOPE"), None);
    }
}
