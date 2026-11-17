//! The gnlqa HTTP server under failure and abuse: a panicking handler frees
//! its connection slot (review conc#10 / cluster-nlqa#20), and the existing
//! request bounds — body cap, connection cap, read deadline — are exercised
//! over a real socket (review cluster-nlqa#24).

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use common::{get, post};
use gnlqa::{HttpServer, LlmClient, LlmRequest, MockKb, MockLlm, SolveEngine};

const TOKEN: &str = "robustness-token";
const Q: &str = r#"{"question":"q?"}"#;

fn serve(server: HttpServer) -> SocketAddr {
    let addr = server.local_addr().unwrap();
    thread::spawn(move || server.serve_forever());
    addr
}

struct PanickingLlm;
impl LlmClient for PanickingLlm {
    fn complete(&self, _req: &LlmRequest) -> gnlqa::Result<String> {
        panic!("simulated bug inside engine.ask");
    }
}

#[test]
fn a_panicking_handler_answers_500_and_frees_its_slot() {
    let engine = Arc::new(SolveEngine::new(
        Box::new(PanickingLlm),
        Box::new(MockKb::new(vec![])),
    ));
    let addr = serve(
        HttpServer::bind(engine, "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN)
            .with_max_conn(2),
    );
    assert_eq!(get(addr, "/health").status, 200);
    // More panics than slots: each must be answered and give its slot back
    // (the connection closes only after the slot is released, so reading the
    // reply to EOF is the synchronisation point).
    for i in 0..4 {
        let r = post(addr, "/ask", Q, Some(TOKEN));
        assert_eq!(r.status, 500, "panicking request {i}: {r:?}");
        assert!(r.body.contains("internal error"), "{r:?}");
    }
    for i in 0..3 {
        let h = get(addr, "/health");
        assert_eq!(h.status, 200, "health after panics ({i}): {h:?}");
    }
}

#[test]
fn an_oversized_body_is_refused_without_reading_it() {
    let engine = Arc::new(SolveEngine::new(
        Box::new(MockLlm::fixed("x")),
        Box::new(MockKb::new(vec![])),
    ));
    let addr = serve(
        HttpServer::bind(engine, "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN),
    );
    // Announce 2 MiB (over the 1 MiB cap) but send none of it.
    let req = format!(
        "POST /ask HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n",
        2 << 20
    );
    let r = common::send_raw(addr, req.as_bytes());
    assert_eq!(r.status, 400, "{r:?}");
}

/// An LLM that reports when a call starts and blocks until released.
struct GateLlm {
    entered: Mutex<Sender<()>>,
    release: Mutex<Receiver<()>>,
}
impl LlmClient for GateLlm {
    fn complete(&self, _req: &LlmRequest) -> gnlqa::Result<String> {
        self.entered.lock().unwrap().send(()).unwrap();
        let _ = self.release.lock().unwrap().recv();
        Ok(r#"{"qtype":"list"}"#.to_string())
    }
}

#[test]
fn connections_beyond_the_cap_get_503_until_a_slot_frees() {
    let (entered_tx, entered_rx) = channel();
    let (release_tx, release_rx) = channel();
    let llm = GateLlm {
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    };
    let engine = Arc::new(SolveEngine::new(
        Box::new(llm),
        Box::new(MockKb::new(vec![])),
    ));
    let addr = serve(
        HttpServer::bind(engine, "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN)
            .with_max_conn(1),
    );
    // Occupy the only slot with a request parked inside the LLM call.
    let busy = thread::spawn(move || post(addr, "/ask", Q, Some(TOKEN)));
    entered_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("hang guard: the request reached the LLM");
    // The refusal is written without reading the request, so send nothing.
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let refused = common::parse_response(&raw);
    assert_eq!(refused.status, 503, "{refused:?}");
    assert!(refused.body.contains("server busy"));
    // Release every parked call; the first request completes and frees the slot.
    drop(release_tx);
    let done = busy.join().unwrap();
    assert_ne!(done.status, 503, "{done:?}");
    assert_eq!(get(addr, "/health").status, 200);
}

#[test]
fn a_silent_client_is_cut_off_at_the_read_deadline() {
    let engine = Arc::new(SolveEngine::new(
        Box::new(MockLlm::fixed("x")),
        Box::new(MockKb::new(vec![])),
    ));
    let addr = serve(
        HttpServer::bind(engine, "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN)
            .with_read_deadline(Duration::from_millis(300)),
    );
    // Half a request line, then silence: the server must answer and close on
    // its own (the client's 60 s read timeout is only a hang guard).
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    s.write_all(b"POST /ask HTTP/1.1\r\nHost: t\r\n").unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let r = common::parse_response(&raw);
    assert_eq!(r.status, 400, "{r:?}");
    // and the slot is free again
    assert_eq!(get(addr, "/health").status, 200);
}
