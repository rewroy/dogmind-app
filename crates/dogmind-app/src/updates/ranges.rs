//! Fetching only what an update changes. A setup is one file of some half a gigabyte, of which a few files change from one release to the next, and the install it is for has the rest. Beside
//! each setup a release has its `.parts` file, which says where each file of the setup is in it. With that, the install's own record of its files (the `sum` lines of its manifest, which say
//! what each file hashed to, and how large it was and when it was written to then), and HTTP's `Range`, this fetches the setup's program and the files that are new or changed, and puts a
//! setup together of them: one in which each other file has the method KEPT, with no bytes, and which the installer takes from the install it is run on. It is a setup like any other: it
//! opens, and it installs.
//!
//! Nothing is trusted that was not checked. The `.parts` file is checked by the size and the SHA-256 GitHub published for it, as the setup is whole. What it says is then what each part is
//! checked by: the program by its hash, and each file's bytes by inflating them and comparing size and SHA-256. Whatever does not come out as it was said to be is a failure of this way, and
//! the setup is fetched whole, as it was before this was: the person is told no more of it than of any other fetch.
//!
//! The package is read and written as `dogmind-setup`'s `pack.rs` does, which this cannot link to: a test that has that file in it checks them against each other.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use dogmind_agent::http::{Failure, Response, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{first_hop, follow, hex, Failed, Setup, INSTALL_MANIFEST};

/// What the package begins with, and the length of what follows it: the place of the index, its length and its hash.
const MAGIC: &[u8; 16] = b"DogmindSetup\x1a\x01\0\0";
const HEADER: u64 = 16 + 8 + 8 + 32;
const STORED: u8 = 0;
const DEFLATED: u8 = 1;
const KEPT: u8 = 2;
/// What the `.parts` file says it is. Any other is a file of a later program.
const PARTS_FORMAT: u64 = 1;
/// The most a `.parts` file may be: it is some 150 bytes for each file of a setup, and a setup has some ten thousand. More is no `.parts` file, and memory is all it could cost.
pub(super) const MOST_OF_PARTS: u64 = 32 << 20;
/// Two pieces of the setup that are wanted and are no further apart than this are fetched by one request, and what is between them is thrown away: a connection of its own for each costs about as
/// long as this takes to come.
const JOIN_UP_TO: u64 = 1 << 20;
/// The most requests for the pieces: when more pieces than this are left, the ones that are nearest are joined, a little further apart each time, until they are not.
const MOST_REQUESTS: usize = 24;

/// Why the range way was not gone, in a line for whoever reads a test: the person is told none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Not {
    /// The person stopped it. This is the one that ends the fetch, and does not go on to the whole setup.
    Stopped,
    Because(&'static str),
}

pub(super) const NO_RECORDS: &str = "the install has no record of its files";
pub(super) const NOT_ALLOWED: &str = "an address that a download may not be fetched from";
pub(super) const PARTS_NOT_FETCHED: &str = "the parts file could not be fetched";
pub(super) const PARTS_DIFFER: &str = "the parts file is not the size and hash that GitHub published";
pub(super) const PARTS_UNREADABLE: &str = "the parts file is not one that this reads, or does not describe this setup";
pub(super) const NOT_WORTH_IT: &str = "most of the setup is wanted, and one fetch of all of it is what that comes to";
pub(super) const NOT_A_RANGE: &str = "the server did not answer with the range that was asked for";
pub(super) const WHOLE_FILE: &str = "the server answered with the whole file where a range was asked for";
pub(super) const NOT_FETCHED: &str = "a range could not be fetched";
pub(super) const DAMAGED: &str = "what came is not what the parts file says it is";
pub(super) const NOT_WRITTEN: &str = "the setup could not be written";

/// What a result of this comes to: the setup that was put together, as it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Assembled {
    pub size: u64,
    pub sha256: String,
}

pub(super) struct Job<'a> {
    pub version: &'a str,
    /// The setup of the release, as GitHub published it: where it is, and the size and hash of all of it.
    pub setup: &'a Setup,
    /// The `.parts` file beside it, in the same way.
    pub parts: &'a Setup,
    /// The install folder this is for.
    pub install: &'a Path,
    pub test_server: Option<&'a Url>,
}

fn is_safe(relative: &str) -> bool {
    // (as `pack::is_safe` of `dogmind-setup`: Windows opens `a.` and `a ` as `a`, and a device has its name in any folder)
    const DEVICES: [&str; 26] = ["con", "prn", "aux", "nul", "conin$", "conout$", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "com0", "lpt0"];
    !relative.is_empty()
        && relative.split('\\').all(|part| !part.is_empty() && !part.ends_with(['.', ' ']) && !DEVICES.contains(&part.to_ascii_lowercase().as_str()))
        && !relative.contains(['/', ':', '*', '?', '"', '<', '>', '|'])
        && !relative.chars().any(char::is_control)
}

/// A hash from its hex, in either case; none for anything that is not exactly 64 hex digits.
fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (at, pair) in text.as_bytes().chunks(2).enumerate() {
        out[at] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// What the installer wrote down of a file it put in the install folder: its hash, and the size and last-write time (as Windows keeps it, in steps of 100 ns) that the file had after that.
/// A file that still has them is that file. As `Record` of `dogmind-setup`'s install.rs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Record {
    pub sha256: [u8; 32],
    pub size: u64,
    pub modified: u64,
}

fn modified_of(data: &std::fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    data.last_write_time()
}

