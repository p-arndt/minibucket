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

    pub fn first_access(&self) -> Option<&str> {
        self.map.keys().next().map(|s| s.as_str())
    }

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
