// AWS SigV4 verification (HTTP header style). Sufficient for AWS SDKs
// talking to S3, including STREAMING-AWS4-HMAC-SHA256-PAYLOAD requests
// (we verify the seed signature only — chunk signatures are accepted).

use crate::hmac::hmac_sha256;
use crate::http::Headers;
use crate::sha256::{hex, sha256};
use crate::url::{encode_component, encode_path_sigv4, parse_query};

#[derive(Debug)]
pub struct AuthInfo {
    pub access_key: String,
    pub date: String,
    pub region: String,
    pub service: String,
    pub signed_headers: Vec<String>,
    pub signature: String,
    pub amz_date: String,
    pub payload_hash: String,
}

#[derive(Debug)]
pub enum AuthError {
    Missing,
    Malformed(&'static str),
    UnknownKey,
    BadSignature,
}

pub fn parse_authorization(headers: &Headers) -> Result<AuthInfo, AuthError> {
    let auth = headers.get("authorization").ok_or(AuthError::Missing)?;
    if !auth.starts_with("AWS4-HMAC-SHA256") {
        return Err(AuthError::Malformed("not AWS4-HMAC-SHA256"));
    }
    let rest = auth["AWS4-HMAC-SHA256".len()..].trim_start();
    let mut credential = "";
    let mut signed_headers = "";
    let mut signature = "";
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = v;
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed_headers = v;
        } else if let Some(v) = part.strip_prefix("Signature=") {
            signature = v;
        }
    }
    if credential.is_empty() || signed_headers.is_empty() || signature.is_empty() {
        return Err(AuthError::Malformed("missing field"));
    }
    let cparts: Vec<&str> = credential.split('/').collect();
    if cparts.len() != 5 || cparts[4] != "aws4_request" {
        return Err(AuthError::Malformed("bad credential scope"));
    }
    let amz_date = headers.get("x-amz-date").unwrap_or("").to_string();
    let payload_hash = headers
        .get("x-amz-content-sha256")
        .unwrap_or("UNSIGNED-PAYLOAD")
        .to_string();
    Ok(AuthInfo {
        access_key: cparts[0].to_string(),
        date: cparts[1].to_string(),
        region: cparts[2].to_string(),
        service: cparts[3].to_string(),
        signed_headers: signed_headers.split(';').map(|s| s.to_string()).collect(),
        signature: signature.to_string(),
        amz_date,
        payload_hash,
    })
}

pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let mut k = Vec::with_capacity(4 + secret.len());
    k.extend_from_slice(b"AWS4");
    k.extend_from_slice(secret.as_bytes());
    let kd = hmac_sha256(&k, date.as_bytes());
    let kr = hmac_sha256(&kd, region.as_bytes());
    let ks = hmac_sha256(&kr, service.as_bytes());
    hmac_sha256(&ks, b"aws4_request")
}

pub fn canonical_request(
    method: &str,
    raw_path: &str,
    query_raw: &str,
    headers: &Headers,
    signed_headers: &[String],
    payload_hash: &str,
) -> String {
    // Canonical URI: the path, percent-encoded per SigV4 (S3: single-encoded).
    // We re-encode from the decoded form to ensure canonical output regardless
    // of how the client presented it on the wire.
    let decoded_path = crate::url::percent_decode_str(raw_path);
    let canonical_uri = encode_path_sigv4(&decoded_path);

    // Canonical query string: parse, encode each key/value, sort by encoded key.
    let parts = parse_query(query_raw);
    let mut encoded: Vec<(String, String)> = parts
        .into_iter()
        .map(|(k, v)| (encode_component(&k), encode_component(&v)))
        .collect();
    encoded.sort_by(|a, b| match a.0.cmp(&b.0) {
        std::cmp::Ordering::Equal => a.1.cmp(&b.1),
        o => o,
    });
    let canonical_query = encoded
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    // Canonical headers.
    let mut canonical_headers = String::new();
    for name in signed_headers {
        let v = headers.get(name).unwrap_or("");
        let collapsed = collapse_ws(v);
        canonical_headers.push_str(&name.to_ascii_lowercase());
        canonical_headers.push(':');
        canonical_headers.push_str(&collapsed);
        canonical_headers.push('\n');
    }
    let signed_str = signed_headers.join(";");

    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, canonical_uri, canonical_query, canonical_headers, signed_str, payload_hash
    )
}

