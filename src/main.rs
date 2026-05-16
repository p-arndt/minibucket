// frust: a tiny, dependency-free S3-compatible object storage server.

mod hmac;
mod http;
mod md5;
mod s3;
mod sha256;
mod sigv4;
mod storage;
mod url;
mod util;

use std::env;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use crate::http::Response;
use crate::s3::{error_response, Server};
use crate::storage::Storage;

struct Config {
    bind: String,
    root: PathBuf,
    access_key: String,
    secret_key: String,
    region: String,
    anonymous: bool,
}

fn parse_args() -> Config {
    let mut cfg = Config {
        bind: "127.0.0.1:9000".to_string(),
        root: PathBuf::from("./data"),
        access_key: "minioadmin".to_string(),
        secret_key: "minioadmin".to_string(),
        region: "us-east-1".to_string(),
        anonymous: false,
    };
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => cfg.bind = args.next().unwrap_or(cfg.bind),
            "--root" => cfg.root = PathBuf::from(args.next().unwrap_or_default()),
            "--access-key" => cfg.access_key = args.next().unwrap_or(cfg.access_key),
            "--secret-key" => cfg.secret_key = args.next().unwrap_or(cfg.secret_key),
            "--region" => cfg.region = args.next().unwrap_or(cfg.region),
            "--anonymous" => cfg.anonymous = true,
            "--help" | "-h" => {
                println!("frust — minimal S3-compatible server\n");
                println!("Usage: frust [--bind ADDR] [--root DIR] [--access-key K] [--secret-key S] [--region R] [--anonymous]");
                println!("Defaults: --bind 127.0.0.1:9000 --root ./data --access-key minioadmin --secret-key minioadmin --region us-east-1");
                std::process::exit(0);
            }
            _ => {
                eprintln!("unknown arg: {}", a);
                std::process::exit(2);
            }
        }
    }
    cfg
}

fn main() {
    let cfg = parse_args();
    let storage = Storage::new(cfg.root.clone()).expect("init storage");
    let server = Arc::new(Server {
        storage,
        access_key: cfg.access_key.clone(),
        secret_key: cfg.secret_key.clone(),
        require_auth: !cfg.anonymous,
        region: cfg.region.clone(),
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
        eprintln!("  access-key: {}", cfg.access_key);
        eprintln!("  secret-key: {}", cfg.secret_key);
        eprintln!("  region:     {}", cfg.region);
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

    // Keep a separate handle for writing responses; the request owns a
    // BufReader<TcpStream> for reading the body.
    let mut sock = stream.try_clone()?;
    let req = match crate::http::read_request(stream) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[parse] {:?} {}", peer, e);
            return Ok(());
        }
    };

    eprintln!(
        "[req] {:?} {} {} (q={})",
        peer, req.method, req.raw_path, req.query_raw
    );

    if srv.require_auth {
        match crate::sigv4::parse_authorization(&req.headers) {
            Ok(info) => {
                if info.access_key != srv.access_key {
                    return error_response(
                        &mut sock,
                        403,
                        "InvalidAccessKeyId",
                        "The access key does not match",
                        &crate::util::request_id(),
                        &req.path,
                    );
                }
                if let Err(e) = crate::sigv4::verify(
                    &req.method,
                    &req.raw_path,
                    &req.query_raw,
                    &req.headers,
                    &srv.secret_key,
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

    if let Err(e) = crate::s3::dispatch(&srv, req, &mut sock) {
        eprintln!("[handler] {}", e);
        let resp = Response::new(500).header("Connection", "close");
        let _ = resp.write_headers(&mut sock, Some(0));
    }
    let _ = sock.flush();
    Ok(())
}
