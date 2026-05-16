// S3 API handlers. Dispatched from main loop after auth.

use std::io::{Read, Seek, SeekFrom, Write};

use crate::http::{AwsChunkedReader, FixedReader, Headers, Request, Response};
use crate::storage::{Storage, StorageError};
use crate::url::parse_query;
use crate::util::{iso8601, request_id, xml_escape};

pub struct Server {
    pub storage: Storage,
    pub credentials: crate::creds::Credentials,
    pub require_auth: bool,
    pub region: String,
    pub domain: Option<String>, // for virtual-hosted-style addressing
}

pub fn dispatch(
    srv: &Server,
    mut req: Request,
    sock: &mut std::net::TcpStream,
    chunk_ctx: Option<crate::sigv4::ChunkContext>,
) -> std::io::Result<()> {
    let method = req.method.clone();
    let path = req.path.clone();
    let query = parse_query(&req.query_raw);

    // Resolve bucket + key. Prefer virtual-hosted style if the Host header
    // matches the configured domain (e.g. bucket.s3.local).
    let (bucket, key) = resolve_addressing(srv, &req, &path);

    let rid = request_id();

    // Multipart upload routes (detected before the generic ones).
    if has_q(&query, "uploads") {
        return match method.as_str() {
            "POST" if !key.is_empty() => {
                crate::multipart::create_multipart(srv, sock, &bucket, &key, &req.headers, &rid)
            }
            "GET" if key.is_empty() => {
                crate::multipart::list_multipart_uploads(srv, sock, &bucket, &rid)
            }
            _ => error_response(sock, 400, "InvalidRequest", "uploads route", &rid, &req.path),
        };
    }
    if let Some(upload_id) = qget(&query, "uploadId") {
        let upload_id = upload_id.to_string();
        let part_number = qget(&query, "partNumber").and_then(|s| s.parse::<u32>().ok());
        return match method.as_str() {
            "PUT" if part_number.is_some() => crate::multipart::upload_part(
                srv, &mut req, sock, &bucket, &key, &upload_id, part_number.unwrap(),
                &rid, chunk_ctx,
            ),
            "POST" => crate::multipart::complete_multipart(
                srv, &mut req, sock, &bucket, &key, &upload_id, &rid,
            ),
            "DELETE" => crate::multipart::abort_multipart(srv, sock, &bucket, &key, &upload_id, &rid),
            "GET" => crate::multipart::list_parts(srv, sock, &bucket, &key, &upload_id, &rid),
            _ => error_response(sock, 400, "InvalidRequest", "uploadId route", &rid, &req.path),
        };
    }

    // Tagging routes.
    if has_q(&query, "tagging") {
        return match method.as_str() {
            "GET" if !key.is_empty() => crate::tagging::get_object_tagging(srv, sock, &bucket, &key, &rid),
            "PUT" if !key.is_empty() => crate::tagging::put_object_tagging(srv, &mut req, sock, &bucket, &key, &rid),
            "DELETE" if !key.is_empty() => crate::tagging::delete_object_tagging(srv, sock, &bucket, &key, &rid),
            _ => error_response(sock, 501, "NotImplemented", "bucket tagging not implemented", &rid, &req.path),
        };
    }

    let result: std::io::Result<()> = match method.as_str() {
        "GET" if bucket.is_empty() => list_buckets(srv, sock, &rid),
        "GET" if key.is_empty() && has_q(&query, "location") => bucket_location(srv, sock, &bucket, &rid),
        "HEAD" if key.is_empty() => head_bucket(srv, sock, &bucket, &rid),
        "PUT" if key.is_empty() => create_bucket(srv, sock, &bucket, &rid),
        "DELETE" if key.is_empty() => delete_bucket(srv, sock, &bucket, &rid),
        "GET" if key.is_empty() => list_objects(srv, sock, &bucket, &query, &rid),
        "POST" if key.is_empty() && has_q(&query, "delete") => {
            delete_objects(srv, &mut req, sock, &bucket, &rid)
        }
        "PUT" if req.headers.get("x-amz-copy-source").is_some() => {
            copy_object(srv, sock, &bucket, &key, &req.headers, &rid)
        }
        "PUT" => put_object(srv, &mut req, sock, &bucket, &key, &rid, chunk_ctx),
        "GET" => get_object(srv, sock, &bucket, &key, &req.headers, &rid, false),
        "HEAD" => get_object(srv, sock, &bucket, &key, &req.headers, &rid, true),
        "DELETE" => delete_object(srv, sock, &bucket, &key, &rid),
        _ => error_response(sock, 501, "NotImplemented", "Method not supported", &rid, &req.path),
    };
    result
}

