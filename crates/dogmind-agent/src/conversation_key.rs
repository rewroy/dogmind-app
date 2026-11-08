//! The key of the conversations kept on this PC, when the person has chosen to have them encrypted.
//!
//! A random 256-bit data key (the DEK) encrypts the conversations. It is made once and never changes, so changing the passphrase
//! never touches the conversations. It lies on the disk only wrapped (AES-256-GCM), in `conversation-key.json` beside the settings,
//! under keys derived with scrypt from the person's passphrase and from a recovery code shown once (about 160 bits), and, if the
//! person chose "remember on this PC", under DPAPI for the Windows user, so that the app asks nothing at its start. Any one of the
//! three unwraps the same key. Losing the passphrase and the recovery code loses the conversations, by design: nothing else opens them.
//! The DEK is in memory only, from an unlock to the end of the program, and is cleared when its holder is dropped.
//!
//! The key model is the one of the Python Dogmind (`security/conversation_key.py`): the same wrapping, the same scrypt cost and the
//! same recovery code. The keystore file is this app's own.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

pub const KEYSTORE: &str = "conversation-key.json";
const VERSION: u32 = 1;
/// scrypt's cost: 2^15 (32 MiB, a few tens of milliseconds), r 8, p 1, as the Python Dogmind has it.
const SCRYPT_N: u32 = 1 << 15;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
/// What a keystore may ask of the machine to be opened: it is read from a file that anyone with the folder can change.
const SCRYPT_MOST_BYTES: u64 = 256 << 20;
const SALT_BYTES: usize = 16;
pub(crate) const NONCE_BYTES: usize = 12;
pub(crate) const TAG_BYTES: usize = 16;
const DEK_BYTES: usize = 32;
/// 20 random bytes, written as 32 letters and digits: about 160 bits, a second factor and not a PIN.
const RECOVERY_BYTES: usize = 20;
const RECOVERY_ALPHABET: &[u8; 32] = b"ABCDEFGHJKMNPQRSTVWXYZ0123456789";
/// What DPAPI mixes into what it wraps, so that another program's blob of the same user is not taken for ours.
const DPAPI_ENTROPY: &[u8] = b"dogmind conversation key 1";

// What a person is told. English here as all the agent's words; `lang` in the app has each in Japanese.
pub const WRONG_SECRET: &str = "That is not right.";
pub const LOCKED: &str = "The conversations are encrypted and not unlocked yet.";
pub const OTHER_KEY: &str = "This conversation is encrypted with another key, which is not the one of this PC.";
pub const NOT_REMEMBERED: &str = "Windows would not keep the key for this PC, so nothing was changed.";
pub const EMPTY_PASSPHRASE: &str = "A passphrase cannot be empty.";
pub const KEYSTORE_DAMAGED: &str = "The file that holds the key of the conversations is damaged.";
pub const NEWER_FORMAT: &str = "This conversation was written by a newer version of Dogmind.";
pub const WORDS: [&str; 7] = [WRONG_SECRET, LOCKED, OTHER_KEY, NOT_REMEMBERED, EMPTY_PASSPHRASE, KEYSTORE_DAMAGED, NEWER_FORMAT];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// No keystore: the conversations are not encrypted.
    Missing,
    Corrupt(String),
    /// The passphrase or the recovery code is not the one.
    Wrong,
    /// The keystore has no copy for DPAPI, or DPAPI will not open it here (another PC, another user, a new Windows).
    NotRemembered,
    Empty,
    Exists,
    Failed(String),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Missing => write!(out, "the conversations are not encrypted: there is no key file"),
            KeyError::Corrupt(why) => write!(out, "{KEYSTORE_DAMAGED} ({why})"),
            KeyError::Wrong => out.write_str(WRONG_SECRET),
            KeyError::NotRemembered => out.write_str("this PC does not hold the key"),
            KeyError::Empty => out.write_str(EMPTY_PASSPHRASE),
            KeyError::Exists => out.write_str("the conversations are already encrypted"),
            KeyError::Failed(why) => out.write_str(why),
        }
    }
}

impl std::error::Error for KeyError {}

/// The data key, and what encrypts with it. Cleared when the last holder lets go.
pub struct Cipher {
    dek: [u8; DEK_BYTES],
    key: LessSafeKey,
    fingerprint: String,
}

impl Drop for Cipher {
    fn drop(&mut self) {
        for byte in &mut self.dek {
            // SAFETY: a plain byte of this value; volatile, so that the clearing is not optimized away.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "Cipher({})", self.fingerprint)
    }
}

fn random(bytes: &mut [u8]) -> Result<(), KeyError> {
    SystemRandom::new().fill(bytes).map_err(|_| KeyError::Failed("the system has no random bytes to give".into()))
}

pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N], KeyError> {
    let mut bytes = [0u8; N];
    random(&mut bytes)?;
    Ok(bytes)
}

fn fingerprint_of(dek: &[u8; DEK_BYTES]) -> String {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, dek), b"dogmind conversation-dek-fp");
    hex(&tag.as_ref()[..16])
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok()).collect()
}

