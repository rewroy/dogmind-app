//! Asking GitHub whether a newer release of Dogmind is out, and nothing more: the app downloads nothing and installs nothing by itself.
//! One GET of the latest release goes out, with the program's name and version and nothing that says who is asking; what comes back is
//! a version and a page, and the person decides whether to open it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use dogmind_agent::http::{self, Failure, Url};

/// The newest release that is neither a draft nor a pre-release. 404: there is none yet, or the repository is not public yet.
const LATEST_RELEASE: &str = "https://api.github.com/repos/rewroy/dogmind-app/releases/latest";
/// The only place a release's page may be: what the answer names is opened only when it begins like this.
const RELEASE_PAGES: &str = "https://github.com/rewroy/dogmind-app/releases/";
/// Where a person is taken when the answer's own address is not one of those.
const LATEST_PAGE: &str = "https://github.com/rewroy/dogmind-app/releases/latest";
/// How long the answer is waited for, from the first connection to the last byte.
const GIVE_UP_AFTER: Duration = Duration::from_secs(10);
/// An answer is a few kilobytes: more than this is no release, and memory is all it could cost.
const MOST_OF_AN_ANSWER: usize = 1 << 20;
/// A release's name is shown in a line of the window: past this it is cut.
const MOST_OF_A_VERSION: usize = 40;

/// How long a look stands for: the app asks on its own at most once in this many seconds.
pub const EVERY: u64 = 24 * 60 * 60;

/// The release that is newest, as the person is told of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Its tag without the `v` in front: `0.2.0`.
    pub version: String,
    /// Where to read of it, already one of the addresses that may be opened.
    pub page: String,
}

/// What a look came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// What is newest is not newer than this app (or its name is not a version, which does not count as newer).
    UpToDate,
    Newer(Release),
    /// GitHub has no release of it to give.
    NoRelease,
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
    /// Whether GitHub was heard from. Only then has a look been made that counts against the day: with no connection, the next try is cheap and may find one.
    pub fn was_answered(&self) -> bool {
        !matches!(self, Outcome::Failed(Trouble::NotReached | Trouble::TooSlow))
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

/// Whether `found` is a later version than `running`, by the numbers (1.10.0 is later than 1.9.0). A name that is no version is not later.
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

/// The newest release a `releases/latest` answer tells of. `None`: it is not that kind of answer.
pub fn read_release(body: &str) -> Option<Release> {
    let answer: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = answer.get("tag_name")?.as_str()?.trim();
    let version: String = tag.strip_prefix(['v', 'V']).unwrap_or(tag).chars().take(MOST_OF_A_VERSION).collect();
    if version.is_empty() {
        return None;
    }
    let named = answer.get("html_url").and_then(serde_json::Value::as_str).unwrap_or("");
    Some(Release { version, page: release_page(named) })
}

/// Where to ask. A debug build can be pointed at a server of its own with `DOGMIND_TEST_RELEASES_URL`; a build for release cannot be told anything.
/// `None`: a debug build running on a made-up machine (see `machine::kept_from_the_real_machine`) with no server to ask, which asks nobody.
fn address() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        address_for_testing(std::env::var("DOGMIND_TEST_RELEASES_URL").ok(), crate::machine::kept_from_the_real_machine())
    }
    #[cfg(not(debug_assertions))]
    {
        Some(LATEST_RELEASE.to_string())
    }
}

#[cfg(debug_assertions)]
fn address_for_testing(named: Option<String>, on_a_made_up_machine: bool) -> Option<String> {
    match named.map(|named| named.trim().to_string()).filter(|named| !named.is_empty()) {
        Some(named) => Some(named),
        None if on_a_made_up_machine => None,
        None => Some(LATEST_RELEASE.to_string()),
    }
}

/// Whether there is anywhere to ask: the app does not look on its own where it cannot.
pub fn can_ask() -> bool {
    address().is_some()
}

