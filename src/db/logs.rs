//! The append-only redo and update logs of the library API
//! ([`Database::enable_redo_log`](super::Database::enable_redo_log),
//! [`enable_update_log`](super::Database::enable_update_log),
//! [`backup_incremental`](super::Database::backup_incremental)): record
//! framing, the walk that finds the last whole commit of a log, and the
//! appender that keeps a log crash-consistent (review durability#6, db#8,
//! robust#10, db#9).
//!
//! **Records.** A record is a header line, a body of the length the header
//! gives, and a newline:
//!
//! * redo: `REDO <op> <graph_len> <triple_len>[ G] c=<crc>\n<graph><triple>\n`
//! * update: `REC <unix_millis> <changed> <byte_len> c=<crc>\n<sparql>\n`
//!
//! **Commits.** The records of one commit (an auto-commit statement, a
//! transaction, one direct-API mutation) are appended with one `write`, and
//! the last of them carries `e=<crc>` instead of `c=<crc>`. `<crc>` is the
//! CRC-32 (IEEE) of the header line up to and including `c=` / `e=`, a
//! newline and the body, in eight lowercase hex digits. A reader applies a
//! commit only once it has read its `e=` record, so a torn append never
//! replays in part.
//!
//! **Compatibility.** Records written before checksums existed have no
//! `c=` / `e=` token: each is a commit of its own and is read as before.
//! Readers that predate the token ignore header tokens they do not know, so
//! they read new records unchanged (they apply them one by one).
//!
//! **Torn tails.** A crash can leave the last append torn: a record that is
//! incomplete at the end of the file, or whose checksum or terminator is
//! wrong with no record header after it, or a commit whose `e=` record is
//! missing at the end of the file. That tail is ignored by replay and cut off
//! by the next append (or backup), which therefore always starts on a commit
//! boundary. An append that fails (`ENOSPC`, `EFBIG`, a failed `fdatasync`)
//! is cut off again at once and its records are kept for the next append.
//! Any other malformed record is corruption, which replay reports instead
//! of skipping (lengths are checked arithmetic: a crafted length is an
//! error, never a panic).

use std::fmt::Write as _;
use std::fs;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::fsutil;
use crate::error::{GStoreError, Result};

/// Which log a buffer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LogKind {
    Redo,
    Update,
}

impl LogKind {
    /// The first word of every record header.
    fn magic(self) -> &'static str {
        match self {
            LogKind::Redo => "REDO",
            LogKind::Update => "REC",
        }
    }

    /// The log's conventional file name, for messages.
    pub(super) fn name(self) -> &'static str {
        match self {
            LogKind::Redo => "redo.log",
            LogKind::Update => "update.log",
        }
    }

    /// The body length a header's fields (after the magic word) declare:
    /// redo `<op> <graph_len> <triple_len> …`, update `<millis> <changed>
    /// <byte_len> …`.
    fn body_len(self, fields: &[&str]) -> BodyLen {
        let num = |i: usize| -> Option<std::result::Result<usize, ()>> {
            let f = fields.get(i)?;
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // All digits, yet no `usize`: a length no write produces.
            Some(f.parse().map_err(|_| ()))
        };
        let len = match self {
            LogKind::Redo => match (fields.first(), num(1), num(2)) {
                (Some(_), Some(Ok(g)), Some(Ok(t))) => g.checked_add(t).ok_or(()),
                (Some(_), Some(_), Some(_)) => Err(()),
                _ => return BodyLen::Malformed,
            },
            LogKind::Update => match num(2) {
                Some(len) => len,
                None => return BodyLen::Malformed,
            },
        };
        match len {
            Ok(n) => BodyLen::Len(n),
            Err(()) => BodyLen::OutOfRange,
        }
    }
}

/// What a record header says its body length is.
enum BodyLen {
    Len(usize),
    /// A field is missing or not a number.
    Malformed,
    /// A number beyond `usize`, or a sum that overflows (review db#9): no
    /// write produces it, so it is corruption, never a torn tail.
    OutOfRange,
}

// ---- checksums ------------------------------------------------------------------

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 (IEEE 802.3, reflected), fed in pieces.
struct Crc(u32);