impl Cipher {
    fn from_dek(dek: [u8; DEK_BYTES]) -> Arc<Cipher> {
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &dek).expect("a 32-byte key"));
        let cipher = Arc::new(Cipher { fingerprint: fingerprint_of(&dek), dek, key });
        register(&cipher);
        cipher
    }

    /// A fresh key, for a test or a new keystore.
    pub(crate) fn generate() -> Result<Arc<Cipher>, KeyError> {
        Ok(Cipher::from_dek(random_bytes::<DEK_BYTES>()?))
    }

    /// What tells this key from another, and a wrong secret from a right one: 16 bytes of an HMAC under the key, which say nothing of it.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// A nonce of 12 fresh random bytes, then the ciphertext and its tag. `aad` is bound to it: it opens only with the same.
    pub fn seal(&self, plain: &[u8], aad: &[u8]) -> Vec<u8> {
        let nonce = random_bytes::<NONCE_BYTES>().expect("the system gives random bytes");
        let mut out = Vec::with_capacity(NONCE_BYTES + plain.len() + TAG_BYTES);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(plain);
        let tag = self.key.seal_in_place_separate_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut out[NONCE_BYTES..]).expect("sealing in place cannot fail");
        out.extend_from_slice(tag.as_ref());
        out
    }

    /// As `open`, for a blob that is not wanted after: opened where it lies, with no copy of it.
    pub fn open_owned(&self, mut blob: Vec<u8>, aad: &[u8]) -> Option<Vec<u8>> {
        if blob.len() < NONCE_BYTES + TAG_BYTES {
            return None;
        }
        let nonce = Nonce::try_assume_unique_for_key(&blob[..NONCE_BYTES]).ok()?;
        let plain_len = self.key.open_within(nonce, Aad::from(aad), &mut blob, NONCE_BYTES..).ok()?.len();
        blob.truncate(plain_len);
        Some(blob)
    }

    /// What `seal` made, opened; none if one byte of it, or the `aad`, is not as it was.
    pub fn open(&self, blob: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
        if blob.len() < NONCE_BYTES + TAG_BYTES {
            return None;
        }
        let nonce = Nonce::try_assume_unique_for_key(&blob[..NONCE_BYTES]).ok()?;
        let mut work = blob[NONCE_BYTES..].to_vec();
        let plain_len = self.key.open_in_place(nonce, Aad::from(aad), &mut work).ok()?.len();
        work.truncate(plain_len);
        Some(work)
    }
}

// The keys that are unlocked in this program, by fingerprint: a file says which key it is under, and is opened with it if it is here.
static UNLOCKED: Mutex<Vec<Arc<Cipher>>> = Mutex::new(Vec::new());

fn register(cipher: &Arc<Cipher>) {
    let mut held = UNLOCKED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !held.iter().any(|known| known.fingerprint == cipher.fingerprint) {
        held.push(cipher.clone());
    }
}

/// The unlocked key that has this fingerprint.
pub fn find(fingerprint: &str) -> Option<Arc<Cipher>> {
    UNLOCKED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().find(|known| known.fingerprint == fingerprint).cloned()
}

/// Whether any key is unlocked in this program.
pub fn any_unlocked() -> bool {
    !UNLOCKED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_empty()
}

/// A key is no longer held for reading: its files stay, and are locked again.
pub fn forget(fingerprint: &str) {
    UNLOCKED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).retain(|known| known.fingerprint != fingerprint);
}

/// Whether the conversations are written encrypted now: what every agent and the window share, so that a change is seen by all of them at once.
pub struct Policy {
    state: Mutex<State>,
}

#[derive(Clone)]
enum State {
    Off,
    /// The person has chosen encryption and the key is not in memory yet: nothing is opened or written.
    Locked,
    On(Arc<Cipher>),
}

impl Default for Policy {
    fn default() -> Policy {
        Policy { state: Mutex::new(State::Off) }
    }
}

impl Policy {
    pub fn off() -> Arc<Policy> {
        Arc::new(Policy { state: Mutex::new(State::Off) })
    }

    pub fn locked() -> Arc<Policy> {
        Arc::new(Policy { state: Mutex::new(State::Locked) })
    }

    pub fn on(cipher: Arc<Cipher>) -> Arc<Policy> {
        Arc::new(Policy { state: Mutex::new(State::On(cipher)) })
    }

    fn set(&self, state: State) {
        *self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
    }

    pub fn set_on(&self, cipher: Arc<Cipher>) {
        self.set(State::On(cipher));
    }

    pub fn set_off(&self) {
        self.set(State::Off);
    }

    pub fn set_locked(&self) {
        self.set(State::Locked);
    }

    pub fn is_locked(&self) -> bool {
        matches!(*self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()), State::Locked)
    }

    /// The key a new file is written under: none when conversations are kept as they are; an error while the key is not unlocked, when nothing may be written.
    pub fn writer(&self) -> Result<Option<Arc<Cipher>>, String> {
        match &*self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            State::Off => Ok(None),
            State::Locked => Err(LOCKED.to_string()),
            State::On(cipher) => Ok(Some(cipher.clone())),
        }
    }
}

