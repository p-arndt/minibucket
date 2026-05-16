// Percent encoding/decoding for URLs and SigV4 canonicalization.

pub fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        if b == b'+' {
            out.push(b' ');
        } else {
            out.push(b);
        }
        i += 1;
    }
    out
}

pub fn percent_decode_str(s: &str) -> String {
    String::from_utf8_lossy(&percent_decode(s)).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn unreserved(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~')
}

// AWS SigV4 canonical URI: percent-encode every byte except unreserved
// and '/' (the path separator). Already-encoded sequences are encoded
// again per AWS spec (encode twice for non-S3) -- for S3 we encode once.
pub fn encode_path_sigv4(path: &str) -> String {
    let mut s = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        if unreserved(b) || b == b'/' {
            s.push(b as char);
        } else {
            s.push('%');
            s.push_str(&format!("{:02X}", b));
        }
    }
    s
}

pub fn encode_component(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if unreserved(b) {
            o.push(b as char);
        } else {
            o.push('%');
            o.push_str(&format!("{:02X}", b));
        }
    }
    o
}

// Parse a query string into key/value pairs, keys/values percent-decoded.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    if q.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        if let Some(eq) = part.find('=') {
            let k = percent_decode_str(&part[..eq]);
            let v = percent_decode_str(&part[eq + 1..]);
            out.push((k, v));
        } else {
            out.push((percent_decode_str(part), String::new()));
        }
    }
    out
}
