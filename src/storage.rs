// Filesystem-backed bucket/object storage.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::md5::Md5;
use crate::sha256::hex;

#[derive(Clone)]
pub struct Storage {
    pub root: PathBuf,
}

pub struct ObjectMeta {
    pub content_type: String,
    pub size: u64,
    pub etag: String,
    pub last_modified: u64,
}

pub struct BucketInfo {
    pub name: String,
    pub creation_date: u64,
}

#[derive(Debug)]
pub enum StorageError {
    Io(io::Error),
    InvalidName,
    NotFound,
    Exists,
    NotEmpty,
}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self { StorageError::Io(e) }
}

pub fn valid_bucket(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 3 || bytes.len() > 63 { return false; }
    let mut prev_dot = false;
    for (i, &b) in bytes.iter().enumerate() {
        let ok = matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.');
        if !ok { return false; }
        if (i == 0 || i == bytes.len() - 1) && (b == b'-' || b == b'.') {
            return false;
        }
        if b == b'.' && prev_dot { return false; }
        prev_dot = b == b'.';
    }
    true
}

pub fn valid_key(key: &str) -> bool {
    if key.is_empty() || key.len() > 1024 { return false; }
    if key.contains('\0') || key.contains('\\') { return false; }
    for seg in key.split('/') {
        if seg == ".." || seg == "." { return false; }
    }
    true
}

impl Storage {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(root.join("buckets"))?;
        Ok(Self { root })
    }

    fn bucket_dir(&self, bucket: &str) -> PathBuf {
        self.root.join("buckets").join(bucket)
    }
    fn marker(&self, bucket: &str) -> PathBuf {
        self.bucket_dir(bucket).join(".bucket")
    }
    fn data_path(&self, bucket: &str, key: &str) -> PathBuf {
        self.bucket_dir(bucket).join("data").join(key)
    }
    fn meta_path(&self, bucket: &str, key: &str) -> PathBuf {
        self.bucket_dir(bucket).join("meta").join(format!("{}.meta", key))
    }

    pub fn list_buckets(&self) -> io::Result<Vec<BucketInfo>> {
        let mut out = Vec::new();
        let dir = self.root.join("buckets");
        if !dir.exists() { return Ok(out); }
        for ent in fs::read_dir(dir)? {
            let ent = ent?;
            if !ent.file_type()?.is_dir() { continue; }
            let name = match ent.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let marker = ent.path().join(".bucket");
            if !marker.exists() { continue; }
            let created = read_marker_time(&marker).unwrap_or(0);
            out.push(BucketInfo { name, creation_date: created });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        if !valid_bucket(bucket) { return Err(StorageError::InvalidName); }
        let dir = self.bucket_dir(bucket);
        if self.marker(bucket).exists() { return Err(StorageError::Exists); }
        fs::create_dir_all(dir.join("data"))?;
        fs::create_dir_all(dir.join("meta"))?;
        let mut f = File::create(self.marker(bucket))?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        writeln!(f, "{}", now)?;
        Ok(())
    }

    pub fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        if !valid_bucket(bucket) { return Err(StorageError::InvalidName); }
        if !self.marker(bucket).exists() { return Err(StorageError::NotFound); }
        // Must be empty.
        let data_dir = self.bucket_dir(bucket).join("data");
        if has_any_file(&data_dir)? {
            return Err(StorageError::NotEmpty);
        }
        fs::remove_dir_all(self.bucket_dir(bucket))?;
        Ok(())
    }

    pub fn bucket_exists(&self, bucket: &str) -> bool {
        self.marker(bucket).exists()
    }

    pub fn put_object_writer(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<ObjectWriter, StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        if !valid_key(key) { return Err(StorageError::InvalidName); }
        let path = self.data_path(bucket, key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp-upload");
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        Ok(ObjectWriter {
            file,
            md5: Md5::new(),
            size: 0,
            final_path: path,
            tmp_path: tmp,
            meta_path: self.meta_path(bucket, key),
        })
    }

    pub fn get_object(&self, bucket: &str, key: &str) -> Result<(ObjectMeta, File), StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        let p = self.data_path(bucket, key);
        if !p.exists() { return Err(StorageError::NotFound); }
        let meta = read_meta(&self.meta_path(bucket, key)).unwrap_or_else(|_| ObjectMeta {
            content_type: "application/octet-stream".into(),
            size: 0,
            etag: String::new(),
            last_modified: 0,
        });
        let f = File::open(&p)?;
        Ok((meta, f))
    }

    pub fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        let p = self.data_path(bucket, key);
        if !p.exists() { return Err(StorageError::NotFound); }
        let meta = read_meta(&self.meta_path(bucket, key))?;
        Ok(meta)
    }

    pub fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        let p = self.data_path(bucket, key);
        if p.exists() {
            fs::remove_file(&p)?;
            let _ = fs::remove_file(self.meta_path(bucket, key));
            // Try to prune empty parent directories within data/.
            let data_root = self.bucket_dir(bucket).join("data");
            let mut cur = p.parent().map(|p| p.to_path_buf());
            while let Some(d) = cur {
                if d == data_root || !d.starts_with(&data_root) { break; }
                if fs::read_dir(&d).map(|mut it| it.next().is_none()).unwrap_or(false) {
                    let _ = fs::remove_dir(&d);
                    cur = d.parent().map(|p| p.to_path_buf());
                } else {
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: usize,
        marker: Option<&str>,
    ) -> Result<ListResult, StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        let data_root = self.bucket_dir(bucket).join("data");
        let mut all_keys: Vec<(String, fs::Metadata)> = Vec::new();
        walk(&data_root, &data_root, &mut all_keys)?;
        all_keys.sort_by(|a, b| a.0.cmp(&b.0));

        let mut contents = Vec::new();
        let mut common_prefixes: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut next_marker: Option<String> = None;

        for (key, md) in &all_keys {
            if !key.starts_with(prefix) { continue; }
            if let Some(m) = marker {
                if key.as_str() <= m { continue; }
            }
            if let Some(delim) = delimiter {
                let rest = &key[prefix.len()..];
                if let Some(i) = rest.find(delim) {
                    let cp = format!("{}{}{}", prefix, &rest[..i], delim);
                    if !common_prefixes.contains(&cp) {
                        if contents.len() + common_prefixes.len() >= max_keys {
                            truncated = true;
                            next_marker = Some(key.clone());
                            break;
                        }
                        common_prefixes.push(cp);
                    }
                    continue;
                }
            }
            if contents.len() + common_prefixes.len() >= max_keys {
                truncated = true;
                next_marker = Some(key.clone());
                break;
            }
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let meta = read_meta(&self.meta_path(bucket, key)).ok();
            contents.push(ObjectListEntry {
                key: key.clone(),
                size: md.len(),
                last_modified: mtime,
                etag: meta.map(|m| m.etag).unwrap_or_default(),
            });
        }

        Ok(ListResult { contents, common_prefixes, truncated, next_marker })
    }
}