// Returns (bucket, key) from either virtual-hosted style or path style.
fn resolve_addressing(srv: &Server, req: &Request, path: &str) -> (String, String) {
    let trimmed = path.trim_start_matches('/');
    if let (Some(domain), Some(host)) = (srv.domain.as_ref(), req.headers.get("host")) {
        let host = host.split(':').next().unwrap_or(host);
        let suffix = format!(".{}", domain);
        if let Some(bucket) = host.strip_suffix(&suffix) {
            if !bucket.is_empty() {
                return (bucket.to_string(), trimmed.to_string());
            }
        }
    }
    match trimmed.find('/') {
        Some(i) => (trimmed[..i].to_string(), trimmed[i + 1..].to_string()),
        None => (trimmed.to_string(), String::new()),
    }
}

fn has_q(q: &[(String, String)], name: &str) -> bool {
    q.iter().any(|(k, _)| k == name)
}
fn qget<'a>(q: &'a [(String, String)], name: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

// ---------- handlers ----------

fn list_buckets(srv: &Server, sock: &mut std::net::TcpStream, rid: &str) -> std::io::Result<()> {
    let buckets = srv.storage.list_buckets().unwrap_or_default();
    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    body.push_str(r#"<ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
    body.push_str("<Owner><ID>frust</ID><DisplayName>frust</DisplayName></Owner><Buckets>");
    for b in &buckets {
        body.push_str(&format!(
            "<Bucket><Name>{}</Name><CreationDate>{}</CreationDate></Bucket>",
            xml_escape(&b.name),
            iso8601(b.creation_date)
        ));
    }
    body.push_str("</Buckets></ListAllMyBucketsResult>");
    write_xml(sock, 200, &body, rid)
}

fn bucket_location(srv: &Server, sock: &mut std::net::TcpStream, bucket: &str, rid: &str) -> std::io::Result<()> {
    if !srv.storage.bucket_exists(bucket) {
        return error_response(sock, 404, "NoSuchBucket", "The specified bucket does not exist", rid, bucket);
    }
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">{}</LocationConstraint>"#,
        xml_escape(&srv.region)
    );
    write_xml(sock, 200, &body, rid)
}

fn head_bucket(srv: &Server, sock: &mut std::net::TcpStream, bucket: &str, rid: &str) -> std::io::Result<()> {
    if srv.storage.bucket_exists(bucket) {
        let resp = Response::new(200)
            .header("x-amz-request-id", rid)
            .header("x-amz-bucket-region", &srv.region);
        resp.write_headers(sock, Some(0))?;
        Ok(())
    } else {
        error_response(sock, 404, "NoSuchBucket", "The specified bucket does not exist", rid, bucket)
    }
}

fn create_bucket(srv: &Server, sock: &mut std::net::TcpStream, bucket: &str, rid: &str) -> std::io::Result<()> {
    match srv.storage.create_bucket(bucket) {
        Ok(()) => {
            let resp = Response::new(200)
                .header("Location", &format!("/{}", bucket))
                .header("x-amz-request-id", rid);
            resp.write_headers(sock, Some(0))
        }
        Err(StorageError::InvalidName) => {
            error_response(sock, 400, "InvalidBucketName", "Bucket name is invalid", rid, bucket)
        }
        Err(StorageError::Exists) => {
            error_response(sock, 409, "BucketAlreadyOwnedByYou", "Bucket exists", rid, bucket)
        }
        Err(e) => {
            error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, bucket)
        }
    }
}

fn delete_bucket(srv: &Server, sock: &mut std::net::TcpStream, bucket: &str, rid: &str) -> std::io::Result<()> {
    match srv.storage.delete_bucket(bucket) {
        Ok(()) => {
            let resp = Response::new(204).header("x-amz-request-id", rid);
            resp.write_headers(sock, Some(0))
        }
        Err(StorageError::NotFound) => {
            error_response(sock, 404, "NoSuchBucket", "The specified bucket does not exist", rid, bucket)
        }
        Err(StorageError::NotEmpty) => {
            error_response(sock, 409, "BucketNotEmpty", "Bucket is not empty", rid, bucket)
        }
        Err(StorageError::InvalidName) => {
            error_response(sock, 400, "InvalidBucketName", "Bucket name is invalid", rid, bucket)
        }
        Err(e) => {
            error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, bucket)
        }
    }
}

