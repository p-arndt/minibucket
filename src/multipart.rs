// S3 multipart upload.
//
// Layout under <root>/buckets/<bucket>/uploads/<upload-id>/:
//   .info                # one line: <unix-ts>\n<content-type>\n<key>\n
//   parts/<NNNNN>        # part data
//   parts/<NNNNN>.meta   # one line: <md5-hex>\n<size>\n

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::http::{AwsChunkedReader, FixedReader, Headers, Request, Response};
use crate::md5::Md5;
use crate::s3::{error_response, read_body_all, write_xml, Server};
use crate::sha256::hex;
use crate::util::{iso8601, xml_escape};

fn upload_dir(srv: &Server, bucket: &str, upload_id: &str) -> PathBuf {
    srv.storage
        .root
        .join("buckets")
        .join(bucket)
        .join("uploads")
        .join(upload_id)
}

fn part_path(srv: &Server, bucket: &str, upload_id: &str, n: u32) -> PathBuf {
    upload_dir(srv, bucket, upload_id)
        .join("parts")
        .join(format!("{:05}", n))
}

fn part_meta(srv: &Server, bucket: &str, upload_id: &str, n: u32) -> PathBuf {
    upload_dir(srv, bucket, upload_id)
        .join("parts")
        .join(format!("{:05}.meta", n))
}

fn new_upload_id() -> String {
    // 32 hex chars derived from time + a counter.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mix1 = n
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mix2 = (n ^ c)
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(c);
    format!("{:016X}{:016X}", mix1, mix2)
}

pub fn create_multipart(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    headers: &Headers,
    rid: &str,
) -> std::io::Result<()> {
    if !srv.storage.bucket_exists(bucket) {
        return error_response(sock, 404, "NoSuchBucket", "no such bucket", rid, bucket);
    }
    if !crate::storage::valid_key(key) {
        return error_response(sock, 400, "InvalidArgument", "bad key", rid, key);
    }
    let upload_id = new_upload_id();
    let dir = upload_dir(srv, bucket, &upload_id);
    fs::create_dir_all(dir.join("parts"))?;
    let ct = headers.get("content-type").unwrap_or("application/octet-stream");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut info = File::create(dir.join(".info"))?;
    writeln!(info, "{}", now)?;
    writeln!(info, "{}", ct)?;
    writeln!(info, "{}", key)?;

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>{}</Bucket><Key>{}</Key><UploadId>{}</UploadId></InitiateMultipartUploadResult>"#,
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(&upload_id),
    );
    write_xml(sock, 200, &body, rid)
}

pub fn upload_part(
    srv: &Server,
    req: &mut Request,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    _key: &str,
    upload_id: &str,
    part_number: u32,
    rid: &str,
    chunk_ctx: Option<crate::sigv4::ChunkContext>,
) -> std::io::Result<()> {
    if part_number < 1 || part_number > 10_000 {
        return error_response(sock, 400, "InvalidArgument", "partNumber out of range", rid, upload_id);
    }
    let dir = upload_dir(srv, bucket, upload_id);
    if !dir.exists() {
        return error_response(sock, 404, "NoSuchUpload", "unknown uploadId", rid, upload_id);
    }
    let data_path = part_path(srv, bucket, upload_id, part_number);
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).open(&data_path)?;
    let mut md5 = Md5::new();
    let mut size: u64 = 0;

    let is_chunked = req.headers.get("content-encoding").map(|v| v.contains("aws-chunked")).unwrap_or(false);
    let content_sha = req.headers.get("x-amz-content-sha256").unwrap_or("");
    let streaming = is_chunked
        || content_sha == "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"
        || content_sha == "STREAMING-UNSIGNED-PAYLOAD-TRAILER";

    let mut buf = vec![0u8; 64 * 1024];
    if streaming {
        let mut r = AwsChunkedReader::new(&mut req.reader).with_chunk_ctx(chunk_ctx);
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 { break; }
            file.write_all(&buf[..n])?;
            md5.update(&buf[..n]);
            size += n as u64;
        }
    } else {
        let remaining = req.headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0u64);
        let mut r = FixedReader { r: &mut req.reader, remaining };
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 { break; }
            file.write_all(&buf[..n])?;
            md5.update(&buf[..n]);
            size += n as u64;
        }
    }
    file.flush()?;
    drop(file);

    let digest = md5.finalize();
    let etag_hex = hex(&digest);
    let mut mf = File::create(part_meta(srv, bucket, upload_id, part_number))?;
    writeln!(mf, "{}", etag_hex)?;
    writeln!(mf, "{}", size)?;

    let resp = Response::new(200)
        .header("ETag", &format!("\"{}\"", etag_hex))
        .header("x-amz-request-id", rid);
    resp.write_headers(sock, Some(0))?;
    Ok(())
}