fn collapse_ws(s: &str) -> String {
    let s = s.trim();
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' || c == '\t' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

pub fn string_to_sign(amz_date: &str, scope: &str, canonical_req: &str) -> String {
    let h = hex(&sha256(canonical_req.as_bytes()));
    format!("AWS4-HMAC-SHA256\n{}\n{}\n{}", amz_date, scope, h)
}

pub fn verify(
    method: &str,
    raw_path: &str,
    query_raw: &str,
    headers: &Headers,
    secret: &str,
    info: &AuthInfo,
) -> Result<(), AuthError> {
    let canon = canonical_request(
        method,
        raw_path,
        query_raw,
        headers,
        &info.signed_headers,
        &info.payload_hash,
    );
    let scope = format!("{}/{}/{}/aws4_request", info.date, info.region, info.service);
    let sts = string_to_sign(&info.amz_date, &scope, &canon);
    let key = signing_key(secret, &info.date, &info.region, &info.service);
    let sig = hex(&hmac_sha256(&key, sts.as_bytes()));
    if constant_time_eq(sig.as_bytes(), info.signature.as_bytes()) {
        Ok(())
    } else {
        eprintln!("[sigv4] mismatch\n--- canonical ---\n{}\n--- sts ---\n{}\n--- expected ---\n{}\n--- got ---\n{}",
            canon, sts, sig, info.signature);
        Err(AuthError::BadSignature)
    }
}

// Per-chunk signing context for STREAMING-AWS4-HMAC-SHA256-PAYLOAD.
// Each chunk: string_to_sign = "AWS4-HMAC-SHA256-PAYLOAD\n<amzdate>\n<scope>\n<prev-sig>\n<sha256-of-empty>\n<sha256-of-chunk>"
//             chunk_signature = hex(HMAC(signing_key, string_to_sign))
// The seed signature is from the Authorization header; each verified chunk's
// computed signature becomes prev-sig for the next.
pub struct ChunkContext {
    pub signing_key: [u8; 32],
    pub amz_date: String,
    pub scope: String,
    pub prev_signature: String,
    pub empty_hash: String,
}

impl ChunkContext {
    pub fn new(secret: &str, info: &AuthInfo) -> Self {
        let key = signing_key(secret, &info.date, &info.region, &info.service);
        let scope = format!("{}/{}/{}/aws4_request", info.date, info.region, info.service);
        Self {
            signing_key: key,
            amz_date: info.amz_date.clone(),
            scope,
            prev_signature: info.signature.clone(),
            empty_hash: hex(&sha256(b"")),
        }
    }

    pub fn expected_signature(&self, chunk_data: &[u8]) -> String {
        let chunk_hash = hex(&sha256(chunk_data));
        let sts = format!(
            "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}\n{}\n{}\n{}",
            self.amz_date, self.scope, self.prev_signature, self.empty_hash, chunk_hash
        );
        hex(&hmac_sha256(&self.signing_key, sts.as_bytes()))
    }

    pub fn verify_and_advance(&mut self, chunk_data: &[u8], got: &str) -> Result<(), AuthError> {
        let expected = self.expected_signature(chunk_data);
        if constant_time_eq(expected.as_bytes(), got.as_bytes()) {
            self.prev_signature = expected;
            Ok(())
        } else {
            eprintln!("[sigv4-chunk] mismatch\n  expected: {}\n  got:      {}", expected, got);
            Err(AuthError::BadSignature)
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // AWS official SigV4 test: get-vanilla
    // https://docs.aws.amazon.com/general/latest/gr/sigv4-signed-request-examples.html
    #[test]
    fn signing_key_official() {
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }
}
