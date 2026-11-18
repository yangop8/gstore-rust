# r5-cluster — Experimental cluster (frozen, D4): security and crash fixes only, rest to the backlog

## Changelog

- **Behaviour change: `gnode cluster` needs a state choice** (review
  cluster-nlqa#11). `gnode cluster <id> <bind_addr> [peer_id@addr ...]` now
  requires either `--state-dir DIR` (the replica's Raft term, vote and log
  are journaled in `DIR` and fsynced before it answers; it can be restarted)
  or `--ephemeral` (the old in-memory behaviour; it prints a warning that the
  replica must never be restarted under the same id). Without either it
  exits with an error naming both. Before, every replica was ephemeral: one
  that voted in term T and restarted came back at term 0 with no vote and
  voted a second time in term T (two leaders of one term), and with an empty
  log it could help elect a leader that lacks committed writes. Shard mode
  (`gnode [bind_addr] [data_file]`) is unchanged.
- **New `ClusterNode::bind_persistent(id, peers, state_dir, addr, cfg)`**
  (review cluster-nlqa#11): term, vote and log changes go to an append-only,
  CRC-checked journal `state_dir/raft.journal` (header: magic `GSRAFTJ1` and
  the owning node id) and are fsynced before the node answers a vote or
  append, sends a message, or counts its own entry toward a commit. A restart
  reloads term, vote and log and rebuilds the triple store as the log is
  re-committed; a torn tail (what a crash leaves in the last, never
  acknowledged batch: the file ends inside a record or a 512-byte sector of
  it reads back as zeros, later records of that same batch possibly intact)
  is cut off, while an intact record that does not decode (a newer format,
  a damaged file) and a damaged record a crash cannot have left (its bytes
  all present with a wrong checksum, or followed by well-formed records of a
  later batch) refuse the start, naming the byte offset, instead of being
  cut off with every record after it; a new state directory is created durably (the
  parent of each created directory is fsynced); a directory of another node
  id is refused, and so is a second running copy of the same node (exclusive
  advisory lock on the journal while the node lives); a journal write failure
  stops the node for good (fail-stop): from then on it answers, applies and
  writes nothing, even once the disk recovers. A single-node leader now
  commits its log when it is elected (before: only at the next write).
  `ClusterNode::bind` documents the restart hazard of its in-memory state.
- **Behaviour change: forged Raft terms are refused** (review
  cluster-nlqa#2). One `RequestVote` at term `u64::MAX` used to wedge a
  `ClusterNode` cluster for good (the next election overflowed: a panic that
  poisoned the node in debug builds, a wrap to 0 in release). A message may
  now move a node's term forward by at most 2^32; larger jumps (in requests
  and replies) are answered with the current term and not adopted; elections
  use checked arithmetic (a node at the last term stays a follower); a lying
  `AppendAck` match index is clamped to the leader's log; an
  `InstallSnapshot` beyond index 2^62 is refused.
- **Behaviour change: `ShardedRaftCluster` reports only committed writes**
  (review cluster-nlqa#4). `insert` / `remove` return `Some(true)` only once
  the write is committed and `None` when it could not be committed now (no
  leader, or no quorum — the entry may still commit later; retry is safe);
  `insert_all` counts committed triples only (it reported 5 "newly
  committed" triples while the cluster held none). New
  `ReplicaGroup::commit(op) -> Result<index, ProposeError>` (the
  `ClusterNode::propose_and_wait` contract for the synchronous driver).
- **Behaviour change: a `ClusterNode` routes shard inserts through the log
  and refuses `PurgeShard`** (review cluster-nlqa#3). `Request::Insert` sent
  to a cluster node answers `Bool` (was it new) once committed, as
  `RemoteShard::insert` expects (it got `Err("expected a Bool response")` for
  a committed write), and `Error` (with the leader hint) when the node is not
  the leader or cannot confirm the commit. `PurgeShard` — which deleted a
  bucket on one replica outside the Raft log and diverged the replicas for
  good — is refused with `Error`; reads (including `ScanShard`) are served as
  before.
- **Behaviour change: `ShardCluster` membership changes are resumable and
  never lose data** (reviews cluster-nlqa#1, #10). A migration that fails part
  way leaves its buckets *stranded* (listed by the new
  `ShardCluster::stranded_buckets`) and returns the error; repeating the same
  `add_node` / `remove_node`, or the new `ShardCluster::resume_migrations`,
  finishes them (moves are idempotent). Any other membership change first
  resumes them and is refused, changing nothing, while one stays stranded.
  `remove_node` drains every bucket of the leaving node (not only those it
  owned), keeps its endpoint until all moved, and refuses to remove the last
  member while it holds triples. Nothing is purged before the copy reached
  its target. While a bucket is stranded, `try_triple_count` /
  `try_pred_card` de-duplicate instead of summing. Re-adding an existing
  member never replaces its endpoint (it used to drop a `Local` member's
  data): the same address is a retry, a different address — or a seeded
  `Local` endpoint whose triples would be discarded — is refused with
  `InvalidInput`. Before, a failed join's retry was a silent no-op and a
  later leave dropped the stranded triples (128 of 400 in the reviewer's
  run), every call returning `Ok`.
- **Behaviour change: shard-node connections time out** (review
  cluster-nlqa#9, merged sec#19 / conc#11). `ShardNode` bounds each read and
  write on an accepted connection (`ShardNode::DEFAULT_IO_TIMEOUT` = 60 s;
  `ShardNode::with_io_timeout`), so an idle or stalled client no longer pins
  its thread forever; `RemoteShard` reconnects on its next call.
- **The RPC frame reader no longer preallocates** (reviews robust#15,
  sec#19, merged into cluster-nlqa#9). `rpc::read_message` grows its buffer
  with the bytes received (64 KiB first) instead of allocating the frame the
  unauthenticated length prefix announces — up to 256 MiB per connection for
  four bytes.
- **Oversized replies are reported** (review cluster-nlqa#9). A reply larger
  than one RPC frame is answered with `Response::Error` naming its size (new
  `rpc::encode_reply`) on `ShardNode` and `ClusterNode` connections, and the
  connection stays usable; before, the node built the reply, failed to send
  it and dropped the connection, so the client saw a reset and retried.
- **Corrected test** (review cluster-nlqa#12):
  `shard_survives_n_over_2_replica_failures_with_quorum` asserted
  `Some(true)` ("committed") for a write that cannot commit; it now expects
  `None` and checks that the leader still appended the entry.
- Tests: `cluster::review_fix_tests` (23 tests; failures injected with
  `test_support::Link::fail_after`, a link that dies after exactly n RPCs,
  with forged messages, and with a journal file handle swapped for a
  read-only one; wall clocks are hang guards only),
  `rpc::tests::an_oversized_reply_is_encoded_as_an_error`,
  `tests/rpc_frame_reader_allocates_what_arrives.rs` (counting allocator),
  `tests/gnode_restarted_replica_votes_once_per_term.rs` (real binary: vote,
  SIGKILL, restart, second candidate refused; required state choice).

## GAP_PLAN status line

none

## Docs notes

- README.md, "Experimental: cluster": replace "keep all state in memory"
  with: shard nodes and coordinators keep their data in memory; a
  `gnode cluster` replica needs `--state-dir DIR` (durable Raft term, vote
  and log, so it can be restarted safely) or `--ephemeral` (in memory; never
  restart it under the same id). Add: shard-node connections idle or stalled
  for 60 s are closed; a failed `ShardCluster` membership change is resumed
  by repeating it (or `resume_migrations`) and blocks other changes until
  then; the wire protocol is still unauthenticated — trusted networks only.
- docs/DEPLOY.md (where `gnode` is listed among the packaged binaries): one
  line that `gnode cluster` requires `--state-dir DIR` or `--ephemeral`.
  This is not a gserver upstream-compat switch (`gnode` has no upstream 1.4
  counterpart and no config key), so it does not go into the compat-switch
  table.
- docs/STORAGE_BACKEND.md: the raft journal (`raft.journal`, magic
  `GSRAFTJ1`) is a new file of the experimental cluster, not a database
  format; it carries its own magic, no existing format changed, so no
  format-registry entry is needed (D7).
- docs/REFACTOR_BACKLOG.md, cluster section (frozen experimental code, D4) —
  what remains of this package's ids, and the deferred ones:
  - cluster-nlqa#9 / sec#19 / conc#11 (rest): no authentication of shard or
    Raft RPCs (any client that reaches the port can read, `ClientWrite`
    deletes, or send a plausible-term `InstallSnapshot` that replaces a
    replica's store); one thread per connection with no cap or worker pool;
    `ShardNode::serve_forever` spins on persistent accept errors at the fd
    limit (`incoming().flatten()`; a connection cap fixes both);
    `ClusterNode::run_server` polls `accept` every 2 ms; `store.lock()
    .unwrap()` in `dispatch` lets one panic poison a shard for every
    connection; `RemoteShard` holds one connection mutex across each round
    trip (callers to one shard serialize) and retries a request once on a
    fresh connection, so an `Insert` whose response was lost reports "not
    inserted" (needs idempotency tokens). Reason: D4 — authentication and the
    connection model are a redesign of a frozen experimental transport.
  - cluster-nlqa#2 (rest): the term bound is per message (2^32), so it stops
    the one-message wedge but not a sequence: a few forged plausible-step
    messages to one node can still move it more than 2^32 terms past its
    peers, which then refuse its messages and it theirs — that node is cut
    off for good (and with `--state-dir` the forged term is durable). Any
    bound short of authenticated peer RPCs leaves a variant of this. Reason:
    D4 — peer authentication is the fix (above).
  - cluster-nlqa#1 (rest): a member whose endpoint is gone for good cannot be
    evicted while it has stranded buckets — every membership change resumes
    them first — so the cluster's membership is frozen until the node comes
    back (before the fix such a node could not be dropped either: its
    endpoint stayed and every read failed); an explicit "forget node, accept
    the loss" operation is missing. While a bucket is stranded,
    `ShardCluster::insert` reports a triple that still sits only on the move's
    source as new (`insert_all` can over-count; the counts read back are
    exact). Reason: D4 — new membership API on a frozen experimental
    coordinator.
  - cluster-nlqa#12 (rest): wall-clock assertions remain in
    `raft_tests::a_hung_peer_does_not_stall_the_healthy_majority` (commit in
    < 250 ms, no churn for 1.5 s) and
    `rpc::tests::loopback_round_trips_are_not_throttled_by_delayed_ack`
    (< 1500 ms); `ShardNode::serve_forever` has no shutdown, so test shard
    threads never stop; no tests for self/duplicate peers (cluster-nlqa#7),
    `gnode`'s `load_id_triples` / peer parsing, or the write-side
    `MAX_FRAME` check (needs a 256 MiB payload). Reason: D4 — only the
    failure paths of the fixed findings got new tests.
  - cluster-nlqa#11 (rest): the raft journal is never compacted (the
    transport never compacts its log either); config-change entries and
    snapshots still interact as the module docs describe; `gnode` shard
    mode keeps its data in memory. Reason: D4.
  - cluster-nlqa#5 (membership changes never alter the voter set),
    cluster-nlqa#6 (unbatched AppendEntries; catch-up stalls past
    `MAX_FRAME`), cluster-nlqa#7 (own id / duplicate peers inflate the
    quorum), cluster-nlqa#8 (`DefaultHasher` placement across processes):
    deferred per the plan (D4), unchanged.

## Measured numbers

- No hot path touched: `src/cluster.rs`, `src/rpc.rs` and `src/bin/gnode.rs`
  are not on gserver's query, storage or HTTP paths, so the bench smoke gate
  (convention 8) does not apply and was not run.
- Frame reader: reading a forged prefix announcing `MAX_FRAME` (268435456
  bytes) followed by 3 bytes allocated a 268435456-byte block before the fix
  and less than 1 MiB after (asserted by
  `tests/rpc_frame_reader_allocates_what_arrives.rs`; the first buffer is
  64 KiB).
- Reviewer repro (cluster-nlqa#1, link dies after 30 inserts of a
  400-triple migration): before, `try_triple_count` read 409 after the
  retried join and triples were lost by the leave; after, 400 throughout and
  none lost.
- Test runtime on this 4-vCPU machine (other builds running):
  `cargo test -p gstore --lib cluster:: rpc::` 77 tests in about 2 s; the two
  new integration tests under 0.1 s each.

## Requests/Not yet

- No edits outside the owned files (`src/cluster.rs`, `src/rpc.rs`,
  `src/bin/gnode.rs`) besides the package's two new test files
  (`tests/rpc_frame_reader_allocates_what_arrives.rs`,
  `tests/gnode_restarted_replica_votes_once_per_term.rs`) and this fragment.
- `rpc::Reader`, `put_u32/put_u64/put_entries/put_triples` became
  `pub(crate)` (the raft journal reuses the wire encodings); no public API
  was removed or changed in signature. New public items:
  `ClusterNode::bind_persistent`, `ShardCluster::stranded_buckets`,
  `ShardCluster::resume_migrations`, `ReplicaGroup::commit`,
  `ShardNode::DEFAULT_IO_TIMEOUT`, `ShardNode::with_io_timeout`,
  `rpc::encode_reply`.
- Closing step: fold the README / DEPLOY / STORAGE_BACKEND / REFACTOR_BACKLOG
  notes above; the CHANGELOG entries are behaviour changes of experimental,
  library-only code (none touches gserver).
- Partial ids (remainders in the backlog note above): cluster-nlqa#9 (with
  sec#19, conc#11) and cluster-nlqa#12; every other id of the package is
  fixed (cluster-nlqa#1 and #2 with the residual limits recorded above).
- Independent review of this package: the raft journal now refuses an
  intact but undecodable record instead of cutting it off, creates a new
  state directory durably (reusing `db::fsutil::create_dir_durable`, no
  edit there), and stays stopped after a failed write (before, a retried
  vote whose journal write had failed was granted once the disk recovered);
  four tests in `cluster::review_fix_tests` cover these.
- Round-5 review follow-up (cluster-nlqa#11): replay stopped at the first
  record whose checksum failed and cut the file there, so one flipped bit in
  an older vote record silently dropped every later, acknowledged vote and
  entry (node 3 restarted at term 0 and granted a second vote in term 8).
  `RaftJournal::open` now cuts only what a crash in the last batch can leave
  (see the Changelog bullet) and otherwise refuses with `InvalidData`,
  leaving the file untouched; the scan for later records checks each
  candidate's kind and length before its CRC. No format change (`append`'s
  batch encoding moved into `RaftJournal::batch`, unchanged).
  `cluster::review_fix_tests::a_damaged_journal_record_before_acknowledged_ones_refuses_the_start`
  (flipped bit and zero-filled first record before an acknowledged vote,
  flipped bit in the last record: refused, file unchanged; a last batch
  with its hard-state record zero-filled and its log suffix intact: cut)
  failed before the fix at the first refusal.