fn list_objects(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    query: &[(String, String)],
    rid: &str,
) -> std::io::Result<()> {
    let prefix = qget(query, "prefix").unwrap_or("");
    let delimiter = qget(query, "delimiter");
    let max_keys: usize = qget(query, "max-keys")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
        .min(1000);
    let marker = qget(query, "marker");
    let is_v2 = qget(query, "list-type") == Some("2");
    let continuation = qget(query, "continuation-token");
    let start_after = qget(query, "start-after");
    let effective_marker = if is_v2 {
        continuation.or(start_after)
    } else {
        marker
    };

    let res = match srv.storage.list_objects(bucket, prefix, delimiter, max_keys, effective_marker) {
        Ok(r) => r,
        Err(StorageError::NotFound) => {
            return error_response(sock, 404, "NoSuchBucket", "The specified bucket does not exist", rid, bucket);
        }
        Err(e) => {
            return error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, bucket);
        }
    };

    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    if is_v2 {
        body.push_str(r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
        body.push_str(&format!("<Name>{}</Name>", xml_escape(bucket)));
        body.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(prefix)));
        body.push_str(&format!("<KeyCount>{}</KeyCount>", res.contents.len()));
        body.push_str(&format!("<MaxKeys>{}</MaxKeys>", max_keys));
        if let Some(d) = delimiter {
            body.push_str(&format!("<Delimiter>{}</Delimiter>", xml_escape(d)));
        }
        body.push_str(&format!("<IsTruncated>{}</IsTruncated>", res.truncated));
        if let (true, Some(nm)) = (res.truncated, res.next_marker.as_ref()) {
            body.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", xml_escape(nm)));
        }
    } else {
        body.push_str(r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
        body.push_str(&format!("<Name>{}</Name>", xml_escape(bucket)));
        body.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(prefix)));
        body.push_str(&format!("<Marker>{}</Marker>", xml_escape(marker.unwrap_or(""))));
        body.push_str(&format!("<MaxKeys>{}</MaxKeys>", max_keys));
        if let Some(d) = delimiter {
            body.push_str(&format!("<Delimiter>{}</Delimiter>", xml_escape(d)));
        }
        body.push_str(&format!("<IsTruncated>{}</IsTruncated>", res.truncated));
        if let (true, Some(nm)) = (res.truncated, res.next_marker.as_ref()) {
            body.push_str(&format!("<NextMarker>{}</NextMarker>", xml_escape(nm)));
        }
    }
    for c in &res.contents {
        body.push_str("<Contents>");
        body.push_str(&format!("<Key>{}</Key>", xml_escape(&c.key)));
        body.push_str(&format!("<LastModified>{}</LastModified>", iso8601(c.last_modified)));
        body.push_str(&format!("<ETag>&quot;{}&quot;</ETag>", c.etag));
        body.push_str(&format!("<Size>{}</Size>", c.size));
        body.push_str("<StorageClass>STANDARD</StorageClass>");
        body.push_str("</Contents>");
    }
    for cp in &res.common_prefixes {
        body.push_str("<CommonPrefixes>");
        body.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(cp)));
        body.push_str("</CommonPrefixes>");
    }
    body.push_str("</ListBucketResult>");

    write_xml(sock, 200, &body, rid)
}

fn put_object(
    srv: &Server,
    req: &mut Request,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    rid: &str,
    chunk_ctx: Option<crate::sigv4::ChunkContext>,
) -> std::io::Result<()> {
    if key.is_empty() {
        return error_response(sock, 400, "InvalidRequest", "Empty key", rid, key);
    }

    let mut writer = match srv.storage.put_object_writer(bucket, key) {
        Ok(w) => w,
        Err(StorageError::NotFound) => {
            // Drain body and return 404.
            drain_body(req)?;
            return error_response(sock, 404, "NoSuchBucket", "The specified bucket does not exist", rid, bucket);
        }
        Err(StorageError::InvalidName) => {
            drain_body(req)?;
            return error_response(sock, 400, "InvalidArgument", "Invalid key name", rid, key);
        }
        Err(e) => {
            drain_body(req)?;
            return error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, key);
        }
    };

    let content_type = req
        .headers
        .get("content-type")
        .unwrap_or("application/octet-stream")
        .to_string();

    let is_chunked = req.headers.get("content-encoding").map(|v| v.contains("aws-chunked")).unwrap_or(false);
    let content_sha = req.headers.get("x-amz-content-sha256").unwrap_or("");
    let streaming = is_chunked
        || content_sha == "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"
        || content_sha == "STREAMING-UNSIGNED-PAYLOAD-TRAILER";

    let declared_len: Option<u64> = req
        .headers
        .get("x-amz-decoded-content-length")
        .and_then(|v| v.parse().ok())
        .or_else(|| req.headers.get("content-length").and_then(|v| v.parse().ok()));

    let mut buf = vec![0u8; 64 * 1024];
    if streaming {
        let mut r = AwsChunkedReader::new(&mut req.reader).with_chunk_ctx(chunk_ctx);
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 { break; }
            if let Err(e) = writer.write(&buf[..n]) {
                writer.abort();
                return error_response(sock, 500, "InternalError", &format!("write: {}", e), rid, key);
            }
        }
    } else {
        let remaining = req
            .headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0u64);
        let mut r = FixedReader { r: &mut req.reader, remaining };
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 { break; }
            if let Err(e) = writer.write(&buf[..n]) {
                writer.abort();
                return error_response(sock, 500, "InternalError", &format!("write: {}", e), rid, key);
            }
        }
    }

    let (etag, size) = match writer.finish(&content_type) {
        Ok(v) => v,
        Err(e) => return error_response(sock, 500, "InternalError", &format!("finalize: {}", e), rid, key),
    };

    if let Some(d) = declared_len {
        if streaming && d != size {
            eprintln!("[put] declared {} but got {} bytes", d, size);
        }
    }

    let resp = Response::new(200)
        .header("ETag", &format!("\"{}\"", etag))
        .header("x-amz-request-id", rid);
    resp.write_headers(sock, Some(0))?;
    Ok(())
}

