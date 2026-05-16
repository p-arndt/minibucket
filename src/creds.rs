// Credential store: access-key -> secret-key.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Default)]
pub struct Credentials {
    pub map: HashMap<String, String>,
}

impl Credentials {
    pub fn new() -> Self { Self::default() }

    pub fn add(&mut self, access: &str, secret: &str) {
        self.map.insert(access.to_string(), secret.to_string());
    }

    pub fn secret_for(&self, access: &str) -> Option<&str> {
        self.map.get(access).map(|s| s.as_str())
    }

    pub fn is_empty(&self) -> bool { self.map.is_empty() }

    // File format: lines of `ACCESS_KEY=SECRET_KEY`. `#` starts a comment.
    pub fn load_file(path: &Path) -> io::Result<Self> {
        let mut c = Self::new();
        let text = fs::read_to_string(path)?;
        for (i, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() { continue; }
            let eq = line.find('=').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}:{}: expected KEY=SECRET", path.display(), i + 1),
                )
            })?;
            let access = line[..eq].trim();
            let secret = line[eq + 1..].trim();
            if access.is_empty() || secret.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "blank key or secret"));
            }
            c.add(access, secret);
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("minibucket_creds_{}_{}", nanos, name));
        p
    }

    #[test]
    fn add_and_lookup() {
        let mut c = Credentials::new();
        assert!(c.is_empty());
        c.add("AKIA", "secret");
        assert!(!c.is_empty());
        assert_eq!(c.secret_for("AKIA"), Some("secret"));
        assert_eq!(c.secret_for("missing"), None);
    }

    #[test]
    fn load_file_parses_lines() {
        let p = tmp_path("ok.creds");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, "# a comment").unwrap();
        writeln!(f, "AKIA=secret1").unwrap();
        writeln!(f, "  KEY2 = secret2   # trailing comment").unwrap();
        writeln!(f, "").unwrap();
        drop(f);
        let c = Credentials::load_file(&p).unwrap();
        assert_eq!(c.secret_for("AKIA"), Some("secret1"));
        assert_eq!(c.secret_for("KEY2"), Some("secret2"));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn load_file_rejects_malformed() {
        let p = tmp_path("bad.creds");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, "no_equals_here").unwrap();
        drop(f);
        let err = match Credentials::load_file(&p) {
            Err(e) => e,
            Ok(_) => panic!("expected InvalidData"),
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn load_file_rejects_blank_secret() {
        let p = tmp_path("blank.creds");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, "AKIA=").unwrap();
        drop(f);
        assert!(Credentials::load_file(&p).is_err());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn load_file_missing_returns_io_error() {
        let p = tmp_path("does_not_exist.creds");
        assert!(Credentials::load_file(&p).is_err());
    }
}