impl Crc {
    fn new() -> Crc {
        Crc(0xFFFF_FFFF)
    }

    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = CRC_TABLE[((self.0 ^ u32::from(b)) & 0xFF) as usize] ^ (self.0 >> 8);
        }
    }

    fn finish(&self) -> u32 {
        !self.0
    }
}

/// Length of the checksum token's digits.
const CRC_DIGITS: usize = 8;

// ---- writing records --------------------------------------------------------------

/// Append one record to `out`: `prefix` is its header line without the
/// checksum token, `body` its body in pieces; `last` marks the record that
/// ends its commit.
fn frame(out: &mut Vec<u8>, prefix: &str, body: &[&[u8]], last: bool) {
    let tag = if last { " e=" } else { " c=" };
    let mut crc = Crc::new();
    crc.update(prefix.as_bytes());
    crc.update(tag.as_bytes());
    crc.update(b"\n");
    for piece in body {
        crc.update(piece);
    }
    let mut header = String::with_capacity(prefix.len() + tag.len() + CRC_DIGITS + 1);
    header.push_str(prefix);
    header.push_str(tag);
    let _ = writeln!(header, "{:08x}", crc.finish());
    out.extend_from_slice(header.as_bytes());
    for piece in body {
        out.extend_from_slice(piece);
    }
    out.push(b'\n');
}

/// Append one redo record (`op` `I`, `D`, `C` or `N`; `graph`: a named
/// graph's IRI) to `out`.
pub(super) fn frame_redo(out: &mut Vec<u8>, op: u8, graph: Option<&str>, triple: &str, last: bool) {
    let g = graph.unwrap_or("");
    let flag = if graph.is_some() { " G" } else { "" };
    let prefix = format!("REDO {} {} {}{flag}", op as char, g.len(), triple.len());
    frame(out, &prefix, &[g.as_bytes(), triple.as_bytes()], last);
}

/// Append one update-log record (an applied UPDATE request, `changed`
/// triples, at `millis` since the epoch) to `out`.
pub(super) fn frame_update(
    out: &mut Vec<u8>,
    millis: u128,
    changed: usize,
    sparql: &str,
    last: bool,
) {
    let prefix = format!("REC {millis} {changed} {}", sparql.len());
    frame(out, &prefix, &[sparql.as_bytes()], last);
}

// ---- reading records ---------------------------------------------------------------

/// One whole record of a log buffer.
#[derive(Debug)]
pub(super) struct Record<'a> {
    /// Byte offset of its header.
    pub(super) start: usize,
    /// The header's fields after the magic word (the checksum token
    /// excluded).
    pub(super) fields: Vec<&'a str>,
    /// Its body.
    pub(super) body: &'a [u8],
    /// Byte offset just past its terminating newline.
    end: usize,
    /// `None` for a record from before checksums (a commit of its own),
    /// else whether it ends its commit.
    ends_commit: Option<bool>,
}

/// What [`record_at`] found.
enum Step<'a> {
    Record(Record<'a>),
    /// The log is torn from `at` on.
    Torn {
        at: usize,
        why: String,
    },
    End,
}

/// Where a log's records end.
#[derive(Debug)]
pub(super) struct Walk {
    /// Byte length of the whole commits (corrupt ones included, when they
    /// are skipped): everything before a torn tail.
    pub(super) end: usize,
    /// Where a torn tail starts, and why it is one.
    pub(super) torn: Option<(usize, String)>,
}

/// Whether a record header follows offset `pos` (a newline then the magic
/// word): a defective record with one after it is corruption, without one a
/// torn tail.
fn header_after(bytes: &[u8], pos: usize, kind: LogKind) -> Option<usize> {
    let magic = kind.magic().as_bytes();
    let from = pos.checked_add(1)?;
    bytes
        .get(from..)?
        .windows(magic.len() + 2)
        .position(|w| w[0] == b'\n' && &w[1..=magic.len()] == magic && w[magic.len() + 1] == b' ')
        .map(|i| from + i + 1)
}

/// A defect in the record at `pos`: a torn tail when no record header
/// follows it, else corruption.
fn torn_or_corrupt(bytes: &[u8], pos: usize, kind: LogKind, why: String) -> Result<Step<'_>> {
    if header_after(bytes, pos, kind).is_some() {
        return Err(corrupt(kind, pos, &why));
    }
    Ok(Step::Torn { at: pos, why })
}