/// The `sum SHA256 SIZE MODIFIED PATH` lines of the install's manifest, by path in lower case. An install made before they were written has none, and nothing here can be done for it.
pub(super) fn records_of(install: &Path) -> HashMap<String, Record> {
    let mut records = HashMap::new();
    let Ok(text) = std::fs::read_to_string(install.join(INSTALL_MANIFEST)) else { return records };
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("sum ") else { continue };
        let mut words = rest.splitn(4, ' ');
        let (Some(hash), Some(size), Some(modified), Some(path)) = (words.next(), words.next(), words.next(), words.next()) else { continue };
        let (Some(sha256), Ok(size), Ok(modified)) = (unhex(hash), size.parse(), modified.parse()) else { continue };
        if is_safe(path) {
            records.insert(path.to_lowercase(), Record { sha256, size, modified });
        }
    }
    records
}

/// One file of the setup, as the `.parts` file has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub path: String,
    pub size: u64,
    pub offset: u64,
    pub stored: u64,
    pub method: u8,
    pub sha256: [u8; 32],
}

/// What the `.parts` file says, once it has been checked to be a sound description of a setup of this version and this size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Parts {
    /// Where the package begins: the length of the program before it.
    pub base: u64,
    pub stub_sha256: [u8; 32],
    pub entries: Vec<Entry>,
    pub index_at: u64,
}

#[derive(Deserialize)]
struct PartsFile {
    format: u64,
    version: String,
    length: u64,
    base: u64,
    stub_sha256: String,
    header: PartsHeader,
    entries: Vec<PartsEntry>,
}

#[derive(Deserialize)]
struct PartsHeader {
    index_at: u64,
    index_length: u64,
}

#[derive(Deserialize)]
struct PartsEntry {
    path: String,
    size: u64,
    offset: u64,
    stored: u64,
    method: u8,
    sha256: String,
}

/// The `.parts` file in `bytes`, for the setup of this version and this many bytes. Everything that is read is checked to be what a package of one file after another can be: the entries
/// in the order of their bytes, none out of the file, none twice, and each with the room its method needs.
pub(super) fn read_parts(bytes: &[u8], version: &str, setup_size: u64) -> Result<Parts, Not> {
    let unreadable = Not::Because(PARTS_UNREADABLE);
    let file: PartsFile = serde_json::from_slice(bytes).map_err(|_| unreadable)?;
    let end = file.base.checked_add(file.header.index_at).and_then(|at| at.checked_add(file.header.index_length));
    if file.format != PARTS_FORMAT || file.version != version || file.length != setup_size || file.base == 0 || end != Some(file.length) || file.entries.len() > 1 << 20 {
        return Err(unreadable);
    }
    let stub_sha256 = unhex(&file.stub_sha256).ok_or(unreadable)?;
    let empty: [u8; 32] = Sha256::digest(b"").into();
    let (mut next, mut seen) = (HEADER, HashSet::new());
    let mut entries = Vec::with_capacity(file.entries.len());
    for entry in file.entries {
        let sha256 = unhex(&entry.sha256).ok_or(unreadable)?;
        let room = match entry.method {
            STORED => entry.stored == entry.size,
            DEFLATED => true,
            _ => false,
        };
        let stored_end = entry.offset.checked_add(entry.stored);
        // (a file with no bytes stored is an empty one, which is its own hash)
        let sound = room && is_safe(&entry.path) && seen.insert(entry.path.to_lowercase()) && entry.offset >= next && stored_end.is_some_and(|stored_end| stored_end <= file.header.index_at) && (entry.stored > 0 || (entry.size == 0 && sha256 == empty));
        if !sound {
            return Err(unreadable);
        }
        next = stored_end.unwrap_or(next);
        entries.push(Entry { path: entry.path, size: entry.size, offset: entry.offset, stored: entry.stored, method: entry.method, sha256 });
    }
    Ok(Parts { base: file.base, stub_sha256, entries, index_at: file.header.index_at })
}

/// A piece of the setup that is wanted, by where it is in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    /// The program before the package.
    Stub,
    /// The bytes of this entry, by its place in the `.parts` file.
    Entry(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    start: u64,
    end: u64,
    piece: Piece,
}

/// What is to be fetched: which files are wanted, the pieces of the setup that holds them and the program, and the requests that fetch those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Plan {
    /// For each entry of the `.parts` file, whether its bytes are fetched. The others are kept from the install.
    pub wanted: Vec<bool>,
    segments: Vec<Segment>,
    /// The requests, by their first byte and one past their last, in the order of the setup.
    pub ranges: Vec<(u64, u64)>,
    /// What the requests come to, with what is thrown away between the pieces that they join.
    pub bytes: u64,
}

/// Whether a file's bytes are to be fetched: when the install has no record of it, or the record says another file, or the file in the folder is not as it was when it was written down.
fn is_wanted(entry: &Entry, record: Option<&Record>, install: &Path) -> bool {
    let Some(record) = record else { return true };
    if record.sha256 != entry.sha256 || record.size != entry.size {
        return true;
    }
    match std::fs::metadata(install.join(&entry.path)) {
        Ok(data) => !(data.is_file() && data.len() == record.size && modified_of(&data) == record.modified),
        Err(_) => true,
    }
}

fn joined(segments: &[Segment], gap: u64) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for segment in segments {
        match ranges.last_mut() {
            Some(last) if segment.start.saturating_sub(last.1) <= gap => last.1 = segment.end,
            _ => ranges.push((segment.start, segment.end)),
        }
    }
    ranges
}