pub struct ListResult {
    pub contents: Vec<ObjectListEntry>,
    pub common_prefixes: Vec<String>,
    pub truncated: bool,
    pub next_marker: Option<String>,
}

pub struct ObjectListEntry {
    pub key: String,
    pub size: u64,
    pub last_modified: u64,
    pub etag: String,
}

pub struct ObjectWriter {
    file: File,
    md5: Md5,
    size: u64,
    final_path: PathBuf,
    tmp_path: PathBuf,
    meta_path: PathBuf,
}

impl ObjectWriter {
    pub fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file.write_all(buf)?;
        self.md5.update(buf);
        self.size += buf.len() as u64;
        Ok(())
    }
    pub fn finish(mut self, content_type: &str) -> io::Result<(String, u64)> {
        self.file.flush()?;
        drop(self.file);
        if let Some(parent) = self.meta_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&self.tmp_path, &self.final_path)?;
        let digest = self.md5.finalize();
        let etag = hex(&digest);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let mut mf = File::create(&self.meta_path)?;
        writeln!(mf, "content-type: {}", content_type)?;
        writeln!(mf, "size: {}", self.size)?;
        writeln!(mf, "etag: {}", etag)?;
        writeln!(mf, "last-modified: {}", now)?;
        Ok((etag, self.size))
    }
    pub fn abort(self) {
        let _ = fs::remove_file(&self.tmp_path);
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, fs::Metadata)>) -> io::Result<()> {
    if !dir.exists() { return Ok(()); }
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let ft = ent.file_type()?;
        let p = ent.path();
        if ft.is_dir() {
            walk(root, &p, out)?;
        } else if ft.is_file() {
            let rel = p.strip_prefix(root).unwrap_or(&p);
            let mut key = String::new();
            for (i, comp) in rel.components().enumerate() {
                if i > 0 { key.push('/'); }
                if let std::path::Component::Normal(s) = comp {
                    key.push_str(&s.to_string_lossy());
                }
            }
            // Skip in-progress uploads.
            if key.ends_with(".tmp-upload") { continue; }
            let md = ent.metadata()?;
            out.push((key, md));
        }
    }
    Ok(())
}

fn has_any_file(dir: &Path) -> io::Result<bool> {
    if !dir.exists() { return Ok(false); }
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let ft = ent.file_type()?;
        if ft.is_file() { return Ok(true); }
        if ft.is_dir() {
            if has_any_file(&ent.path())? { return Ok(true); }
        }
    }
    Ok(false)
}

fn read_marker_time(p: &Path) -> io::Result<u64> {
    let mut f = File::open(p)?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(s.trim().parse().unwrap_or(0))
}

fn read_meta(p: &Path) -> Result<ObjectMeta, StorageError> {
    if !p.exists() { return Err(StorageError::NotFound); }
    let mut f = File::open(p)?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    let mut content_type = String::from("application/octet-stream");
    let mut size = 0u64;
    let mut etag = String::new();
    let mut last_modified = 0u64;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("content-type: ") { content_type = v.to_string(); }
        else if let Some(v) = line.strip_prefix("size: ") { size = v.parse().unwrap_or(0); }
        else if let Some(v) = line.strip_prefix("etag: ") { etag = v.to_string(); }
        else if let Some(v) = line.strip_prefix("last-modified: ") { last_modified = v.parse().unwrap_or(0); }
    }
    Ok(ObjectMeta { content_type, size, etag, last_modified })
}