fn copy_object(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    dst_bucket: &str,
    dst_key: &str,
    headers: &Headers,
    rid: &str,
) -> std::io::Result<()> {
    let source = match headers.get("x-amz-copy-source") {
        Some(s) => s,
        None => return error_response(sock, 400, "InvalidArgument", "missing x-amz-copy-source", rid, dst_key),
    };
    let decoded = crate::url::percent_decode_str(source.trim_start_matches('/'));
    let (src_bucket, src_key) = match decoded.find('/') {
        Some(i) => (decoded[..i].to_string(), decoded[i + 1..].to_string()),
        None => return error_response(sock, 400, "InvalidArgument", "copy-source must be /bucket/key", rid, dst_key),
    };

    let (meta, mut src_file) = match srv.storage.get_object(&src_bucket, &src_key) {
        Ok(v) => v,
        Err(StorageError::NotFound) => {
            return error_response(sock, 404, "NoSuchKey", "source not found", rid, &src_key);
        }
        Err(e) => return error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, &src_key),
    };

    let mut writer = match srv.storage.put_object_writer(dst_bucket, dst_key) {
        Ok(w) => w,
        Err(StorageError::NotFound) => {
            return error_response(sock, 404, "NoSuchBucket", "destination bucket missing", rid, dst_bucket);
        }
        Err(e) => return error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, dst_key),
    };

    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = src_file.read(&mut buf)?;
        if n == 0 { break; }
        if let Err(e) = writer.write(&buf[..n]) {
            writer.abort();
            return error_response(sock, 500, "InternalError", &format!("{}", e), rid, dst_key);
        }
    }
    let (etag, _size) = match writer.finish(&meta.content_type) {
        Ok(v) => v,
        Err(e) => return error_response(sock, 500, "InternalError", &format!("{}", e), rid, dst_key),
    };
    let now = crate::util::iso8601(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><CopyObjectResult><LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag></CopyObjectResult>"#,
        now, etag
    );
    write_xml(sock, 200, &body, rid)
}

