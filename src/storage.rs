// Filesystem-backed bucket/object storage with optional versioning.
//
// Layout (per bucket):
//   .bucket                              - creation time
//   .versioning                          - "Enabled" | "Suspended" (absent = disabled)
//   data/<key>                           - latest version data (always present for live keys)
//   meta/<key>.meta                      - latest version metadata
//   versions/<key>/<vid>.data            - per-version data (versioning enabled only)
//   versions/<key>/<vid>.meta            - per-version metadata
//   versions/<key>/<vid>.delete-marker   - delete-marker sentinel (zero-byte)

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
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
    pub version_id: Option<String>,
}

pub struct BucketInfo {
    pub name: String,
    pub creation_date: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersioningStatus {
    Disabled,
    Enabled,
    Suspended,
}

impl VersioningStatus {
    pub fn records_versions(self) -> bool {
        matches!(self, Self::Enabled | Self::Suspended)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "",
            Self::Enabled => "Enabled",
            Self::Suspended => "Suspended",
        }
    }
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

// Lexicographically sortable version id: "<unix-seconds-zero-padded>-<counter>-<random>".
pub fn new_version_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(1);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let secs = now.as_secs();
    let nanos = now.subsec_nanos();
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Sortable + unique even within the same second.
    format!("{:013}-{:09}-{:08x}", secs, nanos, c)
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
    fn versions_dir(&self, bucket: &str, key: &str) -> PathBuf {
        self.bucket_dir(bucket).join("versions").join(key)
    }
    fn versioning_file(&self, bucket: &str) -> PathBuf {
        self.bucket_dir(bucket).join(".versioning")
    }

    pub fn versioning_status(&self, bucket: &str) -> VersioningStatus {
        let p = self.versioning_file(bucket);
        if let Ok(s) = fs::read_to_string(&p) {
            match s.trim() {
                "Enabled" => return VersioningStatus::Enabled,
                "Suspended" => return VersioningStatus::Suspended,
                _ => {}
            }
        }
        VersioningStatus::Disabled
    }