pub(super) fn plan(parts: &Parts, records: &HashMap<String, Record>, install: &Path) -> Plan {
    let wanted: Vec<bool> = parts.entries.iter().map(|entry| is_wanted(entry, records.get(&entry.path.to_lowercase()), install)).collect();
    let mut segments = vec![Segment { start: 0, end: parts.base, piece: Piece::Stub }];
    for (at, entry) in parts.entries.iter().enumerate() {
        if wanted[at] && entry.stored > 0 {
            segments.push(Segment { start: parts.base + entry.offset, end: parts.base + entry.offset + entry.stored, piece: Piece::Entry(at) });
        }
    }
    let mut gap = JOIN_UP_TO;
    let mut ranges = joined(&segments, gap);
    while ranges.len() > MOST_REQUESTS {
        gap *= 4;
        ranges = joined(&segments, gap);
    }
    let bytes = ranges.iter().map(|(start, end)| end - start).sum();
    Plan { wanted, segments, ranges, bytes }
}

fn put_text(out: &mut Vec<u8>, words: &str) {
    out.extend_from_slice(&(words.len() as u16).to_le_bytes());
    out.extend_from_slice(words.as_bytes());
}

/// The header and the index of the setup that is put together.
struct Layout {
    header: Vec<u8>,
    index: Vec<u8>,
    /// Where the index is in the package: after the header and the bytes of each file that is fetched.
    index_at: u64,
}

/// The index of the setup that is put together, and its header: each file that is not fetched is KEPT, with no place and no bytes, and the others follow the header one after another.
fn layout(version: &str, parts: &Parts, wanted: &[bool]) -> Layout {
    let mut entries = Vec::with_capacity(parts.entries.len());
    let mut at = HEADER;
    for (entry, wanted) in parts.entries.iter().zip(wanted) {
        if *wanted {
            entries.push(Entry { offset: at, ..entry.clone() });
            at += entry.stored;
        } else {
            entries.push(Entry { offset: 0, stored: 0, method: KEPT, ..entry.clone() });
        }
    }
    let mut index = Vec::new();
    put_text(&mut index, version);
    index.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for entry in &entries {
        put_text(&mut index, &entry.path);
        index.extend_from_slice(&entry.size.to_le_bytes());
        index.extend_from_slice(&entry.offset.to_le_bytes());
        index.extend_from_slice(&entry.stored.to_le_bytes());
        index.push(entry.method);
        index.extend_from_slice(&entry.sha256);
    }
    let mut header = Vec::with_capacity(HEADER as usize);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&at.to_le_bytes());
    header.extend_from_slice(&(index.len() as u64).to_le_bytes());
    header.extend_from_slice(&Sha256::digest(&index));
    Layout { header, index, index_at: at }
}

