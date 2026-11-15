//! The HTTP connection layer against adversarial clients (review H-1 and the
//! framing findings): what one client may make a worker of the bounded pool
//! wait for, how many connections one address may hold, and which request
//! framings are refused.
//!
//! * **Deadlines** (sec#3, http#1, robust#3, http#3): a request body must
//!   arrive at `min_body_rate` on average after a grace of `read_timeout`
//!   (the allowance is earned by the bytes received, whatever length was
//!   declared), and a TLS handshake and request head within the head
//!   deadline (`read_timeout`), however the bytes trickle in. Each
//!   test runs a one-worker server, lets a client drip its request one byte at
//!   a time, and checks that the server lets go of that connection and serves
//!   a concurrent request on its only worker.
//! * **Per-address cap** (sec#3, http#1): `max_conns_per_ip` connections from
//!   one address at most; a further one is closed while other addresses are
//!   served.
//! * **Framing** (sec#16, http#14; RFC 9112 §5, §6, §7.1): conflicting or
//!   unsupported `Content-Length` / `Transfer-Encoding`, malformed header
//!   lines and chunk sizes are answered `400` and the connection is closed.
//! * **Control characters** (robust#14): a header line with a bare CR, NUL or
//!   another control character (HTAB excepted) is refused, so it can never be
//!   reflected into a response header.
//!
//! The only timeout is [`HANG_GUARD`]; no assertion depends on elapsed time.

use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::net::Ipv4Addr;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use gstore::http_api::ApiServer;
use gstore::server::{HttpOptions, Server, ShutdownReport};
use gstore::Database;

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";

/// The only timeout of these tests: a hang guard, never what they assert.
const HANG_GUARD: Duration = Duration::from_secs(30);

/// The pause between two bytes of a dripping client — every single read the
/// server makes is answered well within its read timeout.
const DRIP: Duration = Duration::from_millis(40);

/// The read timeout of the servers the dripping clients talk to.
const READ_TIMEOUT: Duration = Duration::from_millis(300);

const GET_STATUS: &str = "GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

// --- servers -----------------------------------------------------------------

/// A [`Server`] serving on a thread; shut down (and joined) when dropped.
struct Running {
    server: Arc<Server>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<ShutdownReport>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(t) = self.serving.take() {
            let _ = t.join();
        }
    }
}

fn server(opts: HttpOptions) -> (Arc<Server>, SocketAddr) {
    let db = Database::build_from_str("framing", DATA).unwrap();
    let server = Server::bind(db, "127.0.0.1:0")
        .unwrap()
        .with_http_options(opts)
        .with_shutdown_grace(Duration::from_millis(200));
    let addr = server.local_addr().unwrap();
    (Arc::new(server), addr)
}

fn serve(opts: HttpOptions) -> Running {
    let (server, addr) = server(opts);
    let s2 = Arc::clone(&server);
    Running {
        server,
        addr,
        serving: Some(thread::spawn(move || s2.serve_forever())),
    }
}

/// One worker, a short read timeout, everything else default (the default
/// `min_body_rate` included).
fn one_worker() -> HttpOptions {
    HttpOptions {
        threads: 1,
        read_timeout: READ_TIMEOUT,
        ..HttpOptions::default()
    }
}

/// A temporary directory, removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore-framing-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// --- clients -----------------------------------------------------------------

/// Write `raw` on a fresh connection and read until the server closes it (at
/// most [`HANG_GUARD`]); a reset or a timeout ends the reading too. Returns
/// what was received.
fn exchange(mut s: TcpStream, raw: &[u8]) -> String {
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    // A refused connection may already be closed: what it answers decides.
    let _ = s.write_all(raw);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&got).into_owned()
}

fn http(addr: SocketAddr, raw: &[u8]) -> String {
    exchange(TcpStream::connect(addr).unwrap(), raw)
}

/// What a dripping client saw.
#[derive(Debug)]
struct Dripped {
    /// Bytes of `body` it sent.
    sent: usize,
    /// The server closed the connection before the client sent everything
    /// (or gave up at the hang guard).
    let_go: bool,
}

