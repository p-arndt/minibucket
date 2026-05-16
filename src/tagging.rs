// Object tagging: PutObjectTagging, GetObjectTagging, DeleteObjectTagging.
// Tags are stored as `key=value` pairs (one per line) in a sidecar file.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::http::{Request, Response};
use crate::s3::{error_response, read_body_all, write_xml, Server};
use crate::util::xml_escape;

fn tag_path(srv: &Server, bucket: &str, key: &str) -> PathBuf {
    srv.storage
        .root
        .join("buckets")
        .join(bucket)
        .join("tags")
        .join(format!("{}.tags", key))
}

pub fn get_object_tagging(
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
    write_xml(sock, 200, &body, rid)
}

pub fn put_object_tagging(
    srv: &Server,
    req: &mut Request,
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
