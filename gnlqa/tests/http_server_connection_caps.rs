//! Round-5 review follow-up of cluster-nlqa#13: a connection takes its slot
//! when it is accepted, but the rate limit and the token check see a request
//! only once it is complete. Before the per-client cap, one address that
//! opened `max_conn` connections and never finished a request held every
//! slot, and an authenticated client from another address got 503 "server
//! busy". The second address needs a client socket bound before `connect`,
//! which `std` does not offer, so the test is Linux-only (127.0.0.0/8 is
//! local there).
#![cfg(target_os = "linux")]

mod common;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::os::fd::FromRawFd;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gnlqa::{HttpServer, MockKb, MockLlm, SolveEngine};

const TOKEN: &str = "conn-cap-token";

/// A TCP connection to `to` from the local address `from` (port chosen by
/// the kernel).
fn connect_from(from: Ipv4Addr, to: SocketAddrV4) -> TcpStream {
    use std::os::raw::c_int;
    #[repr(C)]
    struct SockaddrIn {
        family: u16,
        port: [u8; 2],
        addr: [u8; 4],
        zero: [u8; 8],
    }
    extern "C" {
        fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
        fn bind(fd: c_int, addr: *const SockaddrIn, len: u32) -> c_int;
        fn connect(fd: c_int, addr: *const SockaddrIn, len: u32) -> c_int;
    }
    const AF_INET: c_int = 2;
    const SOCK_STREAM: c_int = 1;
    const SOCK_CLOEXEC: c_int = 0o2_000_000;
    let sockaddr = |ip: Ipv4Addr, port: u16| SockaddrIn {
        family: AF_INET as u16,
        port: port.to_be_bytes(),
        addr: ip.octets(),
        zero: [0; 8],
    };
    let len = std::mem::size_of::<SockaddrIn>() as u32;
    // SAFETY: plain socket calls on a descriptor this function owns; it is
    // handed to `TcpStream` (which closes it) before any early return.
    unsafe {
        let fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        let stream = TcpStream::from_raw_fd(fd);
        let local = sockaddr(from, 0);
        assert_eq!(
            bind(fd, &local, len),
            0,
            "bind {from}: {}",
            std::io::Error::last_os_error()
        );
        let remote = sockaddr(*to.ip(), to.port());
        assert_eq!(
            connect(fd, &remote, len),
            0,
            "connect {to}: {}",
            std::io::Error::last_os_error()
        );
        stream
    }
}

/// Send `raw` on `s` and read the response until the server closes.
fn exchange(mut s: TcpStream, raw: &str) -> common::Response {
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap(); // hang guard
    s.write_all(raw.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    common::parse_response(&String::from_utf8_lossy(&buf))
}

#[test]
fn idle_connections_from_one_address_do_not_lock_out_another() {
    let engine = Arc::new(SolveEngine::new(
        Box::new(MockLlm::fixed("x")),
        Box::new(MockKb::new(vec![])),
    ));
    let server = HttpServer::bind(engine, "127.0.0.1:0")
        .unwrap()
        .with_token(TOKEN)
        .with_max_conn(8);
    let SocketAddr::V4(addr) = server.local_addr().unwrap() else {
        unreachable!("bound to an IPv4 address")
    };
    thread::spawn(move || server.serve_forever());

    // The reviewer's probe: 8 connections from one address, each sending only
    // the start of a request head and then nothing.
    let flooder = Ipv4Addr::new(127, 0, 0, 1);
    let idle: Vec<TcpStream> = (0..8)
        .map(|_| {
            let mut s = connect_from(flooder, addr);
            s.write_all(b"POST /ask HTTP/1.1\r\nHost: x\r\n").unwrap();
            s
        })
        .collect();

    // The server accepts in connection order, so every idle connection has
    // been given its slot or refused before this one is looked at.
    let other = Ipv4Addr::new(127, 0, 0, 2);
    let health = exchange(
        connect_from(other, addr),
        "GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(health.status, 200, "another address is served: {health:?}");
    let body = r#"{"question":"q?"}"#;
    let ask = exchange(
        connect_from(other, addr),
        &format!(
            "POST /ask HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(
        ask.status != 503 && ask.status != 429 && ask.status != 0,
        "an authenticated request from another address is served: {ask:?}"
    );

    // The flooding address itself gets 429 at once, answered without reading
    // the request (so this client sends nothing).
    let refused = exchange(connect_from(flooder, addr), "");
    assert_eq!(refused.status, 429, "{refused:?}");
    assert!(refused.body.contains("too many connections"), "{refused:?}");
    drop(idle);
}