/// Where a program's own bytes end, by its section table, in the bytes of it: as `overlay_start` of `dogmind-setup`'s pack.rs.
fn overlay_start(program: &[u8]) -> Option<u64> {
    let word = |at: usize, length: usize| -> Option<u64> {
        let bytes = program.get(at..at.checked_add(length)?)?;
        Some(bytes.iter().rev().fold(0u64, |value, byte| value << 8 | u64::from(*byte)))
    };
    let header = word(0x3C, 4)? as usize;
    if program.get(header..header + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = word(header + 6, 2)? as usize;
    let optional = word(header + 20, 2)? as usize;
    let table = header + 24 + optional;
    let mut end = 0u64;
    for section in 0..sections {
        let at = table + section * 40;
        let (size, start) = (word(at + 16, 4)?, word(at + 20, 4)?);
        if size > 0 {
            end = end.max(start + size);
        }
    }
    Some(end)
}

/// A writer that counts and hashes what is written to it, and keeps none of it.
struct Counted {
    digest: Sha256,
    count: u64,
}

impl Write for Counted {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.digest.update(bytes);
        self.count += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a file's stored bytes come to, as they arrive: stored as they are, or inflated.
enum Check {
    Stored(Counted),
    Deflated(flate2::write::DeflateDecoder<Counted>),
}

impl Check {
    fn of(method: u8) -> Check {
        let counted = Counted { digest: Sha256::new(), count: 0 };
        if method == DEFLATED { Check::Deflated(flate2::write::DeflateDecoder::new(counted)) } else { Check::Stored(counted) }
    }

    /// More of the stored bytes. What comes out may not be more than the file is said to be.
    fn feed(&mut self, bytes: &[u8], most: u64) -> Result<(), Not> {
        let damaged = Not::Because(DAMAGED);
        match self {
            Check::Stored(counted) => {
                counted.write_all(bytes).map_err(|_| damaged)?;
                (counted.count <= most).then_some(()).ok_or(damaged)
            }
            Check::Deflated(decoder) => {
                let mut at = 0;
                while at < bytes.len() {
                    match decoder.write(&bytes[at..]) {
                        // (the stream has ended, and what follows it is not read: the installer unpacks it so)
                        Ok(0) => break,
                        Ok(count) => at += count,
                        Err(_) => return Err(damaged),
                    }
                    if decoder.get_ref().count > most {
                        return Err(damaged);
                    }
                }
                Ok(())
            }
        }
    }

    /// The size and the hash of what all of it came to.
    fn finish(self) -> Result<(u64, [u8; 32]), Not> {
        let counted = match self {
            Check::Stored(counted) => counted,
            Check::Deflated(decoder) => decoder.finish().map_err(|_| Not::Because(DAMAGED))?,
        };
        Ok((counted.count, counted.digest.finalize().into()))
    }
}

/// The setup that is put together, written as it is made, and hashed as it is written.
struct Output {
    file: std::io::BufWriter<std::fs::File>,
    digest: Sha256,
    written: u64,
}

impl Output {
    fn put(&mut self, bytes: &[u8]) -> Result<(), Not> {
        self.digest.update(bytes);
        self.written += bytes.len() as u64;
        self.file.write_all(bytes).map_err(|_| Not::Because(NOT_WRITTEN))
    }
}

/// What takes the bytes of the setup as they come, in the order of the setup, from one request after another: the program is kept, and checked by its hash when it is all here; each file's
/// stored bytes are written as they come and checked by inflating them; what is between them is not wanted.
struct Assembly<'a> {
    parts: &'a Parts,
    segments: &'a [Segment],
    layout: Layout,
    out: Output,
    /// The segment that bytes are in now, and whether its first byte has come.
    current: usize,
    begun: bool,
    stub: Vec<u8>,
    check: Option<Check>,
}

impl Assembly<'_> {
    /// `bytes`, which are the setup's from `at` on.
    fn feed(&mut self, mut at: u64, mut bytes: &[u8]) -> Result<(), Not> {
        let unexpected = Not::Because(NOT_A_RANGE);
        while !bytes.is_empty() {
            let segment = *self.segments.get(self.current).ok_or(unexpected)?;
            if at < segment.start {
                // (between two pieces that one request joins)
                let thrown = (segment.start - at).min(bytes.len() as u64) as usize;
                at += thrown as u64;
                bytes = &bytes[thrown..];
                continue;
            }
            if !self.begun {
                // (a piece is come to at its first byte, as each request begins at one)
                if at != segment.start {
                    return Err(unexpected);
                }
                self.begun = true;
                if let Piece::Entry(index) = segment.piece {
                    self.check = Some(Check::of(self.parts.entries[index].method));
                }
            }
            let take = (segment.end - at).min(bytes.len() as u64) as usize;
            let (these, rest) = bytes.split_at(take);
            match segment.piece {
                Piece::Stub => self.stub.extend_from_slice(these),
                Piece::Entry(index) => {
                    self.out.put(these)?;
                    self.check.as_mut().ok_or(unexpected)?.feed(these, self.parts.entries[index].size)?;
                }
            }
            at += take as u64;
            bytes = rest;
            if at == segment.end {
                self.end_of(segment)?;
                self.current += 1;
                self.begun = false;
            }
        }
        Ok(())
    }

    fn end_of(&mut self, segment: Segment) -> Result<(), Not> {
        match segment.piece {
            Piece::Stub => {
                // (the program is what the `.parts` file says, and ends where it says: the installer finds its package there)
                if Sha256::digest(&self.stub).as_slice() != self.parts.stub_sha256 || overlay_start(&self.stub) != Some(self.parts.base) {
                    return Err(Not::Because(DAMAGED));
                }
                let stub = std::mem::take(&mut self.stub);
                self.out.put(&stub)?;
                let header = std::mem::take(&mut self.layout.header);
                self.out.put(&header)
            }
            Piece::Entry(index) => {
                let entry = &self.parts.entries[index];
                let (size, sha256) = self.check.take().ok_or(Not::Because(DAMAGED))?.finish()?;
                if size == entry.size && sha256 == entry.sha256 { Ok(()) } else { Err(Not::Because(DAMAGED)) }
            }
        }
    }

    /// Everything has come: the index goes on the end, and the file is on the disk.
    fn finish(mut self) -> Result<Assembled, Not> {
        if self.current != self.segments.len() {
            return Err(Not::Because(NOT_A_RANGE));
        }
        let index = std::mem::take(&mut self.layout.index);
        self.out.put(&index)?;
        let written = self.out.written;
        self.out.file.flush().map_err(|_| Not::Because(NOT_WRITTEN))?;
        let file = self.out.file.into_inner().map_err(|_| Not::Because(NOT_WRITTEN))?;
        file.sync_all().map_err(|_| Not::Because(NOT_WRITTEN))?;
        if written != self.parts.base + self.layout.index_at + index.len() as u64 {
            return Err(Not::Because(NOT_WRITTEN));
        }
        Ok(Assembled { size: written, sha256: hex(&self.out.digest.finalize()) })
    }
}

fn failed(failure: Failed) -> Not {
    match failure {
        Failed::Cancelled => Not::Stopped,
        Failed::Refused(200) => Not::Because(WHOLE_FILE),
        Failed::NotAllowed => Not::Because(NOT_ALLOWED),
        _ => Not::Because(NOT_FETCHED),
    }
}

fn stopped_or(failure: Failure, otherwise: Not) -> Not {
    match failure {
        Failure::Cancelled => Not::Stopped,
        _ => otherwise,
    }
}

/// The `.parts` file, fetched as a setup is, sent on only where a download may be, and kept only if it has the size and the hash that GitHub published of it.
fn fetch_parts(first: &Url, parts: &Setup, test_server: Option<&Url>, stop: &AtomicBool) -> Result<Vec<u8>, Not> {
    let headers = [("User-Agent".to_string(), format!("Dogmind/{}", env!("CARGO_PKG_VERSION"))), ("Accept".to_string(), "application/octet-stream".to_string())];
    let (mut response, _) = follow(first, &headers, 200, test_server, stop).map_err(|failure| match failed(failure) {
        Not::Because(NOT_FETCHED) => Not::Because(PARTS_NOT_FETCHED),
        other => other,
    })?;
    let differ = Not::Because(PARTS_DIFFER);
    if parts.size > MOST_OF_PARTS || response.header("content-length").is_some_and(|length| length.trim().parse::<u64>().ok() != Some(parts.size)) {
        return Err(differ);
    }
    let mut body = Vec::with_capacity(parts.size as usize);
    loop {
        match response.next() {
            Ok(Some(bytes)) => {
                if body.len() as u64 + bytes.len() as u64 > parts.size {
                    return Err(differ);
                }
                body.extend_from_slice(&bytes);
            }
            Ok(None) => break,
            Err(failure) => return Err(stopped_or(failure, Not::Because(PARTS_NOT_FETCHED))),
        }
    }
    if body.len() as u64 != parts.size || hex(&Sha256::digest(&body)) != parts.sha256 {
        return Err(differ);
    }
    Ok(body)
}

