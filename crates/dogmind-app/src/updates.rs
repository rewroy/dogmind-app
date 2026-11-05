//! Asking GitHub whether a newer release of Dogmind is out, and fetching its setup for the person to install: the app installs nothing without being told to.
//! One GET of the project's list of releases goes out (the twenty newest, as GitHub orders them), with the program's name and version and nothing that says who is asking.
//! The highest version in the list is the one that counts, by its tag and not by its place in the list or its date: a pre-release counts as well, a draft never does, and a tag
//! that is no version is passed over. What comes back is a version, a page, and, when the release has its setup among its files with the size and the SHA-256 GitHub publishes for
//! it, that setup.
//!
//! A copy that an installer put in its folder (it has the installer's manifest) fetches that setup, on its own look or the person's: into the `updates` folder of its data, under a
//! name that ends in `.partial` until its size and its hash are those GitHub published, and then under its own, with a note of what was checked, which stays for the next start.
//! Anything else is deleted. Only github.com is asked, and where it sends the download on: `*.githubusercontent.com`, five times at most. A debug build pointed at a test server
//! (`DOGMIND_TEST_RELEASES_URL`) may fetch from that same server as well; a build for release cannot.
//!
//! A release also has, beside its setup, a `.parts` file that says where each file of the setup is in it. With that, and the record that the installer keeps in the manifest of what it
//! installed, the app fetches by ranges only the files of the setup that are new or have changed, and puts a setup together of those and the program (`ranges`). It checks all of it as
//! it checks a setup fetched whole, and if anything is not as it should be it fetches the setup whole, as it did before.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use dogmind_agent::http::{self, Failure, Url};
use sha2::{Digest, Sha256};

mod ranges;

/// The twenty newest releases, pre-releases included (`releases/latest` leaves them out, and answers 404 while every release of the project is one). A request that does not
/// sign in is not given drafts. 404: the repository is not public yet.
const RELEASES: &str = "https://api.github.com/repos/rewroy/dogmind-app/releases?per_page=20";
/// The only place a release's page may be: what the answer names is opened only when it begins like this.
const RELEASE_PAGES: &str = "https://github.com/rewroy/dogmind-app/releases/";
/// Where a person is taken when the answer's own address is not one of those.
const LATEST_PAGE: &str = "https://github.com/rewroy/dogmind-app/releases/latest";
/// How long the answer is waited for, from the first connection to the last byte.
const GIVE_UP_AFTER: Duration = Duration::from_secs(10);
/// A release is some ten kilobytes with its notes and its files, and the notes may be up to 125,000 characters: twenty of the longest are some 2.5 MB. More than this is no list of
/// releases, and memory is all it could cost.
const MOST_OF_AN_ANSWER: usize = 4 << 20;
/// A release's name is shown in a line of the window: past this it is cut.
const MOST_OF_A_VERSION: usize = 40;
/// Where a setup is first asked for, on github.com: this project's own downloads.
const SETUP_DOWNLOADS: &str = "/rewroy/dogmind-app/releases/download/";
/// What an address may come to, with the long signed ones GitHub's storage sends the download on to.
const MOST_OF_AN_ADDRESS: usize = 4096;
/// GitHub keeps no file of a release that is larger than this, and nothing larger is a setup of Dogmind.
const MOST_OF_A_SETUP: u64 = 2 << 30;
/// How many times a download may be sent on.
const MOST_REDIRECTS: usize = 5;
/// How long the first byte of a download is waited for.
const CONNECT: Duration = Duration::from_secs(15);

/// How long a look stands for: the app asks on its own at most once in this many seconds.
pub const EVERY: u64 = 24 * 60 * 60;

/// What the installer puts in the folder it installs to, and what says that a copy runs from an install: as `install::MANIFEST` of `dogmind-setup`, which this cannot link to.
pub const INSTALL_MANIFEST: &str = "dogmind-install.txt";
/// Where setups are kept, in the app's data folder.
pub const FOLDER: &str = "updates";
/// What is written beside a setup that has been checked, so that the next start has it and does not fetch it again.
const RECORD: &str = "ready.txt";

/// A file of a release, as GitHub lists it among the release's files, with what it published of it to check it by: its setup, and the `.parts` file beside that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    /// Where it is fetched from (`browser_download_url`), already an address a setup may be fetched from.
    pub url: String,
    pub size: u64,
    /// Its SHA-256, in lower case, as the `digest` GitHub publishes for the file says.
    pub sha256: String,
    /// For a setup: the `DogmindSetup-VERSION.parts` beside it, when the release has that with a SHA-256 digest too. With it an install that keeps a record of its files can be updated by
    /// fetching only what changed. None: the setup is fetched whole.
    pub parts: Option<Box<Setup>>,
}

/// The release that is newest, as the person is told of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Its tag without the `v` in front: `0.2.0`, or `0.2.0-beta.1` for a pre-release.
    pub version: String,
    /// Where to read of it, already one of the addresses that may be opened.
    pub page: String,
    /// Its setup, when it has `DogmindSetup-VERSION.exe` among its files with a SHA-256 digest: only then can the app fetch it. None: the person is given the page.
    pub setup: Option<Setup>,
}

/// The name a release's setup has on GitHub and in the `updates` folder.
pub fn setup_file_name(version: &str) -> String {
    format!("DogmindSetup-{version}.exe")
}

/// The name the `.parts` file beside a release's setup has on GitHub.
fn parts_file_name(version: &str) -> String {
    format!("DogmindSetup-{version}.parts")
}

/// A version fit to be in a file name: what a tag is made of, and no more.
fn is_plain_version(version: &str) -> bool {
    !version.is_empty() && version.chars().all(|letter| letter.is_ascii_alphanumeric() || matches!(letter, '.' | '-'))
}

/// What a look came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// What is newest is not newer than this app (or the releases there are have names that are no versions, which do not count as newer). A pre-release of this app's own
    /// version, or of an earlier one, is not newer.
    UpToDate,
    /// The highest version GitHub lists is later than this app's: a pre-release too, never a draft.
    Newer(Release),
    /// GitHub has no release of it to give: the list is empty or holds drafts only, or the repository is not public yet (404).
    NoRelease,
    /// A debug build run as a test, on a made-up machine with no server of its own to ask: it asks nobody, and that is not a failure to reach GitHub.
    NotAsked,
    Failed(Trouble),
}

/// Why a look could not be made, in the few ways a person has something to do about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    NotReached,
    TooSlow,
    /// GitHub answered with this, and not with a release.
    Refused(u16),
    Unreadable,
}

impl Outcome {
    /// Whether the look learned something, which is all that counts against the day: that this app is the newest, that a newer one is out, or that GitHub has no release to give.
    /// A refusal (403 is GitHub's limit for requests that do not sign in, which a shared address or a VPN reaches early), a server's error and an answer that is no list of releases
    /// (a proxy's or a captive portal's page) learned nothing, and neither did a look that never heard from GitHub, or that asked no one: the hourly timer tries again at its next tick.
    pub fn learned_something(&self) -> bool {
        matches!(self, Outcome::UpToDate | Outcome::Newer(_) | Outcome::NoRelease)
    }
}

/// Whether a day has gone by since the last answer. A time ahead of now is a clock that was set back: nothing says when the next look is due, so it is.
pub fn is_due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|last| last > now || now - last >= EVERY)
}

/// Now, in seconds since 1970.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// Whether the person is to be told of this version on their own: not when they have closed the notice of this very one. A newer one is told again.
pub fn is_to_be_told(found: &str, dismissed: Option<&str>) -> bool {
    match dismissed {
        None => true,
        Some(dismissed) => match (parse_version(found), parse_version(dismissed)) {
            (Some(found), Some(dismissed)) => found != dismissed,
            _ => found.trim() != dismissed.trim(),
        },
    }
}

/// The release to tell the person of without being asked, from what a look came to: only the app's own look tells, only while it is switched on, and not of a version
/// whose notice was closed.
pub fn release_to_tell(outcome: &Outcome, on_its_own: bool, switched_on: bool, dismissed: Option<&str>) -> Option<Release> {
    match outcome {
        Outcome::Newer(release) if on_its_own && switched_on && is_to_be_told(&release.version, dismissed) => Some(release.clone()),
        _ => None,
    }
}

/// A version as a tag writes it: up to three numbers, a `v` before them, and perhaps a pre-release after a hyphen.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    numbers: [u64; 3],
    pre_release: Option<String>,
}

fn parse_version(text: &str) -> Option<Version> {
    let text = text.trim();
    let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
    // (what follows a plus is the build's, and does not order versions)
    let text = text.split_once('+').map_or(text, |(before, _)| before);
    let (numbers_text, pre_release) = match text.split_once('-') {
        Some((before, after)) if !after.is_empty() => (before, Some(after.to_string())),
        Some(_) => return None,
        None => (text, None),
    };
    let mut numbers = [0u64; 3];
    for (at, part) in numbers_text.split('.').enumerate() {
        // (`parse` alone would take a sign)
        if at >= numbers.len() || part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        numbers[at] = part.parse().ok()?;
    }
    Some(Version { numbers, pre_release })
}

/// Pre-releases by their dot-separated parts: a number is less than a name, numbers go by their value, and the shorter of two that begin alike comes first.
fn order_pre_releases(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.split('.'), b.split('.'));
    loop {
        match (a.next(), b.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => {
                let number = |part: &str| part.parse::<u64>().ok().filter(|_| part.bytes().all(|byte| byte.is_ascii_digit()));
                let order = match (number(a), number(b)) {
                    (Some(a), Some(b)) => a.cmp(&b),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a.cmp(b),
                };
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> std::cmp::Ordering {
        self.numbers.cmp(&other.numbers).then_with(|| match (&self.pre_release, &other.pre_release) {
            (None, None) => std::cmp::Ordering::Equal,
            // (1.0.0-rc1 comes before 1.0.0)
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => order_pre_releases(a, b),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Whether `found` is a later version than `running`, by the numbers (1.10.0 is later than 1.9.0). A pre-release comes before the release it leads up to, so `0.2.0-beta.1` is
/// not later than `0.2.0` (and `0.2.0` is later than `0.2.0-beta.1`), but is later than `0.1.9`. A name that is no version is not later.
pub fn is_newer(found: &str, running: &str) -> bool {
    match (parse_version(found), parse_version(running)) {
        (Some(found), Some(running)) => found > running,
        _ => false,
    }
}

/// The address to open for a release: the one the answer names when it is on this project's release pages and has nothing odd in it, else the page of the latest.
pub fn release_page(named: &str) -> String {
    let plain = named.bytes().all(|byte| byte.is_ascii_graphic()) && !named.contains("..") && !named.contains(['\\', '"', '<', '>']);
    if named.starts_with(RELEASE_PAGES) && named.len() <= 300 && plain {
        named.to_string()
    } else {
        LATEST_PAGE.to_string()
    }
}

/// The page of the release of this version, as a tag names it: for a setup that was fetched before this start, which no look has named the page of.
pub fn release_page_of(version: &str) -> String {
    release_page(&format!("{RELEASE_PAGES}tag/v{version}"))
}

/// `read_releases_for` with no test server: what the app reads of GitHub's own answer.
#[cfg(test)]
pub fn read_releases(body: &str, running: &str) -> Option<Outcome> {
    read_releases_for(body, running, None)
}

/// What a `releases` answer (a list of releases) comes to for a program of version `running`. `None`: it is not that kind of answer.
/// The list is not taken to be in any order: of the entries that are not drafts and whose tag is a version, pre-releases included, the one with the highest version is the
/// newest (the first of two that are the same). No entry at all, or drafts only, is `NoRelease`; entries but none with a version in its tag is `UpToDate`, as a name that is no
/// version is never newer.
/// A setup may also be fetched from `test_server`: the server of a debug build that was pointed at one, and never anything in a build for release.
fn read_releases_for(body: &str, running: &str, test_server: Option<&Url>) -> Option<Outcome> {
    let serde_json::Value::Array(list) = serde_json::from_str(body).ok()? else { return None };
    let (mut looked_like_releases, mut published) = (false, false);
    let mut newest: Option<(&str, &serde_json::Value)> = None;
    for entry in &list {
        let Some(tag) = entry.get("tag_name").and_then(serde_json::Value::as_str) else { continue };
        looked_like_releases = true;
        if entry.get("draft").and_then(serde_json::Value::as_bool) == Some(true) {
            continue;
        }
        published = true;
        let tag = tag.trim();
        if parse_version(tag).is_some() && newest.is_none_or(|(best, _)| is_newer(tag, best)) {
            newest = Some((tag, entry));
        }
    }
    // (a list with something in it, and nothing in it that tells of a release, is someone else's page)
    if !list.is_empty() && !looked_like_releases {
        return None;
    }
    Some(match newest {
        Some((tag, entry)) if is_newer(tag, running) => {
            let whole = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
            let version: String = whole.chars().take(MOST_OF_A_VERSION).collect();
            let named = entry.get("html_url").and_then(serde_json::Value::as_str).unwrap_or("");
            // (a name that was cut is no name of a file: such a release is offered by its page)
            let setup = if version == whole { setup_of(entry, &version, test_server) } else { None };
            Outcome::Newer(Release { version, page: release_page(named), setup })
        }
        Some(_) => Outcome::UpToDate,
        None if published => Outcome::UpToDate,
        None => Outcome::NoRelease,
    })
}

/// The setup among the files of this release: `DogmindSetup-VERSION.exe`, with a size and a SHA-256 digest GitHub published for it and an address it may be fetched from. A release
/// that lacks any of these has none: the app does not fetch what it could not check.
fn setup_of(entry: &serde_json::Value, version: &str, test_server: Option<&Url>) -> Option<Setup> {
    if !is_plain_version(version) {
        return None;
    }
    let mut setup = file_of(entry, &setup_file_name(version), MOST_OF_A_SETUP, test_server)?;
    setup.parts = parts_of(entry, version, test_server).map(Box::new);
    Some(setup)
}

/// The `.parts` file beside the setup among the files of this release, with a size and a SHA-256 digest GitHub published for it and an address it may be fetched from. A release that
/// lacks any of these has none, and its setup is fetched whole.
fn parts_of(entry: &serde_json::Value, version: &str, test_server: Option<&Url>) -> Option<Setup> {
    if !is_plain_version(version) {
        return None;
    }
    file_of(entry, &parts_file_name(version), ranges::MOST_OF_PARTS, test_server)
}

/// The file of this name among the files of a release, when it has a size of at most `most`, a SHA-256 digest and an address a download may be fetched from.
fn file_of(entry: &serde_json::Value, name: &str, most: u64, test_server: Option<&Url>) -> Option<Setup> {
    let asset = entry.get("assets")?.as_array()?.iter().find(|asset| asset.get("name").and_then(serde_json::Value::as_str) == Some(name))?;
    let size = asset.get("size")?.as_u64().filter(|size| (1..=most).contains(size))?;
    let sha256 = digest_hex(asset.get("digest")?.as_str()?)?;
    let url = asset.get("browser_download_url")?.as_str()?;
    first_hop(url, test_server)?;
    Some(Setup { url: url.to_string(), size, sha256, parts: None })
}

/// The hash in a `sha256:HEX` digest, in lower case. Another kind of digest, or a hash of another length, is none.
fn digest_hex(digest: &str) -> Option<String> {
    let hex = digest.trim().strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| hex.to_ascii_lowercase())
}

/// An address read for what it is, when it has nothing odd in it: all of it plain characters, and a host that is a name.
fn plain_address(address: &str) -> Option<Url> {
    let plain = address.len() <= MOST_OF_AN_ADDRESS && address.bytes().all(|byte| byte.is_ascii_graphic()) && !address.contains(['\\', '"', '<', '>']);
    let mut url = Url::parse(address).ok().filter(|_| plain)?;
    url.host = url.host.to_ascii_lowercase();
    let name = !url.host.is_empty() && url.host.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b':'));
    (name && !url.host.contains("..")).then_some(url)
}

/// Where a debug build's test server is, if that is where this address is too: the same loopback host and port, over plain HTTP.
fn is_the_test_server(url: &Url, test_server: Option<&Url>) -> bool {
    test_server.is_some_and(|server| !server.tls && !url.tls && matches!(server.host.as_str(), "127.0.0.1" | "localhost" | "::1") && url.host == server.host && url.port == server.port)
}

/// The address a setup is first fetched from: this project's own downloads on github.com, over https. Anything else, a lookalike's host or a file of another project, is none.
fn first_hop(address: &str, test_server: Option<&Url>) -> Option<Url> {
    let url = plain_address(address)?;
    let github = url.tls && url.port == 443 && url.host == "github.com" && url.path.starts_with(SETUP_DOWNLOADS) && !url.path.contains("..");
    (github || is_the_test_server(&url, test_server)).then_some(url)
}

/// Where a download may be sent on to: github.com, or a host of GitHub's storage (`*.githubusercontent.com`), over https.
fn next_hop(address: &str, test_server: Option<&Url>) -> Option<Url> {
    let url = plain_address(address)?;
    let github = url.tls && url.port == 443 && (url.host == "github.com" || url.host.strip_suffix(".githubusercontent.com").is_some_and(|name| !name.is_empty()));
    (github || is_the_test_server(&url, test_server)).then_some(url)
}

/// The server of a debug build that was pointed at one with `DOGMIND_TEST_RELEASES_URL`, which it may fetch a setup from as well. A build for release has none, whatever it is told.
fn test_server() -> Option<Url> {
    #[cfg(debug_assertions)]
    {
        std::env::var("DOGMIND_TEST_RELEASES_URL").ok().and_then(|named| Url::parse(named.trim()).ok())
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// The server a look was made at, when it is the test server: a debug build, asked of a loopback address. A look at GitHub's has none, and a build for release asks GitHub alone.
fn test_server_of(asked: &Url) -> Option<Url> {
    (cfg!(debug_assertions) && !asked.tls).then(|| asked.clone())
}

/// Where to ask. A debug build can be pointed at a server of its own with `DOGMIND_TEST_RELEASES_URL`; a build for release cannot be told anything.
/// `None`: a debug build running on a made-up machine (see `machine::kept_from_the_real_machine`) with no server to ask, which asks nobody (`Outcome::NotAsked`).
fn address() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        address_for_testing(std::env::var("DOGMIND_TEST_RELEASES_URL").ok(), crate::machine::kept_from_the_real_machine())
    }
    #[cfg(not(debug_assertions))]
    {
        Some(RELEASES.to_string())
    }
}

#[cfg(debug_assertions)]
fn address_for_testing(named: Option<String>, on_a_made_up_machine: bool) -> Option<String> {
    match named.map(|named| named.trim().to_string()).filter(|named| !named.is_empty()) {
        Some(named) => Some(named),
        None if on_a_made_up_machine => None,
        None => Some(RELEASES.to_string()),
    }
}

/// Whether there is anywhere to ask: the app does not look on its own where it cannot.
pub fn can_ask() -> bool {
    address().is_some()
}

/// Asks, and waits for the answer: off the window's thread. Never longer than ten seconds.
pub fn check() -> Outcome {
    check_of(address())
}

fn check_of(address: Option<String>) -> Outcome {
    match address {
        Some(address) => check_at(&address, env!("CARGO_PKG_VERSION"), GIVE_UP_AFTER),
        None => Outcome::NotAsked,
    }
}

/// The look, at this address, for a program of this version. The answer is waited for on a thread of its own, so that what takes
/// longer than `give_up_after` (a name that does not resolve is not something a socket can be told to stop) is let go of at the time.
fn check_at(address: &str, running: &str, give_up_after: Duration) -> Outcome {
    let stop = Arc::new(AtomicBool::new(false));
    let (tell, told) = mpsc::channel();
    let (address, running, stopped_by) = (address.to_string(), running.to_string(), stop.clone());
    std::thread::spawn(move || {
        let _ = tell.send(ask(&address, &running, give_up_after, &stopped_by));
    });
    match told.recv_timeout(give_up_after) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => {
            stop.store(true, Ordering::Relaxed);
            Outcome::Failed(Trouble::TooSlow)
        }
        Err(RecvTimeoutError::Disconnected) => Outcome::Failed(Trouble::NotReached),
    }
}

fn trouble(failure: Failure) -> Outcome {
    Outcome::Failed(match failure {
        Failure::Cancelled => Trouble::TooSlow,
        Failure::Connect(_) | Failure::Stream(_) => Trouble::NotReached,
    })
}

fn ask(address: &str, running: &str, give_up_after: Duration, stop: &AtomicBool) -> Outcome {
    let Ok(url) = Url::parse(address) else { return Outcome::Failed(Trouble::NotReached) };
    // What goes out says what program is asking and that it wants GitHub's JSON, and no more: no sign-in, no cookie, nothing of whoever is at the keyboard.
    let headers = [
        ("Accept".to_string(), "application/vnd.github+json".to_string()),
        ("User-Agent".to_string(), format!("Dogmind/{running}")),
    ];
    let mut response = match http::request("GET", &url, &headers, None, give_up_after / 2, stop) {
        Ok(response) => response,
        Err(failure) => return trouble(failure),
    };
    match response.status {
        200 => {}
        404 => return Outcome::NoRelease,
        status => return Outcome::Failed(Trouble::Refused(status)),
    }
    let mut body = Vec::new();
    loop {
        match response.next() {
            Ok(Some(bytes)) => {
                body.extend_from_slice(&bytes);
                if body.len() > MOST_OF_AN_ANSWER {
                    return Outcome::Failed(Trouble::Unreadable);
                }
            }
            Ok(None) => break,
            Err(failure) => return trouble(failure),
        }
    }
    read_releases_for(&String::from_utf8_lossy(&body), running, test_server_of(&url).as_ref()).unwrap_or(Outcome::Failed(Trouble::Unreadable))
}

/// Whether this folder is an install: the installer put its manifest there. A copy unpacked from the zip has none, and an update through the installer would turn its folder into
/// an install, with a Start menu entry and an entry in Installed apps that its person did not ask for.
pub fn is_install(folder: &Path) -> bool {
    folder.join(INSTALL_MANIFEST).is_file()
}

/// The folder this copy runs from, when it is an install.
pub fn install_folder() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let folder = exe.parent()?;
    is_install(folder).then(|| folder.to_path_buf())
}

/// The release whose setup is to be fetched now, from what a look came to. Only a copy in an install can be updated through the installer. The app's own look fetches while the person
/// has it switched on, and not the setup of a version whose notice they closed; a look the person asked for fetches whatever it finds, as that is what they asked it for.
pub fn setup_to_fetch(outcome: &Outcome, on_its_own: bool, switched_on: bool, dismissed: Option<&str>, installed: bool) -> Option<Release> {
    match outcome {
        Outcome::Newer(release) if installed && release.setup.is_some() && (!on_its_own || (switched_on && is_to_be_told(&release.version, dismissed))) => Some(release.clone()),
        _ => None,
    }
}

/// What the installer is run with to update the install in `folder`: it waits for the programs with these numbers (this app, and the engine it started) to end, replaces the files, and
/// starts the app again. `DogmindSetup.exe --update --dir FOLDER --wait-pid PID...`.
pub fn setup_arguments(folder: &Path, wait_for: &[u32]) -> Vec<std::ffi::OsString> {
    let mut arguments: Vec<std::ffi::OsString> = vec!["--update".into(), "--dir".into(), folder.as_os_str().to_os_string()];
    for pid in wait_for {
        arguments.extend(["--wait-pid".into(), pid.to_string().into()]);
    }
    arguments
}

/// Why a setup was not fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failed {
    /// The person stopped it.
    Cancelled,
    /// The app's own try, on a connection that is metered: it waits for the person's.
    Metered,
    NotReached,
    /// GitHub answered with this, and not with the file.
    Refused(u16),
    /// The address, or where the download was sent on to, is not one a setup may be fetched from, or it was sent on too many times.
    NotAllowed,
    /// What came is not what GitHub published: not its size, or not its hash.
    DidNotMatch,
    /// The file could not be written, or read.
    Disk(String),
}

