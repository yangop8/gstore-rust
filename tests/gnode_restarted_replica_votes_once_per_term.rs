//! review cluster-nlqa#11: `gnode cluster` kept the Raft term, vote and log in
//! memory only, so a replica that granted its vote in term T and restarted
//! came back at term 0 with no vote and granted a second candidate in the same
//! term T — two leaders of one term (Election Safety broken), not just lost
//! data. A replica now persists term, vote and log (fsync before it answers)
//! in its `--state-dir`; running without persistence needs an explicit
//! `--ephemeral`.
//!
//! The test drives the real binary: a vote is granted, the process is
//! SIGKILLed (no orderly shutdown), restarted on the same state directory, and
//! asked by a different candidate for a vote in the same term.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gstore::rpc::{round_trip, Request, Response};

/// A unique scratch directory, removed when dropped (also on failure).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore_gnode_vote_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create a temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A running `gnode cluster` replica, SIGKILLed when dropped.
struct Replica {
    child: Child,
    addr: SocketAddr,
}

impl Replica {
    /// Start replica 2 of a three-node cluster whose peers 0 and 1 are
    /// `peers` (listeners that never answer, so the replica never wins an
    /// election), persisting its state in `state_dir`.
    fn start(state_dir: &Path, peers: &[SocketAddr; 2]) -> Replica {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gnode"))
            .arg("cluster")
            .arg("2")
            .arg("127.0.0.1:0")
            .arg(format!("0@{}", peers[0]))
            .arg(format!("1@{}", peers[1]))
            .arg("--state-dir")
            .arg(state_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn gnode");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("read gnode's startup line");
        let addr = line
            .split_whitespace()
            .skip_while(|w| *w != "on")
            .nth(1)
            .and_then(|a| a.parse().ok());
        match addr {
            Some(addr) => Replica { child, addr },
            None => {
                let _ = child.kill();
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                let _ = child.wait();
                panic!("gnode did not start: stdout {line:?}, stderr {err:?}");
            }
        }
    }

    /// Ask for a vote in `term` on behalf of `candidate` (whose log is empty,
    /// as the replica's is); returns `(reply term, granted)`.
    fn request_vote(&self, term: u64, candidate: u64) -> (u64, bool) {
        let mut s = TcpStream::connect(self.addr).expect("connect to gnode");
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let reply = round_trip(
            &mut s,
            &Request::RequestVote {
                term,
                candidate_id: candidate,
                last_log_index: 0,
                last_log_term: 0,
            },
        )
        .expect("vote round trip");
        match reply {
            Response::Vote { term, granted } => (term, granted),
            other => panic!("expected a Vote reply, got {other:?}"),
        }
    }

    /// Crash the replica: SIGKILL, no chance to flush anything.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Replica {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_restarted_replica_does_not_vote_twice_in_one_term() {
    let dir = TempDir::new("restart");
    // Peers that accept connections but never answer: the replica keeps
    // campaigning on its own, never wins, and its own terms stay far below T.
    let peer0 = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer1 = TcpListener::bind("127.0.0.1:0").unwrap();
    let peers = [peer0.local_addr().unwrap(), peer1.local_addr().unwrap()];
    const T: u64 = 1_000_000;

    let replica = Replica::start(&dir.0, &peers);
    let (term, granted) = replica.request_vote(T, 0);
    assert!(granted, "the first candidate of term {T} gets the vote");
    assert_eq!(term, T);
    replica.kill();

    let replica = Replica::start(&dir.0, &peers);
    let (term, granted) = replica.request_vote(T, 1);
    assert!(
        !granted,
        "a restarted replica voted a second time in term {T} (reply term {term})"
    );
    assert!(term >= T, "the restarted replica forgot term {T}: {term}");
    // The first candidate asking again is still granted the (same) vote.
    let (_, again) = replica.request_vote(T, 0);
    assert!(again || term > T, "the recorded vote is for candidate 0");
}

#[test]
fn cluster_mode_requires_a_state_dir_or_an_explicit_ephemeral_choice() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gnode"))
        .args(["cluster", "2", "127.0.0.1:0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run gnode");
    // Hang guard only: a gnode that accepted the command line runs forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll gnode") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("gnode cluster started without a state choice");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!status.success(), "started without a state choice");
    assert!(
        stderr.contains("--state-dir") && stderr.contains("--ephemeral"),
        "the error names both choices: {stderr}"
    );
}