fn get_object(
    srv: &Server,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    key: &str,
    headers: &Headers,
    rid: &str,
    head_only: bool,
) -> std::io::Result<()> {
    let (meta, mut file) = match srv.storage.get_object(bucket, key) {
        Ok(v) => v,
        Err(StorageError::NotFound) => {
            return error_response(sock, 404, "NoSuchKey", "The specified key does not exist", rid, key);
        }
        Err(e) => {
            return error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, key);
        }
    };
    let size = file.metadata()?.len();

    let range = headers.get("range").and_then(parse_range);
    let (status, start, length) = match range {
        Some((s, e)) => {
            let end = e.unwrap_or(size - 1).min(size - 1);
            if s > end {
                return error_response(sock, 416, "InvalidRange", "Bad range", rid, key);
            }
            (206, s, end - s + 1)
        }
        None => (200, 0u64, size),
    };

    let mut resp = Response::new(status)
        .header("Content-Type", &meta.content_type)
        .header("ETag", &format!("\"{}\"", meta.etag))
        .header("Last-Modified", &crate::util::http_date(meta.last_modified))
        .header("Accept-Ranges", "bytes")
        .header("x-amz-request-id", rid);
    if status == 206 {
        let cr = format!("bytes {}-{}/{}", start, start + length - 1, size);
        resp = resp.header("Content-Range", &cr);
    }
    resp.write_headers(sock, Some(length))?;
    if head_only { return Ok(()); }

    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut buf = vec![0u8; 64 * 1024];
    let mut remaining = length;
    while remaining > 0 {
        let want = (remaining.min(buf.len() as u64)) as usize;
        let n = file.read(&mut buf[..want])?;
        if n == 0 { break; }
        sock.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

fn parse_range(v: &str) -> Option<(u64, Option<u64>)> {
    let v = v.strip_prefix("bytes=")?;
    let mut parts = v.splitn(2, '-');
    let s: u64 = parts.next()?.parse().ok()?;
    let e = parts.next()?;
    let end = if e.is_empty() { None } else { Some(e.parse().ok()?) };
    Some((s, end))
}

fn delete_object(srv: &Server, sock: &mut std::net::TcpStream, bucket: &str, key: &str, rid: &str) -> std::io::Result<()> {
    match srv.storage.delete_object(bucket, key) {
        Ok(()) => {
            let resp = Response::new(204).header("x-amz-request-id", rid);
            resp.write_headers(sock, Some(0))
        }
        Err(StorageError::NotFound) => {
            error_response(sock, 404, "NoSuchBucket", "The specified bucket does not exist", rid, bucket)
        }
        Err(e) => error_response(sock, 500, "InternalError", &format!("{:?}", e), rid, key),
    }
}

fn delete_objects(
    srv: &Server,
    req: &mut Request,
    sock: &mut std::net::TcpStream,
    bucket: &str,
    rid: &str,
) -> std::io::Result<()> {
    // Read body (small XML) -- supports fixed or aws-chunked.
    let body = read_body_all(req)?;
    let s = String::from_utf8_lossy(&body);
    // Naive XML parse: pull <Key>...</Key> values.
    let mut keys = Vec::new();
    let mut idx = 0;
    while let Some(start) = s[idx..].find("<Key>") {
        let from = idx + start + 5;
        if let Some(end) = s[from..].find("</Key>") {
            keys.push(s[from..from + end].to_string());
            idx = from + end + 6;
        } else {
            break;
        }
    }

    let mut body_out = String::new();
    body_out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    body_out.push_str(r#"<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
    for k in &keys {
        match srv.storage.delete_object(bucket, k) {
            Ok(()) => {
                body_out.push_str(&format!("<Deleted><Key>{}</Key></Deleted>", xml_escape(k)));
            }
            Err(_) => {
                body_out.push_str(&format!(
                    "<Error><Key>{}</Key><Code>InternalError</Code><Message>delete failed</Message></Error>",
                    xml_escape(k)
                ));
            }
        }
    }
    body_out.push_str("</DeleteResult>");
    write_xml(sock, 200, &body_out, rid)
}

pub fn read_body_all(req: &mut Request) -> std::io::Result<Vec<u8>> {
    let is_chunked = req.headers.get("content-encoding").map(|v| v.contains("aws-chunked")).unwrap_or(false);
    let mut out = Vec::new();
    if is_chunked {
        let mut r = AwsChunkedReader::new(&mut req.reader);
        r.read_to_end(&mut out)?;
    } else {
        let remaining = req.headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0u64);
        let mut r = FixedReader { r: &mut req.reader, remaining };
        r.read_to_end(&mut out)?;
    }
    Ok(out)
}

fn drain_body(req: &mut Request) -> std::io::Result<()> {
    let _ = read_body_all(req)?;
    Ok(())
}

// ---------- response helpers ----------

pub fn write_xml(sock: &mut std::net::TcpStream, status: u16, body: &str, rid: &str) -> std::io::Result<()> {
    let resp = Response::new(status)
        .header("x-amz-request-id", rid)
        .header("Content-Type", "application/xml");
    resp.write_headers(sock, Some(body.len() as u64))?;
    sock.write_all(body.as_bytes())?;
    Ok(())
}

pub fn error_response(
    sock: &mut std::net::TcpStream,
    status: u16,
    code: &str,
    message: &str,
    rid: &str,
    resource: &str,
) -> std::io::Result<()> {
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>{}</Code><Message>{}</Message><Resource>{}</Resource><RequestId>{}</RequestId></Error>"#,
        xml_escape(code),
        xml_escape(message),
        xml_escape(resource),
        xml_escape(rid),
    );
    write_xml(sock, status, &body, rid)
}