fn corrupt(kind: LogKind, pos: usize, why: &str) -> GStoreError {
    GStoreError::Database(format!("{} is corrupt at byte {pos}: {why}", kind.name()))
}

/// Split a header line's trailing checksum token off: `(line up to and
/// including "c=" / "e=", ends_commit, crc)`, or `None` without one.
fn checksum_token(line: &str) -> Option<(&str, bool, u32)> {
    let digits_at = line.len().checked_sub(CRC_DIGITS)?;
    let tag_at = digits_at.checked_sub(3)?;
    let (head, digits) = (line.get(..digits_at)?, line.get(digits_at..)?);
    let ends = match line.get(tag_at..digits_at)? {
        " c=" => false,
        " e=" => true,
        _ => return None,
    };
    if !digits
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some((head, ends, u32::from_str_radix(digits, 16).ok()?))
}

/// The record starting at `pos`.
fn record_at(bytes: &[u8], pos: usize, kind: LogKind) -> Result<Step<'_>> {
    if pos >= bytes.len() {
        return Ok(Step::End);
    }
    let Some(nl) = bytes[pos..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| pos + i)
    else {
        return torn_or_corrupt(bytes, pos, kind, "incomplete record header".into());
    };
    let Ok(line) = std::str::from_utf8(&bytes[pos..nl]) else {
        return torn_or_corrupt(bytes, pos, kind, "non-UTF-8 record header".into());
    };
    let checksum = checksum_token(line);
    let fields_text = match checksum {
        Some((head, _, _)) => &head[..head.len() - 3],
        None => line,
    };
    let mut words = fields_text.split_whitespace();
    if words.next() != Some(kind.magic()) {
        return torn_or_corrupt(
            bytes,
            pos,
            kind,
            format!("malformed record header {line:?}"),
        );
    }
    let fields: Vec<&str> = words.collect();
    let out_of_range = || {
        corrupt(
            kind,
            pos,
            &format!("record length out of range in {line:?}"),
        )
    };
    let body_len = match kind.body_len(&fields) {
        BodyLen::Len(n) => n,
        BodyLen::Malformed => {
            return torn_or_corrupt(
                bytes,
                pos,
                kind,
                format!("malformed record header {line:?}"),
            )
        }
        BodyLen::OutOfRange => return Err(out_of_range()),
    };
    let body_start = nl + 1;
    let Some(body_end) = body_start.checked_add(body_len) else {
        return Err(out_of_range());
    };
    if body_end >= bytes.len() {
        // The body, or its terminating newline, is missing.
        return torn_or_corrupt(bytes, pos, kind, "incomplete record".into());
    }
    if bytes[body_end] != b'\n' {
        return torn_or_corrupt(bytes, pos, kind, "record terminator missing".into());
    }
    let body = &bytes[body_start..body_end];
    if let Some((head, _, want)) = checksum {
        let mut crc = Crc::new();
        crc.update(head.as_bytes());
        crc.update(b"\n");
        crc.update(body);
        if crc.finish() != want {
            return torn_or_corrupt(bytes, pos, kind, "checksum mismatch".into());
        }
    }
    Ok(Step::Record(Record {
        start: pos,
        fields,
        body,
        end: body_end + 1,
        ends_commit: checksum.map(|(_, ends, _)| ends),
    }))
}