/// Write `head`, then `body` one byte per [`DRIP`] — telling `started` once
/// the first body byte is out — until the server closes the connection (EOF
/// or a reset; what it sends before, e.g. a `400` or TLS session tickets, is
/// ignored), everything was sent, or [`HANG_GUARD`] has passed.
fn drip(mut s: impl Read + Write, head: &[u8], body: &[u8], started: mpsc::Sender<()>) -> Dripped {
    s.write_all(head).unwrap();
    s.flush().unwrap();
    let t0 = Instant::now();
    let mut started = Some(started);
    for (i, b) in body.iter().enumerate() {
        if s.write_all(&[*b]).and_then(|()| s.flush()).is_err() {
            return Dripped {
                sent: i,
                let_go: true,
            };
        }
        if let Some(tx) = started.take() {
            let _ = tx.send(());
        }
        // The read's timeout (DRIP, set by the caller) is the pause between
        // two bytes; EOF or an error other than that timeout means the server
        // let go.
        let mut buf = [0u8; 1024];
        match s.read(&mut buf) {
            Ok(n) if n > 0 => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            _ => {
                return Dripped {
                    sent: i + 1,
                    let_go: true,
                }
            }
        }
        if t0.elapsed() > HANG_GUARD {
            break;
        }
    }
    Dripped {
        sent: body.len(),
        let_go: false,
    }
}

/// A TCP connection whose reads wait at most [`DRIP`] (the dripping pace).
fn dripping_conn(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(DRIP)).unwrap();
    s
}

/// Start a dripping client on its own thread; returns once its connection
/// was made and its first body byte is out (so a connection made afterwards
/// queues behind it for the one worker).
fn start_dripper(
    conn: impl Read + Write + Send + 'static,
    head: Vec<u8>,
    body: Vec<u8>,
) -> thread::JoinHandle<Dripped> {
    let (tx, rx) = mpsc::channel();
    let t = thread::spawn(move || drip(conn, &head, &body, tx));
    rx.recv_timeout(HANG_GUARD)
        .expect("the dripping client sent its first body byte");
    t
}

fn assert_let_go(dripper: thread::JoinHandle<Dripped>, total: usize) {
    let d = dripper.join().unwrap();
    assert!(
        d.let_go && d.sent < total,
        "the server must close the dripping connection before its {total} bytes arrive: {d:?}"
    );
}

// --- the request body deadline -------------------------------------------------

fn post_head(len: usize) -> Vec<u8> {
    format!(
        "POST /sparql HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/sparql-query\r\n\
         Content-Length: {len}\r\n\r\n"
    )
    .into_bytes()
}