fn disk(error: std::io::Error) -> Failed {
    Failed::Disk(error.to_string())
}

fn failed_of(failure: Failure) -> Failed {
    match failure {
        Failure::Cancelled => Failed::Cancelled,
        Failure::Connect(_) | Failure::Stream(_) => Failed::NotReached,
    }
}

/// A setup that has been fetched and checked, and waits in the `updates` folder to be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub version: String,
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The size of a file and the SHA-256 of all of it, read a block at a time.
fn hash_of(path: &Path) -> std::io::Result<(u64, String)> {
    hash_reader(std::fs::File::open(path)?)
}

/// The same, of what a reader gives from where it is to its end.
fn hash_reader(mut reader: impl std::io::Read) -> std::io::Result<(u64, String)> {
    let (mut hasher, mut size, mut block) = (Sha256::new(), 0u64, vec![0u8; 1 << 20]);
    loop {
        let count = reader.read(&mut block)?;
        if count == 0 {
            return Ok((size, hex(&hasher.finalize())));
        }
        hasher.update(&block[..count]);
        size += count as u64;
    }
}

/// Why a setup that was fetched and checked is not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotRun {
    /// The file is not there, or its bytes are not those whose hash was noted.
    Changed,
    /// It could not be opened, held or read: the system's words.
    Refused(String),
}

impl Ready {
    /// Whether the file is there at the size it was checked at.
    pub fn is_there(&self) -> bool {
        std::fs::metadata(&self.path).is_ok_and(|file| file.is_file() && file.len() == self.size)
    }

    /// Whether the file is still what was checked: all of its bytes, by their hash. A second or two for a setup of this size.
    pub fn is_sound(&self) -> bool {
        hash_of(&self.path).is_ok_and(|(size, sha256)| size == self.size && sha256 == self.sha256)
    }

    /// Whether the file is still what was checked, ahead of its being run, and held so until it has been: it is opened for reading, shared with readers only (not with writing, deleting
    /// or renaming), and its bytes are hashed through that handle. The handle comes back with the file sound, and while it is open no one can change the file between the check and the
    /// start (`run_held`). A second or two for a setup of this size: run off the window's thread.
    pub fn hold_if_sound(&self) -> Result<std::fs::File, NotRun> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let file = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&self.path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => NotRun::Changed,
            _ => NotRun::Refused(error.to_string()),
        })?;
        match hash_reader(&file) {
            Ok((size, sha256)) if size == self.size && sha256 == self.sha256 => Ok(file),
            Ok(_) => Err(NotRun::Changed),
            Err(error) => Err(NotRun::Refused(error.to_string())),
        }
    }
}

/// The setup run with `arguments`, while the handle that checked it is still open; then that is let go of. (Windows reads the file to start it: a handle that shares reading does not stand in the way.)
pub fn run_held(ready: &Ready, held: std::fs::File, arguments: &[std::ffi::OsString]) -> std::io::Result<std::process::Child> {
    let started = std::process::Command::new(&ready.path).args(arguments).spawn();
    drop(held);
    started
}

#[cfg(test)]
fn write_record(folder: &Path, ready: &Ready) -> std::io::Result<()> {
    write_record_of(folder, ready, None)
}

/// The note of a setup that was checked. A setup that was put together here is not the file that GitHub published, and its note says which that was, so that a look that finds the same
/// release does not fetch it again.
fn write_record_of(folder: &Path, ready: &Ready, published: Option<&Setup>) -> std::io::Result<()> {
    let mut text = format!("version {}\nsize {}\nsha256 {}\n", ready.version, ready.size, ready.sha256);
    if let Some(published) = published {
        text.push_str(&format!("published_size {}\npublished_sha256 {}\n", published.size, published.sha256));
    }
    let partial = folder.join(format!("{RECORD}.partial"));
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, folder.join(RECORD))
}

/// The size and the hash of the setup that GitHub published, which the setup in the folder was put together in place of, if its note says so.
fn read_published(folder: &Path) -> Option<(u64, String)> {
    let text = std::fs::read_to_string(folder.join(RECORD)).ok()?;
    let value = |name: &str| text.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')).map(str::trim);
    Some((value("published_size")?.parse().ok()?, digest_hex(&format!("sha256:{}", value("published_sha256")?))?))
}

/// What the note beside a checked setup says, as a setup that is there to be run, whether or not the file is. A note that is not in order is none.
fn read_record(folder: &Path) -> Option<Ready> {
    let text = std::fs::read_to_string(folder.join(RECORD)).ok()?;
    let value = |name: &str| text.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')).map(str::trim);
    let version = value("version").filter(|version| is_plain_version(version) && parse_version(version).is_some())?;
    let size = value("size")?.parse().ok().filter(|size| (1..=MOST_OF_A_SETUP).contains(size))?;
    let sha256 = digest_hex(&format!("sha256:{}", value("sha256")?))?;
    Some(Ready { version: version.to_string(), path: folder.join(setup_file_name(version)), size, sha256 })
}

/// The setup a start finds already checked in the `updates` folder, when it is for a version later than the one that runs and the file is there: it is offered, and not fetched again.
/// (The file's bytes are read again by `Ready::is_sound` before it is offered, off the window's thread.)
pub fn kept(folder: &Path, running: &str) -> Option<Ready> {
    read_record(folder).filter(|ready| is_newer(&ready.version, running) && ready.is_there())
}

/// `kept`, with the file's bytes read again, which is a second or two for a setup of this size: run off the window's thread. A file whose bytes are not those that were checked is taken away
/// with its note, and then there is none.
pub fn kept_and_sound(folder: &Path, running: &str) -> Option<Ready> {
    let ready = kept(folder, running)?;
    if ready.is_sound() {
        return Some(ready);
    }
    let _ = std::fs::remove_file(&ready.path);
    clean_up(folder, running);
    None
}

/// The version a file of the `updates` folder is the setup of.
fn version_of_setup_file(name: &str) -> Option<&str> {
    name.strip_prefix("DogmindSetup-")?.strip_suffix(".exe").filter(|version| is_plain_version(version))
}

/// What a start takes out of the `updates` folder: any file that is still `.partial`, and the setup of a version that is not later than the one that runs, which has been done with.
/// A setup of a later version stays, as the one that waits for the person. Nothing else in the folder is touched.
pub fn clean_up(folder: &Path, running: &str) {
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if (name.ends_with(".partial") || version_of_setup_file(&name).is_some_and(|version| !is_newer(version, running))) && entry.path().is_file() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    // (a note of a file that is not there is of nothing)
    if folder.join(RECORD).exists() && kept(folder, running).is_none() {
        let _ = std::fs::remove_file(folder.join(RECORD));
    }
}

/// What a fetched setup leaves no use for: the setups of other versions, which the one that was checked has taken the place of.
fn remove_other_setups(folder: &Path, keep: &str) {
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != keep && (name.ends_with(".partial") || version_of_setup_file(&name).is_some()) && entry.path().is_file() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The setup of a release in `folder`, checked: fetched from GitHub, or found there from an earlier try that was checked. With the release's `parts` file, and when this copy runs from an
/// install that kept a record of its files, only the files of the setup that are new or changed are fetched, and a setup is put together of them (see `ranges`); when that cannot be done
/// the setup is fetched whole. `on` is told how many bytes of how many have come; `stop` ends it, and then, as after any failure, nothing of it is left. Run off the window's thread.
pub fn obtain(version: &str, setup: &Setup, folder: &Path, on: &mut dyn FnMut(u64, u64), stop: &AtomicBool) -> Result<Ready, Failed> {
    obtain_within(version, setup, install_folder().as_deref(), folder, test_server().as_ref(), on, stop)
}

/// `obtain_within`, for the whole setup alone.
#[cfg(test)]
fn obtain_from(version: &str, setup: &Setup, folder: &Path, test_server: Option<&Url>, on: &mut dyn FnMut(u64, u64), stop: &AtomicBool) -> Result<Ready, Failed> {
    obtain_within(version, setup, None, folder, test_server, on, stop)
}

/// The setup in `folder`; `install` is the install folder that it is for, when there is one: with that, and the `.parts` file of the release, it is put together of parts.
fn obtain_within(version: &str, setup: &Setup, install: Option<&Path>, folder: &Path, test_server: Option<&Url>, on: &mut dyn FnMut(u64, u64), stop: &AtomicBool) -> Result<Ready, Failed> {
    if !is_plain_version(version) {
        return Err(Failed::NotAllowed);
    }
    // (checked already, and kept: it is not fetched again. One that was put together here stands for the setup that GitHub published, and its note says so.)
    let of_this_release = |ready: &Ready| (ready.size == setup.size && ready.sha256 == setup.sha256) || read_published(folder) == Some((setup.size, setup.sha256.clone()));
    if let Some(ready) = read_record(folder).filter(|ready| ready.version == version && of_this_release(ready) && ready.is_there()) {
        return Ok(ready);
    }
    let first = first_hop(&setup.url, test_server).ok_or(Failed::NotAllowed)?;
    std::fs::create_dir_all(folder).map_err(disk)?;
    let name = setup_file_name(version);
    let (partial, path) = (folder.join(format!("{name}.partial")), folder.join(&name));
    // (a note of the file that is about to be replaced is no longer true of it. A note of another version's setup, fetched and checked before, stays with that file until the new setup is here:
    // `write_record` replaces it then, and `remove_other_setups` takes the old file. A fetch that fails leaves that setup to be offered again.)
    if read_record(folder).is_some_and(|ready| ready.version == version) {
        let _ = std::fs::remove_file(folder.join(RECORD));
    }
    // The files that changed, by ranges, when that can be done. Whatever goes wrong with it (but the person's stop), the setup is fetched whole, and no more is said of it.
    let mut put_together = None;
    if let (Some(parts), Some(install)) = (setup.parts.as_deref(), install) {
        match ranges::assemble(&ranges::Job { version, setup, parts, install, test_server }, &partial, on, stop) {
            Ok(assembled) => put_together = Some(assembled),
            Err(ranges::Not::Stopped) => {
                let _ = std::fs::remove_file(&partial);
                return Err(Failed::Cancelled);
            }
            Err(ranges::Not::Because(_why)) => {
                let _ = std::fs::remove_file(&partial);
            }
        }
    }
    if put_together.is_none() {
        if let Err(failed) = stream(&first, setup, &partial, test_server, on, stop) {
            let _ = std::fs::remove_file(&partial);
            return Err(failed);
        }
    }
    if let Err(error) = std::fs::rename(&partial, &path) {
        let _ = std::fs::remove_file(&partial);
        return Err(disk(error));
    }
    // (what is hashed is what was written, as it is for a setup fetched whole)
    let ready = match &put_together {
        Some(assembled) => Ready { version: version.to_string(), path, size: assembled.size, sha256: assembled.sha256.clone() },
        None => Ready { version: version.to_string(), path, size: setup.size, sha256: setup.sha256.clone() },
    };
    if let Err(error) = write_record_of(folder, &ready, put_together.as_ref().map(|_| setup)) {
        let _ = std::fs::remove_file(&ready.path);
        return Err(disk(error));
    }
    remove_other_setups(folder, &name);
    Ok(ready)
}

/// The answer to a GET of `first` that has this status, with the address that gave it. It is sent on at most five times, and only to github.com and GitHub's storage; any other answer is an
/// error. What is asked for goes to each address it is sent on to as it went to the first.
fn follow<'a>(first: &Url, headers: &[(String, String)], accepted: u16, test_server: Option<&Url>, stop: &'a AtomicBool) -> Result<(http::Response<'a>, Url), Failed> {
    let (mut url, mut sent_on) = (first.clone(), 0);
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(Failed::Cancelled);
        }
        let response = http::request("GET", &url, headers, None, CONNECT, stop).map_err(failed_of)?;
        match response.status {
            status if status == accepted => return Ok((response, url)),
            301 | 302 | 303 | 307 | 308 => {
                sent_on += 1;
                url = match response.header("location").and_then(|location| next_hop(location, test_server)) {
                    Some(next) if sent_on <= MOST_REDIRECTS => next,
                    _ => return Err(Failed::NotAllowed),
                };
            }
            status => return Err(Failed::Refused(status)),
        }
    }
}

