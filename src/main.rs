// frust: a tiny, dependency-free S3-compatible object storage server.

mod creds;
mod hmac;
mod http;
mod md5;
mod multipart;
mod s3;
mod sha256;
mod sigv4;
mod storage;
mod tagging;
mod url;
mod util;

use std::env;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use crate::creds::Credentials;
use crate::http::Response;
use crate::s3::{error_response, Server};
use crate::storage::Storage;

struct Config {
    bind: String,
    root: PathBuf,
    creds: Credentials,
    region: String,
    anonymous: bool,
    domain: Option<String>,
}

fn parse_args() -> Config {
    let mut cfg = Config {
        bind: "127.0.0.1:9000".to_string(),
        root: PathBuf::from("./data"),
        creds: Credentials::new(),
        region: "us-east-1".to_string(),
        anonymous: false,
        domain: None,
    };
    let mut pending_access: Option<String> = None;
    let mut pending_secret: Option<String> = None;
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => cfg.bind = args.next().unwrap_or(cfg.bind),
            "--root" => cfg.root = PathBuf::from(args.next().unwrap_or_default()),
            "--access-key" => pending_access = args.next(),
            "--secret-key" => pending_secret = args.next(),
            "--credentials" => {
                let p = args.next().unwrap_or_default();
                let c = Credentials::load_file(std::path::Path::new(&p))
                    .unwrap_or_else(|e| {
                        eprintln!("failed to load credentials from {}: {}", p, e);
                        std::process::exit(2);
                    });
                for (k, v) in c.map {
                    cfg.creds.add(&k, &v);
                }
            }
            "--region" => cfg.region = args.next().unwrap_or(cfg.region),
            "--domain" => cfg.domain = args.next(),
            "--anonymous" => cfg.anonymous = true,
            "--help" | "-h" => {
                println!("frust — minimal S3-compatible server\n");
                println!("Usage: frust [options]");
                println!("  --bind ADDR              default 127.0.0.1:9000");
                println!("  --root DIR               default ./data");
                println!("  --access-key K           access key id (use with --secret-key)");
                println!("  --secret-key S           secret key (must follow --access-key)");
                println!("  --credentials FILE       load multiple KEY=SECRET lines");
                println!("  --region R               default us-east-1");
                println!("  --domain D               enable virtual-hosted addressing for bucket.D");
                println!("  --anonymous              disable auth (dev only)");
                std::process::exit(0);
            }
            _ => {
                eprintln!("unknown arg: {}", a);
                std::process::exit(2);
            }
        }
        if let (Some(a), Some(s)) = (pending_access.clone(), pending_secret.clone()) {
            cfg.creds.add(&a, &s);
            pending_access = None;
            pending_secret = None;
        }
    }
    if pending_access.is_some() || pending_secret.is_some() {
        eprintln!("--access-key and --secret-key must be provided together");
        std::process::exit(2);
    }
    if !cfg.anonymous && cfg.creds.is_empty() {
        // Default dev credential.
        cfg.creds.add("minioadmin", "minioadmin");
    }
    cfg
}

fn main() {
    let cfg = parse_args();
    let storage = Storage::new(cfg.root.clone()).expect("init storage");
    let server = Arc::new(Server {
        storage,
        credentials: cfg.creds.clone(),
        require_auth: !cfg.anonymous,
        region: cfg.region.clone(),
        domain: cfg.domain.clone(),
    });

    let listener = TcpListener::bind(&cfg.bind).expect("bind");
    eprintln!(
        "frust listening on http://{} (root: {})",
        cfg.bind,
        cfg.root.display()
    );
    if cfg.anonymous {
        eprintln!("  anonymous mode (no auth required)");
    } else {
        eprintln!("  region: {}", cfg.region);
        for k in cfg.creds.map.keys() {
            eprintln!("  access-key: {}", k);
        }
        if let Some(d) = &cfg.domain {
            eprintln!("  virtual-hosted domain: *.{}", d);
        }
    }

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let srv = Arc::clone(&server);
                thread::spawn(move || {
                    if let Err(e) = handle(srv, s) {
                        eprintln!("[conn] {}", e);
                    }
                });
            }
            Err(e) => eprintln!("accept: {}", e),
        }
    }
}

fn handle(srv: Arc<Server>, stream: TcpStream) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let peer = stream.peer_addr().ok();

    let mut sock = stream.try_clone()?;
    let req = match crate::http::read_request(stream) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[parse] {:?} {}", peer, e);
            return Ok(());
        }
    };

    eprintln!(
        "[req] {:?} {} {} (q={}) host={}",
        peer,
        req.method,
        req.raw_path,
        req.query_raw,
        req.headers.get("host").unwrap_or("-")
    );

    let mut chunk_ctx: Option<crate::sigv4::ChunkContext> = None;

    if srv.require_auth {
        match crate::sigv4::parse_authorization(&req.headers) {
            Ok(info) => {
                let secret = match srv.credentials.secret_for(&info.access_key) {
                    Some(s) => s.to_string(),
                    None => {
                        return error_response(
                            &mut sock,
                            403,
                            "InvalidAccessKeyId",
                            "Unknown access key",
                            &crate::util::request_id(),
                            &req.path,
                        );
                    }
                };
                if let Err(e) = crate::sigv4::verify(
                    &req.method,
                    &req.raw_path,
                    &req.query_raw,
                    &req.headers,
                    &secret,
                    &info,
                ) {
                    eprintln!("[auth] verify failed: {:?}", e);
                    return error_response(
                        &mut sock,
                        403,
                        "SignatureDoesNotMatch",
                        "The signature does not match",
                        &crate::util::request_id(),
                        &req.path,
                    );
                }
                // Build chunk-signing context for streaming PUTs.
                if info.payload_hash == "STREAMING-AWS4-HMAC-SHA256-PAYLOAD" {
                    chunk_ctx = Some(crate::sigv4::ChunkContext::new(&secret, &info));
                }
            }
            Err(crate::sigv4::AuthError::Missing) => {
                return error_response(
                    &mut sock,
                    403,
                    "AccessDenied",
                    "Authorization required",
                    &crate::util::request_id(),
                    &req.path,
                );
            }
            Err(e) => {
                eprintln!("[auth] malformed: {:?}", e);
                return error_response(
                    &mut sock,
                    400,
                    "InvalidRequest",
                    "Malformed Authorization header",
                    &crate::util::request_id(),
                    &req.path,
                );
            }
        }
    }

    if let Err(e) = crate::s3::dispatch(&srv, req, &mut sock, chunk_ctx) {
        eprintln!("[handler] {}", e);
        let resp = Response::new(500).header("Connection", "close");
        let _ = resp.write_headers(&mut sock, Some(0));
    }
    let _ = sock.flush();
    Ok(())
}