/// Asks, and waits for the answer: off the window's thread. Never longer than ten seconds.
pub fn check() -> Outcome {
    match address() {
        Some(address) => check_at(&address, env!("CARGO_PKG_VERSION"), GIVE_UP_AFTER),
        None => Outcome::Failed(Trouble::NotReached),
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
    match read_release(&String::from_utf8_lossy(&body)) {
        Some(release) if is_newer(&release.version, running) => Outcome::Newer(release),
        Some(_) => Outcome::UpToDate,
        None => Outcome::Failed(Trouble::Unreadable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// What `releases/latest` answers, as GitHub sends it (shortened: the assets and the author's other fields are not read).
    const LATEST: &str = r#"{
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

    #[test]
    fn a_latest_release_is_read_for_its_version_and_page() {
        let release = read_release(LATEST).unwrap();
        assert_eq!(release, Release { version: "0.2.0".into(), page: "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0".into() });
    }

    #[test]
    fn what_is_no_release_is_not_read_as_one() {
        for body in ["", "not json", "[]", "{}", r#"{"message":"Not Found","documentation_url":"https://docs.github.com"}"#, r#"{"tag_name":7}"#, r#"{"tag_name":"v"}"#, r#"{"tag_name":""}"#] {
            assert_eq!(read_release(body), None, "{body}");
        }
        // (no page named: the latest's is the one to open)
        assert_eq!(read_release(r#"{"tag_name":"1.0.0"}"#).unwrap().page, LATEST_PAGE);
        assert_eq!(read_release(&format!(r#"{{"tag_name":"v{}"}}"#, "9".repeat(500))).unwrap().version.len(), MOST_OF_A_VERSION);
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
        for outcome in [Outcome::UpToDate, Outcome::NoRelease, Outcome::Failed(Trouble::NotReached)] {
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
        assert_eq!(address_for_testing(None, false).as_deref(), Some(LATEST_RELEASE));
        assert_eq!(address_for_testing(Some("  ".into()), false).as_deref(), Some(LATEST_RELEASE));
        assert_eq!(address_for_testing(None, true), None);
        assert_eq!(address_for_testing(Some(String::new()), true), None);
    }

    /// A server on this machine for one request: what it was sent (the head), and an address to ask. It answers with a length and does not wait for the close,
    /// which on this machine now and then never reaches a listener. `answer`: the whole of what it says.
    fn answering(answer: String) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://127.0.0.1:{}/repos/rewroy/dogmind-app/releases/latest", listener.local_addr().unwrap().port());
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
        let (address, hearing) = answering(with_length("200 OK", LATEST));
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::Newer(Release { version: "0.2.0".into(), page: "https://github.com/rewroy/dogmind-app/releases/tag/v0.2.0".into() }));
        let head = hearing.recv_timeout(Duration::from_secs(5)).unwrap();
        let lines: Vec<&str> = head.split("\r\n").filter(|line| !line.is_empty()).collect();
        assert_eq!(lines[0], "GET /repos/rewroy/dogmind-app/releases/latest HTTP/1.1");
        let names: Vec<String> = lines[1..].iter().map(|line| line.split(':').next().unwrap().to_ascii_lowercase()).collect();
        assert!(lines.contains(&"Accept: application/vnd.github+json") && lines.contains(&"User-Agent: Dogmind/0.1.0"), "{head}");
        assert!(names.iter().all(|name| ["host", "accept", "user-agent", "connection"].contains(&name.as_str())), "{names:?}");
    }

    #[test]
    fn the_running_version_or_a_later_one_is_up_to_date() {
        for running in ["0.2.0", "0.3.1"] {
            let (address, _) = answering(with_length("200 OK", LATEST));
            assert_eq!(check_at(&address, running, Duration::from_secs(10)), Outcome::UpToDate, "{running}");
        }
        let (address, _) = answering(with_length("200 OK", r#"{"tag_name":"nightly","html_url":"https://github.com/rewroy/dogmind-app/releases/tag/nightly"}"#));
        assert_eq!(check_at(&address, "0.1.0", Duration::from_secs(10)), Outcome::UpToDate);
    }

    #[test]
    fn a_404_is_no_release_yet_and_not_an_error() {
        let (address, _) = answering(with_length("404 Not Found", r#"{"message":"Not Found","documentation_url":"https://docs.github.com/rest/releases/releases#get-the-latest-release","status":"404"}"#));
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::NoRelease);
        assert!(outcome.was_answered());
    }

    #[test]
    fn other_answers_and_chunked_bodies_come_out_right() {
        let (address, _) = answering(with_length("403 Forbidden", r#"{"message":"API rate limit exceeded"}"#));
        let outcome = check_at(&address, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::Failed(Trouble::Refused(403)));
        assert!(outcome.was_answered());
        let (address, _) = answering(with_length("200 OK", "<html>a login page</html>"));
        assert_eq!(check_at(&address, "0.1.0", Duration::from_secs(10)), Outcome::Failed(Trouble::Unreadable));
        let (half, rest) = LATEST.split_at(100);
        let chunked = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{half}\r\n{:x}\r\n{rest}\r\n0\r\n\r\n", half.len(), rest.len());
        let (address, _) = answering(chunked);
        assert!(matches!(check_at(&address, "0.1.0", Duration::from_secs(10)), Outcome::Newer(_)));
    }

    #[test]
    fn nobody_listening_and_nobody_answering_are_told_apart_and_neither_counts_as_a_look() {
        // a server that takes the connection and hangs up (not a port that was just let go of, which another test's listener may be given)
        let hangs_up = TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://127.0.0.1:{}/x", hangs_up.local_addr().unwrap().port());
        std::thread::spawn(move || drop(hangs_up.accept()));
        let outcome = check_at(&closed, "0.1.0", Duration::from_secs(10));
        assert_eq!(outcome, Outcome::Failed(Trouble::NotReached));
        assert!(!outcome.was_answered());
        assert_eq!(check_at("not an address", "0.1.0", Duration::from_secs(10)), Outcome::Failed(Trouble::NotReached));
        // a server that takes the request and says nothing
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://127.0.0.1:{}/x", silent.local_addr().unwrap().port());
        let began = std::time::Instant::now();
        let outcome = check_at(&address, "0.1.0", Duration::from_millis(400));
        assert_eq!(outcome, Outcome::Failed(Trouble::TooSlow));
        assert!(!outcome.was_answered());
        assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
        drop(silent);
    }
}
