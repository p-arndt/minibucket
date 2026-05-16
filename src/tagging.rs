// Object tagging: PutObjectTagging, GetObjectTagging, DeleteObjectTagging.
// Tags are stored as `key=value` pairs (one per line) in a sidecar file.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::http::{Request, Response};
use crate::s3::{error_response, read_body_all, Server};
use crate::util::xml_escape;

fn tag_path(srv: &Server, bucket: &str, key: &str) -> PathBuf {
    srv.storage
        .root
        .join("buckets")
        .join(bucket)
        .join("tags")
        .join(format!("{}.tags", key))
}

pub fn build_get_object_tagging(
    srv: &Server,
    bucket: &str,
    key: &str,
    rid: &str,
) -> crate::http::BuiltResponse {
    if !srv.storage.bucket_exists(bucket) {
        return crate::s3::build_error(404, "NoSuchBucket", "no such bucket", rid, bucket);
    }
    let p = tag_path(srv, bucket, key);
    let mut tags: Vec<(String, String)> = Vec::new();
    if p.exists() {
        if let Ok(s) = fs::read_to_string(&p) {
            for line in s.lines() {
                if let Some(eq) = line.find('=') {
                    tags.push((line[..eq].to_string(), line[eq + 1..].to_string()));
                }
            }
        }
    }
    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="UTF-8"?><Tagging xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><TagSet>"#);
    for (k, v) in &tags {
        body.push_str(&format!(
            "<Tag><Key>{}</Key><Value>{}</Value></Tag>",
            xml_escape(k),
            xml_escape(v)
        ));
    }
    body.push_str("</TagSet></Tagging>");
    crate::http::BuiltResponse::new(200)
        .header("x-amz-request-id", rid)
        .xml(body)
}

pub fn get_object_tagging(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    rid: &str,
) -> std::io::Result<()> {
    build_get_object_tagging(srv, bucket, key, rid).write_to(sock)
}

pub fn put_object_tagging<R: std::io::BufRead>(
    srv: &Server,
    req: &mut Request<R>,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    rid: &str,
) -> std::io::Result<()> {
    if !srv.storage.bucket_exists(bucket) {
        return error_response(sock, 404, "NoSuchBucket", "no such bucket", rid, bucket);
    }
    let body = read_body_all(req)?;
    let xml = String::from_utf8_lossy(&body);

    let mut tags: Vec<(String, String)> = Vec::new();
    let mut idx = 0;
    while let Some(s) = xml[idx..].find("<Tag>") {
        let tag_start = idx + s + 5;
        let tag_end = match xml[tag_start..].find("</Tag>") {
            Some(e) => tag_start + e,
            None => break,
        };
        let body = &xml[tag_start..tag_end];
        let k = extract_inner(body, "Key").unwrap_or_default();
        let v = extract_inner(body, "Value").unwrap_or_default();
        if !k.is_empty() {
            tags.push((k, v));
        }
        idx = tag_end + 6;
    }

    let p = tag_path(srv, bucket, key);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::File::create(&p)?;
    for (k, v) in &tags {
        writeln!(f, "{}={}", k, v)?;
    }
    let resp = Response::new(200).header("x-amz-request-id", rid);
    resp.write_headers(sock, Some(0))?;
    Ok(())
}

pub fn delete_object_tagging(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    rid: &str,
) -> std::io::Result<()> {
    if !srv.storage.bucket_exists(bucket) {
        return error_response(sock, 404, "NoSuchBucket", "no such bucket", rid, bucket);
    }
    let p = tag_path(srv, bucket, key);
    let _ = fs::remove_file(&p);
    let resp = Response::new(204).header("x-amz-request-id", rid);
    resp.write_headers(sock, Some(0))?;
    Ok(())
}

fn extract_inner(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let i = s.find(&open)? + open.len();
    let j = s[i..].find(&close)?;
    Some(s[i..i + j].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_inner_basic() {
        assert_eq!(extract_inner("<Key>k1</Key>", "Key"), Some("k1".into()));
        assert_eq!(
            extract_inner("<Tag><Key>k</Key><Value>v</Value></Tag>", "Value"),
            Some("v".into())
        );
    }

    #[test]
    fn extract_inner_missing() {
        assert_eq!(extract_inner("<Foo>x</Foo>", "Bar"), None);
        assert_eq!(extract_inner("<Key>k</Key>", "Other"), None);
    }

    #[test]
    fn extract_inner_first_occurrence() {
        // First Key wins; second is ignored.
        assert_eq!(
            extract_inner("<Key>a</Key><Key>b</Key>", "Key"),
            Some("a".into())
        );
    }

    // ---- handler-level tests via BuiltResponse ----

    use crate::storage::Storage;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("minibucket_thandler_{}_{}", label, nanos));
        p
    }

    struct ScopedRoot(PathBuf);
    impl Drop for ScopedRoot {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    fn make_server(label: &str) -> (Server, ScopedRoot) {
        let root = tmp_root(label);
        let storage = Storage::new(root.clone()).unwrap();
        (
            Server {
                storage,
                credentials: crate::creds::Credentials::new(),
                require_auth: false,
                region: "us-east-1".into(),
                domain: None,
            },
            ScopedRoot(root),
        )
    }

    #[test]
    fn build_get_object_tagging_404_for_missing_bucket() {
        let (srv, _g) = make_server("tag_404");
        let r = build_get_object_tagging(&srv, "missing", "k", "rid");
        assert_eq!(r.status, 404);
    }

    #[test]
    fn build_get_object_tagging_empty_when_no_sidecar() {
        let (srv, _g) = make_server("tag_empty");
        srv.storage.create_bucket("buck").unwrap();
        let r = build_get_object_tagging(&srv, "buck", "k", "rid");
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body.into_bytes().unwrap()).unwrap();
        assert!(body.contains("<TagSet></TagSet>"));
    }

    #[test]
    fn build_get_object_tagging_reads_sidecar() {
        let (srv, _g) = make_server("tag_present");
        srv.storage.create_bucket("buck").unwrap();
        // Write the sidecar directly — exercises only the read path.
        let p = tag_path(&srv, "buck", "k");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, "env=prod").unwrap();
        writeln!(f, "team=storage").unwrap();
        drop(f);
        let r = build_get_object_tagging(&srv, "buck", "k", "rid");
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body.into_bytes().unwrap()).unwrap();
        assert!(body.contains("<Key>env</Key><Value>prod</Value>"));
        assert!(body.contains("<Key>team</Key><Value>storage</Value>"));
    }
}