/// The setup's own address, resolved to GitHub's storage once, and asked for a range at a time.
struct Source<'a> {
    first: Url,
    /// Where the first request ended up, after it was sent on: the rest are asked of that.
    resolved: Option<Url>,
    /// Whether that address has gone stale (403, as a signed address does after a while) and been resolved again: it is once.
    resolved_again: bool,
    headers: Vec<(String, String)>,
    total: u64,
    test_server: Option<&'a Url>,
    stop: &'a AtomicBool,
}

impl<'a> Source<'a> {
    /// The answer to a request for these bytes of the setup, from `start` to one before `end`, that is checked to be them: 206, and the `Content-Range` that was asked for.
    fn open(&mut self, start: u64, end: u64) -> Result<Response<'a>, Not> {
        let mut headers = self.headers.clone();
        headers.push(("Range".to_string(), format!("bytes={start}-{}", end - 1)));
        loop {
            let from = self.resolved.clone().unwrap_or_else(|| self.first.clone());
            match follow(&from, &headers, 206, self.test_server, self.stop) {
                Ok((response, at)) => {
                    let asked = format!("bytes {start}-{}/{}", end - 1, self.total);
                    let right = response.header("content-range").is_some_and(|said| said.trim().eq_ignore_ascii_case(&asked)) && response.header("content-length").is_none_or(|length| length.trim().parse::<u64>().ok() == Some(end - start));
                    if !right {
                        return Err(Not::Because(NOT_A_RANGE));
                    }
                    self.resolved = Some(at);
                    return Ok(response);
                }
                Err(Failed::Refused(403)) if self.resolved.is_some() && !self.resolved_again => {
                    self.resolved = None;
                    self.resolved_again = true;
                }
                Err(other) => return Err(failed(other)),
            }
        }
    }
}

/// The setup put together in `partial`: the program, and the files of the setup that this install does not have, fetched by ranges, and the other files left to the install. `on` is told how
/// many bytes of how many have come, of those that are fetched. When this is not `Ok` nothing says the setup is not to be fetched whole, but for the person's stop: the caller deletes
/// `partial`, and goes on to fetch it so.
pub(super) fn assemble(job: &Job, partial: &Path, on: &mut dyn FnMut(u64, u64), stop: &AtomicBool) -> Result<Assembled, Not> {
    let records = records_of(job.install);
    if records.is_empty() {
        return Err(Not::Because(NO_RECORDS));
    }
    let first_parts = first_hop(&job.parts.url, job.test_server).ok_or(Not::Because(NOT_ALLOWED))?;
    let first = first_hop(&job.setup.url, job.test_server).ok_or(Not::Because(NOT_ALLOWED))?;
    let parts = read_parts(&fetch_parts(&first_parts, job.parts, job.test_server, stop)?, job.version, job.setup.size)?;
    let plan = plan(&parts, &records, job.install);
    // (what is nearly all of the setup is no saving: it is fetched whole)
    if plan.bytes.saturating_mul(4) >= job.setup.size.saturating_mul(3) {
        return Err(Not::Because(NOT_WORTH_IT));
    }
    let layout = layout(job.version, &parts, &plan.wanted);
    let file = std::fs::File::create(partial).map_err(|_| Not::Because(NOT_WRITTEN))?;
    let mut assembly = Assembly {
        parts: &parts,
        segments: &plan.segments,
        layout,
        out: Output { file: std::io::BufWriter::with_capacity(1 << 20, file), digest: Sha256::new(), written: 0 },
        current: 0,
        begun: false,
        stub: Vec::new(),
        check: None,
    };
    // (what goes out says what program asks and that it wants the file, as a fetch of the whole does)
    let headers = vec![("User-Agent".to_string(), format!("Dogmind/{}", env!("CARGO_PKG_VERSION"))), ("Accept".to_string(), "application/octet-stream".to_string())];
    let mut source = Source { first, resolved: None, resolved_again: false, headers, total: job.setup.size, test_server: job.test_server, stop };
    let mut done = 0u64;
    on(0, plan.bytes);
    for &(start, end) in &plan.ranges {
        let mut response = source.open(start, end)?;
        let mut at = start;
        loop {
            if stop.load(Ordering::Relaxed) {
                return Err(Not::Stopped);
            }
            match response.next() {
                Ok(Some(bytes)) => {
                    if at + bytes.len() as u64 > end {
                        return Err(Not::Because(NOT_A_RANGE));
                    }
                    assembly.feed(at, &bytes)?;
                    at += bytes.len() as u64;
                    done += bytes.len() as u64;
                    on(done, plan.bytes);
                }
                Ok(None) => break,
                Err(failure) => return Err(stopped_or(failure, Not::Because(NOT_FETCHED))),
            }
        }
        if at != end {
            return Err(Not::Because(NOT_A_RANGE));
        }
    }
    assembly.finish()
}

/// The installer's own `pack.rs`, in the tests of this: what it writes is what this reads, and what this puts together is opened by it.
#[cfg(test)]
#[allow(dead_code, unused_imports)]
#[path = "../../../dogmind-setup/src/pack.rs"]
pub(super) mod setup_pack;

#[cfg(test)]
pub(super) mod support {
    use super::setup_pack;
    use std::path::Path;