    pub fn set_versioning_status(&self, bucket: &str, status: VersioningStatus) -> Result<(), StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        let p = self.versioning_file(bucket);
        match status {
            VersioningStatus::Disabled => {
                let _ = fs::remove_file(&p);
            }
            _ => {
                fs::write(&p, status.as_str())?;
            }
        }
        Ok(())
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
        let data_dir = self.bucket_dir(bucket).join("data");
        let versions_dir = self.bucket_dir(bucket).join("versions");
        if has_any_file(&data_dir)? || has_any_file(&versions_dir)? {
            return Err(StorageError::NotEmpty);
        }
        fs::remove_dir_all(self.bucket_dir(bucket))?;
        Ok(())
    }

    pub fn bucket_exists(&self, bucket: &str) -> bool {
        self.marker(bucket).exists()
    }

    pub fn put_object_writer(&self, bucket: &str, key: &str) -> Result<ObjectWriter, StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        if !valid_key(key) { return Err(StorageError::InvalidName); }
        let path = self.data_path(bucket, key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp-upload");
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        let versioning = self.versioning_status(bucket);
        Ok(ObjectWriter {
            file,
            md5: Md5::new(),
            size: 0,
            final_path: path,
            tmp_path: tmp,
            meta_path: self.meta_path(bucket, key),
            versioning,
            versions_dir: self.versions_dir(bucket, key),
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
            version_id: None,
        });
        let f = File::open(&p)?;
        Ok((meta, f))
    }

    pub fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(ObjectMeta, File), StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        let vdir = self.versions_dir(bucket, key);
        let data = vdir.join(format!("{}.data", version_id));
        if !data.exists() {
            // Could be a delete marker — return NotFound; caller maps to MethodNotAllowed.
            return Err(StorageError::NotFound);
        }
        let meta_p = vdir.join(format!("{}.meta", version_id));
        let mut meta = read_meta(&meta_p).unwrap_or(ObjectMeta {
            content_type: "application/octet-stream".into(),
            size: 0,
            etag: String::new(),
            last_modified: 0,
            version_id: None,
        });
        meta.version_id = Some(version_id.to_string());
        let f = File::open(&data)?;
        Ok((meta, f))
    }

    pub fn is_delete_marker(&self, bucket: &str, key: &str, version_id: &str) -> bool {
        self.versions_dir(bucket, key)
            .join(format!("{}.delete-marker", version_id))
            .exists()
    }

    // Returns Some(version_id) of the delete marker if versioning is on; None otherwise.
    pub fn delete_object(&self, bucket: &str, key: &str) -> Result<Option<String>, StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        let versioning = self.versioning_status(bucket);
        let data_p = self.data_path(bucket, key);
        let meta_p = self.meta_path(bucket, key);

        if versioning.records_versions() {
            let vid = new_version_id();
            let vdir = self.versions_dir(bucket, key);
            fs::create_dir_all(&vdir)?;
            // Create the delete-marker sentinel.
            File::create(vdir.join(format!("{}.delete-marker", vid)))?;
            // Write meta for the marker (records the time + that it's a delete marker).
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
            let mut mf = File::create(vdir.join(format!("{}.meta", vid)))?;
            writeln!(mf, "delete-marker: true")?;
            writeln!(mf, "last-modified: {}", now)?;
            // Remove the live mirror; previous versions still live under versions/.
            if data_p.exists() { let _ = fs::remove_file(&data_p); }
            let _ = fs::remove_file(&meta_p);
            self.prune_empty_data_dirs(bucket, &data_p);
            return Ok(Some(vid));
        }

        if data_p.exists() {
            fs::remove_file(&data_p)?;
            let _ = fs::remove_file(&meta_p);
            self.prune_empty_data_dirs(bucket, &data_p);
        }
        Ok(None)
    }

    pub fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<bool, StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        let vdir = self.versions_dir(bucket, key);
        let data = vdir.join(format!("{}.data", version_id));
        let meta = vdir.join(format!("{}.meta", version_id));
        let marker = vdir.join(format!("{}.delete-marker", version_id));
        let was_marker = marker.exists();
        let _ = fs::remove_file(&data);
        let _ = fs::remove_file(&meta);
        let _ = fs::remove_file(&marker);

        // If this version was the live one, promote the new latest (or remove the mirror).
        self.repromote_latest(bucket, key)?;
        // Tidy up empty version dir.
        if vdir.exists() {
            if let Ok(mut it) = fs::read_dir(&vdir) {
                if it.next().is_none() {
                    let _ = fs::remove_dir(&vdir);
                    // Also prune empty parents under versions/.
                    let versions_root = self.bucket_dir(bucket).join("versions");
                    let mut cur = vdir.parent().map(|p| p.to_path_buf());
                    while let Some(d) = cur {
                        if d == versions_root || !d.starts_with(&versions_root) { break; }
                        if fs::read_dir(&d).map(|mut it| it.next().is_none()).unwrap_or(false) {
                            let _ = fs::remove_dir(&d);
                            cur = d.parent().map(|p| p.to_path_buf());
                        } else { break; }
                    }
                }
            }
        }
        Ok(was_marker)
    }

    // Re-establishes the data/<key> + meta/<key>.meta mirror to reflect the
    // newest non-delete-marker version, or clears the mirror if the latest
    // version is a delete marker / no versions remain.
    fn repromote_latest(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        let vdir = self.versions_dir(bucket, key);
        if !vdir.exists() {
            // No versions tracked — leave the mirror alone (non-versioned bucket case).
            return Ok(());
        }
        // Collect all version ids by scanning *.data and *.delete-marker.
        let mut entries: Vec<(String, bool)> = Vec::new(); // (vid, is_marker)
        for e in fs::read_dir(&vdir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(vid) = name.strip_suffix(".data") {
                entries.push((vid.to_string(), false));
            } else if let Some(vid) = name.strip_suffix(".delete-marker") {
                entries.push((vid.to_string(), true));
            }
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0)); // descending
        let data_p = self.data_path(bucket, key);
        let meta_p = self.meta_path(bucket, key);
        if entries.is_empty() || entries[0].1 {
            // Latest is a delete marker (or nothing). Drop the mirror.
            let _ = fs::remove_file(&data_p);
            let _ = fs::remove_file(&meta_p);
            self.prune_empty_data_dirs(bucket, &data_p);
            return Ok(());
        }
        let latest_vid = &entries[0].0;
        let src_data = vdir.join(format!("{}.data", latest_vid));
        let src_meta = vdir.join(format!("{}.meta", latest_vid));
        if let Some(parent) = data_p.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = meta_p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src_data, &data_p)?;
        if src_meta.exists() {
            fs::copy(&src_meta, &meta_p)?;
        }
        Ok(())
    }

    fn prune_empty_data_dirs(&self, bucket: &str, last_path: &Path) {
        let data_root = self.bucket_dir(bucket).join("data");
        let mut cur = last_path.parent().map(|p| p.to_path_buf());
        while let Some(d) = cur {
            if d == data_root || !d.starts_with(&data_root) { break; }
            if fs::read_dir(&d).map(|mut it| it.next().is_none()).unwrap_or(false) {
                let _ = fs::remove_dir(&d);
                cur = d.parent().map(|p| p.to_path_buf());
            } else { break; }
        }
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

    // Walk all versions across all keys in the bucket.
    pub fn list_versions(&self, bucket: &str) -> Result<Vec<VersionEntry>, StorageError> {
        if !self.bucket_exists(bucket) { return Err(StorageError::NotFound); }
        let vroot = self.bucket_dir(bucket).join("versions");
        let mut out = Vec::new();
        if !vroot.exists() { return Ok(out); }
        walk_versions(&vroot, &vroot, &mut out)?;
        // Group by key; within each key, mark the highest-vid non-marker entry as latest.
        out.sort_by(|a, b| match a.key.cmp(&b.key) {
            std::cmp::Ordering::Equal => b.version_id.cmp(&a.version_id),
            o => o,
        });
        let mut last_key = String::new();
        let mut latest_set = false;
        for v in out.iter_mut() {
            if v.key != last_key {
                last_key = v.key.clone();
                latest_set = false;
            }
            if !latest_set {
                v.is_latest = true;
                latest_set = true;
            }
        }
        Ok(out)
    }
}