/// What a file of text that is kept whole (the notes, an index) says on its first line when it is sealed: the format and the fingerprint of the key. The second line is the text sealed, in base64.
const SEALED_TEXT: &str = "dogmind-sealed 1 ";

fn text_aad(name: &str) -> Vec<u8> {
    format!("dogmind sealed text 1 {name}").into_bytes()
}

/// A file's whole text, sealed under the key with the file's name as additional data: it opens only as that file.
pub fn seal_text(cipher: &Cipher, name: &str, text: &str) -> String {
    format!("{SEALED_TEXT}{}\n{}\n", cipher.fingerprint(), b64e(&cipher.seal(text.as_bytes(), &text_aad(name))))
}

/// Whether what a file holds is text that is sealed, by its first line.
pub fn is_sealed_text(bytes: &[u8]) -> bool {
    bytes.starts_with(SEALED_TEXT.as_bytes())
}

/// A file's text as it is: what a plain file holds, or what a sealed one holds when its key is here. An error in words when it is not (the key is not unlocked, or is another's), or when the file does not open: it is damaged, or it is another file's.
pub fn open_text(name: &str, bytes: &[u8]) -> Result<String, String> {
    if !is_sealed_text(bytes) {
        return String::from_utf8(bytes.to_vec()).map_err(|error| error.to_string());
    }
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    let fingerprint = lines.next().and_then(|first| first.strip_prefix(SEALED_TEXT)).unwrap_or("").trim();
    let cipher = find(fingerprint).ok_or_else(|| if any_unlocked() { OTHER_KEY.to_string() } else { LOCKED.to_string() })?;
    let blob = lines.next().and_then(|line| b64d(line.trim())).ok_or_else(|| "this file is damaged".to_string())?;
    let plain = cipher.open(&blob, &text_aad(name)).ok_or_else(|| "this file does not open: it is damaged, or it is another's".to_string())?;
    String::from_utf8(plain).map_err(|error| error.to_string())
}

/// The recovery code as it is shown: five letters and digits at a time.
fn format_recovery(raw: &[u8; RECOVERY_BYTES]) -> String {
    let mut letters = String::new();
    for place in 0..RECOVERY_BYTES * 8 / 5 {
        let mut group = 0u8;
        for bit in 0..5 {
            let at = place * 5 + bit;
            group = group << 1 | (raw[at / 8] >> (7 - at % 8)) & 1;
        }
        letters.push(char::from(RECOVERY_ALPHABET[group as usize]));
    }
    letters.as_bytes().chunks(5).map(|chunk| std::str::from_utf8(chunk).unwrap_or("")).collect::<Vec<_>>().join("-")
}

/// What a person typed of a code, as it is derived from: without dashes and spaces, in capitals, and with the letters that the alphabet leaves out read as the digits they look like.
fn normalize_recovery(code: &str) -> String {
    code.chars().filter(|c| !c.is_whitespace() && *c != '-').flat_map(char::to_uppercase).map(|c| match c {
        'O' => '0',
        'I' | 'L' => '1',
        other => other,
    }).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScryptCost {
    n: u32,
    r: u32,
    p: u32,
}

/// What the file holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    version: u32,
    kdf: String,
    scrypt: ScryptCost,
    passphrase_salt: String,
    recovery_salt: String,
    dek_wrapped_passphrase: String,
    dek_wrapped_recovery: String,
    /// Only when the person chose to remember the key on this PC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dek_wrapped_dpapi: Option<String>,
    dek_fingerprint: String,
    /// A change of every conversation that has begun and not ended (`encrypt` or `decrypt`): the next start finishes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<String>,
}

/// A change of all the conversations that is under way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    Encrypt,
    Decrypt,
}

impl Pending {
    fn word(self) -> &'static str {
        match self {
            Pending::Encrypt => "encrypt",
            Pending::Decrypt => "decrypt",
        }
    }
}

pub(crate) fn b64e(bytes: &[u8]) -> String {
    crate::images::base64(bytes)
}

pub(crate) fn b64d(text: &str) -> Option<Vec<u8>> {
    b64d_bytes(text.as_bytes())
}

/// What a letter of the standard alphabet is worth; 255 for what is not one.
const B64_VALUE: [u8; 256] = {
    let mut table = [255u8; 256];
    let letters = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut at = 0;
    while at < 64 {
        table[letters[at] as usize] = at as u8;
        at += 1;
    }
    table
};