    /// As random as it needs to be for deflate to find nothing, and another for another seed.
    pub fn noisy(length: usize, seed: u64) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ seed.wrapping_mul(0xD6E8_FEB8_6659_FD93);
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// A program for a setup to be put on, small: a header, one section, and nothing in it.
    pub fn mini_exe() -> Vec<u8> {
        let mut program = vec![0u8; 0x1200];
        program[..2].copy_from_slice(b"MZ");
        program[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        program[0x40..0x44].copy_from_slice(b"PE\0\0");
        program[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        program[0x54..0x56].copy_from_slice(&0u16.to_le_bytes());
        program[0x58 + 16..0x58 + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        program[0x58 + 20..0x58 + 24].copy_from_slice(&0x200u32.to_le_bytes());
        for (at, byte) in program[0x200..0x1200].iter_mut().enumerate() {
            *byte = (at as u8).wrapping_mul(7);
        }
        program
    }

    /// The setup of a version as `dogmind-setup --pack` makes it from these files, in `dir`, with its `.parts` file beside it: where the setup is, and what it holds.
    pub fn make_setup(dir: &Path, version: &str, files: &[(&str, &[u8])]) -> std::path::PathBuf {
        let folder = dir.join(format!("in-{version}"));
        setup_pack::tests::tree(&folder, files);
        let stub = dir.join("stub.exe");
        std::fs::write(&stub, mini_exe()).unwrap();
        let exe = dir.join(format!("DogmindSetup-{version}.exe"));
        setup_pack::pack(&stub, &folder, &exe, version, 6, &mut |_| {}).unwrap();
        exe
    }

    /// The last-write time of a file as the installer writes it down.
    pub fn modified(path: &Path) -> u64 {
        use std::os::windows::fs::MetadataExt;
        std::fs::metadata(path).unwrap().last_write_time()
    }

    /// An install folder as the installer leaves one: these files, and a manifest that has a `sum` line for each if `with_sums`, and the lines of 0.1.4 if not.
    pub fn make_install(folder: &Path, files: &[(&str, &[u8])], with_sums: bool) {
        use sha2::{Digest, Sha256};
        setup_pack::tests::tree(folder, files);
        let mut manifest = String::from("version 0.1.9\n");
        for (name, _) in files {
            manifest.push_str(&format!("file {name}\n"));
        }
        if with_sums {
            for (name, bytes) in files {
                manifest.push_str(&format!("sum {} {} {} {name}\n", setup_pack::hex(&Sha256::digest(bytes).into()), bytes.len(), modified(&folder.join(name))));
            }
        }
        std::fs::write(folder.join(super::INSTALL_MANIFEST), manifest).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::support::*;
    use super::*;
    use setup_pack::Package;

    #[test]
    fn the_numbers_of_the_package_are_the_installers() {
        assert_eq!((MAGIC, HEADER, STORED, DEFLATED, KEPT, PARTS_FORMAT), (setup_pack::MAGIC, setup_pack::HEADER, setup_pack::STORED, setup_pack::DEFLATED, setup_pack::KEPT, setup_pack::PARTS_FORMAT));
    }

    #[test]
    fn the_program_ends_where_the_installer_finds_its_package() {
        let dir = tempfile::tempdir().unwrap();
        let mini = dir.path().join("mini.exe");
        std::fs::write(&mini, mini_exe()).unwrap();
        for program in [mini, std::env::current_exe().unwrap()] {
            let wanted = setup_pack::overlay_start(&mut std::fs::File::open(&program).unwrap()).unwrap();
            assert_eq!(overlay_start(&std::fs::read(&program).unwrap()), Some(wanted), "{}", program.display());
        }
        assert_eq!(overlay_start(b"MZ"), None);
        assert_eq!(overlay_start(&[0u8; 4096]), None, "no header of a program");
    }

    #[test]
    fn a_name_is_judged_as_the_installer_judges_it() {
        for name in ["..\\x", "a\\..\\..\\x", "C:\\x", "\\x", "a:stream", "a\\\\b", "", ".", "a/b", ".. \\x", "..\\ x\\..  \\y", "a\\... \\b", "a.", "a ", "NUL", "tools\\con", "a\nb", "a", "a\\b.txt", "\u{65e5}\u{672c}\\c d", "node_modules\\.bin\\x", "fonts\\BIZUDGothic-Regular.ttf"] {
            assert_eq!(is_safe(name), setup_pack::is_safe(name), "{name:?}");
        }
    }

    #[test]
    fn a_hash_is_read_from_hex_as_the_installer_reads_it() {
        for text in ["", "abc", &"g".repeat(64), &"0".repeat(63), &"0".repeat(65), &"aB".repeat(32), &"0123456789abcdef".repeat(4)] {
            assert_eq!(unhex(text), setup_pack::unhex(text), "{text:?}");
        }
        assert_eq!(hex(&unhex(&"AB".repeat(32)).unwrap()), "ab".repeat(32));
    }

    #[test]
    fn the_installers_manifest_is_read_for_its_records_and_nothing_that_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        std::fs::write(
            dir.path().join(INSTALL_MANIFEST),
            format!("version 2\ntest\nfile a.exe\nsum {hash} 5 123 A.exe\nsum {hash} 5 123\nsum nothing 5 123 b.exe\nsum {hash} five 123 c.exe\nsum {hash} 5 123 ..\\d.exe\nsum {hash} 7 99 e f\\g h.exe\ntook checking=1\n"),
        )
        .unwrap();
        let records = records_of(dir.path());
        let mut names: Vec<&String> = records.keys().collect();
        names.sort();
        assert_eq!(names, ["a.exe", "e f\\g h.exe"], "by lower case path, a path with spaces whole, nothing that is not in order");
        assert_eq!(records["e f\\g h.exe"], Record { sha256: unhex(hash).unwrap(), size: 7, modified: 99 });
        // an install of 0.1.4 has `file` lines and nothing else; and a folder with none has no records at all
        std::fs::write(dir.path().join(INSTALL_MANIFEST), "version 0.1.4\nfile a.exe\n").unwrap();
        assert!(records_of(dir.path()).is_empty());
        assert!(records_of(&dir.path().join("none")).is_empty());
    }

    #[test]
    fn pieces_that_are_near_are_one_request_and_there_are_not_many_requests() {
        let segment = |start, end| Segment { start, end, piece: Piece::Stub };
        let pieces = [segment(0, 100), segment(150, 200), segment(10_000, 10_100)];
        assert_eq!(joined(&pieces, 49), [(0, 100), (150, 200), (10_000, 10_100)]);
        assert_eq!(joined(&pieces, 50), [(0, 200), (10_000, 10_100)], "a gap as large as that is joined");
        assert_eq!(joined(&pieces, 10_000), [(0, 10_100)]);
        assert_eq!(joined(&[], 5), []);
    }

    /// A parts file as the installer's `--pack` writes it, of a folder, and the setup it describes.
    fn parts_of_a_setup() -> (tempfile::TempDir, std::path::PathBuf, serde_json::Value) {
        let dir = tempfile::tempdir().unwrap();
        let exe = make_setup(dir.path(), "1.2.3", &[("a.txt", b"hello"), ("b.bin", &noisy(5000, 1)), ("c\\d.txt", &vec![b'x'; 9000]), ("empty", b"")]);
        let parts = serde_json::from_slice(&std::fs::read(setup_pack::parts_path(&exe)).unwrap()).unwrap();
        (dir, exe, parts)
    }

    #[test]
    fn a_parts_file_is_read_as_what_the_installer_wrote_and_nothing_that_does_not_describe_the_setup() {
        let (_dir, exe, parts) = parts_of_a_setup();
        let length = std::fs::metadata(&exe).unwrap().len();
        let read = |value: &serde_json::Value| read_parts(&serde_json::to_vec(value).unwrap(), "1.2.3", length);
        let package = Package::open(&exe).unwrap();
        let sound = read(&parts).unwrap();
        assert_eq!(sound.base, package.base);
        assert_eq!(sound.entries.len(), 4);
        for (entry, wanted) in sound.entries.iter().zip(&package.entries) {
            assert_eq!((entry.path.as_str(), entry.size, entry.offset, entry.stored, entry.method, entry.sha256), (wanted.path.as_str(), wanted.size, wanted.offset, wanted.stored, wanted.method, wanted.sha256));
        }
        assert_eq!(read_parts(&serde_json::to_vec(&parts).unwrap(), "1.2.4", length), Err(Not::Because(PARTS_UNREADABLE)), "another version");
        assert_eq!(read_parts(&serde_json::to_vec(&parts).unwrap(), "1.2.3", length + 1), Err(Not::Because(PARTS_UNREADABLE)), "another length");
        let broken = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut value = parts.clone();
            change(&mut value);
            read(&value)
        };
        let cases: Vec<(&str, Box<dyn Fn(&mut serde_json::Value)>)> = vec![
            ("a format of a later program", Box::new(|v| v["format"] = 2.into())),
            ("no program before the package", Box::new(|v| v["base"] = 0.into())),
            ("a program that ends elsewhere", Box::new(|v| v["base"] = (package.base + 1).into())),
            ("a hash that is none", Box::new(|v| v["stub_sha256"] = "nothing".into())),
            ("a file hash that is none", Box::new(|v| v["entries"][0]["sha256"] = "nothing".into())),
            ("a name that leads out", Box::new(|v| v["entries"][0]["path"] = "..\\x".into())),
            ("a name twice, whatever its case", Box::new(|v| v["entries"][1]["path"] = "A.TXT".into())),
            ("a way of packing that is none", Box::new(|v| v["entries"][0]["method"] = 9.into())),
            ("a way that is KEPT, which no setup made by --pack has", Box::new(|v| v["entries"][0]["method"] = 2.into())),
            ("stored bytes of a file that is stored as it is, of another length", Box::new(|v| v["entries"][1]["stored"] = 4999.into())),
            ("a file before the header", Box::new(|v| v["entries"][0]["offset"] = 3.into())),
            ("files in another order than their bytes", Box::new(|v| v["entries"].as_array_mut().unwrap().swap(0, 1))),
            ("a file that goes past the index", Box::new(|v| v["entries"][2]["stored"] = 99_999_999u64.into())),
            ("a file of no bytes that is not an empty one", Box::new(|v| v["entries"][3]["size"] = 1.into())),
            ("an empty file that is not the hash of nothing", Box::new(|v| v["entries"][3]["sha256"] = "00".repeat(32).into())),
            ("a header that does not reach the end", Box::new(|v| v["header"]["index_length"] = 1.into())),
            ("no entries key", Box::new(|v| {
                v.as_object_mut().unwrap().remove("entries");
            })),
        ];
        for (what, change) in &cases {
            assert_eq!(broken(change.as_ref()), Err(Not::Because(PARTS_UNREADABLE)), "{what}");
        }
        assert_eq!(read_parts(b"not json", "1.2.3", length), Err(Not::Because(PARTS_UNREADABLE)));
        assert_eq!(read_parts(b"", "1.2.3", length), Err(Not::Because(PARTS_UNREADABLE)));
    }

    #[test]
    fn the_files_wanted_are_those_the_install_has_no_record_of_or_whose_record_no_longer_fits() {
        let (_dir, exe, parts) = parts_of_a_setup();
        let length = std::fs::metadata(&exe).unwrap().len();
        let parts = read_parts(&serde_json::to_vec(&parts).unwrap(), "1.2.3", length).unwrap();
        let install = tempfile::tempdir().unwrap();
        let files: [(&str, &[u8]); 4] = [("a.txt", b"hello"), ("b.bin", &noisy(5000, 1)), ("c\\d.txt", &[b'x'; 9000]), ("empty", b"")];
        make_install(install.path(), &files, true);
        let records = records_of(install.path());
        let wanted = |records: &HashMap<String, Record>| plan(&parts, records, install.path()).wanted;
        assert_eq!(wanted(&records), [false, false, false, false], "all as they were written down");
        // no record of a file
        let mut fewer = records.clone();
        fewer.remove("b.bin");
        assert_eq!(wanted(&fewer), [false, true, false, false]);
        // another hash in the record than the setup's: the setup has another file
        let mut other = records.clone();
        other.get_mut("a.txt").unwrap().sha256[0] ^= 1;
        assert_eq!(wanted(&other), [true, false, false, false]);
        // the file in the folder has another size, another time, or is not there
        std::fs::write(install.path().join("a.txt"), b"hellO!").unwrap();
        assert_eq!(wanted(&records), [true, false, false, false]);
        std::fs::write(install.path().join("a.txt"), b"hello").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(install.path().join("b.bin")).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600)).unwrap();
        drop(file);
        assert_eq!(wanted(&records), [true, true, false, false], "a.txt was written again, and b.bin touched");
        std::fs::remove_file(install.path().join("c\\d.txt")).unwrap();
        assert_eq!(wanted(&records), [true, true, true, false]);
        // the plan: the program, then the pieces of the files that are wanted; the empty one has none
        let wanted_all = plan(&parts, &HashMap::new(), install.path());
        assert!(wanted_all.wanted.iter().all(|wanted| *wanted));
        let last = &parts.entries[2];
        assert_eq!(wanted_all.ranges, [(0, parts.base + last.offset + last.stored)], "the pieces are a header apart, and are one request");
        assert_eq!(wanted_all.bytes, parts.base + last.offset + last.stored);
    }