pub struct VersionEntry {
    pub key: String,
    pub version_id: String,
    pub is_delete_marker: bool,
    pub is_latest: bool,
    pub size: u64,
    pub last_modified: u64,
    pub etag: String,
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
    versioning: VersioningStatus,
    versions_dir: PathBuf,
}

impl ObjectWriter {
    pub fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file.write_all(buf)?;
        self.md5.update(buf);
        self.size += buf.len() as u64;
        Ok(())
    }
    // Returns (etag, size, version_id-if-versioning-enabled).
    pub fn finish(mut self, content_type: &str) -> io::Result<(String, u64, Option<String>)> {
        self.file.flush()?;
        drop(self.file);
        if let Some(parent) = self.meta_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&self.tmp_path, &self.final_path)?;
        let digest = self.md5.finalize();
        let etag = hex(&digest);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        let version_id = if self.versioning.records_versions() {
            let vid = new_version_id();
            fs::create_dir_all(&self.versions_dir)?;
            // Mirror data + meta into the versions store.
            fs::copy(&self.final_path, self.versions_dir.join(format!("{}.data", vid)))?;
            let mut vmf = File::create(self.versions_dir.join(format!("{}.meta", vid)))?;
            writeln!(vmf, "content-type: {}", content_type)?;
            writeln!(vmf, "size: {}", self.size)?;
            writeln!(vmf, "etag: {}", etag)?;
            writeln!(vmf, "last-modified: {}", now)?;
            writeln!(vmf, "version-id: {}", vid)?;
            Some(vid)
        } else {
            None
        };

        let mut mf = File::create(&self.meta_path)?;
        writeln!(mf, "content-type: {}", content_type)?;
        writeln!(mf, "size: {}", self.size)?;
        writeln!(mf, "etag: {}", etag)?;
        writeln!(mf, "last-modified: {}", now)?;
        if let Some(v) = &version_id {
            writeln!(mf, "version-id: {}", v)?;
        }
        Ok((etag, self.size, version_id))
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
            if key.ends_with(".tmp-upload") { continue; }
            let md = ent.metadata()?;
            out.push((key, md));
        }
    }
    Ok(())
}

