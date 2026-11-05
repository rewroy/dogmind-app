//! The package: what follows the executable's last section, so that it is one file and a signature can still go on its end. Each file is deflated by itself, so that a run can skip
//! what is already in place and go on from where another stopped, and nothing is ever held in memory but a block. The index at the end says where each is and what its hash must be.
//!
//!     MAGIC, the index's place, length and hash
//!     each file's bytes
//!     the index

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const MAGIC: &[u8; 16] = b"DogmindSetup\x1a\x01\0\0";
const HEADER: u64 = 16 + 8 + 8 + 32;
const BLOCK: usize = 1 << 20;
const STORED: u8 = 0;
const DEFLATED: u8 = 1;

fn invalid(words: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, words.into())
}

/// A hash in lower case hex.
pub fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A hash from its hex, in either case; none for anything that is not exactly 64 hex digits.
pub fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (at, pair) in text.as_bytes().chunks(2).enumerate() {
        out[at] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Where what follows an executable's sections begins, or where its file ends when nothing does. By its own header, and not from the end: a signature may come after.
pub fn overlay_start(file: &mut (impl Read + Seek)) -> io::Result<u64> {
    fn read_at(file: &mut (impl Read + Seek), at: u64, buffer: &mut [u8]) -> io::Result<()> {
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(buffer)
    }
    let mut word = [0u8; 4];
    read_at(file, 0x3C, &mut word)?;
    let header = u64::from(u32::from_le_bytes(word));
    let mut coff = [0u8; 24];
    read_at(file, header, &mut coff)?;
    if &coff[..4] != b"PE\0\0" {
        return Err(invalid("not an executable"));
    }
    let sections = u64::from(u16::from_le_bytes([coff[6], coff[7]]));
    let optional = u64::from(u16::from_le_bytes([coff[20], coff[21]]));
    let table = header + 24 + optional;
    let mut end = 0u64;
    for section in 0..sections {
        let mut raw = [0u8; 40];
        read_at(file, table + section * 40, &mut raw)?;
        let size = u64::from(u32::from_le_bytes([raw[16], raw[17], raw[18], raw[19]]));
        let start = u64::from(u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]]));
        if size > 0 {
            end = end.max(start + size);
        }
    }
    Ok(end)
}

/// What stays out of a package: symbols are kept by whoever releases it, the mark that a folder was opened to the sandbox belongs to that machine, and the rest are what an update left.
pub fn is_shipped(relative: &str) -> bool {
    let name = relative.rsplit('\\').next().unwrap_or(relative).to_lowercase();
    // (`NAME.previous-TIME.EXT`: a file the app or its engine was running from when a new build came in, moved aside for it)
    !(name.ends_with(".pdb") || name == ".open-to-sandbox" || name.ends_with(".dogmind-new") || name.contains(".previous-"))
}

/// A path in the package: below its folder, and nothing that could lead out of it or into a stream of a file.
pub fn is_safe(relative: &str) -> bool {
    // (Windows opens `a.` and `a ` as `a`, so that `.. ` can be `..`; and a device has its name in any folder)
    const DEVICES: [&str; 26] = ["con", "prn", "aux", "nul", "conin$", "conout$", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "com0", "lpt0"];
    !relative.is_empty()
        && relative.split('\\').all(|part| !part.is_empty() && !part.ends_with(['.', ' ']) && !DEVICES.contains(&part.to_ascii_lowercase().as_str()))
        && !relative.contains(['/', ':', '*', '?', '"', '<', '>', '|'])
        && !relative.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// From the package's folder, with backslashes.
    pub path: String,
    pub size: u64,
    pub offset: u64,
    pub stored: u64,
    pub method: u8,
    pub sha256: [u8; 32],
}

/// What a package says of itself.
pub struct Package {
    file: File,
    base: u64,
    pub version: String,
    pub entries: Vec<Entry>,
}

fn take_u16(bytes: &mut &[u8]) -> io::Result<u16> {
    let (head, tail) = bytes.split_at_checked(2).ok_or_else(|| invalid("the index ends early"))?;
    *bytes = tail;
    Ok(u16::from_le_bytes([head[0], head[1]]))
}

fn take_u64(bytes: &mut &[u8]) -> io::Result<u64> {
    let (head, tail) = bytes.split_at_checked(8).ok_or_else(|| invalid("the index ends early"))?;
    *bytes = tail;
    Ok(u64::from_le_bytes(head.try_into().expect("eight bytes")))
}

fn take_text(bytes: &mut &[u8]) -> io::Result<String> {
    let length = usize::from(take_u16(bytes)?);
    let (head, tail) = bytes.split_at_checked(length).ok_or_else(|| invalid("the index ends early"))?;
    *bytes = tail;
    String::from_utf8(head.to_vec()).map_err(|_| invalid("a name in the index is not text"))
}