/// sec#3 / http#1 / robust#3: a body trickling in (each byte far within the
/// read timeout) no longer holds the only worker: the connection is closed
/// once `read_timeout` plus what the bytes received earned at
/// `min_body_rate` has passed, and the request queued behind it is served.
/// (Before: the worker waited for all 1000 bytes, 40 s, and the queued
/// request with it.)
#[test]
fn a_dripped_body_is_cut_off_and_the_worker_serves_the_next_request() {
    let srv = serve(one_worker());
    let body = vec![b' '; 1000];
    let dripper = start_dripper(dripping_conn(srv.addr), post_head(body.len()), body);
    let resp = http(srv.addr, GET_STATUS.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");
    assert_let_go(dripper, 1000);
}

/// robust#3's scenario: a client declaring a large body (64 MiB, far within
/// `max_body`) and dripping it gains nothing by the size it announces — the
/// allowance grows with the bytes actually received, not with the declared
/// length, so the connection is cut off after `read_timeout` plus what the
/// few bytes sent earned. (Before: a declared-length allowance gave it
/// `read_timeout + 64 MiB / 64 KiB/s`, over 17 minutes; with `Content-Length:
/// 1 GiB` against gserver's default `upload_max_body_size`, over 4 hours.)
#[test]
fn a_dripped_body_gains_nothing_by_declaring_a_large_length() {
    let srv = serve(one_worker());
    let dripper = start_dripper(
        dripping_conn(srv.addr),
        post_head(64 << 20),
        vec![b' '; 1000],
    );
    let resp = http(srv.addr, GET_STATUS.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");
    assert_let_go(dripper, 1000);
}

/// The same for a chunked body, whose allowance grows with the bytes
/// received: it must keep up `min_body_rate` on average.
#[test]
fn a_dripped_chunked_body_is_cut_off_too() {
    let srv = serve(one_worker());
    let head =
        b"POST /sparql HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/sparql-query\r\n\
                 Transfer-Encoding: chunked\r\n\r\n3e8\r\n"
            .to_vec();
    let dripper = start_dripper(dripping_conn(srv.addr), head, vec![b' '; 1000]);
    let resp = http(srv.addr, GET_STATUS.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");
    assert_let_go(dripper, 1000);
}

/// The upstream-compatible `/api` server shares the connection layer: a body
/// dripped to `/api` cannot hold its only worker either.
#[test]
fn a_dripped_api_body_cannot_hold_the_api_server() {
    let dir = TmpDir::new("api");
    let api = ApiServer::bind(&dir.0, "Framing-root-1", "127.0.0.1:0")
        .unwrap()
        .with_http_options(one_worker());
    let addr = api.local_addr().unwrap();
    let api = Arc::new(api);
    let a2 = Arc::clone(&api);
    let serving = thread::spawn(move || a2.serve_forever());

    let body = vec![b' '; 1000];
    let head = format!(
        "POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    let dripper = start_dripper(dripping_conn(addr), head.into_bytes(), body);
    let resp = http(addr, GET_STATUS.as_bytes());
    assert!(
        resp.starts_with("HTTP/1.1 "),
        "the queued request was answered: {resp:?}"
    );
    assert_let_go(dripper, 1000);
    api.shutdown();
    let _ = serving.join();
}

/// The deadline is a floor on the rate, not a cap on the size: a large body
/// sent at full speed, and a small one sent slowly within its allowance, are
/// both served.
#[test]
fn bodies_within_their_allowance_are_served() {
    let srv = serve(HttpOptions {
        threads: 1,
        read_timeout: Duration::from_secs(5),
        ..HttpOptions::default()
    });
    // 256 KiB in one go: in long before its deadline (5 s + 4 s).
    let mut big = b"ASK {} #".to_vec();
    big.resize(256 * 1024, b'x');
    let mut raw = post_head(big.len());
    raw.splice(
        raw.len() - 2..,
        b"Connection: close\r\n\r\n".iter().copied(),
    );
    raw.extend_from_slice(&big);
    let resp = http(srv.addr, &raw);
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");

    // 5 bytes, one every 40 ms (200 ms in all): within the 5 s allowance.
    let mut s = TcpStream::connect(srv.addr).unwrap();
    let mut head = post_head(5);
    head.splice(
        head.len() - 2..,
        b"Connection: close\r\n\r\n".iter().copied(),
    );
    s.write_all(&head).unwrap();
    for b in b"ASK{}" {
        thread::sleep(DRIP); // pacing, not synchronisation
        s.write_all(&[*b]).unwrap();
    }
    let resp = exchange(s, b"");
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");
}

/// The allowance is earned as the body arrives: a body that keeps up the
/// rate is served even though, as a whole, it takes several times the read
/// timeout — the deadline is a rate floor, not a fixed cap.
#[test]
fn a_body_that_keeps_up_the_rate_may_outlast_the_read_timeout() {
    let srv = serve(HttpOptions {
        threads: 1,
        read_timeout: Duration::from_secs(1),
        min_body_rate: 1000,
        ..HttpOptions::default()
    });
    // 3000 bytes in 30 slices, one every 50 ms (2000 bytes/s, 1.5 s in
    // all): every slice extends the deadline by 100 ms, so it stays at
    // least the 1 s grace ahead of the client.
    let mut body = b"ASK {} #".to_vec();
    body.resize(3000, b'x');
    let mut s = TcpStream::connect(srv.addr).unwrap();
    let mut head = post_head(body.len());
    head.splice(
        head.len() - 2..,
        b"Connection: close\r\n\r\n".iter().copied(),
    );
    s.write_all(&head).unwrap();
    for slice in body.chunks(100) {
        thread::sleep(Duration::from_millis(50)); // pacing, not synchronisation
        s.write_all(slice).unwrap();
    }
    let resp = exchange(s, b"");
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");
}

// --- the per-address connection cap ---------------------------------------------

/// A TCP connection to `to` from the local address `from` (another loopback
/// address stands for another client).
#[cfg(target_os = "linux")]
fn connect_from(from: Ipv4Addr, to: SocketAddr) -> TcpStream {
    use std::os::fd::FromRawFd;

    #[repr(C)]
    struct SockAddrIn {
        family: u16,
        port: u16,
        addr: [u8; 4],
        zero: [u8; 8],
    }
    extern "C" {
        fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
        fn bind(fd: i32, addr: *const SockAddrIn, len: u32) -> i32;
        fn connect(fd: i32, addr: *const SockAddrIn, len: u32) -> i32;
        fn close(fd: i32) -> i32;
    }
    const AF_INET: i32 = 2;
    const SOCK_STREAM: i32 = 1;
    const SOCK_CLOEXEC: i32 = 0o2000000;
    let SocketAddr::V4(to) = to else {
        panic!("an IPv4 server address: {to}")
    };
    let sockaddr = |ip: Ipv4Addr, port: u16| SockAddrIn {
        family: AF_INET as u16,
        port: port.to_be(),
        addr: ip.octets(),
        zero: [0; 8],
    };
    let len = std::mem::size_of::<SockAddrIn>() as u32;
    // SAFETY: plain socket calls on a descriptor this function owns; it is
    // closed on failure and handed to `TcpStream` (which closes it) on success.
    unsafe {
        let fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
        assert!(fd >= 0, "socket: {}", io::Error::last_os_error());
        let me = sockaddr(from, 0);
        let server = sockaddr(*to.ip(), to.port());
        if bind(fd, &me, len) != 0 || connect(fd, &server, len) != 0 {
            let e = io::Error::last_os_error();
            close(fd);
            panic!("connecting from {from} to {to}: {e}");
        }
        TcpStream::from_raw_fd(fd)
    }
}

/// sec#3 / http#1: with `max_conns_per_ip = 2`, a third concurrent
/// connection from one address is closed unanswered while another address is
/// served; once one of the two ends, the address may connect again.
#[cfg(target_os = "linux")]
#[test]
fn the_per_address_cap_refuses_only_the_address_over_it() {
    let srv = serve(HttpOptions {
        threads: 4,
        max_conns_per_ip: 2,
        ..HttpOptions::default()
    });
    let me = Ipv4Addr::new(127, 0, 0, 1);
    let other = Ipv4Addr::new(127, 0, 0, 2);
    // Two connections from 127.0.0.1 stay open, each halfway through a
    // request head (made before the third one, so accepted before it).
    let mut held: Vec<TcpStream> = (0..2)
        .map(|_| {
            let mut s = connect_from(me, srv.addr);
            s.write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n")
                .unwrap();
            s
        })
        .collect();
    let third = exchange(connect_from(me, srv.addr), GET_STATUS.as_bytes());
    assert!(
        !third.starts_with("HTTP/1.1"),
        "a third connection from 127.0.0.1 is refused: {third:?}"
    );
    let resp = exchange(connect_from(other, srv.addr), GET_STATUS.as_bytes());
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "127.0.0.2 is served: {resp:?}"
    );

    // One held connection finishes its request and closes: the server
    // released its place before the client saw the end of the response.
    let first = held.remove(0);
    let resp = exchange(first, b"Connection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");
    let resp = exchange(connect_from(me, srv.addr), GET_STATUS.as_bytes());
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "127.0.0.1 may connect again: {resp:?}"
    );
    drop(held);
}

// --- framing (RFC 9112) ---------------------------------------------------------

/// A request the next one is appended to, as a front end that frames the
/// first one differently would forward it on a shared connection.
const SMUGGLED: &str = "GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n";

fn responses(resp: &str) -> usize {
    resp.matches("HTTP/1.1 ").count()
}

/// sec#16 / http#14: every ambiguous or unsupported framing is answered with
/// one `400` and the connection is closed — nothing after it (a smuggled
/// request) is served.
#[test]
fn ambiguous_framing_is_refused_and_the_connection_closed() {
    let srv = serve(HttpOptions::default());
    let p =
        "POST /sparql HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/sparql-query\r\n";
    let cases: Vec<(&str, String)> = vec![
        // Content-Length and Transfer-Encoding together (RFC 9112 §6.1).
        ("CL+TE", format!("{p}Content-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n{SMUGGLED}")),
        ("TE+CL", format!("{p}Transfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n0\r\n\r\n{SMUGGLED}")),
        ("TE xchunked+CL", format!(
            "{p}Content-Length: {}\r\nTransfer-Encoding: xchunked\r\n\r\n0\r\n\r\n{SMUGGLED}",
            5 + SMUGGLED.len()
        )),
        // Differing Content-Length values (RFC 9110 §8.6).
        ("dup CL", format!("{p}Content-Length: {}\r\nContent-Length: 0\r\n\r\n{SMUGGLED}", SMUGGLED.len())),
        ("CL list", format!("{p}Content-Length: 5, 6\r\n\r\nASK{{}}")),
        ("CL list of equal values", format!("{p}Content-Length: 5, 5\r\n\r\nASK{{}}")),
        // A Content-Length that is not plain decimal digits.
        ("CL +5", format!("{p}Content-Length: +5\r\n\r\nASK{{}}")),
        ("CL -5", format!("{p}Content-Length: -5\r\n\r\nASK{{}}")),
        ("CL 0x5", format!("{p}Content-Length: 0x5\r\n\r\nASK{{}}")),
        ("CL 5abc", format!("{p}Content-Length: 5abc\r\n\r\nASK{{}}")),
        ("CL empty", format!("{p}Content-Length: \r\n\r\nASK{{}}")),
        // A transfer coding other than exactly `chunked` (RFC 9112 §6.1, §6.3).
        ("TE gzip", format!("{p}Transfer-Encoding: gzip\r\n\r\n5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("TE gzip, chunked", format!("{p}Transfer-Encoding: gzip, chunked\r\n\r\n5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("TE chunked, gzip", format!("{p}Transfer-Encoding: chunked, gzip\r\n\r\n5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("TE chunked twice", format!("{p}Transfer-Encoding: chunked, chunked\r\n\r\n5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("TE identity", format!("{p}Transfer-Encoding: identity\r\n\r\n")),
        ("TE chunked + TE identity", format!(
            "{p}Transfer-Encoding: chunked\r\nTransfer-Encoding: identity\r\n\r\n5\r\nASK{{}}\r\n0\r\n\r\n"
        )),
        ("TE in HTTP/1.0", "POST /sparql HTTP/1.0\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nASK{}\r\n0\r\n\r\n".to_string()),
        // Chunk sizes that are not bare hex digits (RFC 9112 §7.1).
        ("chunk +5", format!("{p}Transfer-Encoding: chunked\r\n\r\n+5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("chunk -5", format!("{p}Transfer-Encoding: chunked\r\n\r\n-5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("chunk 0x5", format!("{p}Transfer-Encoding: chunked\r\n\r\n0x5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("chunk ' 5'", format!("{p}Transfer-Encoding: chunked\r\n\r\n 5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("chunk empty", format!("{p}Transfer-Encoding: chunked\r\n\r\n;x\r\nASK{{}}\r\n0\r\n\r\n")),
        // Malformed header lines (RFC 9112 §5.1, §5.2).
        ("space before colon", format!("{p}Content-Length : 5\r\n\r\nASK{{}}")),
        ("tab before colon", format!("{p}Content-Length\t: 5\r\n\r\nASK{{}}")),
        ("no colon", format!("{p}X-Flag\r\nContent-Length: 5\r\n\r\nASK{{}}")),
        ("empty name", format!("{p}: x\r\nContent-Length: 5\r\n\r\nASK{{}}")),
        ("space in name", format!("{p}X Flag: 1\r\nContent-Length: 5\r\n\r\nASK{{}}")),
        ("obs-fold", format!(
            "{p}Content-Length: {}\r\n Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n{SMUGGLED}",
            5 + SMUGGLED.len()
        )),
    ];
    for (name, raw) in &cases {
        let resp = http(srv.addr, raw.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 400"), "{name}: {resp:?}");
        assert_eq!(
            responses(&resp),
            1,
            "{name}: one response, then closed: {resp:?}"
        );
    }
}

/// The legitimate neighbours of the refused framings are still served.
#[test]
fn well_formed_framing_is_served() {
    let srv = serve(HttpOptions::default());
    let p =
        "POST /sparql HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/sparql-query\r\n\
             Connection: close\r\n";
    let cases: Vec<(&str, String)> = vec![
        ("CL", format!("{p}Content-Length: 5\r\n\r\nASK{{}}")),
        ("identical dup CL", format!("{p}Content-Length: 5\r\nContent-Length: 5\r\n\r\nASK{{}}")),
        ("TE chunked", format!("{p}Transfer-Encoding: chunked\r\n\r\n5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("TE Chunked", format!("{p}Transfer-Encoding: Chunked\r\n\r\n5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("TE chunked,", format!("{p}Transfer-Encoding: chunked, \r\n\r\n5\r\nASK{{}}\r\n0\r\n\r\n")),
        ("chunk extensions", format!(
            "{p}Transfer-Encoding: chunked\r\n\r\n2;a=1\r\nAS\r\n003 ;b\r\nK{{}}\r\n0\r\nX-T: t\r\n\r\n"
        )),
        ("upper-case hex", format!("{p}Transfer-Encoding: chunked\r\n\r\nA\r\nASK{{}}     \r\n0\r\n\r\n")),
        ("LF line ends", "POST /sparql HTTP/1.1\nHost: localhost\nContent-Type: application/sparql-query\n\
                          Connection: close\nContent-Length: 5\n\nASK{}"
            .to_string()),
        ("HTAB in a value", format!("{p}X-Note: a\tb\r\nContent-Length: 5\r\n\r\nASK{{}}")),
        ("no space after colon", format!("{p}Content-Length:5\r\n\r\nASK{{}}")),
    ];
    for (name, raw) in &cases {
        let resp = http(srv.addr, raw.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 200"), "{name}: {resp:?}");
        assert!(
            resp.contains("\"boolean\""),
            "{name}: the ASK ran: {resp:?}"
        );
    }
}

// --- control characters in header lines (robust#14) ------------------------------

/// A header line with a bare CR, NUL or another control character (HTAB
/// excepted) is refused with `400`, the response carries no bare CR, and
/// the connection is closed.
#[test]
fn control_characters_in_header_lines_are_refused() {
    let srv = serve(HttpOptions::default());
    let cases: Vec<(&str, &[u8])> = vec![
        // A CORS preflight used to reflect this value into a response header.
        ("bare CR (preflight)", b"OPTIONS /sparql HTTP/1.1\r\nHost: localhost\r\nOrigin: http://w\r\n\
           Access-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: a\rX-Evil: 1\r\n\r\n"),
        ("bare CR", b"GET /status HTTP/1.1\r\nHost: localhost\r\nX-A: a\rb\r\n\r\n"),
        ("CR CR LF", b"GET /status HTTP/1.1\r\nHost: localhost\r\nX-A: a\r\r\n\r\n"),
        ("NUL", b"GET /status HTTP/1.1\r\nHost: localhost\r\nX-A: a\0b\r\n\r\n"),
        ("SOH", b"GET /status HTTP/1.1\r\nHost: localhost\r\nX-A: a\x01b\r\n\r\n"),
        ("ESC", b"GET /status HTTP/1.1\r\nHost: localhost\r\nX-A: a\x1b[31mb\r\n\r\n"),
        ("DEL", b"GET /status HTTP/1.1\r\nHost: localhost\r\nX-A: a\x7fb\r\n\r\n"),
        ("NUL in a name", b"GET /status HTTP/1.1\r\nHost: localhost\r\nX\0A: b\r\n\r\n"),
        ("bare CR in the request line", b"GET /status\rX HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        ("NUL in a trailer", b"POST /sparql HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/sparql-query\r\n\
           Transfer-Encoding: chunked\r\n\r\n5\r\nASK{}\r\n0\r\nX-T: a\0b\r\n\r\n"),
    ];
    for (name, raw) in &cases {
        let resp = http(srv.addr, raw);
        assert!(resp.starts_with("HTTP/1.1 400"), "{name}: {resp:?}");
        assert_eq!(
            responses(&resp),
            1,
            "{name}: one response, then closed: {resp:?}"
        );
        let bare_cr = resp
            .as_bytes()
            .windows(2)
            .any(|w| w[0] == b'\r' && w[1] != b'\n');
        assert!(!bare_cr, "{name}: no bare CR in the response: {resp:?}");
    }
}

// --- TLS: the handshake and the head under the head deadline (http#3) -----------

#[cfg(feature = "tls")]
mod tls {
    use super::*;

    /// A self-signed EC end-entity cert (CN=localhost, SAN DNS:localhost + IP
    /// 127.0.0.1) and its key — the pair of the `server.rs` TLS tests.
    const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBvTCCAWOgAwIBAgIUKHclqAxNOvVOQ3kWyfFT+rCJUHwwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDcwMjIwMjkyMFoXDTM2MDYyOTIw
MjkyMFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D
AQcDQgAEYHEfrA2tZnM3sdpdua9aH0oSv8G2EmRrc10JlY7xgVO8+6VoyIJqtvH5
AxyPvu6UfMZMa+/UFLAmk+WCYEKCi6OBkjCBjzAdBgNVHQ4EFgQUJOZ8AYKC5S5i
QJhbdtMpHgmjlaowHwYDVR0jBBgwFoAUJOZ8AYKC5S5iQJhbdtMpHgmjlaowGgYD
VR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/
BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMAoGCCqGSM49BAMCA0gAMEUCIQCo
ndoh3KosHMemvMMbjxtaNbbXmxGjfsgme3rBkLuziQIgFeUx/cU+Fpw/Dl5McuKP
cCWfronEM/ASghi4/9ldMUI=
-----END CERTIFICATE-----
";
    const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgUBOdoa9/azwl851s
NPlNYkOXVnBaUKf3fk8J38AOaYehRANCAARgcR+sDa1mczex2l25r1ofShK/wbYS
ZGtzXQmVjvGBU7z7pWjIgmq28fkDHI++7pR8xkxr79QUsCaT5YJgQoKL
-----END PRIVATE KEY-----
";

    /// A one-worker HTTPS server; shut down when the returned value drops.
    fn serve_tls() -> Running {
        let dir = TmpDir::new("tls");
        let (cert, key) = (dir.0.join("cert.pem"), dir.0.join("key.pem"));
        std::fs::write(&cert, CERT_PEM).unwrap();
        std::fs::write(&key, KEY_PEM).unwrap();
        let config = gstore::server::load_tls_config(&cert, &key).unwrap();
        drop(dir);
        let (server, addr) = server(one_worker());
        let s2 = Arc::clone(&server);
        Running {
            server,
            addr,
            serving: Some(thread::spawn(move || s2.serve_forever_tls(config))),
        }
    }

    fn client() -> rustls::ClientConnection {
        let mut roots = rustls::RootCertStore::empty();
        let cert = rustls_pemfile::certs(&mut io::BufReader::new(CERT_PEM.as_bytes()))
            .next()
            .unwrap()
            .unwrap();
        roots.add(cert).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        rustls::ClientConnection::new(Arc::new(config), name).unwrap()
    }

    /// `GET /status` over a fresh TLS connection (at most [`HANG_GUARD`]).
    fn https_status(addr: SocketAddr) -> String {
        let sock = TcpStream::connect(addr).unwrap();
        sock.set_read_timeout(Some(HANG_GUARD)).unwrap();
        let mut tls = rustls::StreamOwned::new(client(), sock);
        if tls.write_all(GET_STATUS.as_bytes()).is_err() {
            return String::new();
        }
        let mut resp = Vec::new();
        // No close_notify from the server: rustls reports an EOF error after
        // delivering everything; what was decrypted decides.
        let _ = tls.read_to_end(&mut resp);
        String::from_utf8_lossy(&resp).into_owned()
    }

    /// A completed TLS handshake over a dripping connection, and the TLS
    /// records carrying each of `parts` (encrypted in turn, each in records
    /// of its own) — for the caller to send or drip.
    fn handshaken(addr: SocketAddr, parts: &[&[u8]]) -> (TcpStream, Vec<Vec<u8>>) {
        let mut sock = TcpStream::connect(addr).unwrap();
        sock.set_read_timeout(Some(HANG_GUARD)).unwrap();
        let mut conn = client();
        while conn.is_handshaking() {
            conn.complete_io(&mut sock).unwrap();
        }
        let wires = parts
            .iter()
            .map(|plain| {
                conn.writer().write_all(plain).unwrap();
                let mut wire = Vec::new();
                while conn.wants_write() {
                    conn.write_tls(&mut wire).unwrap();
                }
                wire
            })
            .collect();
        sock.set_read_timeout(Some(DRIP)).unwrap();
        (sock, wires)
    }

    /// http#3: a ClientHello trickling in (each byte far within the read
    /// timeout) no longer holds the only worker: the handshake runs under the
    /// head deadline, so the connection is closed and the queued HTTPS
    /// request is served. (Before: rustls looped over the socket reads inside
    /// one deadline-checked read, and the worker waited for the whole 16 KiB
    /// record.)
    #[test]
    fn a_dripped_tls_handshake_cannot_hold_the_worker() {
        let srv = serve_tls();
        // A handshake record header announcing a 16 KiB ClientHello.
        let head = vec![0x16, 0x03, 0x01, 0x40, 0x00];
        let dripper = start_dripper(dripping_conn(srv.addr), head, vec![1u8; 0x4000]);
        let resp = https_status(srv.addr);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");
        assert_let_go(dripper, 0x4000);
    }

    /// http#3: after the handshake, a request head whose TLS record trickles
    /// in is cut off at the head deadline too.
    #[test]
    fn a_dripped_tls_request_head_cannot_hold_the_worker() {
        let srv = serve_tls();
        let plain = format!(
            "GET /status HTTP/1.1\r\nHost: localhost\r\nX-Pad: {}\r\n\r\n",
            "p".repeat(2000)
        );
        let (sock, mut wires) = handshaken(srv.addr, &[plain.as_bytes()]);
        let wire = wires.remove(0);
        let total = wire.len();
        let dripper = start_dripper(sock, Vec::new(), wire);
        let resp = https_status(srv.addr);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");
        assert_let_go(dripper, total);
    }

    /// sec#3 over TLS: a body whose TLS record trickles in is cut off at the
    /// body deadline.
    #[test]
    fn a_dripped_tls_body_cannot_hold_the_worker() {
        let srv = serve_tls();
        // The head goes out at once, in records of its own; the body's
        // record drips.
        let body = vec![b' '; 1000];
        let (sock, mut wires) = handshaken(srv.addr, &[&post_head(body.len()), &body]);
        let (head, rest) = (wires.remove(0), wires.remove(0));
        let total = rest.len();
        let dripper = start_dripper(sock, head, rest);
        let resp = https_status(srv.addr);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp:?}");
        assert_let_go(dripper, total);
    }
}