// Walk `versions/<...key.../>` and yield one VersionEntry per file we recognise.
// Inside a key directory we see `<vid>.data`, `<vid>.meta`, `<vid>.delete-marker`.
fn walk_versions(root: &Path, dir: &Path, out: &mut Vec<VersionEntry>) -> io::Result<()> {
    if !dir.exists() { return Ok(()); }
    // We need to know which directories represent a key (have any .data/.delete-marker
    // file directly inside). We descend until we find such files.
    let mut has_version_files = false;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut by_vid: std::collections::HashMap<String, (Option<PathBuf>, bool, Option<PathBuf>)> = std::collections::HashMap::new();
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let ft = ent.file_type()?;
        let p = ent.path();
        if ft.is_dir() {
            subdirs.push(p);
        } else {
            let name = ent.file_name().to_string_lossy().to_string();
            if let Some(vid) = name.strip_suffix(".data") {
                has_version_files = true;
                let e = by_vid.entry(vid.to_string()).or_insert((None, false, None));
                e.0 = Some(p.clone());
            } else if let Some(vid) = name.strip_suffix(".delete-marker") {
                has_version_files = true;
                let e = by_vid.entry(vid.to_string()).or_insert((None, false, None));
                e.1 = true;
            } else if let Some(vid) = name.strip_suffix(".meta") {
                let e = by_vid.entry(vid.to_string()).or_insert((None, false, None));
                e.2 = Some(p.clone());
            }
        }
    }

    if has_version_files {
        let rel = dir.strip_prefix(root).unwrap_or(dir);
        let mut key = String::new();
        for (i, comp) in rel.components().enumerate() {
            if i > 0 { key.push('/'); }
            if let std::path::Component::Normal(s) = comp {
                key.push_str(&s.to_string_lossy());
            }
        }
        for (vid, (data, is_marker, meta)) in by_vid {
            let mut size = 0u64;
            let mut etag = String::new();
            let mut last_modified = 0u64;
            if let Some(mp) = meta {
                if let Ok(m) = read_meta(&mp) {
                    size = m.size;
                    etag = m.etag;
                    last_modified = m.last_modified;
                }
            }
            if let Some(dp) = &data {
                if let Ok(md) = fs::metadata(dp) {
                    if size == 0 { size = md.len(); }
                    if last_modified == 0 {
                        last_modified = md.modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                    }
                }
            }
            out.push(VersionEntry {
                key: key.clone(),
                version_id: vid,
                is_delete_marker: is_marker,
                is_latest: false,
                size,
                last_modified,
                etag,
            });
        }
    }

    for sd in subdirs {
        walk_versions(root, &sd, out)?;
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
    let mut version_id: Option<String> = None;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("content-type: ") { content_type = v.to_string(); }
        else if let Some(v) = line.strip_prefix("size: ") { size = v.parse().unwrap_or(0); }
        else if let Some(v) = line.strip_prefix("etag: ") { etag = v.to_string(); }
        else if let Some(v) = line.strip_prefix("last-modified: ") { last_modified = v.parse().unwrap_or(0); }
        else if let Some(v) = line.strip_prefix("version-id: ") { version_id = Some(v.to_string()); }
    }
    Ok(ObjectMeta { content_type, size, etag, last_modified, version_id })
}