/// The setup into `partial`, hashed as it comes. It is sent on at most five times, and only to github.com and GitHub's storage; its size is the one published, in its head if it says and
/// in what arrives; and what arrives has the hash that was published. Anything else is an error, and the caller deletes the file.
fn stream(first: &Url, setup: &Setup, partial: &Path, test_server: Option<&Url>, on: &mut dyn FnMut(u64, u64), stop: &AtomicBool) -> Result<(), Failed> {
    // (what goes out says what program asks and that it wants the file: no sign-in, nothing of whoever is at the keyboard)
    let headers = [("User-Agent".to_string(), format!("Dogmind/{}", env!("CARGO_PKG_VERSION"))), ("Accept".to_string(), "application/octet-stream".to_string())];
    let (mut response, _) = follow(first, &headers, 200, test_server, stop)?;
    if response.header("content-length").is_some_and(|length| length.trim().parse::<u64>().ok() != Some(setup.size)) {
        return Err(Failed::DidNotMatch);
    }
    let file = std::fs::File::create(partial).map_err(disk)?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    let (mut hasher, mut done) = (Sha256::new(), 0u64);
    on(0, setup.size);
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(Failed::Cancelled);
        }
        match response.next() {
            Ok(Some(bytes)) => {
                done += bytes.len() as u64;
                if done > setup.size {
                    return Err(Failed::DidNotMatch);
                }
                hasher.update(&bytes);
                out.write_all(&bytes).map_err(disk)?;
                on(done, setup.size);
            }
            Ok(None) => break,
            Err(failure) => return Err(failed_of(failure)),
        }
    }
    let file = out.into_inner().map_err(|error| disk(error.into_error()))?;
    file.sync_all().map_err(disk)?;
    drop(file);
    if done != setup.size || hex(&hasher.finalize()) != setup.sha256 {
        return Err(Failed::DidNotMatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    /// One entry of what `releases` answers, as GitHub sends it (shortened: the assets and the author's other fields are not read).
    const V0_2_0: &str = r#"{
  "url": "https://api.github.com/repos/rewroy/dogmind-app/releases/123456789",
  "assets_url": "https://api.github.com/repos/rewroy/dogmind-app/releases/123456789/assets",
  "html_url": "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0",
  "id": 123456789,
  "author": { "login": "rewroy", "id": 1, "type": "User", "site_admin": false },
  "tag_name": "v0.2.0",
  "target_commitish": "main",
  "name": "Dogmind 0.2.0",
  "draft": false,
  "prerelease": false,
  "created_at": "2026-11-01T09:00:00Z",
  "published_at": "2026-11-01T09:30:12Z",
  "assets": [
    { "name": "DogmindSetup-0.2.0.exe", "size": 31457280, "browser_download_url": "https://github.com/rewroy/dogmind-app/releases/download/v0.2.0/DogmindSetup-0.2.0.exe" },
    { "name": "DogmindApp-0.2.0.zip", "size": 30408704, "browser_download_url": "https://github.com/rewroy/dogmind-app/releases/download/v0.2.0/DogmindApp-0.2.0.zip" }
  ],
  "body": "Release notes\r\n\r\n- Update check\r\n- Fixes"
}"#;

    /// An entry of the list for a tag, with the fields the app reads and a few it does not.
    fn entry(tag: &str, draft: bool, prerelease: bool) -> String {
        format!(
            r#"{{"id":1,"html_url":"https://github.com/rewroy/dogmind-app/releases/tag/{tag}","tag_name":"{tag}","name":"Dogmind {tag}","draft":{draft},"prerelease":{prerelease},"body":"Notes"}}"#
        )
    }

    fn list_of(entries: &[String]) -> String {
        format!("[{}]", entries.join(","))
    }

    /// The release the person is told of for a tag.
    fn release(tag: &str) -> Release {
        Release { version: tag.trim_start_matches('v').into(), page: format!("https://github.com/rewroy/dogmind-app/releases/tag/{tag}"), setup: None }
    }

    /// What a list of these tags (none a draft; a tag with a hyphen a pre-release) comes to, in the order given.
    fn read_tags(tags: &[&str], running: &str) -> Option<Outcome> {
        let entries: Vec<String> = tags.iter().map(|tag| entry(tag, false, tag.contains('-'))).collect();
        read_releases(&list_of(&entries), running)
    }

    #[test]
    fn a_list_is_read_for_its_highest_version_and_that_ones_page() {
        let outcome = read_releases(&format!("[{V0_2_0}]"), "0.1.0");
        assert_eq!(outcome, Some(Outcome::Newer(Release { version: "0.2.0".into(), page: "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0".into(), setup: None })));
        assert_eq!(read_releases(&format!("[{V0_2_0}]"), "0.2.0"), Some(Outcome::UpToDate));
    }

    #[test]
    fn what_is_no_list_of_releases_is_not_read_as_one() {
        for body in [
            "",
            "not json",
            "null",
            "{}",
            r#"{"message":"Not Found","documentation_url":"https://docs.github.com"}"#,
            r#"{"message":"API rate limit exceeded"}"#,
            // (the answer of `releases/latest`, which is not what is asked for any more)
            V0_2_0,
            "[1,2]",
            r#"["v1.0.0"]"#,
            r#"[{"message":"hello"}]"#,
            r#"[{"tag_name":7}]"#,
            r#"[{"tag_name":null,"draft":false}]"#,
        ] {
            assert_eq!(read_releases(body, "0.1.0"), None, "{body}");
        }
    }

    #[test]
    fn no_entry_and_drafts_only_are_no_release_and_names_that_are_no_versions_are_not_newer() {
        assert_eq!(read_releases("[]", "0.1.0"), Some(Outcome::NoRelease));
        assert_eq!(read_releases(" [ ] ", "0.1.0"), Some(Outcome::NoRelease));
        assert_eq!(read_releases(&list_of(&[entry("v0.9.0", true, false), entry("v0.8.0", true, true)]), "0.1.0"), Some(Outcome::NoRelease));
        // (GitHub has releases then, but none the app can put beside its own version)
        for tags in [&["nightly"][..], &["v", "release-2026", "1.2.3.4", "1.x"], &["-1.0.0", "1.0.0-"]] {
            assert_eq!(read_tags(tags, "0.1.0"), Some(Outcome::UpToDate), "{tags:?}");
        }
        assert_eq!(read_releases(r#"[{"tag_name":""},{"tag_name":"v"}]"#, "0.1.0"), Some(Outcome::UpToDate));
    }

    #[test]
    fn the_page_is_the_one_named_when_it_is_the_projects_and_the_name_is_cut() {
        // (no page named: the latest's is the one to open)
        assert_eq!(read_releases(r#"[{"tag_name":"1.0.0"}]"#, "0.1.0"), Some(Outcome::Newer(Release { version: "1.0.0".into(), page: LATEST_PAGE.into(), setup: None })));
        let evil = r#"[{"tag_name":"v1.0.0","html_url":"https://example.com/rewroy/dogmind-app/releases/tag/v1.0.0"}]"#;
        assert_eq!(read_releases(evil, "0.1.0"), Some(Outcome::Newer(Release { version: "1.0.0".into(), page: LATEST_PAGE.into(), setup: None })));
        let long = format!("[{{\"tag_name\":\"v1.0.0-{}\"}}]", "a".repeat(500));
        let Some(Outcome::Newer(found)) = read_releases(&long, "0.1.0") else { panic!("a long pre-release name") };
        assert_eq!(found.version.len(), MOST_OF_A_VERSION);
    }

    #[test]
    fn the_highest_version_counts_whatever_the_order_of_the_list() {
        let highest = |tags: &[&str]| match read_tags(tags, "0.1.0") {
            Some(Outcome::Newer(found)) => found.version,
            other => panic!("{tags:?}: {other:?}"),
        };
        // (0.10.0 is later than 0.9.0, though it is the earlier by the letters)
        assert_eq!(highest(&["v0.1.1", "v0.10.0", "v0.2.0", "v0.9.0"]), "0.10.0");
        assert_eq!(highest(&["v0.9.0", "v0.2.0", "v0.1.1", "v0.10.0"]), "0.10.0");
        assert_eq!(highest(&["v0.2.0", "v0.1.1"]), "0.2.0");
        assert_eq!(highest(&["v0.1.1", "v0.2.0"]), "0.2.0");
        // a release comes after its pre-releases, and a later pre-release after an earlier one
        assert_eq!(highest(&["v0.3.0-rc.1", "v0.3.0", "v0.3.0-beta.2"]), "0.3.0");
        assert_eq!(highest(&["v0.3.0-beta.2", "v0.3.0-rc.1", "v0.3.0-beta.10"]), "0.3.0-rc.1");
        assert_eq!(highest(&["v0.2.0", "v0.3.0-beta.1", "v0.2.5"]), "0.3.0-beta.1");
        // the first of two that are the same
        assert_eq!(read_tags(&["v0.2.0", "0.2.0"], "0.1.0"), Some(Outcome::Newer(release("v0.2.0"))));
        assert_eq!(read_tags(&["0.2.0", "v0.2.0"], "0.1.0"), Some(Outcome::Newer(Release { version: "0.2.0".into(), page: "https://github.com/rewroy/dogmind-app/releases/tag/0.2.0".into(), setup: None })));
    }

    #[test]
    fn a_tag_that_is_no_version_is_passed_over_even_when_it_looks_the_newest() {
        // (the names that look newest and are none: `nightly` and `latest` by the letters, `1.2.3.4` and the number too large for any version by the numbers)
        let tags = ["nightly", "v1.2.3.4", "latest", "v0.2.0", "release-2026", "v0.1.1", "v99999999999999999999.0.0"];
        assert_eq!(read_tags(&tags, "0.1.0"), Some(Outcome::Newer(release("v0.2.0"))));
        assert_eq!(read_tags(&tags, "0.2.0"), Some(Outcome::UpToDate));
    }

    #[test]
    fn a_draft_is_passed_over_and_a_pre_release_is_not() {
        let entries = [entry("v0.9.0", true, false), entry("v0.3.0-beta.1", false, true), entry("v0.2.0", false, false), entry("v0.8.0-rc.1", true, true)];
        assert_eq!(read_releases(&list_of(&entries), "0.1.0"), Some(Outcome::Newer(release("v0.3.0-beta.1"))));
        assert_eq!(read_releases(&list_of(&entries), "0.3.0-beta.1"), Some(Outcome::UpToDate));
        // (what is not `true` is not a draft)
        let odd = r#"[{"tag_name":"v0.9.0","draft":"yes"},{"tag_name":"v0.2.0","draft":null},{"tag_name":"v0.1.5"}]"#;
        assert_eq!(read_releases(odd, "0.1.0"), Some(Outcome::Newer(Release { version: "0.9.0".into(), page: LATEST_PAGE.into(), setup: None })));
    }

    #[test]
    fn versions_are_compared_by_their_numbers() {
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(!is_newer("1.9.0", "1.10.0"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(is_newer("2", "1.99.99"));
        assert!(is_newer("1.2", "1.1.9"));
        assert!(!is_newer("1.2", "1.2.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("V0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        // a pre-release comes before its release, and a later one after an earlier one
        assert!(is_newer("1.0.0", "1.0.0-rc1"));
        assert!(!is_newer("1.0.0-rc1", "1.0.0"));
        assert!(is_newer("1.0.0-rc.10", "1.0.0-rc.9"));
        assert!(is_newer("1.0.0-rc.1", "1.0.0-beta.5"));
        assert!(is_newer("1.0.0-alpha.1", "1.0.0-alpha"));
        // the build after a plus does not order
        assert!(!is_newer("1.0.0+7", "1.0.0"));
    }

    #[test]
    fn a_pre_release_is_never_newer_than_the_same_version_without_one() {
        // (the tags as a release is published under them)
        assert!(!is_newer("v0.2.0-beta.1", "0.2.0"));
        assert!(is_newer("v0.2.0", "0.2.0-beta.1"));
        assert!(!is_newer("v0.2.0-beta.1", "v0.2.0"));
        assert!(!is_newer("v0.2.0-beta.1", "0.2.0-beta.1"));
        assert!(!is_newer("v0.2.0-beta.1", "0.2.0-beta.2"));
        assert!(is_newer("v0.2.0-beta.2", "0.2.0-beta.1"));
        assert!(is_newer("v0.2.0-rc.1", "0.2.0-beta.2"));
        assert!(!is_newer("v0.2.0-beta.1+build.5", "0.2.0"));
        // but of a version that is later in its numbers it is newer, as the app wants to say of a beta that is ahead of it
        assert!(is_newer("v0.2.0-beta.1", "0.1.3"));
        assert!(is_newer("v0.2.0-beta.1", "0.1.99"));
        assert!(!is_newer("v0.1.3-beta.1", "0.1.3"));
        assert!(is_newer("v0.1.3", "0.1.3-beta.1"));
        // a hyphen in the pre-release's own name does not end it
        assert!(!is_newer("v0.2.0-beta-1", "0.2.0"));
        assert!(is_newer("v0.2.0-beta-2", "0.2.0-beta-1"));
    }

    #[test]
    fn a_tag_that_is_no_version_is_not_newer() {
        for tag in ["", "v", "latest", "nightly", "release-2026", "1.2.3.4", "1..2", "1.x", "-1.0.0", "+1.0.0", "1.0.0-", "1.0.0 beta", "99999999999999999999.0.0"] {
            assert!(!is_newer(tag, "0.1.0"), "{tag}");
        }
        // nor does the running version need to be one
        assert!(!is_newer("9.0.0", "dev"));
    }

    #[test]
    fn a_day_must_go_by_before_it_asks_again() {
        let day = 24 * 60 * 60;
        assert!(is_due(None, 1_000_000));
        assert!(!is_due(Some(1_000_000), 1_000_000));
        assert!(!is_due(Some(1_000_000), 1_000_000 + day - 1));
        assert!(is_due(Some(1_000_000), 1_000_000 + day));
        assert!(is_due(Some(1_000_000), 1_000_000 + 40 * day));
        // a clock that went back: the last look is in the future, and waiting for it would be for ever
        assert!(is_due(Some(1_000_000 + day), 1_000_000));
        assert!(is_due(Some(u64::MAX), 5));
    }

    #[test]
    fn only_a_look_that_learned_something_counts_against_the_day() {
        let release = Release { version: "0.2.0".into(), page: LATEST_PAGE.into(), setup: None };
        for outcome in [Outcome::UpToDate, Outcome::Newer(release), Outcome::NoRelease] {
            assert!(outcome.learned_something(), "{outcome:?}");
        }
        // (a look that asked no one has not heard of anything)
        assert!(!Outcome::NotAsked.learned_something());
        // a limit, an error, a page that is no release, and no way through at all: the next tick of the timer looks again
        for trouble in [Trouble::NotReached, Trouble::TooSlow, Trouble::Refused(403), Trouble::Refused(429), Trouble::Refused(500), Trouble::Refused(502), Trouble::Refused(503), Trouble::Unreadable] {
            assert!(!Outcome::Failed(trouble.clone()).learned_something(), "{trouble:?}");
        }
    }

    #[test]
    fn a_version_the_person_closed_is_not_told_again_but_a_later_one_is() {
        assert!(is_to_be_told("0.2.0", None));
        assert!(!is_to_be_told("0.2.0", Some("0.2.0")));
        assert!(!is_to_be_told("0.2.0", Some("v0.2")));
        assert!(is_to_be_told("0.2.1", Some("0.2.0")));
        assert!(is_to_be_told("0.10.0", Some("0.9.0")));
        // names that are no versions are the same when they are the same
        assert!(!is_to_be_told("spring", Some("spring ")));
        assert!(is_to_be_told("autumn", Some("spring")));
    }

    #[test]
    fn only_the_apps_own_look_tells_and_only_of_a_release_not_closed_and_while_on() {
        let release = Release { version: "0.2.0".into(), page: LATEST_PAGE.into(), setup: None };
        let newer = Outcome::Newer(release.clone());
        assert_eq!(release_to_tell(&newer, true, true, None), Some(release.clone()));
        assert_eq!(release_to_tell(&newer, true, true, Some("0.1.9")), Some(release.clone()));
        assert_eq!(release_to_tell(&newer, true, true, Some("0.2.0")), None, "closed");
        assert_eq!(release_to_tell(&newer, false, true, None), None, "the person's own look says it on the About page");
        assert_eq!(release_to_tell(&newer, true, false, None), None, "switched off while it was asking");
        for outcome in [Outcome::UpToDate, Outcome::NoRelease, Outcome::NotAsked, Outcome::Failed(Trouble::NotReached)] {
            assert_eq!(release_to_tell(&outcome, true, true, None), None);
        }
    }

    #[test]
    fn only_this_projects_release_pages_are_opened() {
        let good = "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0";
        assert_eq!(release_page(good), good);
        assert_eq!(release_page("https://github.com/rewroy/dogmind-app/releases/latest"), LATEST_PAGE);
        for bad in [
            "",
            "https://github.com/rewroy/dogmind-app/releases",
            "https://example.com/rewroy/dogmind-app/releases/tag/v0.2.0",
            "http://github.com/rewroy/dogmind-app/releases/tag/v0.2.0",
            "https://github.com.evil.example/rewroy/dogmind-app/releases/tag/v1",
            "https://github.com/rewroy/dogmind-app-evil/releases/tag/v1",
            "https://github.com/rewroy/dogmind-app/releases/../../../evil/repo",
            "https://github.com/rewroy/dogmind-app/releases/tag/v1 --flag",
            "https://github.com/rewroy/dogmind-app/releases/tag/v1\r\nx",
            "https://github.com/rewroy/dogmind-app/releases/tag/\"v1",
            "file:///C:/Windows/System32/calc.exe",
            "calc.exe",
        ] {
            assert_eq!(release_page(bad), LATEST_PAGE, "{bad}");
        }
        assert_eq!(release_page(&format!("{good}{}", "a".repeat(400))), LATEST_PAGE);
    }

    #[test]
    fn a_setup_kept_from_before_has_its_versions_tag_page() {
        assert_eq!(release_page_of("0.2.0"), "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0");
        assert_eq!(release_page_of("0.2.0-beta.1"), "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0-beta.1");
        // (a name that is no version's is the page of the latest)
        assert_eq!(release_page_of("1 --flag"), LATEST_PAGE);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn only_a_debug_build_is_pointed_elsewhere_and_a_made_up_machine_asks_nobody() {
        assert_eq!(address_for_testing(Some("http://127.0.0.1:9/x".into()), true).as_deref(), Some("http://127.0.0.1:9/x"));
        assert_eq!(address_for_testing(Some(" http://127.0.0.1:9/x ".into()), false).as_deref(), Some("http://127.0.0.1:9/x"));
        assert_eq!(address_for_testing(None, false).as_deref(), Some(RELEASES));
        assert_eq!(address_for_testing(Some("  ".into()), false).as_deref(), Some(RELEASES));
        assert_eq!(address_for_testing(None, true), None);
        assert_eq!(address_for_testing(Some(String::new()), true), None);
    }

    /// A server on this machine for one request: what it was sent (the head), and an address to ask. The address is the one the app asks of GitHub (`RELEASES`) with
    /// this server in place of GitHub, so that what the server hears is the request line the app sends. It answers with a length and does not wait for the close,
    /// which on this machine now and then never reaches a listener. `answer`: the whole of what it says.
    fn answering(answer: String) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = RELEASES.replacen("https://api.github.com", &format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port()), 1);
        assert!(address.starts_with("http://127.0.0.1:"), "{address}");
        let (heard, hearing) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut head = Vec::new();
            let mut block = [0u8; 1024];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut block) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => head.extend_from_slice(&block[..count]),
                }
            }
            let _ = heard.send(String::from_utf8_lossy(&head).into_owned());
            let _ = stream.write_all(answer.as_bytes());
            let _ = stream.flush();
            // (the client has read what the length says, and is not waited for: this thread ends with the connection whenever that comes)
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.read(&mut block);
        });
        (address, hearing)
    }

    fn with_length(status: &str, body: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}", body.len())
    }

    #[test]
    fn a_newer_release_is_found_and_the_request_carries_only_the_program() {
        let (address, hearing) = answering(with_length("200 OK", &format!("[{V0_2_0}]")));
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::Newer(Release { version: "0.2.0".into(), page: "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0".into(), setup: None }));
        let head = hearing.recv_timeout(Duration::from_secs(5)).unwrap();
        let lines: Vec<&str> = head.split("\r\n").filter(|line| !line.is_empty()).collect();
        assert_eq!(lines[0], "GET /repos/rewroy/dogmind-app/releases?per_page=20 HTTP/1.1");
        let names: Vec<String> = lines[1..].iter().map(|line| line.split(':').next().unwrap().to_ascii_lowercase()).collect();
        assert!(lines.contains(&"Accept: application/vnd.github+json") && lines.contains(&"User-Agent: Dogmind/0.1.0"), "{head}");
        assert!(names.iter().all(|name| ["host", "accept", "user-agent", "connection"].contains(&name.as_str())), "{names:?}");
    }

    /// What a look at a server that answers with these tags (a hyphen: a pre-release; a leading `!`: a draft), in this order, comes to for a program of this version.
    fn looked_at(tags: &[&str], running: &str) -> Outcome {
        let entries: Vec<String> = tags
            .iter()
            .map(|tag| match tag.strip_prefix('!') {
                Some(tag) => entry(tag, true, tag.contains('-')),
                None => entry(tag, false, tag.contains('-')),
            })
            .collect();
        let (address, _) = answering(with_length("200 OK", &list_of(&entries)));
        check_at(&address, running, Duration::from_secs(10))
    }

    #[test]
    fn a_newer_pre_release_beside_an_older_full_release_is_newer() {
        // (the first of the list is the newest, as GitHub sends it; the release before it is the one the app is)
        assert_eq!(looked_at(&["v0.2.0-beta.1", "v0.1.1"], "0.1.1"), Outcome::Newer(release("v0.2.0-beta.1")));
        // the same pre-release does not count against the full release it leads up to
        assert_eq!(looked_at(&["v0.2.0-beta.1", "v0.2.0"], "0.2.0"), Outcome::UpToDate);
        assert_eq!(looked_at(&["v0.2.0-beta.1", "v0.1.1"], "0.2.0"), Outcome::UpToDate);
        assert_eq!(looked_at(&["v0.2.0", "v0.2.0-beta.1"], "0.2.0-beta.1"), Outcome::Newer(release("v0.2.0")));
    }

    #[test]
    fn the_running_version_being_the_newest_is_up_to_date() {
        assert_eq!(looked_at(&["v0.1.3", "v0.1.2", "v0.1.1"], "0.1.3"), Outcome::UpToDate);
        assert_eq!(looked_at(&["v0.1.3", "v0.1.2", "v0.1.1"], "0.3.1"), Outcome::UpToDate);
        assert_eq!(looked_at(&["v0.1.3"], "0.1.3"), Outcome::UpToDate);
        // (a name that is no version is never newer, so a list of nothing else is up to date as well)
        assert_eq!(looked_at(&["nightly"], "0.1.0"), Outcome::UpToDate);
        let (address, _) = answering(with_length("200 OK", &format!("[{V0_2_0}]")));
        assert_eq!(check_at(&address, "0.2.0", Duration::from_secs(10)), Outcome::UpToDate);
    }

    #[test]
    fn an_empty_list_is_no_release_and_counts_as_a_look() {
        let (address, _) = answering(with_length("200 OK", "[]"));
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::NoRelease);
        assert!(outcome.learned_something());
        // (so does a list of drafts only, which a request that signs in may be given)
        assert_eq!(looked_at(&["!v0.2.0", "!v0.1.1-beta.1"], "0.1.0"), Outcome::NoRelease);
    }

    #[test]
    fn a_tag_that_is_no_version_is_skipped_and_so_is_a_draft_and_a_list_out_of_order_gives_the_highest() {
        assert_eq!(looked_at(&["nightly", "v1.2.3.4", "v0.2.0", "v0.1.1"], "0.1.1"), Outcome::Newer(release("v0.2.0")));
        assert_eq!(looked_at(&["!v0.9.0", "v0.2.0", "v0.1.1"], "0.1.1"), Outcome::Newer(release("v0.2.0")));
        assert_eq!(looked_at(&["!v0.9.0", "v0.1.1"], "0.1.1"), Outcome::UpToDate);
        assert_eq!(looked_at(&["v0.1.1", "v0.10.0", "v0.2.0", "v0.9.0"], "0.1.1"), Outcome::Newer(release("v0.10.0")));
    }

    #[test]
    fn twenty_releases_with_long_notes_are_still_read() {
        // (20 entries of 100,000 characters of notes: 2 MB, and past what the answer was allowed to be before the list was asked for)
        let notes = "x".repeat(100_000);
        let entries: Vec<String> = (1..=20).map(|at| entry(&format!("v0.1.{at}"), false, false).replace("\"Notes\"", &format!("\"{notes}\""))).collect();
        let body = list_of(&entries);
        assert!(body.len() > 1 << 20 && body.len() < MOST_OF_AN_ANSWER, "{}", body.len());
        let (address, _) = answering(with_length("200 OK", &body));
        assert_eq!(check_at(&address, "0.1.0", Duration::from_secs(10)), Outcome::Newer(release("v0.1.20")));
        // and what is longer than the bound is no list of releases
        let (address, _) = answering(with_length("200 OK", &" ".repeat(MOST_OF_AN_ANSWER + 10)));
        assert_eq!(check_at(&address, "0.1.0", Duration::from_secs(10)), Outcome::Failed(Trouble::Unreadable));
    }

    #[test]
    fn a_404_is_no_release_yet_and_not_an_error() {
        let (address, _) = answering(with_length("404 Not Found", r#"{"message":"Not Found","documentation_url":"https://docs.github.com/rest/releases/releases#list-releases","status":"404"}"#));
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::NoRelease);
        assert!(outcome.learned_something());
    }

    #[test]
    fn other_answers_and_chunked_bodies_come_out_right() {
        let (address, _) = answering(with_length("403 Forbidden", r#"{"message":"API rate limit exceeded"}"#));
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::Failed(Trouble::Refused(403)));
        assert!(!outcome.learned_something(), "GitHub's limit says nothing of releases, and an hour later it may be lifted");
        let (address, _) = answering(with_length("200 OK", "<html>a login page</html>"));
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::Failed(Trouble::Unreadable));
        assert!(!outcome.learned_something());
        // (an answer that is JSON but is no list of releases is a page of someone else's as well: an object, a list of things that are no releases, and the answer of `releases/latest`)
        for body in [r#"{"message":"hello"}"#, r#"[{"message":"hello"}]"#, r#"[1,2,3]"#, V0_2_0] {
            let (address, _) = answering(with_length("200 OK", body));
            let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
            assert_eq!(outcome, Outcome::Failed(Trouble::Unreadable), "{body}");
            assert!(!outcome.learned_something());
        }
        let (address, _) = answering(with_length("503 Service Unavailable", "try later"));
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::Failed(Trouble::Refused(503)));
        assert!(!outcome.learned_something());
        let listed = format!("[{V0_2_0}]");
        let (half, rest) = listed.split_at(100);
        let chunked = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{half}\r\n{:x}\r\n{rest}\r\n0\r\n\r\n", half.len(), rest.len());
        let (address, _) = answering(chunked);
        assert!(matches!(check_at(&address, "0.1.0", Duration::from_secs(10)), Outcome::Newer(_)));
    }

    #[test]
    fn a_look_with_nowhere_to_ask_asks_no_one_and_is_not_a_failure_to_reach_github() {
        let outcome = check_of(None);
        assert_eq!(outcome, Outcome::NotAsked);
        assert!(!outcome.learned_something());
        assert!(!matches!(outcome, Outcome::Failed(_)));
    }

    #[test]
    fn nobody_listening_and_nobody_answering_are_told_apart_and_neither_counts_as_a_look() {
        // a server that takes the connection and hangs up (not a port that was just let go of, which another test's listener may be given)
        let hangs_up = TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://127.0.0.1:{}/x", hangs_up.local_addr().unwrap().port());
        std::thread::spawn(move || drop(hangs_up.accept()));
        let outcome = check_at(&closed, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::Failed(Trouble::NotReached));
        assert!(!outcome.learned_something());
        assert_eq!(check_at("not an address", "0.1.0", Duration::from_secs(10)), Outcome::Failed(Trouble::NotReached));
        // a server that takes the request and says nothing
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://127.0.0.1:{}/x", silent.local_addr().unwrap().port());
        let began = std::time::Instant::now();
        let outcome = check_at(&address, "0.1.0", Duration::from_millis(400));
        assert_eq!(outcome, Outcome::Failed(Trouble::TooSlow));
        assert!(!outcome.learned_something());
        assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
        drop(silent);
    }

    // ---- the setup of a release: what is read of it, and fetching it ----

    const GITHUB_SETUP: &str = "https://github.com/rewroy/dogmind-app/releases/download/v0.2.0/DogmindSetup-0.2.0.exe";
    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// The list of releases GitHub answers with, for one release whose files are these (`{"name":..,"size":..}` and the like).
    fn listing_with(tag: &str, assets: &[String]) -> String {
        format!(
            r#"[{{"id":1,"html_url":"https://github.com/rewroy/dogmind-app/releases/tag/{tag}","tag_name":"{tag}","name":"Dogmind {tag}","draft":false,"prerelease":false,"assets":[{}],"body":"Notes"}}]"#,
            assets.join(",")
        )
    }

    fn asset(name: &str, size: &str, digest: &str, url: &str) -> String {
        format!(r#"{{"name":"{name}","size":{size},"digest":{digest},"browser_download_url":"{url}","state":"uploaded"}}"#)
    }

    fn github_asset() -> String {
        asset("DogmindSetup-0.2.0.exe", "563000000", &format!("\"sha256:{HASH}\""), GITHUB_SETUP)
    }

    /// The setup the app reads of a list, for a program of version 0.1.0.
    fn setup_read_from(body: &str) -> Option<Setup> {
        match read_releases(body, "0.1.0") {
            Some(Outcome::Newer(release)) => release.setup,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_release_with_its_setup_gives_its_address_size_and_digest() {
        let body = listing_with("v0.2.0", &[asset("DogmindApp-0.2.0.zip", "30408704", "\"sha256:ffff\"", "https://github.com/x"), github_asset()]);
        let Some(Outcome::Newer(release)) = read_releases(&body, "0.1.0") else { panic!("a newer release") };
        assert_eq!(release.setup, Some(Setup { url: GITHUB_SETUP.into(), size: 563_000_000, sha256: HASH.into(), parts: None }));
        assert_eq!(release.page, "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0");
        // the hash is kept as lower case, whichever way GitHub writes it
        let upper = listing_with("v0.2.0", &[asset("DogmindSetup-0.2.0.exe", "5", &format!("\"sha256:{}\"", HASH.to_uppercase()), GITHUB_SETUP)]);
        assert_eq!(setup_read_from(&upper).map(|setup| setup.sha256), Some(HASH.to_string()));
        // a pre-release has its setup as any release does, by the name of its tag
        let beta = listing_with("v0.3.0-beta.1", &[asset("DogmindSetup-0.3.0-beta.1.exe", "5", &format!("\"sha256:{HASH}\""), "https://github.com/rewroy/dogmind-app/releases/download/v0.3.0-beta.1/DogmindSetup-0.3.0-beta.1.exe")]);
        assert!(setup_read_from(&beta).is_some());
    }

    #[test]
    fn a_release_without_its_setup_or_its_digest_has_none_and_is_still_offered_by_its_page() {
        let digest = format!("\"sha256:{HASH}\"");
        let by = |name: &str, size: &str, digest: &str, url: &str| listing_with("v0.2.0", &[asset(name, size, digest, url)]);
        let cases = [
            ("no files at all", listing_with("v0.2.0", &[])),
            ("no setup among them", by("DogmindApp-0.2.0.zip", "5", &digest, GITHUB_SETUP)),
            ("the setup of another version", by("DogmindSetup-0.1.9.exe", "5", &digest, GITHUB_SETUP)),
            ("a name that only ends alike", by("xDogmindSetup-0.2.0.exe", "5", &digest, GITHUB_SETUP)),
            ("no digest", by("DogmindSetup-0.2.0.exe", "5", "null", GITHUB_SETUP)),
            ("a digest of another kind", by("DogmindSetup-0.2.0.exe", "5", &format!("\"sha1:{}\"", &HASH[..40]), GITHUB_SETUP)),
            ("a hash too short", by("DogmindSetup-0.2.0.exe", "5", "\"sha256:abcd\"", GITHUB_SETUP)),
            ("a hash that is no hex", by("DogmindSetup-0.2.0.exe", "5", &format!("\"sha256:{}\"", "g".repeat(64)), GITHUB_SETUP)),
            ("no size", by("DogmindSetup-0.2.0.exe", "null", &digest, GITHUB_SETUP)),
            ("a size of nothing", by("DogmindSetup-0.2.0.exe", "0", &digest, GITHUB_SETUP)),
            ("a size no release has", by("DogmindSetup-0.2.0.exe", "99999999999", &digest, GITHUB_SETUP)),
            ("an address that is not https", by("DogmindSetup-0.2.0.exe", "5", &digest, "http://github.com/rewroy/dogmind-app/releases/download/v0.2.0/DogmindSetup-0.2.0.exe")),
            ("an address on another host", by("DogmindSetup-0.2.0.exe", "5", &digest, "https://example.com/rewroy/dogmind-app/releases/download/v0.2.0/DogmindSetup-0.2.0.exe")),
            ("an address on a host that only looks like it", by("DogmindSetup-0.2.0.exe", "5", &digest, "https://github.com.evil.example/rewroy/dogmind-app/releases/download/v0.2.0/DogmindSetup-0.2.0.exe")),
            ("an address of another project", by("DogmindSetup-0.2.0.exe", "5", &digest, "https://github.com/someone/else/releases/download/v0.2.0/DogmindSetup-0.2.0.exe")),
            ("an address that climbs out", by("DogmindSetup-0.2.0.exe", "5", &digest, "https://github.com/rewroy/dogmind-app/releases/download/../../../evil/x.exe")),
            ("a local address, in a build that was not pointed at it", by("DogmindSetup-0.2.0.exe", "5", &digest, "http://127.0.0.1:8080/DogmindSetup-0.2.0.exe")),
        ];
        for (what, body) in cases {
            let Some(Outcome::Newer(release)) = read_releases(&body, "0.1.0") else { panic!("{what}: still a newer release") };
            assert_eq!(release.setup, None, "{what}");
            assert_eq!(release.page, "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0", "{what}");
        }
        // a name that is no name of a file has no setup however well it is listed
        let odd = listing_with("v0.2.0-a:b", &[asset("DogmindSetup-0.2.0-a:b.exe", "5", &digest, "https://github.com/rewroy/dogmind-app/releases/download/v0.2.0-a:b/x")]);
        assert_eq!(setup_read_from(&odd), None);
        let long = format!("v0.2.0-{}", "a".repeat(60));
        let cut = listing_with(&long, &[asset(&format!("DogmindSetup-{}.exe", &long[1..]), "5", &digest, GITHUB_SETUP)]);
        assert_eq!(setup_read_from(&cut), None);
    }

    #[test]
    fn a_setup_is_asked_for_on_github_and_sent_on_to_github_and_its_storage_only() {
        let first = |address: &str| first_hop(address, None).is_some();
        assert!(first(GITHUB_SETUP));
        assert!(first("https://GitHub.com/rewroy/dogmind-app/releases/download/v1/a.exe"));
        for bad in [
            "http://github.com/rewroy/dogmind-app/releases/download/v1/a.exe",
            "https://github.com:8443/rewroy/dogmind-app/releases/download/v1/a.exe",
            "https://github.com@evil.example/rewroy/dogmind-app/releases/download/v1/a.exe",
            "https://evil.example@github.com/rewroy/dogmind-app/releases/download/v1/a.exe",
            "https://objects.githubusercontent.com/rewroy/dogmind-app/releases/download/v1/a.exe",
            "https://github.com/rewroy/dogmind-app/archive/v1.zip",
            "https://github.com/rewroy/dogmind-app/releases/download/../../x",
            "https://github.com/rewroy/dogmind-app/releases/download/v1/a b.exe",
            "https://github.com/rewroy/dogmind-app/releases/download/v1/a\".exe",
            "ftp://github.com/rewroy/dogmind-app/releases/download/v1/a.exe",
            "/rewroy/dogmind-app/releases/download/v1/a.exe",
            "",
        ] {
            assert!(!first(bad), "{bad}");
        }
        let next = |address: &str| next_hop(address, None).is_some();
        for good in [
            "https://github.com/rewroy/dogmind-app/releases/download/v1/a.exe",
            "https://objects.githubusercontent.com/github-production-release-asset/1/2?X-Amz-Signature=abc&response-content-disposition=attachment%3B%20filename%3Da.exe",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r&sig=abc%3D",
            "https://RELEASE-ASSETS.githubusercontent.com/x",
            "https://a.b.githubusercontent.com/x",
        ] {
            assert!(next(good), "{good}");
        }
        for bad in [
            "http://objects.githubusercontent.com/x",
            "https://githubusercontent.com/x",
            "https://.githubusercontent.com/x",
            "https://evilgithubusercontent.com/x",
            "https://objects.githubusercontent.com.evil.example/x",
            "https://evil.example/objects.githubusercontent.com",
            "https://evil.example@objects.githubusercontent.com/x",
            "https://objects.githubusercontent.com:8443/x",
            "https://github.com.evil.example/x",
            "https://example.com/x",
            "file:///C:/Windows/System32/calc.exe",
            "/relative/path",
            "",
        ] {
            assert!(!next(bad), "{bad}");
        }
    }

    #[test]
    fn only_a_debug_builds_own_test_server_is_let_in_beside_github() {
        let server = Url::parse("http://127.0.0.1:5000/repos/x/releases").unwrap();
        let named = |address: &str| Url::parse(address).unwrap();
        assert!(is_the_test_server(&named("http://127.0.0.1:5000/download/a.exe"), Some(&server)));
        // another port of it, another host, or a secure one, is not the server
        for other in ["http://127.0.0.1:5001/a.exe", "http://localhost:5000/a.exe", "https://127.0.0.1:5000/a.exe", "http://192.168.0.1:5000/a.exe"] {
            assert!(!is_the_test_server(&named(other), Some(&server)), "{other}");
        }
        assert!(!is_the_test_server(&named("http://127.0.0.1:5000/a.exe"), None), "no server was named: none is let in");
        // a server that is not on this machine is not let in, whatever a test run is pointed at
        let far = Url::parse("http://example.com:5000/x").unwrap();
        assert!(!is_the_test_server(&named("http://example.com:5000/a.exe"), Some(&far)));
        assert!(first_hop("http://127.0.0.1:5000/a.exe", Some(&server)).is_some() && next_hop("http://127.0.0.1:5000/a.exe", Some(&server)).is_some());
        assert!(first_hop("http://127.0.0.1:5000/a.exe", None).is_none() && next_hop("http://127.0.0.1:5000/a.exe", None).is_none(), "a build for release has no server");
        // the look at a server of the test's is where it is let in; the look at GitHub's is not, and a build for release never has one
        assert_eq!(test_server_of(&server).is_some(), cfg!(debug_assertions));
        assert!(test_server_of(&Url::parse("https://api.github.com/repos/rewroy/dogmind-app/releases").unwrap()).is_none());
    }

    /// What a server of the test's answers a path with.
    enum Answer {
        Whole(Vec<u8>),
        /// These bytes, and then nothing, until the client goes: a download that has stopped coming.
        Stalls(Vec<u8>),
        /// These bytes, and then the server closes its end the moment it has written them, as an HTTP/1.0 server does, and as one does that was told `Connection: close`: the first time the
        /// path is asked for, at once, which on Windows can reset what the client had not read yet; each time after, five milliseconds on, as a close that is kind to its reader.
        Closes(Vec<u8>),
        /// The same, this many milliseconds after, every time.
        ClosesAfter(Vec<u8>, u64),
        /// What a server that keeps files answers: the whole of the body to a request that does not ask for a range of it, and 206 and the range to one that does. What it serves is counted.
        Ranged(Body, Fault),
        /// The first to a request that asks for a range, the second to one that does not.
        ByRange(Box<Answer>, Box<Answer>),
        /// The first to the first request for the path, the second to the second, and the last to each after: a path that changes.
        Turns(Vec<Answer>),
        /// An address that has gone stale.
        Forbidden,
    }

    /// What a ranged answer is of.
    enum Body {
        Bytes(Vec<u8>),
        File(PathBuf),
    }

    impl Body {
        fn len(&self) -> u64 {
            match self {
                Body::Bytes(bytes) => bytes.len() as u64,
                Body::File(path) => std::fs::metadata(path).unwrap().len(),
            }
        }

        /// The bytes from `from` to one before `to`, written to the client a block at a time.
        fn write(&self, stream: &mut std::net::TcpStream, from: u64, to: u64) {
            match self {
                Body::Bytes(bytes) => {
                    let _ = stream.write_all(&bytes[from as usize..to as usize]);
                }
                Body::File(path) => {
                    use std::io::Seek;
                    let mut file = std::fs::File::open(path).unwrap();
                    file.seek(std::io::SeekFrom::Start(from)).unwrap();
                    let mut left = to - from;
                    let mut block = vec![0u8; 1 << 20];
                    while left > 0 {
                        let count = file.read(&mut block[..left.min(1 << 20) as usize]).unwrap();
                        if count == 0 || stream.write_all(&block[..count]).is_err() {
                            return;
                        }
                        left -= count as u64;
                    }
                }
            }
        }
    }

    /// What goes wrong in a ranged answer.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        None,
        /// It says the bytes are those after the ones asked for.
        WrongRange,
        /// It stops half way, and closes.
        Short,
    }

    impl Answer {
        /// The answer that stands for this one to a request: `ByRange` and `Turns` are decided by the request, and what they decide on may be one of them again.
        fn settle(&self, head: &str, earlier: usize) -> &Answer {
            match self {
                Answer::ByRange(ranged, whole) => {
                    if head.lines().any(|line| line.to_ascii_lowercase().starts_with("range:")) { ranged.settle(head, earlier) } else { whole.settle(head, earlier) }
                }
                Answer::Turns(answers) => answers[earlier.min(answers.len() - 1)].settle(head, earlier),
                other => other,
            }
        }
    }

    /// A ranged answer, written to the client; the bytes of the body are added to `sent`.
    fn serve_ranged(stream: &mut std::net::TcpStream, head: &str, body: &Body, fault: Fault, sent: &std::sync::atomic::AtomicU64) {
        let total = body.len();
        let asked = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("range").then(|| value.trim().to_string())
            })
            .and_then(|value| {
                let (from, to) = value.strip_prefix("bytes=")?.split_once('-')?;
                Some((from.parse::<u64>().ok()?, to.parse::<u64>().ok()))
            });
        match asked {
            None => {
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\n\r\n").as_bytes());
                body.write(stream, 0, total);
                sent.fetch_add(total, Ordering::Relaxed);
            }
            Some((from, to)) if from < total => {
                let to = to.map_or(total - 1, |to| to.min(total - 1));
                let said = if fault == Fault::WrongRange { (from + 1, to + 1) } else { (from, to) };
                let _ = stream.write_all(format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{total}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n", said.0, said.1, to - from + 1).as_bytes());
                let until = if fault == Fault::Short { from + (to - from + 1) / 2 } else { to + 1 };
                body.write(stream, from, until);
                sent.fetch_add(until - from, Ordering::Relaxed);
                if fault == Fault::Short {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
            Some(_) => {
                let _ = stream.write_all(format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\n\r\n").as_bytes());
            }
        }
    }

    /// A server on this machine for as many requests as come, each on a thread of its own: the paths it answers (the path is what the request asks for, its query too) and what each request said.
    struct Server {
        port: u16,
        heard: Arc<std::sync::Mutex<Vec<String>>>,
        done: Arc<AtomicBool>,
        /// The bytes of the bodies it has served: of a ranged answer what was asked for, of any other all that was written.
        sent: Arc<std::sync::atomic::AtomicU64>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Relaxed);
        }
    }

    impl Server {
        /// `routes` is told the port first: the answers name the server's own address.
        fn start(routes: impl FnOnce(u16) -> Vec<(String, Answer)>) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let routes = Arc::new(routes(port));
            let (heard, done) = (Arc::new(std::sync::Mutex::new(Vec::<String>::new())), Arc::new(AtomicBool::new(false)));
            let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let (heard_by, done_by, sent_by) = (heard.clone(), done.clone(), sent.clone());
            std::thread::spawn(move || {
                while !done_by.load(Ordering::Relaxed) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    let (routes, heard, done, sent) = (routes.clone(), heard_by.clone(), done_by.clone(), sent_by.clone());
                    std::thread::spawn(move || {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                        let mut head = Vec::new();
                        let mut block = [0u8; 1024];
                        let began = std::time::Instant::now();
                        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                            match stream.read(&mut block) {
                                Ok(0) => return,
                                Ok(count) => head.extend_from_slice(&block[..count]),
                                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) && began.elapsed() < Duration::from_secs(10) => {}
                                Err(_) => return,
                            }
                        }
                        let head = String::from_utf8_lossy(&head).into_owned();
                        let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                        let earlier = heard.lock().unwrap().iter().filter(|known| known.split_whitespace().nth(1) == Some(path.as_str())).count();
                        heard.lock().unwrap().push(head.clone());
                        let found = routes.iter().find(|(known, _)| *known == path).map(|(_, answer)| answer.settle(&head, earlier));
                        let (bytes, stalls, closes_after) = match found {
                            Some(Answer::Whole(bytes)) => (bytes.clone(), false, None),
                            Some(Answer::Stalls(bytes)) => (bytes.clone(), true, None),
                            Some(Answer::Closes(bytes)) => (bytes.clone(), false, Some(if earlier == 0 { 0u64 } else { 5 })),
                            Some(Answer::ClosesAfter(bytes, milliseconds)) => (bytes.clone(), false, Some(*milliseconds)),
                            Some(Answer::Forbidden) => (with_length("403 Forbidden", "gone stale").into_bytes(), false, None),
                            Some(Answer::Ranged(..)) => (Vec::new(), false, None),
                            Some(Answer::ByRange(..) | Answer::Turns(_)) => unreachable!("settled above"),
                            None => (with_length("404 Not Found", "not here").into_bytes(), false, None),
                        };
                        if let Some(Answer::Ranged(body, fault)) = found {
                            serve_ranged(&mut stream, &head, body, *fault, &sent);
                        } else {
                            // (the body of what is written: what follows the head)
                            sent.fetch_add(bytes.windows(4).position(|window| window == b"\r\n\r\n").map_or(0, |at| (bytes.len() - at - 4) as u64), Ordering::Relaxed);
                        }
                        let _ = stream.write_all(&bytes);
                        let _ = stream.flush();
                        if let Some(milliseconds) = closes_after {
                            if milliseconds > 0 {
                                std::thread::sleep(Duration::from_millis(milliseconds));
                            }
                            return;
                        }
                        // (the client has what the length says, and is not waited for; one that is to stall is waited for until it goes)
                        let until = std::time::Instant::now() + if stalls { Duration::from_secs(20) } else { Duration::from_secs(2) };
                        while !done.load(Ordering::Relaxed) && std::time::Instant::now() < until {
                            match stream.read(&mut block) {
                                Ok(0) => return,
                                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                                Ok(_) => {}
                                Err(_) => return,
                            }
                        }
                    });
                }
            });
            Server { port, heard, done, sent }
        }

        /// The bytes of bodies it has served.
        fn sent(&self) -> u64 {
            self.sent.load(Ordering::Relaxed)
        }

        /// The request lines of the requests that asked for a range, with the range.
        fn ranged_requests(&self) -> Vec<String> {
            self.heard
                .lock()
                .unwrap()
                .iter()
                .filter_map(|head| {
                    let range = head.lines().find(|line| line.to_ascii_lowercase().starts_with("range:"))?;
                    Some(format!("{} {}", head.lines().next().unwrap_or(""), range.trim()))
                })
                .collect()
        }

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        fn as_url(&self) -> Url {
            Url::parse(&self.url("/")).unwrap()
        }

        /// The request line of each request it heard, in the order they came.
        fn requests(&self) -> Vec<String> {
            self.heard.lock().unwrap().iter().map(|head| head.lines().next().unwrap_or("").to_string()).collect()
        }
    }

    fn file_bytes(length: usize, seed: u8) -> Vec<u8> {
        (0..length).map(|at| (at as u8).wrapping_mul(31).wrapping_add((at >> 8) as u8).wrapping_add(seed)).collect()
    }

    fn sha_of(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    /// A whole answer: 200 and these bytes, with their length.
    fn file_answer(bytes: &[u8]) -> Answer {
        let mut answer = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n", bytes.len()).into_bytes();
        answer.extend_from_slice(bytes);
        Answer::Whole(answer)
    }

    fn redirect_to(location: &str) -> Answer {
        Answer::Whole(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n").into_bytes())
    }

    /// What is in a folder, by name.
    fn names_in(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(folder).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    const SETUP_PATH: &str = "/download/DogmindSetup-0.2.0.exe";

    /// The setup of 0.2.0 as a server of the test's has it: its bytes, and what GitHub would have published of them.
    fn setup_at(server: &Server, path: &str, bytes: &[u8]) -> Setup {
        Setup { url: server.url(path), size: bytes.len() as u64, sha256: sha_of(bytes), parts: None }
    }

    fn fetched(server: &Server, setup: &Setup, folder: &Path) -> Result<Ready, Failed> {
        obtain_from("0.2.0", setup, folder, Some(&server.as_url()), &mut |_, _| {}, &AtomicBool::new(false))
    }

    #[test]
    fn a_setup_with_the_size_and_hash_that_were_published_is_kept_whole_with_a_note_of_it() {
        let bytes = file_bytes(300_000, 1);
        let server = Server::start(|_| vec![(SETUP_PATH.into(), file_answer(&bytes))]);
        let setup = setup_at(&server, SETUP_PATH, &bytes);
        let folder = tempfile::tempdir().unwrap();
        let mut told = Vec::new();
        let ready = obtain_from("0.2.0", &setup, folder.path(), Some(&server.as_url()), &mut |done, total| told.push((done, total)), &AtomicBool::new(false)).unwrap();
        assert_eq!(ready.path, folder.path().join("DogmindSetup-0.2.0.exe"));
        assert_eq!(std::fs::read(&ready.path).unwrap(), bytes);
        assert_eq!(names_in(folder.path()), ["DogmindSetup-0.2.0.exe", "ready.txt"], "no `.partial` is left");
        assert_eq!(told.first(), Some(&(0, 300_000)));
        assert_eq!(told.last(), Some(&(300_000, 300_000)));
        assert!(told.windows(2).all(|pair| pair[0].0 <= pair[1].0), "progress only goes forward");
        // what went out says what program asks and wants a file, and no more
        let head = server.heard.lock().unwrap()[0].clone();
        let lines: Vec<&str> = head.split("\r\n").filter(|line| !line.is_empty()).collect();
        assert_eq!(lines[0], format!("GET {SETUP_PATH} HTTP/1.1"));
        assert!(lines.contains(&format!("User-Agent: Dogmind/{}", env!("CARGO_PKG_VERSION")).as_str()), "{head}");
        let names: Vec<String> = lines[1..].iter().map(|line| line.split(':').next().unwrap().to_ascii_lowercase()).collect();
        assert!(names.iter().all(|name| ["host", "accept", "user-agent", "connection"].contains(&name.as_str())), "{names:?}");
        assert!(ready.is_sound() && ready.is_there());
    }

    #[test]
    fn a_download_that_does_not_have_the_published_hash_is_refused_and_leaves_no_file() {
        let (bytes, other) = (file_bytes(200_000, 1), file_bytes(200_000, 2));
        let server = Server::start(|_| vec![(SETUP_PATH.into(), file_answer(&other))]);
        let folder = tempfile::tempdir().unwrap();
        // (the same length, so that only the hash can tell)
        let failed = fetched(&server, &setup_at(&server, SETUP_PATH, &bytes), folder.path()).unwrap_err();
        assert_eq!(failed, Failed::DidNotMatch);
        assert_eq!(names_in(folder.path()), Vec::<String>::new(), "no setup, no `.partial`, no note");
    }

    #[test]
    fn a_download_of_another_size_is_refused_whichever_way_it_says_so_and_leaves_no_file() {
        let bytes = file_bytes(100_000, 3);
        let published = Setup { url: String::new(), size: bytes.len() as u64, sha256: sha_of(&bytes), parts: None };
        let chunked = |body: &[u8]| {
            let mut answer = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n", body.len()).into_bytes();
            answer.extend_from_slice(body);
            answer.extend_from_slice(b"\r\n0\r\n\r\n");
            Answer::Whole(answer)
        };
        let longer = [bytes.clone(), b"extra".to_vec()].concat();
        // (its head says another length; a body of chunks, with no length in its head, that is shorter; and one that is longer)
        // (and the bytes that hash as GitHub published, for a size that is not the one it published: only the size can tell)
        let size = bytes.len() as u64;
        let cases = [
            ("a length that is not the published", file_answer(&bytes[..bytes.len() - 1]), size),
            ("chunks that come to less", chunked(&bytes[..bytes.len() - 10]), size),
            ("chunks that come to more", chunked(&longer), size),
            ("a hash as published, and less bytes than the size published", chunked(&bytes), size + 10),
            ("a hash as published, and more bytes than the size published", chunked(&bytes), size - 10),
            ("a hash as published, and a length in the head that is not the size published", file_answer(&bytes), size + 10),
        ];
        for (what, answer, published_size) in cases {
            let server = Server::start(|_| vec![(SETUP_PATH.into(), answer)]);
            let folder = tempfile::tempdir().unwrap();
            let setup = Setup { url: server.url(SETUP_PATH), size: published_size, ..published.clone() };
            assert_eq!(fetched(&server, &setup, folder.path()).unwrap_err(), Failed::DidNotMatch, "{what}");
            assert_eq!(names_in(folder.path()), Vec::<String>::new(), "{what}");
        }
        // a server that has not the file says so, and that is not a size that did not match
        let server = Server::start(|_| Vec::new());
        let folder = tempfile::tempdir().unwrap();
        assert_eq!(fetched(&server, &Setup { url: server.url(SETUP_PATH), ..published }, folder.path()).unwrap_err(), Failed::Refused(404));
        assert_eq!(names_in(folder.path()), Vec::<String>::new());
    }

    #[test]
    fn a_download_sent_on_to_a_host_that_is_not_allowed_is_refused_and_not_followed() {
        let bytes = file_bytes(50_000, 4);
        for (what, location) in [
            ("a host of someone else's", "https://evil.example/DogmindSetup-0.2.0.exe".to_string()),
            ("a host that only ends like GitHub's storage", "https://evilgithubusercontent.com/x".to_string()),
            ("github.com over plain http", "http://github.com/rewroy/dogmind-app/releases/download/v0.2.0/DogmindSetup-0.2.0.exe".to_string()),
            ("a path with no host", "/download/other.exe".to_string()),
            ("a file", "file:///C:/Windows/System32/cmd.exe".to_string()),
        ] {
            let server = Server::start(|_| vec![(SETUP_PATH.into(), redirect_to(&location))]);
            let folder = tempfile::tempdir().unwrap();
            let failed = fetched(&server, &setup_at(&server, SETUP_PATH, &bytes), folder.path()).unwrap_err();
            assert_eq!(failed, Failed::NotAllowed, "{what}");
            assert_eq!(server.requests().len(), 1, "{what}: it asked once, and went nowhere else");
            assert_eq!(names_in(folder.path()), Vec::<String>::new(), "{what}");
        }
        // another port of the machine is not the server of the test's
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let elsewhere = format!("http://127.0.0.1:{}/x", other.local_addr().unwrap().port());
        let server = Server::start(|_| vec![(SETUP_PATH.into(), redirect_to(&elsewhere))]);
        let folder = tempfile::tempdir().unwrap();
        assert_eq!(fetched(&server, &setup_at(&server, SETUP_PATH, &bytes), folder.path()).unwrap_err(), Failed::NotAllowed);
        other.set_nonblocking(true).unwrap();
        assert!(other.accept().is_err(), "nothing went to it");
    }

    #[test]
    fn a_download_sent_on_to_an_allowed_host_is_followed_up_to_five_times() {
        let bytes = file_bytes(80_000, 5);
        // (the server of the test stands in for github.com and its storage, and is the one host allowed, over plain http, and only in a debug build)
        let chain = |hops: usize| {
            Server::start(|port| {
                let mut routes = vec![(SETUP_PATH.to_string(), redirect_to(&format!("http://127.0.0.1:{port}/hop1?sig=a%3Db&x=1")))];
                for hop in 1..=hops {
                    let next = if hop == hops { format!("http://127.0.0.1:{port}/objects/file?sig=abc") } else { format!("http://127.0.0.1:{port}/hop{}?sig=a%3Db&x=1", hop + 1) };
                    routes.push((format!("/hop{hop}?sig=a%3Db&x=1"), redirect_to(&next)));
                }
                routes.push(("/objects/file?sig=abc".to_string(), file_answer(&bytes)));
                routes
            })
        };
        // one hop is the redirect GitHub makes to its storage
        let server = chain(1);
        let folder = tempfile::tempdir().unwrap();
        let ready = fetched(&server, &setup_at(&server, SETUP_PATH, &bytes), folder.path()).unwrap();
        assert_eq!(std::fs::read(ready.path).unwrap(), bytes);
        assert_eq!(server.requests().len(), 3, "the address asked for, the one it sent on to, and the file: {:?}", server.requests());
        // five is as far as it goes: the request that is sent on five times is the last
        let server = chain(4);
        let folder = tempfile::tempdir().unwrap();
        assert!(fetched(&server, &setup_at(&server, SETUP_PATH, &bytes), folder.path()).is_ok(), "five times is allowed");
        assert_eq!(server.requests().len(), 6);
        let server = chain(5);
        let folder = tempfile::tempdir().unwrap();
        assert_eq!(fetched(&server, &setup_at(&server, SETUP_PATH, &bytes), folder.path()).unwrap_err(), Failed::NotAllowed, "a sixth is not followed");
        assert_eq!(server.requests().len(), 6, "{:?}", server.requests());
        assert_eq!(names_in(folder.path()), Vec::<String>::new());
    }

    /// A download from a server of the test's that closes its end at once, tried until something is heard of it. On this machine a connection that its server closes at once is now and then
    /// lost whole: no byte, no close and no reset ever reach the client (plain sockets, with nothing of this code, 3 of 600), and a client waits for a silent server as long as it waits for
    /// any, which is not what is tried here. So an attempt has a few seconds to hear something, ended as Stop ends a download (the watch ends at once when the attempt is over, on a channel),
    /// and one that heard nothing is made again on a server of its own; three of them in a row is a client that is not answered. `start` makes the server and what it published.
    fn fetched_from_one_that_closes(what: &str, start: impl Fn() -> (Server, Setup)) -> (Server, tempfile::TempDir, Result<Ready, Failed>) {
        let mut lost = 0;
        loop {
            let (server, setup) = start();
            let folder = tempfile::tempdir().unwrap();
            let stop = AtomicBool::new(false);
            let (over, watching) = mpsc::channel::<()>();
            let result = std::thread::scope(|scope| {
                let cancel = &stop;
                scope.spawn(move || {
                    if watching.recv_timeout(Duration::from_secs(5)) == Err(RecvTimeoutError::Timeout) {
                        cancel.store(true, Ordering::Relaxed);
                    }
                });
                let result = obtain_from("0.2.0", &setup, folder.path(), Some(&server.as_url()), &mut |_, _| {}, &stop);
                let _ = over.send(());
                result
            });
            match result {
                Err(Failed::Cancelled) => {
                    lost += 1;
                    assert!(lost < 3, "{what}: nothing came, three times");
                }
                result => return (server, folder, result),
            }
        }
    }

    /// What GitHub's storage, a proxy or a small server of someone's may be: it answers HTTP/1.0, or HTTP/1.1 with `Connection: close`, and closes its end the moment it has answered. The first
    /// connection for each address is closed at once, which on Windows can reset it with the answer unread; the app asks again, and what it asks for is followed and checked all the same.
    #[test]
    fn a_download_is_followed_and_checked_from_a_server_that_closes_after_each_answer() {
        let bytes = file_bytes(60_000, 12);
        for (version, closing) in [("1.0", ""), ("1.1", "Connection: close\r\n")] {
            for round in 0..20 {
                let what = format!("HTTP/{version} {closing:?}, round {round}");
                let (server, folder, result) = fetched_from_one_that_closes(&what, || {
                    let server = Server::start(|port| {
                        let redirect = format!("HTTP/{version} 302 Found\r\n{closing}Location: http://127.0.0.1:{port}/storage/DogmindSetup-0.2.0.exe\r\nContent-Length: 0\r\n\r\n");
                        let mut file = format!("HTTP/{version} 200 OK\r\n{closing}Content-Length: {}\r\n\r\n", bytes.len()).into_bytes();
                        file.extend_from_slice(&bytes);
                        vec![(SETUP_PATH.to_string(), Answer::Closes(redirect.into_bytes())), ("/storage/DogmindSetup-0.2.0.exe".to_string(), Answer::ClosesAfter(file, 20))]
                    });
                    let setup = setup_at(&server, SETUP_PATH, &bytes);
                    (server, setup)
                });
                let ready = result.unwrap_or_else(|failed| panic!("{what}: {failed:?}"));
                assert_eq!(std::fs::read(&ready.path).unwrap(), bytes);
                assert!(ready.is_sound());
                let asked = server.requests();
                assert!(asked.iter().any(|line| line.contains(SETUP_PATH)) && asked.iter().any(|line| line.contains("/storage/")), "{asked:?}");
                drop(folder);
            }
        }
        // and what comes is still checked: the bytes of another file, under the right address and size, are refused
        let (_server, folder, result) = fetched_from_one_that_closes("the bytes of another file", || {
            let server = Server::start(|port| {
                let redirect = format!("HTTP/1.0 302 Found\r\nLocation: http://127.0.0.1:{port}/storage/DogmindSetup-0.2.0.exe\r\nContent-Length: 0\r\n\r\n");
                let mut file = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", bytes.len()).into_bytes();
                file.extend_from_slice(&file_bytes(bytes.len(), 13));
                vec![(SETUP_PATH.to_string(), Answer::Closes(redirect.into_bytes())), ("/storage/DogmindSetup-0.2.0.exe".to_string(), Answer::ClosesAfter(file, 20))]
            });
            let setup = setup_at(&server, SETUP_PATH, &bytes);
            (server, setup)
        });
        assert_eq!(result.err(), Some(Failed::DidNotMatch));
        assert_eq!(names_in(folder.path()), Vec::<String>::new());
    }

    #[test]
    fn a_cancelled_download_leaves_no_file() {
        let bytes = file_bytes(400_000, 6);
        let mut head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", bytes.len()).into_bytes();
        head.extend_from_slice(&bytes[..50_000]);
        // stopped from the progress it reports, with the rest of the file still to come
        let server = Server::start(|_| vec![(SETUP_PATH.into(), Answer::Stalls(head.clone()))]);
        let folder = tempfile::tempdir().unwrap();
        let (setup, stop) = (setup_at(&server, SETUP_PATH, &bytes), AtomicBool::new(false));
        let failed = obtain_from("0.2.0", &setup, folder.path(), Some(&server.as_url()), &mut |done, _| if done >= 10_000 { stop.store(true, Ordering::Relaxed) }, &stop).unwrap_err();
        assert_eq!(failed, Failed::Cancelled);
        assert_eq!(names_in(folder.path()), Vec::<String>::new());
        // and stopped while nothing more comes at all: the read is let go of at once, and not at the end of the half hour the server may keep
        let server = Server::start(|_| vec![(SETUP_PATH.into(), Answer::Stalls(head.clone()))]);
        let (setup, stop) = (setup_at(&server, SETUP_PATH, &bytes), Arc::new(AtomicBool::new(false)));
        let pressed = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            pressed.store(true, Ordering::Relaxed);
        });
        let began = std::time::Instant::now();
        assert_eq!(obtain_from("0.2.0", &setup, folder.path(), Some(&server.as_url()), &mut |_, _| {}, &stop).unwrap_err(), Failed::Cancelled);
        assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
        assert_eq!(names_in(folder.path()), Vec::<String>::new());
        // (one that was stopped before it began asks no one)
        let server = Server::start(|_| Vec::new());
        assert_eq!(obtain_from("0.2.0", &setup_at(&server, SETUP_PATH, &bytes), folder.path(), Some(&server.as_url()), &mut |_, _| {}, &AtomicBool::new(true)).unwrap_err(), Failed::Cancelled);
        assert!(server.requests().is_empty());
    }

    #[test]
    fn an_address_that_is_not_allowed_is_not_asked_at_all() {
        let server = Server::start(|_| Vec::new());
        let folder = tempfile::tempdir().unwrap();
        let bytes = file_bytes(10, 0);
        // (in a build for release nothing but GitHub's own is: the test server is the debug build's)
        let setup = Setup { url: server.url(SETUP_PATH), size: 10, sha256: sha_of(&bytes), parts: None };
        assert_eq!(obtain_from("0.2.0", &setup, folder.path(), None, &mut |_, _| {}, &AtomicBool::new(false)).unwrap_err(), Failed::NotAllowed);
        assert!(server.requests().is_empty());
        for url in ["https://evil.example/x.exe", "http://github.com/rewroy/dogmind-app/releases/download/v0.2.0/a.exe"] {
            assert_eq!(obtain_from("0.2.0", &Setup { url: url.into(), ..setup.clone() }, folder.path(), None, &mut |_, _| {}, &AtomicBool::new(false)).unwrap_err(), Failed::NotAllowed, "{url}");
        }
        // and a version that is no name of a file is not made into one
        assert_eq!(obtain_from("0.2.0\\..\\x", &setup, folder.path(), Some(&server.as_url()), &mut |_, _| {}, &AtomicBool::new(false)).unwrap_err(), Failed::NotAllowed);
        assert_eq!(names_in(folder.path()), Vec::<String>::new());
    }

    #[test]
    fn what_was_checked_is_offered_again_at_the_next_start_and_not_fetched_again() {
        let bytes = file_bytes(250_000, 7);
        let server = Server::start(|_| vec![(SETUP_PATH.into(), file_answer(&bytes))]);
        let setup = setup_at(&server, SETUP_PATH, &bytes);
        let folder = tempfile::tempdir().unwrap();
        let first = fetched(&server, &setup, folder.path()).unwrap();
        assert_eq!(server.requests().len(), 1);
        // the next start: nothing asked, nothing fetched, and the same setup is there
        let found = kept(folder.path(), "0.1.1").expect("it is there for the next start");
        assert_eq!(found, first);
        assert!(found.is_sound());
        // (and what is found is offered only for a version later than the one that runs)
        assert_eq!(kept(folder.path(), "0.2.0"), None);
        assert_eq!(kept(folder.path(), "0.3.0"), None);
        assert_eq!(kept(folder.path(), "0.2.0-beta.1").map(|ready| ready.version), Some("0.2.0".to_string()));
        // a look that finds the same release again does not fetch it again
        let again = fetched(&server, &setup, folder.path()).unwrap();
        assert_eq!(again, first);
        assert_eq!(server.requests().len(), 1, "the file that was checked is used: {:?}", server.requests());
        // but a release that is not that one is fetched, and takes its place
        let newer = file_bytes(260_000, 8);
        let server_two = Server::start(|_| vec![("/download/DogmindSetup-0.2.1.exe".into(), file_answer(&newer))]);
        let setup_two = setup_at(&server_two, "/download/DogmindSetup-0.2.1.exe", &newer);
        let second = obtain_from("0.2.1", &setup_two, folder.path(), Some(&server_two.as_url()), &mut |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(names_in(folder.path()), ["DogmindSetup-0.2.1.exe", "ready.txt"], "the one it replaces is not kept as well");
        assert_eq!(kept(folder.path(), "0.1.1"), Some(second));
        // what is changed in the folder since is found out: the file's size at once, its bytes when they are read
        let path = folder.path().join("DogmindSetup-0.2.1.exe");
        let mut changed = newer.clone();
        changed[100] ^= 1;
        std::fs::write(&path, &changed).unwrap();
        assert!(kept(folder.path(), "0.1.1").is_some_and(|ready| !ready.is_sound()), "the same length, other bytes");
        std::fs::write(&path, &changed[..1000]).unwrap();
        assert_eq!(kept(folder.path(), "0.1.1"), None, "a file that is shorter is not what was checked");
    }

    #[test]
    fn a_fetch_that_fails_leaves_the_setup_checked_before_to_be_offered_again_and_one_that_succeeds_takes_its_place() {
        let folder = tempfile::tempdir().unwrap();
        let (old_bytes, new_bytes, other_bytes) = (file_bytes(250_000, 3), file_bytes(260_000, 4), file_bytes(260_000, 5));
        // 0.1.3, fetched and checked by an earlier look, for the copy that runs as 0.1.2
        let old_server = Server::start(|_| vec![("/download/DogmindSetup-0.1.3.exe".into(), file_answer(&old_bytes))]);
        let old_setup = setup_at(&old_server, "/download/DogmindSetup-0.1.3.exe", &old_bytes);
        let old = obtain_from("0.1.3", &old_setup, folder.path(), Some(&old_server.as_url()), &mut |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(names_in(folder.path()), ["DogmindSetup-0.1.3.exe", "ready.txt"]);
        // a fetch of 0.1.4 that fails, three ways: GitHub answers with an error, what comes is not what was published, and the person stops it
        let gone = Server::start(|_| Vec::new());
        let wrong = Server::start(|_| vec![("/download/DogmindSetup-0.1.4.exe".into(), file_answer(&other_bytes))]);
        let new_at = |server: &Server| setup_at(server, "/download/DogmindSetup-0.1.4.exe", &new_bytes);
        for (server, stopped, expected) in [(&gone, false, Failed::Refused(404)), (&wrong, false, Failed::DidNotMatch), (&gone, true, Failed::Cancelled)] {
            let failed = obtain_from("0.1.4", &new_at(server), folder.path(), Some(&server.as_url()), &mut |_, _| {}, &AtomicBool::new(stopped)).unwrap_err();
            assert_eq!(failed, expected);
            assert_eq!(names_in(folder.path()), ["DogmindSetup-0.1.3.exe", "ready.txt"], "{expected:?}: the setup checked before stays, with its note");
            assert_eq!(kept(folder.path(), "0.1.2"), Some(old.clone()), "{expected:?}: and is found for the app to offer again");
            assert_eq!(kept_and_sound(folder.path(), "0.1.2"), Some(old.clone()), "{expected:?}");
        }
        // a fetch of 0.1.4 that succeeds takes the place of 0.1.3: its file and its note go
        let server = Server::start(|_| vec![("/download/DogmindSetup-0.1.4.exe".into(), file_answer(&new_bytes))]);
        let new = obtain_from("0.1.4", &new_at(&server), folder.path(), Some(&server.as_url()), &mut |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(names_in(folder.path()), ["DogmindSetup-0.1.4.exe", "ready.txt"], "the one it replaces is not kept as well");
        assert_eq!(kept(folder.path(), "0.1.2"), Some(new.clone()));
        assert_eq!(read_record(folder.path()), Some(new.clone()), "the note is the new one's");
        // the note of the very file that a fetch is about to replace is not true of it any more: when that fetch fails it is gone, and the file is no longer offered
        let republished = Setup { sha256: sha_of(&other_bytes), ..new_at(&server) };
        assert_eq!(obtain_from("0.1.4", &republished, folder.path(), Some(&server.as_url()), &mut |_, _| {}, &AtomicBool::new(true)).unwrap_err(), Failed::Cancelled);
        assert_eq!(kept(folder.path(), "0.1.2"), None);
        assert!(!folder.path().join("ready.txt").exists());
    }

    #[test]
    fn a_setup_kept_whose_bytes_are_not_those_that_were_checked_is_not_offered_and_is_taken_away() {
        let folder = tempfile::tempdir().unwrap();
        let bytes = file_bytes(100_000, 6);
        let server = Server::start(|_| vec![(SETUP_PATH.into(), file_answer(&bytes))]);
        let ready = fetched(&server, &setup_at(&server, SETUP_PATH, &bytes), folder.path()).unwrap();
        assert_eq!(kept_and_sound(folder.path(), "0.1.0"), Some(ready.clone()));
        // (the same length, other bytes)
        let mut changed = bytes.clone();
        changed[10] ^= 1;
        std::fs::write(&ready.path, &changed).unwrap();
        assert_eq!(kept(folder.path(), "0.1.0"), Some(ready), "its size is what was checked");
        assert_eq!(kept_and_sound(folder.path(), "0.1.0"), None);
        assert_eq!(names_in(folder.path()), Vec::<String>::new(), "neither the file nor its note is left");
    }

    /// A setup file of these bytes in a folder, and the note of what was checked of it.
    fn checked_file(folder: &Path, bytes: &[u8]) -> Ready {
        let path = folder.join("DogmindSetup-0.2.0.exe");
        std::fs::write(&path, bytes).unwrap();
        Ready { version: "0.2.0".into(), path, size: bytes.len() as u64, sha256: sha_of(bytes) }
    }

    #[test]
    fn a_setup_is_run_only_if_its_bytes_are_those_that_were_checked_and_cannot_be_changed_while_held() {
        let folder = tempfile::tempdir().unwrap();
        let bytes = file_bytes(300_000, 9);
        let ready = checked_file(folder.path(), &bytes);
        // as it was checked: held, with every other way to it shut but reading
        let held = ready.hold_if_sound().expect("the bytes are the ones that were checked");
        let elsewhere = folder.path().join("elsewhere.exe");
        assert!(std::fs::OpenOptions::new().write(true).open(&ready.path).is_err(), "no one writes to it");
        assert!(std::fs::write(&ready.path, b"another").is_err());
        assert!(std::fs::rename(&ready.path, &elsewhere).is_err(), "no one moves it");
        assert!(std::fs::remove_file(&ready.path).is_err(), "no one deletes it");
        assert_eq!(std::fs::read(&ready.path).unwrap(), bytes, "reading it is let");
        drop(held);
        assert!(std::fs::OpenOptions::new().write(true).open(&ready.path).is_ok(), "and it is let go of");
        // changed after it was recorded: the same length with one byte other, shorter, longer, empty, gone
        let mut changed = bytes.clone();
        changed[200_000] ^= 1;
        for (words, other) in [("one byte", changed), ("shorter", bytes[..1000].to_vec()), ("longer", [bytes.clone(), vec![0]].concat()), ("empty", Vec::new())] {
            std::fs::write(&ready.path, &other).unwrap();
            assert_eq!(ready.hold_if_sound().map(|_| ()), Err(NotRun::Changed), "{words}");
        }
        std::fs::remove_file(&ready.path).unwrap();
        assert_eq!(ready.hold_if_sound().map(|_| ()), Err(NotRun::Changed), "gone");
        // and what was checked, put back, is sound again
        std::fs::write(&ready.path, &bytes).unwrap();
        assert!(ready.hold_if_sound().is_ok());
        // a file that someone is writing to cannot be held shut: it is refused with the system's words, and is not called changed
        let writer = std::fs::OpenOptions::new().write(true).open(&ready.path).unwrap();
        assert!(matches!(ready.hold_if_sound(), Err(NotRun::Refused(_))));
        drop(writer);
    }

    #[test]
    fn a_setup_is_started_while_the_handle_that_checked_it_is_still_open() {
        // (a copy of the command interpreter stands in for the setup: it is a program, and it ends with the code it is told)
        let folder = tempfile::tempdir().unwrap();
        let program = std::fs::read(std::env::var_os("ComSpec").expect("ComSpec")).unwrap();
        let ready = checked_file(folder.path(), &program);
        let held = ready.hold_if_sound().unwrap();
        assert!(std::fs::remove_file(&ready.path).is_err(), "it is held");
        let mut child = run_held(&ready, held, &["/c".into(), "exit 7".into()]).expect("Windows starts a program whose file a handle holds shut to writers");
        assert_eq!(child.wait().unwrap().code(), Some(7));
        // (the handle is let go of once the program has started; Windows lets go of the program's own file a moment after it ends)
        let removed = (0..40).any(|_| std::fs::remove_file(&ready.path).is_ok() || { std::thread::sleep(Duration::from_millis(50)); false });
        assert!(removed, "nothing holds the file any more");
    }

    #[test]
    fn the_start_takes_out_what_is_done_with_and_nothing_else() {
        let folder = tempfile::tempdir().unwrap();
        let put = |name: &str| std::fs::write(folder.path().join(name), name).unwrap();
        for name in ["DogmindSetup-0.1.0.exe", "DogmindSetup-0.1.2.exe", "DogmindSetup-0.3.0.exe", "DogmindSetup-0.4.0.exe.partial", "DogmindSetup-0.9.9.exe.partial", "notes.txt", "DogmindSetup-0.1.1-beta.1.exe", "DogmindSetup-x.exe"] {
            put(name);
        }
        let ready = Ready { version: "0.3.0".into(), path: folder.path().join("DogmindSetup-0.3.0.exe"), size: "DogmindSetup-0.3.0.exe".len() as u64, sha256: HASH.into() };
        write_record(folder.path(), &ready).unwrap();
        clean_up(folder.path(), "0.1.2");
        // not later than 0.1.2: 0.1.0, 0.1.2 itself, a pre-release before it, and a name that is no version; any `.partial`; 0.3.0, which waits, stays, and so does a file that is not a setup
        assert_eq!(names_in(folder.path()), ["DogmindSetup-0.3.0.exe", "notes.txt", "ready.txt"]);
        assert_eq!(kept(folder.path(), "0.1.2"), Some(ready));
        // the version that waits has been installed: it goes, and so does the note of it
        clean_up(folder.path(), "0.3.0");
        assert_eq!(names_in(folder.path()), ["notes.txt"]);
        // a note of a file that is not there goes too, and so does one that is not in order
        write_record(folder.path(), &Ready { version: "0.5.0".into(), path: folder.path().join("DogmindSetup-0.5.0.exe"), size: 5, sha256: HASH.into() }).unwrap();
        clean_up(folder.path(), "0.3.0");
        assert_eq!(names_in(folder.path()), ["notes.txt"]);
        std::fs::write(folder.path().join("ready.txt"), "version ..\\x\nsize 5\nsha256 nothing\n").unwrap();
        assert_eq!(read_record(folder.path()), None);
        clean_up(folder.path(), "0.3.0");
        assert_eq!(names_in(folder.path()), ["notes.txt"]);
        // a start with no such folder has nothing to take, and makes none
        clean_up(&folder.path().join("none"), "0.3.0");
        assert!(!folder.path().join("none").exists());
    }

    /// (only a debug build lets a setup be fetched from the server of the test's: in a build for release the look finds no setup there, which is what is wanted of it)
    #[cfg(debug_assertions)]
    #[test]
    fn the_automatic_download_starts_after_an_automatic_look_finds_a_newer_release_with_its_setup() {
        let bytes = file_bytes(120_000, 9);
        let server = Server::start(|port| {
            let setup = asset("DogmindSetup-0.2.0.exe", &bytes.len().to_string(), &format!("\"sha256:{}\"", sha_of(&bytes)), &format!("http://127.0.0.1:{port}{SETUP_PATH}"));
            vec![
                ("/repos/rewroy/dogmind-app/releases?per_page=20".to_string(), Answer::Whole(with_length("200 OK", &listing_with("v0.2.0", &[setup])).into_bytes())),
                (SETUP_PATH.to_string(), file_answer(&bytes)),
            ]
        });
        let address = RELEASES.replacen("https://api.github.com", &format!("http://127.0.0.1:{}", server.port), 1);
        // the app's own look, a little after its start
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        let Outcome::Newer(release) = outcome.clone() else { panic!("{outcome:?}") };
        assert_eq!(release.setup.as_ref().map(|setup| setup.size), Some(bytes.len() as u64));
        // what the app does with it: on an install, with the switch on and the notice not closed, it goes and fetches
        let to_fetch = setup_to_fetch(&outcome, true, true, None, true).expect("the setup is to be fetched on its own");
        assert_eq!(to_fetch, release);
        let setup = to_fetch.setup.clone().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let ready = obtain_from(&to_fetch.version, &setup, folder.path(), Some(&Url::parse(&address).unwrap()), &mut |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::read(&ready.path).unwrap(), bytes);
        assert_eq!(server.requests(), ["GET /repos/rewroy/dogmind-app/releases?per_page=20 HTTP/1.1", format!("GET {SETUP_PATH} HTTP/1.1").as_str()], "one look, one fetch");
        // and when it is not to: not on a copy from the zip, not with the switch off, not for a version whose notice was closed, never for a look that found nothing newer
        assert_eq!(setup_to_fetch(&outcome, true, true, None, false), None, "a copy from the zip is given the page");
        assert_eq!(setup_to_fetch(&outcome, true, false, None, true), None, "switched off while the look was made: nothing on its own");
        assert_eq!(setup_to_fetch(&outcome, true, true, Some("0.2.0"), true), None);
        assert!(setup_to_fetch(&outcome, true, true, Some("0.1.9"), true).is_some());
        // the person's own look fetches whatever the switch and the notice say: they asked
        assert!(setup_to_fetch(&outcome, false, false, Some("0.2.0"), true).is_some());
        assert_eq!(setup_to_fetch(&outcome, false, true, None, false), None, "but a copy from the zip is still given the page");
        let page_only = Outcome::Newer(Release { setup: None, ..release });
        assert_eq!(setup_to_fetch(&page_only, true, true, None, true), None);
        for outcome in [Outcome::UpToDate, Outcome::NoRelease, Outcome::NotAsked, Outcome::Failed(Trouble::NotReached)] {
            assert_eq!(setup_to_fetch(&outcome, false, true, None, true), None);
        }
    }

    #[test]
    fn the_installer_is_run_to_update_the_folder_after_this_app_and_its_engine_have_ended() {
        let folder = Path::new("C:\\Users\\me\\AppData\\Local\\Programs\\Dogmind");
        let words = |arguments: Vec<std::ffi::OsString>| arguments.into_iter().map(|word| word.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(words(setup_arguments(folder, &[100])), ["--update", "--dir", "C:\\Users\\me\\AppData\\Local\\Programs\\Dogmind", "--wait-pid", "100"]);
        assert_eq!(words(setup_arguments(folder, &[100, 200])), ["--update", "--dir", "C:\\Users\\me\\AppData\\Local\\Programs\\Dogmind", "--wait-pid", "100", "--wait-pid", "200"]);
    }

    #[test]
    fn a_copy_is_an_install_when_the_installers_manifest_is_in_its_folder() {
        let folder = tempfile::tempdir().unwrap();
        assert!(!is_install(folder.path()), "a copy unpacked from the zip");
        std::fs::write(folder.path().join("dogmind-app.exe"), "x").unwrap();
        std::fs::write(folder.path().join("LICENSE.txt"), "x").unwrap();
        assert!(!is_install(folder.path()), "its files are not the installer's note");
        std::fs::write(folder.path().join(INSTALL_MANIFEST), "version 0.1.1\nfile dogmind-app.exe\n").unwrap();
        assert!(is_install(folder.path()), "what the installer wrote there");
        assert!(!is_install(&folder.path().join("elsewhere")));
        // a folder of that name is no file of the installer's
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join(INSTALL_MANIFEST)).unwrap();
        assert!(!is_install(other.path()));
        // the name is the one the installer writes, which this cannot link to
        assert!(include_str!("../../dogmind-setup/src/install.rs").contains(&format!("pub const MANIFEST: &str = \"{INSTALL_MANIFEST}\";")));
    }

    // ---- a setup put together of parts: only what changed is fetched, by ranges ----

    use super::ranges::{self, setup_pack, support::*};

    const PARTS_PATH: &str = "/download/DogmindSetup-0.2.0.parts";

    fn old_files() -> Vec<(&'static str, Vec<u8>)> {
        vec![("LICENSE.txt", b"license text. ".repeat(1500)), ("big-a.dll", noisy(3 << 20, 1)), ("big-b.dll", noisy(3 << 20, 2)), ("dogmind-app.exe", noisy(200_000, 3)), ("tools\\z.py", noisy(3 << 20, 4))]
    }

    /// The release after: one file is other, and one is new.
    fn new_files() -> Vec<(&'static str, Vec<u8>)> {
        let mut files = old_files();
        files[3].1 = noisy(200_000, 5);
        files.push(("new.txt", b"fresh file, ".repeat(3000)));
        files
    }

    fn borrowed<'a>(files: &'a [(&'static str, Vec<u8>)]) -> Vec<(&'static str, &'a [u8])> {
        files.iter().map(|(name, bytes)| (*name, bytes.as_slice())).collect()
    }

    /// A release of 0.2.0 and the install it is for: the setup and its parts file as `dogmind-setup --pack` made them, and the install folder of 0.1.9 that the installer left.
    struct Scene {
        dir: tempfile::TempDir,
        exe: PathBuf,
        exe_bytes: Vec<u8>,
        parts_bytes: Vec<u8>,
        install: PathBuf,
        folder: PathBuf,
    }

    impl Scene {
        fn new(with_sums: bool) -> Scene {
            let dir = tempfile::tempdir().unwrap();
            let exe = make_setup(dir.path(), "0.2.0", &borrowed(&new_files()));
            let (exe_bytes, parts_bytes) = (std::fs::read(&exe).unwrap(), std::fs::read(setup_pack::parts_path(&exe)).unwrap());
            let install = dir.path().join("install");
            make_install(&install, &borrowed(&old_files()), with_sums);
            let folder = dir.path().join("updates");
            Scene { dir, exe, exe_bytes, parts_bytes, install, folder }
        }

        /// The setup as GitHub lists it, at this server: its size and hash, and its parts file's.
        fn setup(&self, server: &Server) -> Setup {
            let parts = Setup { url: server.url(PARTS_PATH), size: self.parts_bytes.len() as u64, sha256: sha_of(&self.parts_bytes), parts: None };
            Setup { parts: Some(Box::new(parts)), ..setup_at(server, SETUP_PATH, &self.exe_bytes) }
        }

        fn server(&self, exe: Answer, parts: Answer) -> Server {
            Server::start(move |port| {
                vec![
                    (SETUP_PATH.to_string(), redirect_to(&format!("http://127.0.0.1:{port}/storage/setup?sig=1"))),
                    ("/storage/setup?sig=1".to_string(), exe),
                    (PARTS_PATH.to_string(), redirect_to(&format!("http://127.0.0.1:{port}/storage/parts?sig=1"))),
                    ("/storage/parts?sig=1".to_string(), parts),
                ]
            })
        }

        /// A server that has the release as GitHub's storage has it: ranges are answered.
        fn good_server(&self) -> Server {
            self.server(Answer::Ranged(Body::Bytes(self.exe_bytes.clone()), Fault::None), Answer::Ranged(Body::Bytes(self.parts_bytes.clone()), Fault::None))
        }

        fn obtain(&self, server: &Server, setup: &Setup) -> Result<Ready, Failed> {
            obtain_within("0.2.0", setup, Some(&self.install), &self.folder, Some(&server.as_url()), &mut |_, _| {}, &AtomicBool::new(false))
        }

        /// What putting the setup together comes to by itself, which says why it did not when it did not.
        fn assembled(&self, server: &Server, setup: &Setup) -> Result<ranges::Assembled, ranges::Not> {
            let job = ranges::Job { version: "0.2.0", setup, parts: setup.parts.as_deref().unwrap(), install: &self.install, test_server: Some(&server.as_url()) };
            let partial = self.dir.path().join("assembled.partial");
            let result = ranges::assemble(&job, &partial, &mut |_, _| {}, &AtomicBool::new(false));
            let _ = std::fs::remove_file(&partial);
            result
        }

        /// The whole setup is what the folder has after: the bytes GitHub published.
        fn assert_fetched_whole(&self, ready: Result<Ready, Failed>) {
            let ready = ready.expect("the setup is fetched whole when the parts are no way to it");
            assert_eq!((ready.size, ready.sha256.as_str()), (self.exe_bytes.len() as u64, sha_of(&self.exe_bytes).as_str()));
            assert_eq!(std::fs::read(&ready.path).unwrap(), self.exe_bytes);
            assert_eq!(names_in(&self.folder), ["DogmindSetup-0.2.0.exe", "ready.txt"], "no `.partial` is left");
            assert_eq!(read_published(&self.folder), None, "and it is the setup that was published");
        }
    }

    /// Every file of a setup of the installer's making, opened and laid out as an install of 0.2.0 has to have it: kept from the folder, or in the package.
    fn tree_of(path: &Path, install: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut package = setup_pack::Package::open(path).unwrap();
        let mut tree = std::collections::BTreeMap::new();
        for entry in package.entries.clone() {
            let bytes = if entry.method == setup_pack::KEPT {
                let bytes = std::fs::read(install.join(&entry.path)).unwrap();
                assert_eq!(Sha256::digest(&bytes).as_slice(), entry.sha256, "{}: kept from the install, and it is the one", entry.path);
                bytes
            } else {
                let mut bytes = Vec::new();
                package.extract(&entry, &mut bytes, &mut |_| {}).unwrap();
                bytes
            };
            tree.insert(entry.path.clone(), bytes);
        }
        tree
    }

    #[test]
    fn a_range_update_fetches_the_program_and_the_files_that_changed_and_nothing_else() {
        let scene = Scene::new(true);
        let server = scene.good_server();
        let setup = scene.setup(&server);
        let mut told = Vec::new();
        let ready = obtain_within("0.2.0", &setup, Some(&scene.install), &scene.folder, Some(&server.as_url()), &mut |done, total| told.push((done, total)), &AtomicBool::new(false)).unwrap();

        // the bytes served: the parts file, the program, and the stored bytes of the file that changed and the one that is new, which are next to each other in the setup
        let package = setup_pack::Package::open(&scene.exe).unwrap();
        let stored = |name: &str| package.entries.iter().find(|entry| entry.path == name).unwrap().stored;
        let fetched = package.base + stored("dogmind-app.exe") + stored("new.txt");
        assert_eq!(server.sent(), scene.parts_bytes.len() as u64 + fetched);
        assert!(fetched * 20 < scene.exe_bytes.len() as u64, "{fetched} of {}", scene.exe_bytes.len());
        // by two requests, to the storage the setup was sent on to; the setup's own address was asked once
        let storage: Vec<String> = server.ranged_requests().into_iter().filter(|line| line.contains("/storage/setup")).collect();
        assert_eq!(storage.len(), 2, "{:?}", server.requests());
        assert!(storage[0].ends_with(&format!("bytes=0-{}", package.base - 1)));
        assert_eq!(server.requests().iter().filter(|line| line.contains(SETUP_PATH)).count(), 1, "the redirect is followed once: {:?}", server.requests());
        // the progress is of those bytes
        assert_eq!(told.first(), Some(&(0, fetched)));
        assert_eq!(told.last(), Some(&(fetched, fetched)));
        assert!(told.windows(2).all(|pair| pair[0].0 <= pair[1].0));

        // the setup that was put together is a setup: it opens, it says what the release's does, and the files it holds and keeps are the release's, file for file
        assert!(ready.is_there() && ready.is_sound());
        assert_eq!(ready.size, std::fs::metadata(&ready.path).unwrap().len());
        assert!(ready.size * 20 < scene.exe_bytes.len() as u64);
        let made = setup_pack::Package::open(&ready.path).unwrap();
        assert_eq!(made.version, "0.2.0");
        assert_eq!(made.base, package.base);
        let kept: Vec<&str> = made.entries.iter().filter(|entry| entry.method == setup_pack::KEPT).map(|entry| entry.path.as_str()).collect();
        assert_eq!(kept, ["LICENSE.txt", "big-a.dll", "big-b.dll", "tools\\z.py"]);
        for (made, whole) in made.entries.iter().zip(&package.entries) {
            assert_eq!((&made.path, made.size, made.sha256), (&whole.path, whole.size, whole.sha256));
            if made.method != setup_pack::KEPT {
                assert_eq!((made.method, made.stored), (whole.method, whole.stored));
            }
        }
        let wanted: std::collections::BTreeMap<String, Vec<u8>> = new_files().into_iter().map(|(name, bytes)| (name.to_string(), bytes)).collect();
        assert_eq!(tree_of(&ready.path, &scene.install), wanted);
        // and it is kept like any other, with a note that says which release's it stands for: a look that finds the same release does not fetch it again
        assert_eq!(names_in(&scene.folder), ["DogmindSetup-0.2.0.exe", "ready.txt"]);
        assert_eq!(read_published(&scene.folder), Some((setup.size, setup.sha256.clone())));
        assert_eq!(kept_and_sound(&scene.folder, "0.1.9"), Some(ready.clone()));
        let asked = server.requests().len();
        assert_eq!(scene.obtain(&server, &setup).unwrap(), ready);
        assert_eq!(server.requests().len(), asked, "nothing was asked");
    }

    #[test]
    fn a_file_next_to_the_program_joins_its_request_and_one_far_from_it_has_its_own() {
        let scene = Scene::new(true);
        // the license is the first file of the setup, a header after the program; the others that are wanted are 6 MB on, after two that are not
        std::fs::remove_file(scene.install.join("LICENSE.txt")).unwrap();
        let server = scene.good_server();
        let setup = scene.setup(&server);
        scene.obtain(&server, &setup).unwrap();
        let package = setup_pack::Package::open(&scene.exe).unwrap();
        let entry = |name: &str| package.entries.iter().find(|entry| entry.path == name).unwrap().clone();
        let (license, app, new) = (entry("LICENSE.txt"), entry("dogmind-app.exe"), entry("new.txt"));
        let storage: Vec<String> = server.ranged_requests().into_iter().filter(|line| line.contains("/storage/setup")).collect();
        let first_end = package.base + license.offset + license.stored;
        let second = (package.base + app.offset, package.base + new.offset + new.stored);
        assert_eq!(storage.len(), 2, "{storage:?}");
        assert!(storage[0].ends_with(&format!("bytes=0-{}", first_end - 1)), "the program and the license in one: {storage:?}");
        assert!(storage[1].ends_with(&format!("bytes={}-{}", second.0, second.1 - 1)), "{storage:?}");
        assert_eq!(server.sent(), scene.parts_bytes.len() as u64 + first_end + (second.1 - second.0));
    }

    #[test]
    fn when_most_of_the_setup_is_wanted_it_is_fetched_whole() {
        let scene = Scene::new(true);
        // three of the four files that cannot be made smaller are not in the install any more
        for name in ["big-a.dll", "big-b.dll", "tools\\z.py"] {
            std::fs::remove_file(scene.install.join(name)).unwrap();
        }
        let server = scene.good_server();
        let setup = scene.setup(&server);
        assert_eq!(scene.assembled(&server, &setup), Err(ranges::Not::Because(ranges::NOT_WORTH_IT)));
        scene.assert_fetched_whole(scene.obtain(&server, &setup));
        assert!(server.ranged_requests().is_empty(), "no range was asked for: {:?}", server.requests());
    }

    #[test]
    fn a_server_that_ignores_range_has_the_setup_fetched_whole() {
        let scene = Scene::new(true);
        let server = scene.server(file_answer(&scene.exe_bytes), Answer::Ranged(Body::Bytes(scene.parts_bytes.clone()), Fault::None));
        let setup = scene.setup(&server);
        assert_eq!(scene.assembled(&server, &setup), Err(ranges::Not::Because(ranges::WHOLE_FILE)));
        scene.assert_fetched_whole(scene.obtain(&server, &setup));
        // it asked for a range first, and was answered with all of it; then it asked for the file
        assert!(!server.ranged_requests().is_empty());
        let last = server.heard.lock().unwrap().last().unwrap().clone();
        assert!(last.starts_with("GET /storage/setup") && !last.to_ascii_lowercase().contains("range:"), "{last}");
    }

    #[test]
    fn a_range_that_comes_damaged_has_the_setup_fetched_whole() {
        let scene = Scene::new(true);
        let package = setup_pack::Package::open(&scene.exe).unwrap();
        let place = |name: &str| {
            let entry = package.entries.iter().find(|entry| entry.path == name).unwrap();
            (package.base + entry.offset + entry.stored / 2) as usize
        };
        // the program, a file that is stored as it is (dogmind-app.exe), and one that is deflated (new.txt): a byte of each is another
        for (what, at) in [("the program", 100), ("a stored file", place("dogmind-app.exe")), ("a deflated file", place("new.txt"))] {
            let mut damaged = scene.exe_bytes.clone();
            damaged[at] ^= 0x55;
            let answer = || Answer::ByRange(Box::new(Answer::Ranged(Body::Bytes(damaged.clone()), Fault::None)), Box::new(file_answer(&scene.exe_bytes)));
            let server = scene.server(answer(), Answer::Ranged(Body::Bytes(scene.parts_bytes.clone()), Fault::None));
            let setup = scene.setup(&server);
            assert_eq!(scene.assembled(&server, &setup), Err(ranges::Not::Because(ranges::DAMAGED)), "{what}");
            let _ = std::fs::remove_dir_all(&scene.folder);
            scene.assert_fetched_whole(scene.obtain(&server, &setup));
        }
    }

    #[test]
    fn a_missing_or_wrong_parts_file_has_the_setup_fetched_whole() {
        let scene = Scene::new(true);
        // not there
        let server = Server::start(|port| {
            vec![
                (SETUP_PATH.to_string(), redirect_to(&format!("http://127.0.0.1:{port}/storage/setup?sig=1"))),
                ("/storage/setup?sig=1".to_string(), Answer::Ranged(Body::Bytes(scene.exe_bytes.clone()), Fault::None)),
            ]
        });
        let setup = scene.setup(&server);
        assert_eq!(scene.assembled(&server, &setup), Err(ranges::Not::Because(ranges::PARTS_NOT_FETCHED)));
        scene.assert_fetched_whole(scene.obtain(&server, &setup));
        assert!(server.ranged_requests().is_empty(), "no range was asked for: {:?}", server.requests());
        // there, and not the size and the hash that GitHub published of it
        let mut other = scene.parts_bytes.clone();
        let middle = other.len() / 2;
        other[middle] ^= 1;
        let cases = [
            ("a byte other", Answer::Ranged(Body::Bytes(other), Fault::None), 0),
            ("a size other", Answer::Ranged(Body::Bytes(scene.parts_bytes.clone()), Fault::None), 1),
            ("a file that is no parts file", Answer::Ranged(Body::Bytes(b"{}".to_vec()), Fault::None), 2),
        ];
        for (what, parts, published) in cases {
            let server = scene.server(Answer::Ranged(Body::Bytes(scene.exe_bytes.clone()), Fault::None), parts);
            let mut setup = scene.setup(&server);
            match published {
                1 => setup.parts.as_mut().unwrap().size += 1,
                2 => {
                    let nothing = b"{}";
                    let parts = setup.parts.as_mut().unwrap();
                    (parts.size, parts.sha256) = (nothing.len() as u64, sha_of(nothing));
                }
                _ => {}
            }
            let reason = if published == 2 { ranges::PARTS_UNREADABLE } else { ranges::PARTS_DIFFER };
            assert_eq!(scene.assembled(&server, &setup), Err(ranges::Not::Because(reason)), "{what}");
            let _ = std::fs::remove_dir_all(&scene.folder);
            scene.assert_fetched_whole(scene.obtain(&server, &setup));
        }
        // a release that has none among its files has none to fetch
        let server = scene.good_server();
        let mut setup = scene.setup(&server);
        setup.parts = None;
        scene.assert_fetched_whole(scene.obtain(&server, &setup));
        assert!(server.requests().iter().all(|line| !line.contains("parts")), "{:?}", server.requests());
    }

    #[test]
    fn an_install_that_has_no_record_of_its_files_has_the_setup_fetched_whole() {
        // the install of 0.1.4: `file` lines, and no `sum` line of any
        let scene = Scene::new(false);
        let server = scene.good_server();
        let setup = scene.setup(&server);
        assert_eq!(scene.assembled(&server, &setup), Err(ranges::Not::Because(ranges::NO_RECORDS)));
        scene.assert_fetched_whole(scene.obtain(&server, &setup));
        assert!(server.requests().iter().all(|line| !line.contains("parts")) && server.ranged_requests().is_empty(), "nothing but the setup was asked for: {:?}", server.requests());
        // and a copy that is no install at all
        let _ = std::fs::remove_dir_all(&scene.folder);
        let ready = obtain_within("0.2.0", &setup, None, &scene.folder, Some(&server.as_url()), &mut |_, _| {}, &AtomicBool::new(false));
        scene.assert_fetched_whole(ready);
    }

    #[test]
    fn an_address_that_has_gone_stale_is_resolved_again_once() {
        let scene = Scene::new(true);
        let ranged = || Answer::Ranged(Body::Bytes(scene.exe_bytes.clone()), Fault::None);
        let parts = || Answer::Ranged(Body::Bytes(scene.parts_bytes.clone()), Fault::None);
        // the first storage address answers the first range and then goes stale; the setup's own address then sends the request on to another
        let server = Server::start(|port| {
            vec![
                (SETUP_PATH.to_string(), Answer::Turns(vec![redirect_to(&format!("http://127.0.0.1:{port}/storage/one")), redirect_to(&format!("http://127.0.0.1:{port}/storage/two"))])),
                ("/storage/one".to_string(), Answer::Turns(vec![ranged(), Answer::Forbidden])),
                ("/storage/two".to_string(), ranged()),
                (PARTS_PATH.to_string(), redirect_to(&format!("http://127.0.0.1:{port}/storage/parts"))),
                ("/storage/parts".to_string(), parts()),
            ]
        });
        let setup = scene.setup(&server);
        let ready = scene.obtain(&server, &setup).unwrap();
        let wanted: std::collections::BTreeMap<String, Vec<u8>> = new_files().into_iter().map(|(name, bytes)| (name.to_string(), bytes)).collect();
        assert_eq!(tree_of(&ready.path, &scene.install), wanted);
        let asked = server.requests();
        let at = |what: &str| asked.iter().position(|line| line.contains(what)).unwrap_or(usize::MAX);
        assert_eq!(asked.iter().filter(|line| line.contains(SETUP_PATH)).count(), 2, "{asked:?}");
        assert!(at("/storage/one") < at("/storage/two"), "{asked:?}");
        // a second address that goes stale as well is not resolved a third time: the range way is given up, and so is the whole fetch here, which meets the same address
        let server = Server::start(|port| {
            vec![
                (SETUP_PATH.to_string(), redirect_to(&format!("http://127.0.0.1:{port}/storage/one"))),
                ("/storage/one".to_string(), Answer::Turns(vec![ranged(), Answer::Forbidden])),
                (PARTS_PATH.to_string(), redirect_to(&format!("http://127.0.0.1:{port}/storage/parts"))),
                ("/storage/parts".to_string(), parts()),
            ]
        });
        let setup = scene.setup(&server);
        assert_eq!(scene.assembled(&server, &setup), Err(ranges::Not::Because(ranges::NOT_FETCHED)));
        assert_eq!(server.requests().iter().filter(|line| line.contains(SETUP_PATH)).count(), 2, "{:?}", server.requests());
    }

    #[test]
    fn an_answer_that_is_not_the_range_asked_for_has_the_setup_fetched_whole() {
        let scene = Scene::new(true);
        for (what, fault, reason) in [("other bytes than those asked for", Fault::WrongRange, ranges::NOT_A_RANGE), ("fewer bytes than the length said", Fault::Short, ranges::NOT_FETCHED)] {
            let answer = || Answer::ByRange(Box::new(Answer::Ranged(Body::Bytes(scene.exe_bytes.clone()), fault)), Box::new(file_answer(&scene.exe_bytes)));
            let server = scene.server(answer(), Answer::Ranged(Body::Bytes(scene.parts_bytes.clone()), Fault::None));
            let setup = scene.setup(&server);
            assert_eq!(scene.assembled(&server, &setup), Err(ranges::Not::Because(reason)), "{what}");
            let _ = std::fs::remove_dir_all(&scene.folder);
            scene.assert_fetched_whole(scene.obtain(&server, &setup));
        }
        // a server that has no such range to give
        let server = scene.server(Answer::Whole(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\n\r\n".to_vec()), Answer::Ranged(Body::Bytes(scene.parts_bytes.clone()), Fault::None));
        let setup = scene.setup(&server);
        assert_eq!(scene.assembled(&server, &setup), Err(ranges::Not::Because(ranges::NOT_FETCHED)));
    }

    #[test]
    fn a_range_update_that_the_person_stops_leaves_no_file_and_does_not_go_on_to_the_whole_setup() {
        let scene = Scene::new(true);
        let server = scene.good_server();
        let setup = scene.setup(&server);
        let stop = AtomicBool::new(false);
        let failed = obtain_within("0.2.0", &setup, Some(&scene.install), &scene.folder, Some(&server.as_url()), &mut |done, _| if done > 0 { stop.store(true, Ordering::Relaxed) }, &stop).unwrap_err();
        assert_eq!(failed, Failed::Cancelled);
        assert_eq!(names_in(&scene.folder), Vec::<String>::new(), "no setup, no `.partial`, no note");
        let big = scene.exe_bytes.len() as u64;
        assert!(server.sent() < big / 20, "the whole setup was not fetched after it: {} of {big}", server.sent());
    }

    #[test]
    fn a_release_lists_its_parts_file_beside_its_setup_and_only_with_a_size_a_digest_and_an_address_that_are_fit() {
        let digest = format!("\"sha256:{HASH}\"");
        let parts_url = "https://github.com/rewroy/dogmind-app/releases/download/v0.2.0/DogmindSetup-0.2.0.parts";
        let setup = github_asset();
        let parts = asset("DogmindSetup-0.2.0.parts", "150000", &digest, parts_url);
        let read = |assets: &[String]| setup_read_from(&listing_with("v0.2.0", assets));
        let found = read(&[setup.clone(), parts.clone()]).unwrap();
        assert_eq!(found.parts.as_deref(), Some(&Setup { url: parts_url.into(), size: 150_000, sha256: HASH.into(), parts: None }));
        assert_eq!(read(&[parts.clone(), setup.clone()]).unwrap(), found, "in whatever order");
        assert_eq!(read(&[setup.clone()]).unwrap().parts, None);
        for (what, bad) in [
            ("no digest", asset("DogmindSetup-0.2.0.parts", "150000", "null", parts_url)),
            ("a size of nothing", asset("DogmindSetup-0.2.0.parts", "0", &digest, parts_url)),
            ("a size no parts file has", asset("DogmindSetup-0.2.0.parts", "99999999", &digest, parts_url)),
            ("the parts of another version", asset("DogmindSetup-0.1.9.parts", "150000", &digest, parts_url)),
            ("an address on another host", asset("DogmindSetup-0.2.0.parts", "150000", &digest, "https://example.com/rewroy/dogmind-app/releases/download/v0.2.0/DogmindSetup-0.2.0.parts")),
            ("an address of another project", asset("DogmindSetup-0.2.0.parts", "150000", &digest, "https://github.com/someone/else/releases/download/v0.2.0/DogmindSetup-0.2.0.parts")),
        ] {
            let found = read(&[setup.clone(), bad]).unwrap();
            assert_eq!((found.parts, found.url.as_str()), (None, GITHUB_SETUP), "{what}: the setup is still there, and is fetched whole");
        }
        // a parts file of a release with no setup is no way to anything
        let Some(Outcome::Newer(release)) = read_releases(&listing_with("v0.2.0", &[parts]), "0.1.0") else { panic!("a newer release") };
        assert_eq!(release.setup, None);
    }

    /// A real setup, put together from a real install by the app's own code and then installed by the installer: driven by a script (`DOGMIND_RANGE_*` name the files), and not run otherwise.
    /// It serves the second setup from a server of this test's that answers ranges, puts the setup together for the install, and tells what it fetched; the script then runs the result.
    #[test]
    #[ignore = "driven by a script with real files"]
    fn a_real_setup_is_put_together_for_a_real_install_for_a_script() {
        let var = |name: &str| PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} names a file")));
        let (exe, install, out) = (var("DOGMIND_RANGE_SETUP"), var("DOGMIND_RANGE_INSTALL"), var("DOGMIND_RANGE_OUT"));
        let parts_bytes = std::fs::read(setup_pack::parts_path(&exe)).unwrap();
        let (size, sha256) = hash_of(&exe).unwrap();
        let server = Server::start(|port| {
            vec![
                (SETUP_PATH.to_string(), redirect_to(&format!("http://127.0.0.1:{port}/storage/setup?sig=1"))),
                ("/storage/setup?sig=1".to_string(), Answer::Ranged(Body::File(exe.clone()), Fault::None)),
                (PARTS_PATH.to_string(), redirect_to(&format!("http://127.0.0.1:{port}/storage/parts?sig=1"))),
                ("/storage/parts?sig=1".to_string(), Answer::Ranged(Body::Bytes(parts_bytes.clone()), Fault::None)),
            ]
        });
        let parts = Setup { url: server.url(PARTS_PATH), size: parts_bytes.len() as u64, sha256: sha_of(&parts_bytes), parts: None };
        let setup = Setup { url: server.url(SETUP_PATH), size, sha256, parts: Some(Box::new(parts)) };
        let began = std::time::Instant::now();
        let ready = obtain_within("0.2.0", &setup, Some(&install), &out, Some(&server.as_url()), &mut |_, _| {}, &AtomicBool::new(false)).unwrap();
        println!("range path: {} bytes of bodies served, {} of {} for the setup, {} requests, {:?}; made {}", server.sent(), server.sent() - parts_bytes.len() as u64, size, server.requests().len(), began.elapsed(), ready.path.display());
        println!("range path: made setup is {} bytes, sha256 {}", ready.size, ready.sha256);
    }
}