/// Walk the whole commits of a log buffer in order, handing each to
/// `commit`, up to the end or a torn tail. Corruption fails the walk —
/// unless `skip_corrupt`, which goes on at the next record header (the
/// scan that finds where an append may start: corruption earlier in a log
/// is replay's to report, the tail is the appender's to repair).
pub(super) fn walk<'a>(
    bytes: &'a [u8],
    kind: LogKind,
    skip_corrupt: bool,
    mut commit: impl FnMut(&[Record<'a>]) -> Result<()>,
) -> Result<Walk> {
    let mut pos = 0;
    let mut group: Vec<Record<'a>> = Vec::new();
    let mut group_start = 0;
    loop {
        let step = match record_at(bytes, pos, kind) {
            Ok(step) => step,
            Err(e) if !skip_corrupt => return Err(e),
            Err(_) => {
                // Corrupt: resume at the next header (there is one).
                group.clear();
                pos = header_after(bytes, pos, kind).unwrap_or(bytes.len());
                group_start = pos;
                continue;
            }
        };
        match step {
            Step::End if group.is_empty() => {
                return Ok(Walk {
                    end: bytes.len(),
                    torn: None,
                })
            }
            Step::End => {
                return Ok(Walk {
                    end: group_start,
                    torn: Some((group_start, "the log ends inside a commit".into())),
                })
            }
            Step::Torn { at, why } => {
                let at = if group.is_empty() { at } else { group_start };
                return Ok(Walk {
                    end: at,
                    torn: Some((at, why)),
                });
            }
            Step::Record(r) => {
                pos = r.end;
                match r.ends_commit {
                    None if group.is_empty() => {
                        commit(std::slice::from_ref(&r))?;
                        group_start = pos;
                    }
                    None => {
                        let e =
                            corrupt(kind, r.start, "a record without a checksum inside a commit");
                        if !skip_corrupt {
                            return Err(e);
                        }
                        group.clear();
                        group_start = pos;
                    }
                    Some(false) => {
                        if group.is_empty() {
                            group_start = r.start;
                        }
                        group.push(r);
                    }
                    Some(true) => {
                        group.push(r);
                        commit(&group)?;
                        group.clear();
                        group_start = pos;
                    }
                }
            }
        }
    }
}

// ---- the appender ----------------------------------------------------------------

/// The appender of one log file. Committed records whose append failed stay
/// here, in order, and go first into the next append; the file always ends
/// on a commit boundary this appender knows (or rescans for).
#[derive(Debug, Default)]
pub(super) struct LogWriter {
    /// The file this state belongs to.
    path: Option<PathBuf>,
    /// Framed commits not yet in the file (their append failed).
    unwritten: Vec<u8>,
    /// How many commits `unwritten` holds.
    unwritten_commits: usize,
    /// Why the last append failed (while `unwritten` is not empty).
    error: Option<String>,
    /// The file's length after this appender's last append or scan — a
    /// commit boundary — or `None` when unknown (scanned before the next
    /// append).
    valid_len: Option<u64>,
    /// Records were appended since the last `fdatasync`.
    unsynced: bool,
    /// This appender created the file and its directory entry is not
    /// synced yet.
    dir_unsynced: bool,
}

impl LogWriter {
    /// Point this appender at `path`. Moving to another file first tries to
    /// write the commits still owed to the old one there, then forgets them
    /// (they belong to the log that was left).
    pub(super) fn retarget(&mut self, path: &Path, kind: LogKind) {
        if self.path.as_deref() == Some(path) {
            return;
        }
        if !self.unwritten.is_empty() || self.unsynced || self.dir_unsynced {
            let _ = self.flush(kind, true);
        }
        *self = LogWriter {
            path: Some(path.to_path_buf()),
            ..LogWriter::default()
        };
    }

    /// Append one framed commit to the log at `path` (one `write`, then an
    /// `fdatasync` when `sync`), after any commit still owed to it. On
    /// failure the commit is kept for the next append and the error
    /// returned.
    pub(super) fn append(
        &mut self,
        path: &Path,
        kind: LogKind,
        commit: &[u8],
        sync: bool,
    ) -> Result<()> {
        self.retarget(path, kind);
        if commit.is_empty() {
            return self.flush(kind, sync);
        }
        self.unwritten.extend_from_slice(commit);
        self.unwritten_commits += 1;
        self.flush(kind, sync)
    }

    /// Write the commits still owed to the log, and `fdatasync` it when
    /// `sync` (and anything was appended without one). A no-op when there
    /// is nothing to do.
    pub(super) fn flush(&mut self, kind: LogKind, sync: bool) -> Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        if self.unwritten.is_empty() && !(sync && (self.unsynced || self.dir_unsynced)) {
            return Ok(());
        }
        let res = self.write_out(&path, kind, sync);
        if let Err(e) = &res {
            if !self.unwritten.is_empty() {
                self.error = Some(e.to_string());
            }
        }
        res
    }

    fn write_out(&mut self, path: &Path, kind: LogKind, sync: bool) -> Result<()> {
        let fail = |e: std::io::Error| {
            GStoreError::Database(format!(
                "appending to {} '{}' failed: {e}",
                kind.name(),
                path.display()
            ))
        };
        if !self.unwritten.is_empty() {
            let existed = path.exists();
            let mut file = fs::OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .open(path)
                .map_err(fail)?;
            if !existed {
                self.dir_unsynced = true;
            }
            let valid = self.boundary(&mut file, kind, 0)?;
            let written = file.write_all(&self.unwritten).and_then(|()| {
                if sync {
                    file.sync_data()
                } else {
                    Ok(())
                }
            });
            if let Err(e) = written {
                // Cut the partial append off again: the file ends on a
                // commit boundary, and the commits stay owed.
                if file.set_len(valid).is_err() {
                    self.valid_len = None;
                }
                return Err(fail(e));
            }
            self.valid_len = Some(valid + self.unwritten.len() as u64);
            self.unwritten.clear();
            self.unwritten_commits = 0;
            self.error = None;
            self.unsynced = !sync;
        } else if sync && self.unsynced {
            let file = fs::OpenOptions::new()
                .append(true)
                .open(path)
                .map_err(fail)?;
            file.sync_data().map_err(fail)?;
            self.unsynced = false;
        }
        if sync && self.dir_unsynced {
            let dir = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            fsutil::sync_dir(dir)?;
            self.dir_unsynced = false;
        }
        Ok(())
    }

    /// The file's whole length — where the next append starts — cutting a
    /// torn tail off first. Scans from `hint` (a known commit boundary)
    /// unless the length this appender left is still the file's.
    fn boundary(&mut self, file: &mut fs::File, kind: LogKind, hint: u64) -> Result<u64> {
        let len = file.metadata()?.len();
        if self.valid_len == Some(len) {
            return Ok(len);
        }
        let hint = if hint <= len { hint } else { 0 };
        file.seek(SeekFrom::Start(hint))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail)?;
        let end = hint + walk(&tail, kind, true, |_| Ok(()))?.end as u64;
        if end < len {
            file.set_len(end)?;
            file.sync_data()?;
        }
        self.valid_len = Some(end);
        Ok(end)
    }

    /// The log's whole length (the incremental-backup checkpoint unit),
    /// cutting a torn tail off; `hint` is a known commit boundary to scan
    /// from. `0` when the file does not exist.
    pub(super) fn whole_len(&mut self, kind: LogKind, hint: u64) -> Result<u64> {
        let Some(path) = self.path.clone() else {
            return Ok(0);
        };
        let mut file = match fs::OpenOptions::new().read(true).append(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                self.valid_len = None;
                return Ok(0);
            }
            Err(e) => return Err(e.into()),
        };
        self.boundary(&mut file, kind, hint)
    }

    /// Why committed records are missing from the log, while they are.
    pub(super) fn failure(&self, kind: LogKind) -> Option<String> {
        if self.unwritten.is_empty() {
            return None;
        }
        Some(format!(
            "{} committed write(s) could not be appended to {} '{}' ({}); they are kept and \
             written first by the next append",
            self.unwritten_commits,
            kind.name(),
            self.path.as_deref().unwrap_or(Path::new("")).display(),
            self.error.as_deref().unwrap_or("unknown error"),
        ))
    }
}