pub fn complete_multipart(
    srv: &Server,
    req: &mut Request,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    upload_id: &str,
    rid: &str,
) -> std::io::Result<()> {
    let dir = upload_dir(srv, bucket, upload_id);
    if !dir.exists() {
        return error_response(sock, 404, "NoSuchUpload", "unknown uploadId", rid, upload_id);
    }
    // Read content-type from .info.
    let info_text = fs::read_to_string(dir.join(".info")).unwrap_or_default();
    let mut lines = info_text.lines();
    let _ = lines.next();
    let content_type = lines.next().unwrap_or("application/octet-stream").to_string();

    // Parse request body to learn the part order requested by the client.
    let body = read_body_all(req)?;
    let xml = String::from_utf8_lossy(&body);
    let mut requested: Vec<u32> = Vec::new();
    let mut idx = 0;
    while let Some(s) = xml[idx..].find("<Part>") {
        let from = idx + s + 6;
        let to = match xml[from..].find("</Part>") {
            Some(e) => from + e,
            None => break,
        };
        let blk = &xml[from..to];
        if let Some(n_str) = extract_inner(blk, "PartNumber") {
            if let Ok(n) = n_str.parse::<u32>() {
                requested.push(n);
            }
        }
        idx = to + 7;
    }

    // Verify each requested part exists, then concatenate into final object.
    let mut writer = match srv.storage.put_object_writer(bucket, key) {
        Ok(w) => w,
        Err(e) => return error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, key),
    };
    let mut part_md5s: Vec<u8> = Vec::with_capacity(requested.len() * 16);
    let mut buf = vec![0u8; 64 * 1024];
    for n in &requested {
        let p = part_path(srv, bucket, upload_id, *n);
        if !p.exists() {
            writer.abort();
            return error_response(sock, 400, "InvalidPart", &format!("missing part {}", n), rid, key);
        }
        let mut f = File::open(&p)?;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 { break; }
            writer.write(&buf[..n])?;
        }
        // Pull the part's md5 hex from the meta file and decode to bytes.
        let meta_text = fs::read_to_string(part_meta(srv, bucket, upload_id, *n)).unwrap_or_default();
        let first_line = meta_text.lines().next().unwrap_or("");
        if let Some(bytes) = decode_hex16(first_line) {
            part_md5s.extend_from_slice(&bytes);
        }
    }
    let (_etag, _size, _vid) = writer.finish(&content_type)?;

    // S3 multipart ETag: md5(concat(part_md5_bytes)) + "-" + count, hex.
    let final_digest = crate::md5::md5(&part_md5s);
    let final_etag = format!("{}-{}", hex(&final_digest), requested.len());

    // Rewrite the meta sidecar's etag line so list/head returns the multipart ETag.
    rewrite_meta_etag(srv, bucket, key, &final_etag).ok();

    // Cleanup upload directory.
    let _ = fs::remove_dir_all(&dir);

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Location>/{}/{}</Location><Bucket>{}</Bucket><Key>{}</Key><ETag>&quot;{}&quot;</ETag></CompleteMultipartUploadResult>"#,
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(bucket),
        xml_escape(key),
        final_etag,
    );
    write_xml(sock, 200, &body, rid)
}

pub fn abort_multipart(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    _key: &str,
    upload_id: &str,
    rid: &str,
) -> std::io::Result<()> {
    let dir = upload_dir(srv, bucket, upload_id);
    if !dir.exists() {
        return error_response(sock, 404, "NoSuchUpload", "unknown uploadId", rid, upload_id);
    }
    fs::remove_dir_all(&dir)?;
    let resp = Response::new(204).header("x-amz-request-id", rid);
    resp.write_headers(sock, Some(0))?;
    Ok(())
}

