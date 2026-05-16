// Minimal HTTP/1.1 server primitives. Just enough to serve the S3 API.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_LINE_BYTES: usize = 16 * 1024;

pub struct Request {
    pub method: String,
    pub raw_path: String,   // /bucket/key (still percent-encoded)
    pub path: String,       // percent-decoded path
    pub query_raw: String,  // a=1&b=2 (still encoded)
    pub headers: Headers,
    pub reader: BufReader<TcpStream>,
}

#[derive(Default, Clone)]
pub struct Headers {
    // canonical lowercase name -> original-cased name + value
    pub map: HashMap<String, (String, String)>,
    pub order: Vec<String>,
}

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.map.get(&name.to_ascii_lowercase()).map(|(_, v)| v.as_str())
    }
    pub fn insert(&mut self, name: &str, value: &str) {
        let lc = name.to_ascii_lowercase();
        if !self.map.contains_key(&lc) {
            self.order.push(lc.clone());
        }
        self.map.insert(lc, (name.to_string(), value.to_string()));
    }
}

pub fn read_request(stream: TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = read_line_limited(&mut reader, &mut line, MAX_LINE_BYTES)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty"));
    }
    let line = line.trim_end_matches(['\r', '\n']).to_string();
    let mut it = line.splitn(3, ' ');
    let method = it.next().unwrap_or("").to_string();
    let target = it.next().unwrap_or("").to_string();
    let _version = it.next().unwrap_or("HTTP/1.1").to_string();

    let (raw_path, query_raw) = match target.find('?') {
        Some(i) => (target[..i].to_string(), target[i + 1..].to_string()),
        None => (target.clone(), String::new()),
    };
    let path = crate::url::percent_decode_str(&raw_path);

    let mut headers = Headers::default();
    let mut total = 0usize;
    loop {
        let mut hl = String::new();
        let nr = read_line_limited(&mut reader, &mut hl, MAX_LINE_BYTES)?;
        if nr == 0 {
            break;
        }
        total += nr;
        if total > MAX_HEADER_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "headers too large"));
        }
        let trimmed = hl.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(c) = trimmed.find(':') {
            let name = trimmed[..c].trim();
            let value = trimmed[c + 1..].trim();
            headers.insert(name, value);
        }
    }

    Ok(Request { method, raw_path, path, query_raw, headers, reader })
}

fn read_line_limited<R: BufRead>(r: &mut R, out: &mut String, limit: usize) -> io::Result<usize> {
    let mut total = 0;
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let s = std::str::from_utf8(&buf[..=pos])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 header"))?;
            out.push_str(s);
            total += pos + 1;
            r.consume(pos + 1);
            return Ok(total);
        } else {
            let s = std::str::from_utf8(buf)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 header"))?;
            out.push_str(s);
            total += buf.len();
            let consumed = buf.len();
            r.consume(consumed);
            if total > limit {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
        }
    }
}

// --- Body readers ---

// AWS streaming-aws-chunked decoder. Each chunk:
//   <size-hex>;chunk-signature=<sig>\r\n<data>\r\n
// Terminator: 0;chunk-signature=...\r\n\r\n
pub struct AwsChunkedReader<'a> {
    pub r: &'a mut BufReader<TcpStream>,
    pub buf: Vec<u8>,
    pub pos: usize,
    pub done: bool,
    pub chunk_ctx: Option<crate::sigv4::ChunkContext>,
}

impl<'a> AwsChunkedReader<'a> {
    pub fn new(r: &'a mut BufReader<TcpStream>) -> Self {
        Self { r, buf: Vec::new(), pos: 0, done: false, chunk_ctx: None }
    }
    pub fn with_chunk_ctx(mut self, ctx: Option<crate::sigv4::ChunkContext>) -> Self {
        self.chunk_ctx = ctx;
        self
    }
    fn fill_next_chunk(&mut self) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        let mut header = String::new();
        read_line_limited(self.r, &mut header, MAX_LINE_BYTES)?;
        let header = header.trim_end_matches(['\r', '\n']);
        let mut size_hex = "";
        let mut chunk_sig: Option<&str> = None;
        for (i, part) in header.split(';').enumerate() {
            if i == 0 {
                size_hex = part.trim();
            } else if let Some(v) = part.trim().strip_prefix("chunk-signature=") {
                chunk_sig = Some(v);
            }
        }
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        let mut data = vec![0u8; size];
        if size > 0 {
            self.r.read_exact(&mut data)?;
        }
        // Each chunk (including the 0-byte terminator) is followed by CRLF.
        let mut crlf = [0u8; 2];
        self.r.read_exact(&mut crlf)?;

        if let Some(ctx) = self.chunk_ctx.as_mut() {
            let sig = chunk_sig.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "chunk-signature missing")
            })?;
            if ctx.verify_and_advance(&data, sig).is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chunk signature mismatch",
                ));
            }
        }

        if size == 0 {
            self.done = true;
            return Ok(());
        }
        self.buf = data;
        self.pos = 0;
        Ok(())
    }
}

impl<'a> Read for AwsChunkedReader<'a> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            if self.done {
                return Ok(0);
            }
            self.fill_next_chunk()?;
            if self.done && self.pos >= self.buf.len() {
                return Ok(0);
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// Standard fixed-length body
pub struct FixedReader<'a> {
    pub r: &'a mut BufReader<TcpStream>,
    pub remaining: u64,
}
impl<'a> Read for FixedReader<'a> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = (self.remaining.min(out.len() as u64)) as usize;
        let n = self.r.read(&mut out[..cap])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

// --- Response writer ---

pub struct Response {
    pub status: u16,
    pub status_text: &'static str,
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        let text = status_text(status);
        Self { status, status_text: text, headers: Vec::new() }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    pub fn write_headers<W: Write>(&self, w: &mut W, body_len: Option<u64>) -> io::Result<()> {
        write!(w, "HTTP/1.1 {} {}\r\n", self.status, self.status_text)?;
        let mut have_len = false;
        let mut have_type = false;
        let mut have_conn = false;
        let mut have_date = false;
        let mut have_server = false;
        for (k, v) in &self.headers {
            let lk = k.to_ascii_lowercase();
            if lk == "content-length" { have_len = true; }
            if lk == "content-type" { have_type = true; }
            if lk == "connection" { have_conn = true; }
            if lk == "date" { have_date = true; }
            if lk == "server" { have_server = true; }
            write!(w, "{}: {}\r\n", k, v)?;
        }
        if !have_type {
            write!(w, "Content-Type: application/xml\r\n")?;
        }
        if !have_len {
            if let Some(l) = body_len {
                write!(w, "Content-Length: {}\r\n", l)?;
            }
        }
        if !have_conn {
            write!(w, "Connection: close\r\n")?;
        }
        if !have_date {
            write!(w, "Date: {}\r\n", crate::util::http_date_now())?;
        }
        if !have_server {
            write!(w, "Server: frust/0.1\r\n")?;
        }
        write!(w, "\r\n")?;
        Ok(())
    }
}

pub fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        412 => "Precondition Failed",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "OK",
    }
}