fn index_bytes(version: &str, entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    let text = |out: &mut Vec<u8>, words: &str| {
        out.extend_from_slice(&(words.len() as u16).to_le_bytes());
        out.extend_from_slice(words.as_bytes());
    };
    text(&mut out, version);
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for entry in entries {
        text(&mut out, &entry.path);
        out.extend_from_slice(&entry.size.to_le_bytes());
        out.extend_from_slice(&entry.offset.to_le_bytes());
        out.extend_from_slice(&entry.stored.to_le_bytes());
        out.push(entry.method);
        out.extend_from_slice(&entry.sha256);
    }
    out
}

impl Package {
    /// The package on the end of this file, checked as far as its index says: a download that stopped early does not get as far as anything being written.
    pub fn open(path: &Path) -> io::Result<Package> {
        let mut file = File::open(path)?;
        let base = overlay_start(&mut file)?;
        file.seek(SeekFrom::Start(base))?;
        let mut header = [0u8; HEADER as usize];
        if file.read_exact(&mut header).is_err() || &header[..16] != MAGIC {
            return Err(invalid("no package on the end of this file"));
        }
        let index_at = u64::from_le_bytes(header[16..24].try_into().expect("eight bytes"));
        let index_length = u64::from_le_bytes(header[24..32].try_into().expect("eight bytes"));
        if index_length > 256 << 20 || base + index_at + index_length > file.metadata()?.len() {
            return Err(invalid("this file is cut short: it did not all arrive"));
        }
        file.seek(SeekFrom::Start(base + index_at))?;
        let mut index = vec![0u8; index_length as usize];
        file.read_exact(&mut index)?;
        if Sha256::digest(&index).as_slice() != &header[32..64] {
            return Err(invalid("this file is damaged: it did not all arrive as it was made"));
        }
        let mut bytes = index.as_slice();
        let version = take_text(&mut bytes)?;
        let count = take_u64(&mut bytes)?;
        let mut entries = Vec::with_capacity(count.min(1 << 20) as usize);
        for _ in 0..count {
            let path = take_text(&mut bytes)?;
            if !is_safe(&path) {
                return Err(invalid(format!("{path} would land outside its folder")));
            }
            let (size, offset, stored) = (take_u64(&mut bytes)?, take_u64(&mut bytes)?, take_u64(&mut bytes)?);
            let (head, tail) = bytes.split_at_checked(33).ok_or_else(|| invalid("the index ends early"))?;
            bytes = tail;
            entries.push(Entry { path, size, offset, stored, method: head[0], sha256: head[1..].try_into().expect("thirty-two bytes") });
        }
        Ok(Package { file, base, version, entries })
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|entry| entry.size).sum()
    }

    /// This file's content to `out`, block by block, its hash checked at the end. `on_bytes` hears each block as it is written out.
    pub fn extract(&mut self, entry: &Entry, out: &mut impl Write, on_bytes: &mut dyn FnMut(u64)) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(self.base + entry.offset))?;
        let raw = (&mut self.file).take(entry.stored);
        let mut digest = Sha256::new();
        let mut buffer = vec![0u8; BLOCK];
        let mut written = 0u64;
        let mut pump = |source: &mut dyn Read| -> io::Result<()> {
            loop {
                let count = source.read(&mut buffer)?;
                if count == 0 {
                    return Ok(());
                }
                digest.update(&buffer[..count]);
                out.write_all(&buffer[..count])?;
                written += count as u64;
                on_bytes(count as u64);
            }
        };
        match entry.method {
            STORED => pump(&mut std::io::BufReader::with_capacity(BLOCK, raw))?,
            DEFLATED => pump(&mut flate2::read::DeflateDecoder::new(std::io::BufReader::with_capacity(BLOCK, raw)))?,
            other => return Err(invalid(format!("{}: a way of packing this program does not know ({other})", entry.path))),
        }
        if written != entry.size || digest.finalize().as_slice() != entry.sha256 {
            return Err(invalid(format!("{} did not unpack as it was packed: this file is damaged", entry.path)));
        }
        Ok(())
    }
}

/// The hash of a file on the drive.
pub fn hash_file(path: &Path, on_bytes: &mut dyn FnMut(u64)) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; BLOCK];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(digest.finalize().into());
        }
        digest.update(&buffer[..count]);
        on_bytes(count as u64);
    }
}