pub fn list_parts(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    upload_id: &str,
    rid: &str,
) -> std::io::Result<()> {
    let dir = upload_dir(srv, bucket, upload_id);
    if !dir.exists() {
        return error_response(sock, 404, "NoSuchUpload", "unknown uploadId", rid, upload_id);
    }
    let mut parts: Vec<(u32, String, u64)> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir.join("parts")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".meta") { continue; }
            if let Ok(n) = name.parse::<u32>() {
                let meta = fs::read_to_string(part_meta(srv, bucket, upload_id, n)).unwrap_or_default();
                let mut it = meta.lines();
                let etag = it.next().unwrap_or("").to_string();
                let size: u64 = it.next().unwrap_or("0").parse().unwrap_or(0);
                parts.push((n, etag, size));
            }
        }
    }
    parts.sort_by_key(|p| p.0);

    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    body.push_str(r#"<ListPartsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
    body.push_str(&format!("<Bucket>{}</Bucket>", xml_escape(bucket)));
    body.push_str(&format!("<Key>{}</Key>", xml_escape(key)));
    body.push_str(&format!("<UploadId>{}</UploadId>", xml_escape(upload_id)));
    body.push_str("<IsTruncated>false</IsTruncated>");
    for (n, etag, size) in &parts {
        body.push_str("<Part>");
        body.push_str(&format!("<PartNumber>{}</PartNumber>", n));
        body.push_str(&format!("<ETag>&quot;{}&quot;</ETag>", etag));
        body.push_str(&format!("<Size>{}</Size>", size));
        body.push_str("</Part>");
    }
    body.push_str("</ListPartsResult>");
    write_xml(sock, 200, &body, rid)
}

pub fn list_multipart_uploads(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    rid: &str,
) -> std::io::Result<()> {
    if !srv.storage.bucket_exists(bucket) {
        return error_response(sock, 404, "NoSuchBucket", "no such bucket", rid, bucket);
    }
    let uploads_dir = srv.storage.root.join("buckets").join(bucket).join("uploads");
    let mut entries: Vec<(String, String, u64)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&uploads_dir) {
        for e in rd.flatten() {
            let upload_id = e.file_name().to_string_lossy().to_string();
            let info = fs::read_to_string(e.path().join(".info")).unwrap_or_default();
            let mut it = info.lines();
            let ts: u64 = it.next().unwrap_or("0").parse().unwrap_or(0);
            let _ct = it.next().unwrap_or("").to_string();
            let key = it.next().unwrap_or("").to_string();
            entries.push((upload_id, key, ts));
        }
    }
    entries.sort_by(|a, b| a.1.cmp(&b.1));
    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    body.push_str(r#"<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
    body.push_str(&format!("<Bucket>{}</Bucket>", xml_escape(bucket)));
    body.push_str("<IsTruncated>false</IsTruncated>");
    for (id, key, ts) in &entries {
        body.push_str("<Upload>");
        body.push_str(&format!("<Key>{}</Key>", xml_escape(key)));
        body.push_str(&format!("<UploadId>{}</UploadId>", xml_escape(id)));
        body.push_str(&format!("<Initiated>{}</Initiated>", iso8601(*ts)));
        body.push_str("</Upload>");
    }
    body.push_str("</ListMultipartUploadsResult>");
    write_xml(sock, 200, &body, rid)
}

fn extract_inner(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let i = s.find(&open)? + open.len();
    let j = s[i..].find(&close)?;
    Some(s[i..i + j].to_string())
}

fn decode_hex16(s: &str) -> Option<[u8; 16]> {
    let s = s.trim();
    if s.len() != 32 { return None; }
    let mut out = [0u8; 16];
    for i in 0..16 {
        let h = hex_nibble(s.as_bytes()[i * 2])?;
        let l = hex_nibble(s.as_bytes()[i * 2 + 1])?;
        out[i] = (h << 4) | l;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn rewrite_meta_etag(srv: &Server, bucket: &str, key: &str, new_etag: &str) -> std::io::Result<()> {
    // The meta file path mirrors what storage::put_object_writer uses.
    let path = srv
        .storage
        .root
        .join("buckets")
        .join(bucket)
        .join("meta")
        .join(format!("{}.meta", key));
    if !path.exists() { return Ok(()); }
    let text = fs::read_to_string(&path)?;
    let mut out = String::new();
    let mut wrote = false;
    let mut map: HashMap<&str, String> = HashMap::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("etag: ") {
            map.insert("etag", v.to_string());
            wrote = true;
            continue;
        }
    }
    drop(map);
    for line in text.lines() {
        if line.starts_with("etag: ") {
            out.push_str(&format!("etag: {}\n", new_etag));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !wrote {
        out.push_str(&format!("etag: {}\n", new_etag));
    }
    fs::write(&path, out)?;
    Ok(())
}
