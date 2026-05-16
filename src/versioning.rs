// Bucket-versioning routes: GetBucketVersioning, PutBucketVersioning, ListObjectVersions.

use crate::http::{Request, Response};
use crate::s3::{error_response, read_body_all, write_xml, Server};
use crate::storage::{StorageError, VersioningStatus};
use crate::util::{iso8601, xml_escape};

pub fn build_get_versioning(
    srv: &Server,
    bucket: &str,
    rid: &str,
) -> crate::http::BuiltResponse {
    if !srv.storage.bucket_exists(bucket) {
        return crate::s3::build_error(404, "NoSuchBucket", "no such bucket", rid, bucket);
    }
    let status = srv.storage.versioning_status(bucket);
    let body = match status {
        VersioningStatus::Disabled => {
            // S3 returns an empty <VersioningConfiguration/> for unversioned buckets.
            r#"<?xml version="1.0" encoding="UTF-8"?><VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"/>"#.to_string()
        }
        _ => format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>{}</Status></VersioningConfiguration>"#,
            status.as_str()
        ),
    };
    crate::http::BuiltResponse::new(200)
        .header("x-amz-request-id", rid)
        .xml(body)
}

pub fn get_versioning(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    rid: &str,
) -> std::io::Result<()> {
    build_get_versioning(srv, bucket, rid).write_to(sock)
}

pub fn put_versioning<R: std::io::BufRead>(
    srv: &Server,
    req: &mut Request<R>,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    rid: &str,
) -> std::io::Result<()> {
    if !srv.storage.bucket_exists(bucket) {
        return error_response(sock, 404, "NoSuchBucket", "no such bucket", rid, bucket);
    }
    let body = read_body_all(req)?;
    let xml = String::from_utf8_lossy(&body);
    let status = match extract_inner(&xml, "Status").as_deref() {
        Some("Enabled") => VersioningStatus::Enabled,
        Some("Suspended") => VersioningStatus::Suspended,
        _ => {
            return error_response(
                sock,
                400,
                "MalformedXML",
                "Status must be Enabled or Suspended",
                rid,
                bucket,
            );
        }
    };
    if let Err(e) = srv.storage.set_versioning_status(bucket, status) {
        return error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, bucket);
    }
    let resp = Response::new(200).header("x-amz-request-id", rid);
    resp.write_headers(sock, Some(0))?;
    Ok(())
}

pub fn list_versions(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    query: &[(String, String)],
    rid: &str,
) -> std::io::Result<()> {
    let prefix = query
        .iter()
        .find(|(k, _)| k == "prefix")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let delimiter = query
        .iter()
        .find(|(k, _)| k == "delimiter")
        .map(|(_, v)| v.as_str());
    let max_keys: usize = query
        .iter()
        .find(|(k, _)| k == "max-keys")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(1000);

    let versions = match srv.storage.list_versions(bucket) {
        Ok(v) => v,
        Err(StorageError::NotFound) => {
            return error_response(sock, 404, "NoSuchBucket", "no such bucket", rid, bucket);
        }
        Err(e) => {
            return error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, bucket);
        }
    };

    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    body.push_str(r#"<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
    body.push_str(&format!("<Name>{}</Name>", xml_escape(bucket)));
    body.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(prefix)));
    body.push_str(&format!("<MaxKeys>{}</MaxKeys>", max_keys));
    if let Some(d) = delimiter {
        body.push_str(&format!("<Delimiter>{}</Delimiter>", xml_escape(d)));
    }
    body.push_str("<IsTruncated>false</IsTruncated>");

    let mut count = 0usize;
    for v in &versions {
        if !v.key.starts_with(prefix) { continue; }
        if count >= max_keys { break; }
        count += 1;
        let tag = if v.is_delete_marker { "DeleteMarker" } else { "Version" };
        body.push_str(&format!("<{}>", tag));
        body.push_str(&format!("<Key>{}</Key>", xml_escape(&v.key)));
        body.push_str(&format!("<VersionId>{}</VersionId>", xml_escape(&v.version_id)));
        body.push_str(&format!("<IsLatest>{}</IsLatest>", v.is_latest));
        body.push_str(&format!("<LastModified>{}</LastModified>", iso8601(v.last_modified)));
        if !v.is_delete_marker {
            body.push_str(&format!("<ETag>&quot;{}&quot;</ETag>", v.etag));
            body.push_str(&format!("<Size>{}</Size>", v.size));
            body.push_str("<StorageClass>STANDARD</StorageClass>");
        }
        body.push_str(&format!("<Owner><ID>minibucket</ID><DisplayName>minibucket</DisplayName></Owner>"));
        body.push_str(&format!("</{}>", tag));
    }

    body.push_str("</ListVersionsResult>");
    write_xml(sock, 200, &body, rid)
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
    fn parses_status_from_versioning_xml() {
        let xml = r#"<?xml version="1.0"?><VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
        assert_eq!(extract_inner(xml, "Status").as_deref(), Some("Enabled"));
    }

    #[test]
    fn status_str_values() {
        assert_eq!(VersioningStatus::Enabled.as_str(), "Enabled");
        assert_eq!(VersioningStatus::Suspended.as_str(), "Suspended");
        assert_eq!(VersioningStatus::Disabled.as_str(), "");
    }

    #[test]
    fn records_versions_only_for_enabled_and_suspended() {
        assert!(VersioningStatus::Enabled.records_versions());
        assert!(VersioningStatus::Suspended.records_versions());
        assert!(!VersioningStatus::Disabled.records_versions());
    }

    // ---- handler-level tests via BuiltResponse ----

    use crate::storage::Storage;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("minibucket_vhandler_{}_{}", label, nanos));
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
    fn build_get_versioning_404() {
        let (srv, _g) = make_server("ver_404");
        let r = build_get_versioning(&srv, "missing", "rid");
        assert_eq!(r.status, 404);
        let body = String::from_utf8(r.body.into_bytes().unwrap()).unwrap();
        assert!(body.contains("NoSuchBucket"));
    }

    #[test]
    fn build_get_versioning_disabled_returns_empty_config() {
        let (srv, _g) = make_server("ver_disabled");
        srv.storage.create_bucket("buck").unwrap();
        let r = build_get_versioning(&srv, "buck", "rid");
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body.into_bytes().unwrap()).unwrap();
        // Self-closing empty VersioningConfiguration.
        assert!(body.contains("<VersioningConfiguration"));
        assert!(body.contains("/>"));
        assert!(!body.contains("<Status>"));
    }

    #[test]
    fn build_get_versioning_enabled_includes_status() {
        let (srv, _g) = make_server("ver_enabled");
        srv.storage.create_bucket("buck").unwrap();
        srv.storage
            .set_versioning_status("buck", VersioningStatus::Enabled)
            .unwrap();
        let r = build_get_versioning(&srv, "buck", "rid");
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body.into_bytes().unwrap()).unwrap();
        assert!(body.contains("<Status>Enabled</Status>"));
    }
}