/// Read bytes `[start, end)` of the log at `path` (empty when the file does
/// not exist and the range is empty).
pub(super) fn read_range(path: &Path, start: u64, end: u64) -> Result<Vec<u8>> {
    if start == end {
        return Ok(Vec::new());
    }
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut out = vec![0u8; (end - start) as usize];
    file.read_exact(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_of(recs: &[(u8, Option<&str>, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, (op, g, t)) in recs.iter().enumerate() {
            frame_redo(&mut out, *op, *g, t, i + 1 == recs.len());
        }
        out
    }

    fn commits(bytes: &[u8], kind: LogKind) -> Result<(Vec<usize>, Walk)> {
        let mut sizes = Vec::new();
        let w = walk(bytes, kind, false, |c| {
            sizes.push(c.len());
            Ok(())
        })?;
        Ok((sizes, w))
    }

    const T1: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .";
    const T2: &str = "<http://ex/c> <http://ex/p> <http://ex/d> .";

    #[test]
    fn crc32_matches_the_reference_value() {
        let mut c = Crc::new();
        c.update(b"123456789");
        assert_eq!(c.finish(), 0xCBF4_3926);
    }

    #[test]
    fn commits_are_read_whole_and_old_records_one_by_one() {
        let mut log = b"REDO I 0 43\n<http://ex/a> <http://ex/p> <http://ex/b> .\n".to_vec();
        log.extend(commit_of(&[(b'I', None, T1), (b'D', Some("http://g"), T2)]));
        log.extend(commit_of(&[(b'I', None, T2)]));
        let (sizes, w) = commits(&log, LogKind::Redo).unwrap();
        assert_eq!(sizes, [1, 2, 1]);
        assert_eq!(w.end, log.len());
        assert!(w.torn.is_none());
    }

    #[test]
    fn every_cut_of_the_last_commit_is_a_torn_tail_at_its_start() {
        let first = commit_of(&[(b'I', None, T1)]);
        let second = commit_of(&[(b'I', None, T2), (b'D', None, T1)]);
        let log = [first.clone(), second.clone()].concat();
        for cut in first.len()..log.len() {
            let (sizes, w) = commits(&log[..cut], LogKind::Redo).unwrap();
            assert_eq!(sizes, [1], "cut at {cut}");
            assert_eq!(w.end, first.len(), "cut at {cut}");
            assert_eq!(w.torn.is_some(), cut > first.len(), "cut at {cut}");
        }
    }

    #[test]
    fn a_damaged_last_record_is_torn_and_a_damaged_middle_one_corrupt() {
        let first = commit_of(&[(b'I', None, T1)]);
        let second = commit_of(&[(b'I', None, T2)]);
        // The last record zero-filled (a crash after the size was updated).
        let mut log = [first.clone(), second.clone()].concat();
        for b in &mut log[first.len() + 5..] {
            *b = 0;
        }
        let (sizes, w) = commits(&log, LogKind::Redo).unwrap();
        assert_eq!((sizes.len(), w.end), (1, first.len()));
        // One flipped body byte at the end: torn; in the middle: corrupt.
        let mut log = [first.clone(), second.clone()].concat();
        let at = log.len() - 3;
        log[at] ^= 1;
        let (_, w) = commits(&log, LogKind::Redo).unwrap();
        assert_eq!(w.end, first.len());
        let mut log = [first.clone(), second.clone()].concat();
        log[first.len() - 3] ^= 1;
        let err = commits(&log, LogKind::Redo).unwrap_err().to_string();
        assert!(
            err.contains("corrupt at byte 0") && err.contains("checksum"),
            "{err}"
        );
        // The appender's scan skips the corrupt record and keeps the rest.
        assert_eq!(
            walk(&log, LogKind::Redo, true, |_| Ok(())).unwrap().end,
            log.len()
        );
    }

    #[test]
    fn crafted_lengths_are_errors_not_panics() {
        for log in [
            format!("REDO I {} 1\nx\n", usize::MAX - 20),
            format!("REDO I 0 {}\nx\nREDO I 0 1\nx\n", usize::MAX),
            format!("REDO I {} {}\nx\nREDO I 0 1\nx\n", usize::MAX, usize::MAX),
        ] {
            let r = std::panic::catch_unwind(|| commits(log.as_bytes(), LogKind::Redo).map(|_| ()));
            assert!(r.is_ok(), "{log:?} panicked");
        }
        let log = format!("REC 0 0 {}\nx\nREC 0 0 1\nx\n", usize::MAX);
        assert!(commits(log.as_bytes(), LogKind::Update).is_err());
    }

    #[test]
    fn update_records_frame_and_read_back() {
        let mut log = Vec::new();
        frame_update(
            &mut log,
            1,
            2,
            "INSERT DATA { <a> <b> \"REC 0 0 5\nx\" }",
            false,
        );
        frame_update(&mut log, 3, 4, "CLEAR ALL", true);
        let mut bodies = Vec::new();
        let w = walk(&log, LogKind::Update, false, |c| {
            bodies.extend(
                c.iter()
                    .map(|r| String::from_utf8(r.body.to_vec()).unwrap()),
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(w.end, log.len());
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].contains("REC 0 0 5"));
    }
}