/// A line of base64 read as the bytes it is, four letters to three bytes at a time: a session's every line goes through here, and a large one has hundreds of thousands.
pub(crate) fn b64d_bytes(text: &[u8]) -> Option<Vec<u8>> {
    let text = text.trim_ascii();
    let mut end = text.len();
    while end > 0 && text[end - 1] == b'=' {
        end -= 1;
    }
    let text = &text[..end];
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let mut groups = text.chunks_exact(4);
    for group in groups.by_ref() {
        let (a, b, c, d) = (B64_VALUE[group[0] as usize], B64_VALUE[group[1] as usize], B64_VALUE[group[2] as usize], B64_VALUE[group[3] as usize]);
        if (a | b | c | d) & 0x80 != 0 {
            return None;
        }
        out.extend_from_slice(&[a << 2 | b >> 4, b << 4 | c >> 2, c << 6 | d]);
    }
    let rest = groups.remainder();
    let value = |at: usize| B64_VALUE[rest[at] as usize];
    match rest.len() {
        0 => {}
        2 => {
            let (a, b) = (value(0), value(1));
            // (what is left over is the padding's zeros: anything else is not what an encoder made)
            if (a | b) & 0x80 != 0 || b & 0x0F != 0 {
                return None;
            }
            out.push(a << 2 | b >> 4);
        }
        _ => {
            let (a, b, c) = (value(0), value(1), value(2));
            if (a | b | c) & 0x80 != 0 || c & 0x03 != 0 {
                return None;
            }
            out.extend_from_slice(&[a << 2 | b >> 4, b << 4 | c >> 2]);
        }
    }
    Some(out)
}

fn derive(secret: &str, salt: &[u8], cost: &ScryptCost) -> Result<[u8; 32], KeyError> {
    if !cost.n.is_power_of_two() || cost.n < 2 || cost.r == 0 || cost.p == 0 || 128 * u64::from(cost.n) * u64::from(cost.r) > SCRYPT_MOST_BYTES {
        return Err(KeyError::Corrupt("its scrypt cost is not one this app uses".into()));
    }
    let params = scrypt::Params::new(cost.n.trailing_zeros() as u8, cost.r, cost.p, 32).map_err(|error| KeyError::Corrupt(error.to_string()))?;
    let mut key = [0u8; 32];
    scrypt::scrypt(secret.as_bytes(), salt, &params, &mut key).map_err(|error| KeyError::Failed(error.to_string()))?;
    Ok(key)
}

fn slot_aad(slot: &str) -> Vec<u8> {
    format!("dogmind conversation key 1 {slot}").into_bytes()
}

/// The data key under a key encryption key: nonce, ciphertext and tag, in base64.
fn wrap(dek: &[u8; DEK_BYTES], kek: &[u8; 32], slot: &str) -> Result<String, KeyError> {
    let wrapper = Cipher { dek: *kek, key: LessSafeKey::new(UnboundKey::new(&AES_256_GCM, kek).expect("a 32-byte key")), fingerprint: String::new() };
    Ok(b64e(&wrapper.seal(dek, &slot_aad(slot))))
}

fn unwrap_with(blob: &str, kek: &[u8; 32], slot: &str) -> Option<[u8; DEK_BYTES]> {
    let wrapper = Cipher { dek: *kek, key: LessSafeKey::new(UnboundKey::new(&AES_256_GCM, kek).expect("a 32-byte key")), fingerprint: String::new() };
    wrapper.open(&b64d(blob)?, &slot_aad(slot))?.try_into().ok()
}

#[cfg(windows)]
fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};
    let blob = |bytes: &[u8]| CRYPT_INTEGER_BLOB { cbData: bytes.len() as u32, pbData: bytes.as_ptr() as *mut u8 };
    let (input, entropy) = (blob(data), blob(DPAPI_ENTROPY));
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: the blobs point at slices that outlive the call; the output is the system's, freed below with LocalFree.
    let done = unsafe {
        if protect {
            CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        } else {
            CryptUnprotectData(&input, std::ptr::null_mut(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        }
    };
    if done == 0 || output.pbData.is_null() {
        return Err(format!("DPAPI {} failed: {}", if protect { "wrapping" } else { "unwrapping" }, std::io::Error::last_os_error()));
    }
    // SAFETY: the system gave `cbData` bytes at `pbData`; they are copied out, cleared and freed.
    Ok(unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        LocalFree(output.pbData.cast());
        bytes
    })
}

#[cfg(not(windows))]
fn dpapi(_data: &[u8], _protect: bool) -> Result<Vec<u8>, String> {
    Err("DPAPI is Windows' own".into())
}

/// The file of keys, and what it takes to change it.
#[derive(Debug, Clone)]
pub struct Keystore {
    path: PathBuf,
    stored: Stored,
}

pub fn path_in(folder: &Path) -> PathBuf {
    folder.join(KEYSTORE)
}

/// Whether the person has chosen to have the conversations encrypted: there is a key file.
pub fn exists(folder: &Path) -> bool {
    path_in(folder).is_file()
}