    #[test]
    fn the_installers_package_opens_one_that_is_laid_out_here_and_the_files_left_out_are_kept() {
        let (_dir, exe, parts) = parts_of_a_setup();
        let length = std::fs::metadata(&exe).unwrap().len();
        let parts = read_parts(&serde_json::to_vec(&parts).unwrap(), "1.2.3", length).unwrap();
        let bytes = std::fs::read(&exe).unwrap();
        let wanted = [false, true, false, true];
        let layout = layout("1.2.3", &parts, &wanted);
        // the setup put together by hand: the program, the header, the bytes of the files wanted, the index
        let mut out = bytes[..parts.base as usize].to_vec();
        out.extend_from_slice(&layout.header);
        for (entry, wanted) in parts.entries.iter().zip(wanted) {
            if wanted {
                out.extend_from_slice(&bytes[(parts.base + entry.offset) as usize..][..entry.stored as usize]);
            }
        }
        assert_eq!(out.len() as u64, parts.base + layout.index_at);
        out.extend_from_slice(&layout.index);
        let dir = tempfile::tempdir().unwrap();
        let made = dir.path().join("Made.exe");
        std::fs::write(&made, out).unwrap();
        let mut package = Package::open(&made).unwrap();
        assert_eq!(package.version, "1.2.3");
        assert_eq!(package.base, parts.base);
        let methods: Vec<(&str, u8)> = package.entries.iter().map(|entry| (entry.path.as_str(), entry.method)).collect();
        assert_eq!(methods, [("a.txt", KEPT), ("b.bin", STORED), ("c\\d.txt", KEPT), ("empty", STORED)]);
        assert_eq!(package.entries[0].sha256, parts.entries[0].sha256);
        assert_eq!((package.entries[0].offset, package.entries[0].stored), (0, 0));
        let mut back = Vec::new();
        let entry = package.entries[1].clone();
        package.extract(&entry, &mut back, &mut |_| {}).unwrap();
        assert_eq!(back, noisy(5000, 1));
        let entry = package.entries[3].clone();
        package.extract(&entry, &mut Vec::new(), &mut |_| {}).unwrap();
    }

