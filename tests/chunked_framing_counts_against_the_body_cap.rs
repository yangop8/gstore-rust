//! The framing of a chunked request body — chunk-size lines with their
//! extensions, and trailer fields — counts against the body cap (round-1
//! review of the r1-ingest-framing / r1-ingest-caps bounds): only the
//! decoded data used to be compared with the cap, so a client could send
//! megabytes of 60 KiB chunk extensions (or trailers) around a body of a few
//! bytes, all charged to the shared body budget and stretching the
//! `min_body_rate` deadline, before the request was even routed. Such a
//! body is now answered `413` once its framing exceeds the cap's allowance
//! (64 KiB plus a sixteenth of the cap).
//!
//! The client sends just past the allowance and then waits for the answer,
//! so a server that keeps reading would answer nothing until its read
//! timeout (3 s here); socket timeouts are hang guards only.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::server::{HttpOptions, Server};
use gstore::Database;

/// A running single-database server with a 1 KiB body cap; stopped when
/// dropped.
struct Single {
    server: Arc<Server>,
    addr: SocketAddr,
}

impl Drop for Single {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

fn start() -> Single {
    let db = Database::build_from_str("framing", "<http://a> <http://b> <http://c> .\n").unwrap();
    let opts = HttpOptions {
        preauth_max_body: 1024,
        max_body: 1024,
        read_timeout: Duration::from_secs(3),
        ..HttpOptions::default()
    };
    let server = Arc::new(
        Server::bind(db, "127.0.0.1:0")
            .unwrap()
            .with_http_options(opts),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    Single { server, addr }
}

/// Send `head` and then `parts`, and read the status of the first response.
fn status_after(addr: SocketAddr, parts: &[Vec<u8>]) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    s.write_all(
        b"POST /update HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/sparql-update\r\n\
          Transfer-Encoding: chunked\r\n\r\n",
    )
    .unwrap();
    for p in parts {
        if s.write_all(p).is_err() {
            break; // answered (and closed) already
        }
    }
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        match s.read(&mut byte) {
            Ok(1) => line.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&line)
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// A chunk of one data byte whose size line carries a 60 KiB extension.
fn padded_chunk() -> Vec<u8> {
    format!("1;x={}\r\nI\r\n", "e".repeat(60 << 10)).into_bytes()
}

#[test]
fn chunk_extensions_count_against_the_body_cap() {
    let srv = start();
    // Two padded chunks: 2 data bytes, 120 KiB of framing (the allowance of a
    // 1 KiB cap is 64 KiB + 64 bytes).
    let st = status_after(srv.addr, &[padded_chunk(), padded_chunk()]);
    assert_eq!(st, 413, "chunk extensions past the allowance");
    // Ordinary framing of a body within the cap is still accepted.
    let body = b"INSERT DATA { <http://x> <http://y> <http://z> }";
    let mut parts = Vec::new();
    for piece in body.chunks(8) {
        let mut chunk = format!("{:x};note=ok\r\n", piece.len()).into_bytes();
        chunk.extend_from_slice(piece);
        chunk.extend_from_slice(b"\r\n");
        parts.push(chunk);
    }
    parts.push(b"0\r\nX-Trailer: fine\r\n\r\n".to_vec());
    assert_eq!(status_after(srv.addr, &parts), 200);
}

#[test]
fn trailer_fields_count_against_the_body_cap() {
    let srv = start();
    let trailer = format!("X-Pad: {}\r\n", "t".repeat(60 << 10)).into_bytes();
    let st = status_after(
        srv.addr,
        &[b"1\r\nI\r\n0\r\n".to_vec(), trailer.clone(), trailer],
    );
    assert_eq!(st, 413, "trailers past the allowance");
}