impl Keystore {
    pub fn load(folder: &Path) -> Result<Keystore, KeyError> {
        let path = path_in(folder);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(KeyError::Missing),
            Err(error) => return Err(KeyError::Failed(format!("{}: {error}", path.display()))),
        };
        let stored: Stored = serde_json::from_str(&text).map_err(|error| KeyError::Corrupt(error.to_string()))?;
        if stored.version != VERSION || stored.kdf != "scrypt" {
            return Err(KeyError::Corrupt("a version this app does not know".into()));
        }
        Ok(Keystore { path, stored })
    }

    /// Whole or not at all: beside it first, on the disk, and then renamed over it.
    fn save(&self) -> Result<(), KeyError> {
        let text = serde_json::to_string_pretty(&self.stored).map_err(|error| KeyError::Failed(error.to_string()))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| KeyError::Failed(format!("{}: {error}", parent.display())))?;
        }
        let part = self.path.with_extension("json.part");
        std::fs::File::create(&part)
            .and_then(|mut file| {
                std::io::Write::write_all(&mut file, text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&part, &self.path))
            .map_err(|error| KeyError::Failed(format!("{}: {error}", self.path.display())))
    }

    fn cost() -> ScryptCost {
        ScryptCost { n: SCRYPT_N, r: SCRYPT_R, p: SCRYPT_P }
    }

    /// A new key for the conversations, wrapped under `passphrase`, under a recovery code (returned: it is shown once and kept nowhere) and, if `remember`, under DPAPI. Nothing is written
    /// unless all of it could be made. The conversations are not changed by this: `pending` says what is to be done to them, and the next start finishes it if this one is cut short.
    pub fn create(folder: &Path, passphrase: &str, remember: bool, pending: Option<Pending>) -> Result<(Keystore, Arc<Cipher>, String), KeyError> {
        if passphrase.is_empty() {
            return Err(KeyError::Empty);
        }
        if exists(folder) {
            return Err(KeyError::Exists);
        }
        let cipher = Cipher::generate()?;
        let recovery_raw = random_bytes::<RECOVERY_BYTES>()?;
        let recovery = format_recovery(&recovery_raw);
        let (pass_salt, rec_salt) = (random_bytes::<SALT_BYTES>()?, random_bytes::<SALT_BYTES>()?);
        let cost = Keystore::cost();
        let dek_wrapped_dpapi = if remember { Some(b64e(&dpapi(&cipher.dek, true).map_err(|_| KeyError::Failed(NOT_REMEMBERED.into()))?)) } else { None };
        let stored = Stored {
            version: VERSION,
            kdf: "scrypt".into(),
            scrypt: cost.clone(),
            passphrase_salt: b64e(&pass_salt),
            recovery_salt: b64e(&rec_salt),
            dek_wrapped_passphrase: wrap(&cipher.dek, &derive(passphrase, &pass_salt, &cost)?, "passphrase")?,
            dek_wrapped_recovery: wrap(&cipher.dek, &derive(&normalize_recovery(&recovery), &rec_salt, &cost)?, "recovery")?,
            dek_wrapped_dpapi,
            dek_fingerprint: cipher.fingerprint.clone(),
            pending: pending.map(|pending| pending.word().to_string()),
        };
        let keystore = Keystore { path: path_in(folder), stored };
        keystore.save()?;
        Ok((keystore, cipher, recovery))
    }

    fn open_slot(&self, secret: &str, salt: &str, wrapped: &str, slot: &str) -> Result<Arc<Cipher>, KeyError> {
        let salt = b64d(salt).ok_or_else(|| KeyError::Corrupt("a salt".into()))?;
        let kek = derive(secret, &salt, &self.stored.scrypt)?;
        let dek = unwrap_with(wrapped, &kek, slot).ok_or(KeyError::Wrong)?;
        let cipher = Cipher::from_dek(dek);
        // (a key that opens but is not the one this file is for: the file was changed)
        if cipher.fingerprint != self.stored.dek_fingerprint {
            forget(&cipher.fingerprint);
            return Err(KeyError::Wrong);
        }
        Ok(cipher)
    }

    pub fn unlock(&self, passphrase: &str) -> Result<Arc<Cipher>, KeyError> {
        self.open_slot(passphrase, &self.stored.passphrase_salt, &self.stored.dek_wrapped_passphrase, "passphrase")
    }

    pub fn unlock_with_recovery(&self, code: &str) -> Result<Arc<Cipher>, KeyError> {
        self.open_slot(&normalize_recovery(code), &self.stored.recovery_salt, &self.stored.dek_wrapped_recovery, "recovery")
    }

    /// With the copy for this Windows user, if there is one and it opens here: nothing is asked.
    pub fn unlock_remembered(&self) -> Result<Arc<Cipher>, KeyError> {
        let wrapped = self.stored.dek_wrapped_dpapi.as_deref().ok_or(KeyError::NotRemembered)?;
        let blob = b64d(wrapped).ok_or(KeyError::NotRemembered)?;
        let dek: [u8; DEK_BYTES] = dpapi(&blob, false).map_err(|_| KeyError::NotRemembered)?.try_into().map_err(|_| KeyError::NotRemembered)?;
        let cipher = Cipher::from_dek(dek);
        if cipher.fingerprint != self.stored.dek_fingerprint {
            forget(&cipher.fingerprint);
            return Err(KeyError::NotRemembered);
        }
        Ok(cipher)
    }

    pub fn is_remembered(&self) -> bool {
        self.stored.dek_wrapped_dpapi.is_some()
    }

    pub fn fingerprint(&self) -> &str {
        &self.stored.dek_fingerprint
    }

    pub fn pending(&self) -> Option<Pending> {
        match self.stored.pending.as_deref() {
            Some("encrypt") => Some(Pending::Encrypt),
            Some("decrypt") => Some(Pending::Decrypt),
            _ => None,
        }
    }

    pub fn set_pending(&mut self, pending: Option<Pending>) -> Result<(), KeyError> {
        let before = self.stored.pending.take();
        self.stored.pending = pending.map(|pending| pending.word().to_string());
        self.save().inspect_err(|_| self.stored.pending = before)
    }

    fn rewrap(&mut self, new: Stored) -> Result<(), KeyError> {
        let old = std::mem::replace(&mut self.stored, new);
        self.save().inspect_err(|_| self.stored = old)
    }

    /// A new passphrase for the same key: the conversations are not touched, and the recovery code still works.
    pub fn change_passphrase(&mut self, cipher: &Cipher, new: &str) -> Result<(), KeyError> {
        if new.is_empty() {
            return Err(KeyError::Empty);
        }
        let salt = random_bytes::<SALT_BYTES>()?;
        let mut stored = self.stored.clone();
        stored.passphrase_salt = b64e(&salt);
        stored.dek_wrapped_passphrase = wrap(&cipher.dek, &derive(new, &salt, &stored.scrypt)?, "passphrase")?;
        self.rewrap(stored)
    }

    /// The forgotten-passphrase way in: the recovery code opens the key, and a new passphrase is set. The recovery code stays as it is.
    pub fn reset_passphrase_with_recovery(&mut self, code: &str, new: &str) -> Result<Arc<Cipher>, KeyError> {
        if new.is_empty() {
            return Err(KeyError::Empty);
        }
        let cipher = self.unlock_with_recovery(code)?;
        self.change_passphrase(&cipher, new)?;
        Ok(cipher)
    }

    /// A new recovery code, shown once, for the same key: the old one stops working.
    pub fn new_recovery_code(&mut self, cipher: &Cipher) -> Result<String, KeyError> {
        let (raw, salt) = (random_bytes::<RECOVERY_BYTES>()?, random_bytes::<SALT_BYTES>()?);
        let code = format_recovery(&raw);
        let mut stored = self.stored.clone();
        stored.recovery_salt = b64e(&salt);
        stored.dek_wrapped_recovery = wrap(&cipher.dek, &derive(&normalize_recovery(&code), &salt, &stored.scrypt)?, "recovery")?;
        self.rewrap(stored)?;
        Ok(code)
    }

    /// Keeps the key for this Windows user, or stops keeping it: the passphrase and the recovery code are as they were.
    pub fn set_remembered(&mut self, cipher: &Cipher, remember: bool) -> Result<(), KeyError> {
        let mut stored = self.stored.clone();
        stored.dek_wrapped_dpapi = if remember { Some(b64e(&dpapi(&cipher.dek, true).map_err(|_| KeyError::Failed(NOT_REMEMBERED.into()))?)) } else { None };
        self.rewrap(stored)
    }

    /// The key file gone: the conversations must have been decrypted before this.
    pub fn remove(folder: &Path) -> Result<(), KeyError> {
        let path = path_in(folder);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(KeyError::Failed(format!("{}: {error}", path.display()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_cost() -> ScryptCost {
        ScryptCost { n: 16, r: 1, p: 1 }
    }

    /// The scrypt of RFC 7914, section 12, first vector: the wrapper derives what the standard says.
    #[test]
    fn scrypt_is_what_the_standard_says() {
        let params = scrypt::Params::new(4, 1, 1, 64).unwrap();
        let mut out = [0u8; 64];
        scrypt::scrypt(b"", b"", &params, &mut out).unwrap();
        assert_eq!(&hex(&out)[..32], "77d6576238657b203b19ca42c18a0497");
        // (and 32 bytes of it as `derive` takes them, under a small cost: the same first bytes)
        let key = derive("", b"", &fast_cost()).unwrap();
        assert_eq!(hex(&key)[..16], hex(&out[..8]));
    }

    /// The recovery code is the Python Dogmind's: 20 bytes as 32 letters of its alphabet, from the most significant bit, in groups of five.
    #[test]
    fn the_recovery_code_is_written_as_the_python_dogmind_writes_it() {
        let raw: [u8; 20] = std::array::from_fn(|at| at as u8);
        assert_eq!(format_recovery(&raw), "AAATE-A2EAY-DARCA-KBJFT-2DJRB-6JBCE-TX");
        let code = format_recovery(&random_bytes::<20>().unwrap());
        assert_eq!(code.len(), 32 + 6);
        assert_eq!(normalize_recovery(&code.to_lowercase().replace('-', " ")), normalize_recovery(&code));
        assert_eq!(normalize_recovery("o0il-1"), "00111", "letters the alphabet leaves out are the digits they look like");
    }

    #[test]
    fn base64_comes_back_and_what_is_not_base64_is_refused() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|at| (at * 37 + 11) as u8).collect();
            assert_eq!(b64d(&b64e(&bytes)).unwrap(), bytes, "{len} bytes");
        }
        assert!(b64d("AB*D").is_none() && b64d("A").is_none() && b64d("AAAAA").is_none(), "a letter that stands alone is no bytes");
        assert!(b64d("AAB=").is_none(), "padding over bits that are not zero");
    }

    #[test]
    fn a_blob_opens_only_with_its_own_data_and_one_changed_byte_does_not_open() {
        let cipher = Cipher::generate().unwrap();
        let blob = cipher.seal(b"words of a conversation", b"place one");
        assert_eq!(cipher.open(&blob, b"place one").unwrap(), b"words of a conversation");
        assert!(cipher.open(&blob, b"place two").is_none(), "another place");
        for at in 0..blob.len() {
            let mut changed = blob.clone();
            changed[at] ^= 1;
            assert!(cipher.open(&changed, b"place one").is_none(), "byte {at}");
        }
        assert!(cipher.open(&blob[..blob.len() - 1], b"place one").is_none() && cipher.open(&[], b"").is_none());
        assert_ne!(cipher.seal(b"x", b""), cipher.seal(b"x", b""), "a fresh nonce each time");
        assert_ne!(Cipher::generate().unwrap().fingerprint(), cipher.fingerprint());
    }

    fn create(dir: &Path, remember: bool) -> (Keystore, Arc<Cipher>, String) {
        Keystore::create(dir, "a passphrase of the test", remember, None).unwrap()
    }

    #[test]
    fn the_passphrase_and_the_recovery_code_each_open_the_key_and_a_wrong_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cipher, code) = create(dir.path(), false);
        assert!(exists(dir.path()) && !store.is_remembered());
        let again = Keystore::load(dir.path()).unwrap();
        assert_eq!(again.unlock("a passphrase of the test").unwrap().fingerprint(), cipher.fingerprint());
        assert_eq!(again.unlock("a passphrase of the tesT").unwrap_err(), KeyError::Wrong);
        assert_eq!(again.unlock("").unwrap_err(), KeyError::Wrong);
        assert_eq!(again.unlock_with_recovery(&code).unwrap().fingerprint(), cipher.fingerprint());
        assert_eq!(again.unlock_with_recovery(&code.to_lowercase().replace('-', " ")).unwrap().fingerprint(), cipher.fingerprint(), "as it may be typed");
        assert_eq!(again.unlock_with_recovery("AAAAA-AAAAA").unwrap_err(), KeyError::Wrong);
        // the passphrase is not taken for the code, nor the code for the passphrase
        assert_eq!(again.unlock(&code).unwrap_err(), KeyError::Wrong);
        assert_eq!(again.unlock_with_recovery("a passphrase of the test").unwrap_err(), KeyError::Wrong);
        assert_eq!(Keystore::create(dir.path(), "x", false, None).unwrap_err(), KeyError::Exists);
        assert_eq!(Keystore::create(tempfile::tempdir().unwrap().path(), "", false, None).unwrap_err(), KeyError::Empty);
        assert_eq!(again.unlock_remembered().unwrap_err(), KeyError::NotRemembered);
        // nothing of the secrets is in the file
        let text = std::fs::read_to_string(path_in(dir.path())).unwrap();
        assert!(!text.contains("a passphrase of the test") && !text.contains(&code) && !text.contains(&hex(&cipher.dek)), "{text}");
    }

    #[test]
    fn the_recovery_code_sets_a_new_passphrase_and_a_new_code_ends_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let (_, cipher, code) = create(dir.path(), false);
        let mut store = Keystore::load(dir.path()).unwrap();
        let unlocked = store.reset_passphrase_with_recovery(&code, "the new passphrase").unwrap();
        assert_eq!(unlocked.fingerprint(), cipher.fingerprint());
        let store = Keystore::load(dir.path()).unwrap();
        assert_eq!(store.unlock("a passphrase of the test").unwrap_err(), KeyError::Wrong, "the old passphrase is gone");
        assert!(store.unlock("the new passphrase").is_ok());
        assert!(store.unlock_with_recovery(&code).is_ok(), "the code is as it was");
        let mut store = store;
        assert_eq!(store.reset_passphrase_with_recovery("AAAAA", "x").unwrap_err(), KeyError::Wrong);
        assert_eq!(store.reset_passphrase_with_recovery(&code, "").unwrap_err(), KeyError::Empty);
        let newer = store.new_recovery_code(&cipher).unwrap();
        assert_ne!(newer, code);
        let store = Keystore::load(dir.path()).unwrap();
        assert_eq!(store.unlock_with_recovery(&code).unwrap_err(), KeyError::Wrong, "the old code stops");
        assert_eq!(store.unlock_with_recovery(&newer).unwrap().fingerprint(), cipher.fingerprint());
        assert!(store.unlock("the new passphrase").is_ok(), "the passphrase is as it was");
        let mut store = store;
        store.change_passphrase(&cipher, "and another").unwrap();
        assert!(Keystore::load(dir.path()).unwrap().unlock("and another").is_ok());
    }

    /// The copy for this Windows user opens with no question, and is gone when the box is cleared; the passphrase works throughout.
    #[cfg(windows)]
    #[test]
    fn the_copy_for_this_pc_opens_with_no_question_and_is_gone_when_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, cipher, code) = create(dir.path(), true);
        assert!(store.is_remembered());
        let text = std::fs::read_to_string(path_in(dir.path())).unwrap();
        assert!(text.contains("dek_wrapped_dpapi"));
        let again = Keystore::load(dir.path()).unwrap();
        assert_eq!(again.unlock_remembered().unwrap().fingerprint(), cipher.fingerprint());
        store.set_remembered(&cipher, false).unwrap();
        let cleared = Keystore::load(dir.path()).unwrap();
        assert!(!cleared.is_remembered());
        assert!(!std::fs::read_to_string(path_in(dir.path())).unwrap().contains("dpapi"));
        assert_eq!(cleared.unlock_remembered().unwrap_err(), KeyError::NotRemembered);
        assert!(cleared.unlock("a passphrase of the test").is_ok() && cleared.unlock_with_recovery(&code).is_ok());
        let mut store = cleared;
        store.set_remembered(&cipher, true).unwrap();
        assert!(Keystore::load(dir.path()).unwrap().unlock_remembered().is_ok());
        // a blob of another user, or damaged, is not opened
        let mut blob = b64d(Keystore::load(dir.path()).unwrap().stored.dek_wrapped_dpapi.as_deref().unwrap()).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 1;
        assert!(dpapi(&blob, false).is_err());
        assert!(dpapi(&cipher.dek, false).is_err(), "what DPAPI did not wrap");
    }

    #[test]
    fn a_changed_file_is_refused_in_words_and_a_costly_one_is_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _, _) = create(dir.path(), false);
        let path = path_in(dir.path());
        let text = std::fs::read_to_string(&path).unwrap();
        // the wrapped key of the passphrase swapped for the recovery code's: it opens nothing
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let other = value["dek_wrapped_recovery"].clone();
        value["dek_wrapped_passphrase"] = other;
        std::fs::write(&path, value.to_string()).unwrap();
        assert_eq!(Keystore::load(dir.path()).unwrap().unlock("a passphrase of the test").unwrap_err(), KeyError::Wrong);
        // a cost that would take the machine: refused before scrypt runs
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["scrypt"]["n"] = serde_json::json!(1u64 << 30);
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(matches!(Keystore::load(dir.path()).unwrap().unlock("x"), Err(KeyError::Corrupt(_))));
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(Keystore::load(dir.path()), Err(KeyError::Corrupt(_))));
        std::fs::write(&path, text.replace("\"version\": 1", "\"version\": 9")).unwrap();
        assert!(matches!(Keystore::load(dir.path()), Err(KeyError::Corrupt(_))));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(Keystore::load(dir.path()).unwrap_err(), KeyError::Missing);
    }

    #[test]
    fn what_is_pending_is_kept_in_the_file_and_the_key_file_goes_whole() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _, _) = Keystore::create(dir.path(), "p", false, Some(Pending::Encrypt)).unwrap();
        assert_eq!(Keystore::load(dir.path()).unwrap().pending(), Some(Pending::Encrypt));
        store.set_pending(Some(Pending::Decrypt)).unwrap();
        assert_eq!(Keystore::load(dir.path()).unwrap().pending(), Some(Pending::Decrypt));
        store.set_pending(None).unwrap();
        assert_eq!(Keystore::load(dir.path()).unwrap().pending(), None);
        assert!(!dir.path().join("conversation-key.json.part").exists(), "written beside and renamed");
        Keystore::remove(dir.path()).unwrap();
        assert!(!exists(dir.path()));
        Keystore::remove(dir.path()).unwrap();
    }

    #[test]
    fn a_policy_says_what_a_new_file_is_written_under() {
        let cipher = Cipher::generate().unwrap();
        let policy = Policy::off();
        assert!(policy.writer().unwrap().is_none());
        policy.set_locked();
        assert_eq!(policy.writer().unwrap_err(), LOCKED);
        assert!(policy.is_locked());
        policy.set_on(cipher.clone());
        assert_eq!(policy.writer().unwrap().unwrap().fingerprint(), cipher.fingerprint());
        assert_eq!(find(cipher.fingerprint()).unwrap().fingerprint(), cipher.fingerprint());
        forget(cipher.fingerprint());
        assert!(find(cipher.fingerprint()).is_none());
    }
}