    #[test]
    fn what_inflates_to_more_than_a_file_is_refused_and_what_follows_a_stream_is_left_alone() {
        use std::io::Write;
        let data = vec![b'q'; 50_000];
        let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
        encoder.write_all(&data).unwrap();
        let stored = encoder.finish().unwrap();
        let wanted = (data.len() as u64, <[u8; 32]>::from(Sha256::digest(&data)));
        // as it is, in pieces
        let mut check = Check::of(DEFLATED);
        for piece in stored.chunks(7) {
            check.feed(piece, data.len() as u64).unwrap();
        }
        assert_eq!(check.finish().unwrap(), wanted);
        // with more after the stream, as the installer's unpacking does not read it
        let mut check = Check::of(DEFLATED);
        check.feed(&[stored.clone(), vec![0u8; 100]].concat(), data.len() as u64).unwrap();
        assert_eq!(check.finish().unwrap(), wanted);
        // a stream that holds more than the file is said to be
        let mut check = Check::of(DEFLATED);
        assert_eq!(check.feed(&stored, 1000), Err(Not::Because(DAMAGED)));
        // damage: bytes that are no stream, and a stream that stops
        let mut check = Check::of(DEFLATED);
        let result = check.feed(&[0xFFu8; 64], data.len() as u64).and_then(|()| check.finish().map(|_| ()));
        assert_eq!(result, Err(Not::Because(DAMAGED)));
        let mut check = Check::of(DEFLATED);
        check.feed(&stored[..stored.len() / 2], data.len() as u64).unwrap();
        assert_ne!(check.finish().ok(), Some(wanted));
        // stored as it is
        let mut check = Check::of(STORED);
        check.feed(&data, data.len() as u64).unwrap();
        assert_eq!(check.finish().unwrap(), wanted);
        assert_eq!(Check::of(STORED).feed(&data, 10), Err(Not::Because(DAMAGED)));
    }
}