/// Everything in `folder` that ships, in order, with paths from it.
fn files_of(folder: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut found = Vec::new();
    let mut waiting = vec![folder.to_path_buf()];
    while let Some(current) = waiting.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                waiting.push(entry.path());
            } else if kind.is_file() {
                let relative = entry.path().strip_prefix(folder).map_err(|_| invalid("a file outside its folder"))?.to_string_lossy().into_owned();
                if is_shipped(&relative) {
                    if !is_safe(&relative) {
                        return Err(invalid(format!("{relative}: a name this package cannot hold")));
                    }
                    found.push((relative, entry.path()));
                }
            }
        }
    }
    found.sort();
    Ok(found)
}

/// What a package holds, for whoever made it to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PackedSize {
    pub files: u64,
    pub bytes: u64,
    pub stored: u64,
}

/// `stub`, then `folder` as a package: one file, written beside `out` until it is whole. `level`: of deflate, 0 to 9.
pub fn pack(stub: &Path, folder: &Path, out: &Path, version: &str, level: u32, on_file: &mut dyn FnMut(&str)) -> io::Result<PackedSize> {
    let mut source = File::open(stub)?;
    let stub_length = overlay_start(&mut source)?;
    source.seek(SeekFrom::Start(0))?;
    let partial = out.with_extension("packing");
    let mut target = File::create(&partial)?;
    std::io::copy(&mut (&mut source).take(stub_length), &mut target)?;
    target.write_all(&[0u8; HEADER as usize])?;
    let mut entries = Vec::new();
    let mut packed = PackedSize::default();
    let mut at = HEADER;
    let mut buffer = vec![0u8; BLOCK];
    for (path, file) in files_of(folder)? {
        on_file(&path);
        let mut digest = Sha256::new();
        let mut size = 0u64;
        // Deflated; what will not get smaller, as what is already packed, goes in as it is.
        let mut encoder = flate2::write::DeflateEncoder::new(BufWriter::with_capacity(BLOCK, &mut target), flate2::Compression::new(level));
        let mut input = File::open(&file)?;
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
            encoder.write_all(&buffer[..count])?;
            size += count as u64;
        }
        encoder.finish()?.flush()?;
        let mut stored = target.stream_position()? - (stub_length + at);
        let mut method = DEFLATED;
        if stored >= size {
            target.set_len(stub_length + at)?;
            target.seek(SeekFrom::Start(stub_length + at))?;
            input.seek(SeekFrom::Start(0))?;
            std::io::copy(&mut input, &mut target)?;
            stored = size;
            method = STORED;
        }
        entries.push(Entry { path, size, offset: at, stored, method, sha256: digest.finalize().into() });
        at += stored;
        packed.files += 1;
        packed.bytes += size;
        packed.stored += stored;
    }
    let index = index_bytes(version, &entries);
    target.write_all(&index)?;
    target.seek(SeekFrom::Start(stub_length))?;
    target.write_all(MAGIC)?;
    target.write_all(&at.to_le_bytes())?;
    target.write_all(&(index.len() as u64).to_le_bytes())?;
    target.write_all(&Sha256::digest(&index))?;
    target.sync_all()?;
    drop(target);
    std::fs::rename(&partial, out)?;
    Ok(packed)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// An executable to put a package on: this test's own, whose sections say where it ends.
    pub fn own_exe() -> PathBuf {
        std::env::current_exe().unwrap()
    }

    pub fn tree(root: &Path, files: &[(&str, &[u8])]) {
        for (name, bytes) in files {
            let place = root.join(name);
            std::fs::create_dir_all(place.parent().unwrap()).unwrap();
            std::fs::write(place, bytes).unwrap();
        }
    }

    /// As random as it needs to be for deflate to find nothing.
    pub fn noise(length: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        (0..length).map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        }).collect()
    }

    #[test]
    fn a_package_holds_what_was_there_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("in");
        let big = vec![7u8; 3 << 20];
        let random = noise(1 << 20);
        tree(&folder, &[("a.txt", b"hello"), ("deep\\er\\b.bin", &big), ("deep\\noise", &random), ("\u{65e5}\u{672c}\\c", b"x"), ("empty", b""), ("dogmind_app.pdb", b"symbols"), ("tools\\.open-to-sandbox", b""), ("dogmind-app.previous-1.exe", b"old")]);
        let out = dir.path().join("Setup.exe");
        let mut heard = Vec::new();
        let packed = pack(&own_exe(), &folder, &out, "1.2.3", 6, &mut |path| heard.push(path.to_string())).unwrap();
        assert_eq!(packed.files, 5);
        assert!(heard.contains(&"deep\\er\\b.bin".to_string()));
        assert!(!dir.path().join("Setup.packing").exists(), "only the whole file is left");

        let mut package = Package::open(&out).unwrap();
        assert_eq!(package.version, "1.2.3");
        let names: Vec<String> = package.entries.iter().map(|entry| entry.path.clone()).collect();
        assert_eq!(names, ["a.txt", "deep\\er\\b.bin", "deep\\noise", "empty", "\u{65e5}\u{672c}\\c"]);
        let entries = package.entries.clone();
        let of = |name: &str| entries.iter().find(|entry| entry.path == name).unwrap().clone();
        assert!(of("deep\\er\\b.bin").stored < 64 * 1024, "what repeats gets small");
        assert_eq!(of("deep\\noise").method, STORED, "what does not goes in as it is");
        for (name, wanted) in [("a.txt", &b"hello"[..]), ("deep\\er\\b.bin", &big[..]), ("deep\\noise", &random[..]), ("empty", &b""[..])] {
            let (mut back, mut heard) = (Vec::new(), 0);
            package.extract(&of(name), &mut back, &mut |count| heard += count).unwrap();
            assert_eq!(back, wanted, "{name}");
            assert_eq!(heard, wanted.len() as u64);
        }
        assert_eq!(package.total_bytes(), packed.bytes);
    }

    #[test]
    fn the_stub_stays_a_program_and_a_package_comes_off_again() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("in");
        tree(&folder, &[("a", b"1")]);
        let once = dir.path().join("once.exe");
        pack(&own_exe(), &folder, &once, "1", 6, &mut |_| {}).unwrap();
        let length = overlay_start(&mut File::open(own_exe()).unwrap()).unwrap();
        assert_eq!(overlay_start(&mut File::open(&once).unwrap()).unwrap(), length, "what was added is past the sections");
        // Packed from a program that already has a package: its own goes, and only the new one is on the end.
        tree(&folder, &[("b", b"2")]);
        let twice = dir.path().join("twice.exe");
        pack(&once, &folder, &twice, "2", 6, &mut |_| {}).unwrap();
        assert_eq!(Package::open(&twice).unwrap().entries.len(), 2);
    }

    #[test]
    fn a_signature_on_the_end_is_nothing_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("in");
        tree(&folder, &[("a", b"1")]);
        let out = dir.path().join("signed.exe");
        pack(&own_exe(), &folder, &out, "1", 6, &mut |_| {}).unwrap();
        std::fs::OpenOptions::new().append(true).open(&out).unwrap().write_all(&[9u8; 4096]).unwrap();
        let mut package = Package::open(&out).unwrap();
        let entry = package.entries[0].clone();
        let mut back = Vec::new();
        package.extract(&entry, &mut back, &mut |_| {}).unwrap();
        assert_eq!(back, b"1");
    }

    #[test]
    fn damage_is_said_and_nothing_comes_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("in");
        tree(&folder, &[("a", &noise(100_000))]);
        let out = dir.path().join("whole.exe");
        pack(&own_exe(), &folder, &out, "1", 6, &mut |_| {}).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        let start = overlay_start(&mut File::open(&out).unwrap()).unwrap() as usize;

        let cut = dir.path().join("cut.exe");
        std::fs::write(&cut, &bytes[..bytes.len() - 200]).unwrap();
        assert!(Package::open(&cut).err().unwrap().to_string().contains("did not all arrive"));

        let mut flipped = bytes.clone();
        flipped[start + HEADER as usize + 50_000] ^= 1;
        let flip = dir.path().join("flip.exe");
        std::fs::write(&flip, &flipped).unwrap();
        let mut package = Package::open(&flip).unwrap();
        let entry = package.entries[0].clone();
        assert!(package.extract(&entry, &mut Vec::new(), &mut |_| {}).unwrap_err().to_string().contains("damaged"));

        assert!(Package::open(&own_exe()).is_err(), "a program with none");
    }

    #[test]
    fn a_name_cannot_lead_out_of_its_folder() {
        for bad in ["..\\x", "a\\..\\..\\x", "C:\\x", "\\x", "a:stream", "a\\\\b", "", ".", "a/b", ".. \\x", "..\\ x\\..  \\y", "a\\... \\b", "a.", "a ", "NUL", "tools\\con", "a\nb"] {
            assert!(!is_safe(bad), "{bad}");
        }
        for good in ["a", "a\\b.txt", "\u{65e5}\u{672c}\\c d", "node_modules\\.bin\\x"] {
            assert!(is_safe(good), "{good}");
        }
        assert!(is_shipped("a.exe") && !is_shipped("x\\A.PDB") && !is_shipped("tools\\.open-to-sandbox") && !is_shipped("dogmind-app.previous-002327.exe") && !is_shipped("x.dll.dogmind-new"));
        assert!(!is_shipped("straydog.previous-163000.exe") && !is_shipped("cublasLt64_13.previous-163000.dll") && !is_shipped("fonts\\BIZUDGothic-Regular.previous-163000.ttf"));
    }
}
