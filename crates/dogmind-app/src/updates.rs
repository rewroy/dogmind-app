//! Asking GitHub whether a newer release of Dogmind is out, and nothing more: the app downloads nothing and installs nothing by itself.
//! One GET of the project's list of releases goes out (the twenty newest, as GitHub orders them), with the program's name and version and nothing that says who is asking.
//! The highest version in the list is the one that counts, by its tag and not by its place in the list or its date: a pre-release counts as well, a draft never does, and a tag
//! that is no version is passed over. What comes back is a version and a page, and the person decides whether to open it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use dogmind_agent::http::{self, Failure, Url};

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

/// How long a look stands for: the app asks on its own at most once in this many seconds.
pub const EVERY: u64 = 24 * 60 * 60;

/// The release that is newest, as the person is told of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Its tag without the `v` in front: `0.2.0`, or `0.2.0-beta.1` for a pre-release.
    pub version: String,
    /// Where to read of it, already one of the addresses that may be opened.
    pub page: String,
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

/// What a `releases` answer (a list of releases) comes to for a program of version `running`. `None`: it is not that kind of answer.
/// The list is not taken to be in any order: of the entries that are not drafts and whose tag is a version, pre-releases included, the one with the highest version is the
/// newest (the first of two that are the same). No entry at all, or drafts only, is `NoRelease`; entries but none with a version in its tag is `UpToDate`, as a name that is no
/// version is never newer.
pub fn read_releases(body: &str, running: &str) -> Option<Outcome> {
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
            let version: String = tag.strip_prefix(['v', 'V']).unwrap_or(tag).chars().take(MOST_OF_A_VERSION).collect();
            let named = entry.get("html_url").and_then(serde_json::Value::as_str).unwrap_or("");
            Outcome::Newer(Release { version, page: release_page(named) })
        }
        Some(_) => Outcome::UpToDate,
        None if published => Outcome::UpToDate,
        None => Outcome::NoRelease,
    })
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
    read_releases(&String::from_utf8_lossy(&body), running).unwrap_or(Outcome::Failed(Trouble::Unreadable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
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
        Release { version: tag.trim_start_matches('v').into(), page: format!("https://github.com/rewroy/dogmind-app/releases/tag/{tag}") }
    }

    /// What a list of these tags (none a draft; a tag with a hyphen a pre-release) comes to, in the order given.
    fn read_tags(tags: &[&str], running: &str) -> Option<Outcome> {
        let entries: Vec<String> = tags.iter().map(|tag| entry(tag, false, tag.contains('-'))).collect();
        read_releases(&list_of(&entries), running)
    }

    #[test]
    fn a_list_is_read_for_its_highest_version_and_that_ones_page() {
        let outcome = read_releases(&format!("[{V0_2_0}]"), "0.1.0");
        assert_eq!(outcome, Some(Outcome::Newer(Release { version: "0.2.0".into(), page: "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0".into() })));
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
        assert_eq!(read_releases(r#"[{"tag_name":"1.0.0"}]"#, "0.1.0"), Some(Outcome::Newer(Release { version: "1.0.0".into(), page: LATEST_PAGE.into() })));
        let evil = r#"[{"tag_name":"v1.0.0","html_url":"https://example.com/rewroy/dogmind-app/releases/tag/v1.0.0"}]"#;
        assert_eq!(read_releases(evil, "0.1.0"), Some(Outcome::Newer(Release { version: "1.0.0".into(), page: LATEST_PAGE.into() })));
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
        assert_eq!(read_tags(&["0.2.0", "v0.2.0"], "0.1.0"), Some(Outcome::Newer(Release { version: "0.2.0".into(), page: "https://github.com/rewroy/dogmind-app/releases/tag/0.2.0".into() })));
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
        assert_eq!(read_releases(odd, "0.1.0"), Some(Outcome::Newer(Release { version: "0.9.0".into(), page: LATEST_PAGE.into() })));
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
        let release = Release { version: "0.2.0".into(), page: LATEST_PAGE.into() };
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
        let release = Release { version: "0.2.0".into(), page: LATEST_PAGE.into() };
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
        assert_eq!(outcome, Outcome::Newer(Release { version: "0.2.0".into(), page: "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0".into() }));
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
}
