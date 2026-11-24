//! What a checked page loads from well-known CDNs. The check's browser runs in the sandbox with no network, and from there it cannot reach a server of the
//! app's on localhost either (an AppContainer's loopback is closed), so the app fetches the files itself, keeps them in the sandbox's own folder, and the
//! copy of the page that is opened, and the files fetched, point at the kept ones. GET only, to the hosts below; a query string only where the host needs
//! one, so that nothing of the workspace rides out on an address; each file kept by its address, so a page checked again fetches nothing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use regex::Regex;
use sha2::{Digest, Sha256};

/// The CDNs a page's libraries, styles and fonts load from.
pub(super) const HOSTS: [&str; 11] = [
    "unpkg.com",
    "cdn.jsdelivr.net",
    "cdnjs.cloudflare.com",
    "esm.sh",
    "esm.run",
    "ga.jspm.io",
    "cdn.skypack.dev",
    "code.jquery.com",
    "cdn.tailwindcss.com",
    "fonts.googleapis.com",
    "fonts.gstatic.com",
];
/// Those whose addresses carry a query of their own (a font's family, a build's target, Tailwind's plugins).
const QUERY_HOSTS: [&str; 4] = ["fonts.googleapis.com", "esm.sh", "cdn.tailwindcss.com", "cdn.skypack.dev"];
const MOST_FILES: usize = 300;
const MOST_FILE: usize = 16 << 20;
const MOST_BYTES: usize = 96 << 20;
const MOST_REDIRECTS: usize = 5;
/// The folder in the sandbox's own one where the files are kept.
pub(super) const FOLDER: &str = "dogmind-cdn";
const CONNECT: Duration = Duration::from_secs(10);
const EACH: Duration = Duration::from_secs(30);

/// What a server gave for an address: its status, where it sends to instead, its content type, and its bytes.
pub(super) struct Got {
    pub(super) status: u16,
    pub(super) location: Option<String>,
    pub(super) content_type: Option<String>,
    pub(super) body: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Script,
    Style,
    Other,
}

/// What came of a page's addresses: the page with them pointing at kept files, which of the page's own were kept, how many files in all with what those
/// import, and what was not fetched and why.
#[derive(Debug, Default)]
pub(super) struct Fetched {
    pub(super) html: String,
    pub(super) loaded: Vec<String>,
    pub(super) files: usize,
    pub(super) refused: Vec<(String, String)>,
}

/// An address as it is fetched, if it may be: https, one of `HOSTS`, no sign-in, a query only where its host needs one. A protocol-relative one and one of
/// http are the same file over https.
pub(super) fn allowed(address: &str) -> Result<String, String> {
    let rest = address.strip_prefix("https://").or_else(|| address.strip_prefix("http://")).or_else(|| address.strip_prefix("//")).ok_or("not a web address")?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority.to_ascii_lowercase();
    let host = host.strip_suffix(":443").unwrap_or(&host);
    if host.contains(['@', ':']) || !HOSTS.contains(&host) {
        return Err("not one of the CDNs a check fetches from".into());
    }
    if tail.contains('?') && !QUERY_HOSTS.contains(&host) {
        return Err("a query, which this CDN's files do not need".into());
    }
    if tail.split(['/', '?']).any(dots_only) {
        return Err("a `..` in its path".into());
    }
    Ok(format!("https://{host}{}", if tail.is_empty() { "/" } else { tail }))
}

/// A part of a path that Windows opens as `.` or `..`: it drops a name's trailing dots and spaces, so `.. ` and `... ` are `..` to it, and a file kept
/// along such a path would be written above the folder it is kept in.
fn dots_only(part: &str) -> bool {
    !part.is_empty() && part.trim_end_matches(['.', ' ']).is_empty()
}

/// Whether `path` is under `root`, as it is written: no part of it climbs (the parts are cleaned before, so none is a `..` that Windows would make one).
fn under(root: &Path, path: &Path) -> bool {
    path.starts_with(root) && !path.components().any(|part| part == std::path::Component::ParentDir)
}

/// A kept file as the index names it, where it is a file under the folder: relative, no part climbing or one Windows would open as `..`. The index is in
/// the sandbox's temp, and a program there can write it.
fn kept_file(root: &Path, file: &str) -> Option<PathBuf> {
    let path = Path::new(file);
    let plain = path.components().all(|part| matches!(part, std::path::Component::Normal(name) if !dots_only(&name.to_string_lossy())));
    let file = root.join(path);
    (plain && under(root, &file)).then_some(file)
}

/// `https://host/path?query` as its host, its path (with `/` first) and its query.
fn parts(address: &str) -> (&str, &str, Option<&str>) {
    let rest = address.strip_prefix("https://").unwrap_or(address);
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(end);
    let (path, query) = tail.split_once('?').map_or((tail, None), |(path, query)| (path, Some(query)));
    (host, if path.is_empty() { "/" } else { path }, query)
}

/// Where a file is kept: under its host, along its path, so that the files of one package keep their folders. A query is a part of the name, a script or a
/// style sheet whose name says neither is given the extension the browser goes by (a module from `file://` loads by it), and what Windows does not take
/// in a name is `_`.
fn place(root: &Path, address: &str, kind: Kind) -> PathBuf {
    let (host, path, query) = parts(address);
    let mut place = root.join(host);
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
    // (a part Windows would open as `.` or `..` is `_`: the file stays where its host's folder is)
    let clean = |part: &str| if dots_only(part) { "_".to_string() } else { part.chars().map(|c| if "<>:\"|?*\\".contains(c) || c.is_control() { '_' } else { c }).collect::<String>() };
    for part in segments.iter().take(segments.len().saturating_sub(1)) {
        place.push(clean(part));
    }
    let mut name = if path.ends_with('/') || segments.is_empty() {
        if let Some(last) = segments.last().filter(|_| path.ends_with('/')) {
            place.push(clean(last));
        }
        "index".to_string()
    } else {
        clean(segments.last().unwrap_or(&"index"))
    };
    if let Some(query) = query {
        let digest: String = Sha256::digest(query.as_bytes()).iter().take(6).map(|byte| format!("{byte:02x}")).collect();
        name.push_str(&format!("@q{digest}"));
    }
    let lower = name.to_ascii_lowercase();
    match kind {
        Kind::Script if !(lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs")) => name.push_str(".js"),
        Kind::Style if !lower.ends_with(".css") => name.push_str(".css"),
        _ => {}
    }
    place.join(name)
}

/// Where the files under a folder an import map names are kept, as the browser is to be given it.
fn folder_place(root: &Path, address: &str) -> PathBuf {
    let (host, path, _) = parts(address);
    let mut place = root.join(host);
    for part in path.split('/').filter(|part| !part.is_empty() && *part != ".") {
        place.push(if dots_only(part) { "_".to_string() } else { part.chars().map(|c| if "<>:\"|?*\\".contains(c) || c.is_control() { '_' } else { c }).collect::<String>() });
    }
    place
}

/// `spec` as an address, from the file at `base`: a whole address, one from the host's root, or one beside it (`./`, `../`). A bare name is the page's import
/// map's to say, and stays as it is.
fn resolve(base: &str, spec: &str) -> Option<String> {
    if spec.starts_with("https://") || spec.starts_with("http://") {
        return Some(spec.to_string());
    }
    if let Some(rest) = spec.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    let (host, path, _) = parts(base);
    if spec.starts_with('/') {
        return Some(format!("https://{host}{spec}"));
    }
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let mut segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if !path.ends_with('/') {
        segments.pop();
    }
    for part in spec.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                segments.pop();
            }
            part => segments.push(part),
        }
    }
    Some(format!("https://{host}/{}", segments.join("/")))
}

fn kind_of(address: &str, content_type: Option<&str>) -> Kind {
    let content_type = content_type.unwrap_or("").to_ascii_lowercase();
    let (_, path, _) = parts(address);
    let path = path.to_ascii_lowercase();
    if content_type.contains("javascript") || content_type.contains("ecmascript") || path.ends_with(".js") || path.ends_with(".mjs") {
        Kind::Script
    } else if content_type.contains("text/css") || path.ends_with(".css") {
        Kind::Style
    } else {
        Kind::Other
    }
}

fn script_patterns() -> &'static [Regex; 3] {
    static PATTERNS: std::sync::OnceLock<[Regex; 3]> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            Regex::new(r#"(\b(?:import|export)\b[^'"\n;]*?\bfrom\s*)(['"])([^'"\n]+)(['"])"#).expect("the pattern is valid"),
            Regex::new(r#"(\bimport\s*)(['"])([^'"\n]+)(['"])"#).expect("the pattern is valid"),
            Regex::new(r#"(\bimport\(\s*)(['"])([^'"\n]+)(['"])"#).expect("the pattern is valid"),
        ]
    })
}

fn style_patterns() -> &'static [Regex; 2] {
    static PATTERNS: std::sync::OnceLock<[Regex; 2]> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            Regex::new(r#"(url\(\s*)(['"]?)([^'")\s]+)(['"]?\s*\))"#).expect("the pattern is valid"),
            Regex::new(r#"(@import\s+)(['"])([^'"]+)(['"])"#).expect("the pattern is valid"),
        ]
    })
}

/// The addresses a script or a style sheet names, made whole from where it was fetched.
fn named_in(text: &str, base: &str, kind: Kind) -> Vec<String> {
    let mut found = Vec::new();
    let mut take = |pattern: &Regex| {
        for captures in pattern.captures_iter(text) {
            if let Some(address) = resolve(base, &captures[3]) {
                found.push(address);
            }
        }
    };
    match kind {
        Kind::Script => script_patterns().iter().for_each(&mut take),
        Kind::Style => style_patterns().iter().for_each(&mut take),
        Kind::Other => {}
    }
    found
}

/// The same, with each address that was kept as the `file://` address of its file.
fn pointed(text: &str, base: &str, kind: Kind, kept: &HashMap<String, String>) -> String {
    let patterns: Vec<&Regex> = match kind {
        Kind::Script => script_patterns().iter().collect(),
        Kind::Style => style_patterns().iter().collect(),
        Kind::Other => return text.to_string(),
    };
    let mut out = text.to_string();
    for pattern in patterns {
        out = pattern
            .replace_all(&out, |captures: &regex::Captures| {
                let spec = &captures[3];
                match resolve(base, spec).and_then(|address| allowed(&address).ok()).and_then(|address| kept.get(&address)) {
                    Some(file) => format!("{}{}{file}{}", &captures[1], &captures[2], &captures[4]),
                    None => captures[0].to_string(),
                }
            })
            .into_owned();
    }
    out
}

/// The addresses of the CDNs a page names, as written: in its attributes, its scripts' strings and its styles.
fn named_in_page(html: &str) -> Vec<String> {
    let hosts = HOSTS.iter().map(|host| regex::escape(host)).collect::<Vec<_>>().join("|");
    let pattern = Regex::new(&format!(r#"(?i)(?:https?:)?//(?:{hosts})(?::443)?(?:/[^'"`()\s<>]*)?"#)).expect("the pattern is valid");
    let mut found: Vec<String> = Vec::new();
    for address in pattern.find_iter(html) {
        let address = address.as_str().trim_end_matches([',', ';']).to_string();
        if !found.contains(&address) {
            found.push(address);
        }
    }
    found
}

/// What was kept before, by address: the file and the addresses it names.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Index {
    files: HashMap<String, (String, Vec<String>)>,
}

/// A page's CDN files fetched (or found kept) into `temp`, and the page with its addresses pointing at them. `also`: addresses the page needs that are not
/// written in it as they are (a bare name its import map turns into one). `get` fetches one address.
pub(super) fn for_page(html: &str, also: &[String], temp: &Path, get: &mut dyn FnMut(&str) -> Result<Got, String>) -> Fetched {
    let root = temp.join(FOLDER);
    let index_file = root.join("index.json");
    let mut index: Index = std::fs::read(&index_file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
    let written = named_in_page(html);
    let mut fetched = Fetched::default();
    // (the addresses as fetched, and as the page wrote them)
    let mut page_own: Vec<(String, String)> = Vec::new();
    let mut folders: Vec<(String, String)> = Vec::new();
    for address in written.iter().chain(also) {
        match allowed(address) {
            Ok(whole) if whole.ends_with('/') && address.ends_with('/') => folders.push((address.clone(), whole)),
            Ok(whole) => page_own.push((address.clone(), whole)),
            Err(why) => {
                if !fetched.refused.iter().any(|(known, _)| known == address) {
                    fetched.refused.push((address.clone(), why));
                }
            }
        }
    }
    let mut queue: VecDeque<String> = page_own.iter().map(|(_, whole)| whole.clone()).collect();
    let mut seen: HashSet<String> = HashSet::new();
    // address -> its file's `file://` address; and the new files to write once all are known
    let mut kept: HashMap<String, String> = HashMap::new();
    let mut fresh: Vec<(String, String, Kind, PathBuf, Vec<u8>)> = Vec::new();
    let mut bytes = 0usize;
    while let Some(address) = queue.pop_front() {
        if !seen.insert(address.clone()) || seen.len() > MOST_FILES {
            continue;
        }
        // (an address from the index, which a program in the sandbox can write, is one to fetch only as any other is)
        let address = match allowed(&address) {
            Ok(address) => address,
            Err(why) => {
                fetched.refused.push((address, why));
                continue;
            }
        };
        if let Some((file, names)) = index.files.get(&address).filter(|(file, _)| kept_file(&root, file).is_some_and(|file| file.is_file())) {
            kept.insert(address.clone(), super::file_url(&root.join(file)));
            queue.extend(names.iter().cloned());
            continue;
        }
        // (each hop of a redirect to one of the same CDNs: a relative one from where it is)
        let (mut at, mut got) = (address.clone(), None);
        for _ in 0..=MOST_REDIRECTS {
            match get(&at) {
                Ok(answer) if (300..400).contains(&answer.status) => {
                    let Some(next) = answer.location.as_deref().and_then(|to| resolve(&at, to).or_else(|| resolve(&at, &format!("./{to}")))) else { break };
                    match allowed(&next) {
                        Ok(next) => at = next,
                        Err(why) => {
                            got = Some(Err(format!("sent on to {next}: {why}")));
                            break;
                        }
                    }
                }
                Ok(answer) if answer.status == 200 => {
                    got = Some(Ok(answer));
                    break;
                }
                Ok(answer) => {
                    got = Some(Err(format!("HTTP {}", answer.status)));
                    break;
                }
                Err(why) => {
                    got = Some(Err(why));
                    break;
                }
            }
        }
        let answer = match got.unwrap_or_else(|| Err("too many redirects".into())) {
            Ok(answer) if bytes + answer.body.len() <= MOST_BYTES => answer,
            Ok(_) => {
                fetched.refused.push((address, "more than a check fetches".into()));
                continue;
            }
            Err(why) => {
                fetched.refused.push((address, why));
                continue;
            }
        };
        bytes += answer.body.len();
        let kind = kind_of(&at, answer.content_type.as_deref());
        let file = place(&root, &address, kind);
        let text = String::from_utf8_lossy(&answer.body);
        let names: Vec<String> = named_in(&text, &at, kind).into_iter().filter_map(|named| allowed(&named).ok()).collect();
        queue.extend(names.iter().cloned());
        kept.insert(address.clone(), super::file_url(&file));
        let relative = file.strip_prefix(&root).unwrap_or(&file).display().to_string();
        index.files.insert(address.clone(), (relative, names));
        fresh.push((address, at, kind, file, answer.body));
    }
    // (written once every file's place is known, each pointing at the others)
    for (_, at, kind, file, body) in &fresh {
        // (nothing is written but under the folder the files are kept in, whatever an address came to, and nothing through a junction or a link a
        // program in the sandbox put there)
        if !under(&root, file) {
            continue;
        }
        let content = if *kind == Kind::Other { body.clone() } else { pointed(&String::from_utf8_lossy(body), at, *kind, &kept).into_bytes() };
        let _ = crate::temp_writes::write(temp, file, content);
    }
    if !fresh.is_empty() {
        let _ = crate::temp_writes::write(temp, &index_file, serde_json::to_vec(&index).unwrap_or_default());
    }
    let mut html = html.to_string();
    for (written, whole) in &page_own {
        if let Some(file) = kept.get(whole) {
            html = html.replace(&format!("\"{written}\""), &format!("\"{file}\"")).replace(&format!("'{written}'"), &format!("'{file}'")).replace(&format!("({written})"), &format!("({file})"));
            if !fetched.loaded.contains(whole) {
                fetched.loaded.push(whole.clone());
            }
        }
    }
    for (written, whole) in &folders {
        let folder = format!("{}/", super::file_url(&folder_place(&root, whole)));
        html = html.replace(&format!("\"{written}\""), &format!("\"{folder}\""));
    }
    fetched.files = kept.len();
    fetched.html = html;
    fetched
}

/// One address, over the network: GET, no sign-in, its bytes up to `MOST_FILE`.
pub(super) fn get(address: &str, cancel: &AtomicBool) -> Result<Got, String> {
    // (the CDNs alone, and only where their names lead off this machine and its network, as the web tools are)
    let address = allowed(address)?;
    let url = crate::http::Url::parse(&address)?;
    let addresses: Vec<std::net::SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(&(url.host.as_str(), url.port)).map_err(|error| format!("{}: {error}", url.host))?.collect();
    if let Some(bad) = addresses.iter().find(|address| crate::web::blocked_address(address.ip())) {
        return Err(format!("{} is at {}: this machine and its network are not fetched from", url.host, bad.ip()));
    }
    let headers = vec![("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0 Safari/537.36".to_string()), ("Accept".to_string(), "*/*".to_string())];
    let mut response = crate::http::request_at_within(&addresses, "GET", &url, &headers, CONNECT, cancel, EACH).map_err(|error| error.to_string())?;
    if response.header("content-encoding").is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity")) {
        return Err("a compressed answer".into());
    }
    let (status, location, content_type) = (response.status, response.header("location").map(str::to_string), response.header("content-type").map(str::to_string));
    let mut body = Vec::new();
    while let Some(chunk) = response.next().map_err(|error| error.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > MOST_FILE {
            return Err(format!("more than {} MB", MOST_FILE >> 20));
        }
    }
    Ok(Got { status, location, content_type, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A place is under its root whatever an address came to: a part Windows would open as `..` is `_`, and `..%20` is a name.
    #[test]
    fn a_place_stays_under_its_root() {
        let root = Path::new("C:\\temp\\dogmind-cdn");
        for address in ["https://unpkg.com/.. /x.js", "https://unpkg.com/... /... /x.js", "https://unpkg.com/..%20/x.js", "https://unpkg.com/a/. /b.js"] {
            let file = place(root, address, Kind::Script);
            assert!(under(root, &file) && file.starts_with(root.join("unpkg.com")), "{address}: {}", file.display());
            assert!(!file.components().any(|part| part.as_os_str().to_string_lossy().trim_end_matches(['.', ' ']).is_empty()), "{address}: {}", file.display());
            let folder = folder_place(root, address);
            assert!(under(root, &folder), "{address}: {}", folder.display());
        }
        assert!(!under(root, Path::new("C:\\temp\\dogmind-cdn\\..\\x.js")));
        assert!(!under(root, Path::new("C:\\temp\\x.js")));
    }

    #[test]
    fn only_the_cdns_over_https_with_a_query_only_where_one_is_needed() {
        assert_eq!(allowed("https://unpkg.com/three@0.160.0/build/three.module.js").unwrap(), "https://unpkg.com/three@0.160.0/build/three.module.js");
        assert_eq!(allowed("//cdn.jsdelivr.net/npm/x@1/+esm").unwrap(), "https://cdn.jsdelivr.net/npm/x@1/+esm");
        assert_eq!(allowed("http://unpkg.com/a.js").unwrap(), "https://unpkg.com/a.js");
        assert!(allowed("https://fonts.googleapis.com/css2?family=Noto+Sans+JP").is_ok());
        assert!(allowed("https://unpkg.com/a.js?secret=1").is_err(), "a query where none is needed");
        assert!(allowed("https://example.com/a.js").is_err());
        assert!(allowed("https://user@unpkg.com/a.js").is_err());
        assert!(allowed("https://unpkg.com.evil.example/a.js").is_err());
        assert!(allowed("https://unpkg.com/../x").is_err());
        // (Windows opens a part with trailing dots and spaces without them: `.. ` is `..`)
        for climbing in ["https://unpkg.com/.. /x.js", "https://unpkg.com/... /x.js", "https://unpkg.com/. /x.js", "https://unpkg.com/a/.. /.. /x.js"] {
            assert!(allowed(climbing).is_err(), "{climbing}");
        }
        assert!(allowed("https://unpkg.com/..%20/x.js").is_ok(), "a name with %20 in it, as written");
        assert!(allowed("https://unpkg.com/a.b./x.js").is_ok(), "a name that ends in a dot is a name");
        assert!(allowed("file:///C:/x.js").is_err());
    }

    #[test]
    fn an_address_is_made_whole_from_where_its_file_came() {
        let base = "https://unpkg.com/three@0.160.0/examples/jsm/controls/OrbitControls.js";
        assert_eq!(resolve(base, "../../../build/three.module.js").unwrap(), "https://unpkg.com/three@0.160.0/build/three.module.js");
        assert_eq!(resolve(base, "./x.js").unwrap(), "https://unpkg.com/three@0.160.0/examples/jsm/controls/x.js");
        assert_eq!(resolve("https://esm.sh/three@0.160.0", "/v135/three@0.160.0/es2022/three.mjs").unwrap(), "https://esm.sh/v135/three@0.160.0/es2022/three.mjs");
        assert_eq!(resolve(base, "three"), None, "a bare name is the import map's");
    }

    #[test]
    fn a_file_is_kept_along_its_path_with_the_extension_the_browser_goes_by() {
        let root = Path::new(r"C:\t");
        assert_eq!(place(root, "https://unpkg.com/three@0.160.0/build/three.module.js", Kind::Script), root.join("unpkg.com").join("three@0.160.0").join("build").join("three.module.js"));
        assert_eq!(place(root, "https://esm.sh/three@0.160.0", Kind::Script), root.join("esm.sh").join("three@0.160.0.js"));
        let font = place(root, "https://fonts.googleapis.com/css2?family=Noto+Sans", Kind::Style);
        assert!(font.file_name().unwrap().to_string_lossy().starts_with("css2@q") && font.extension().unwrap() == "css", "{}", font.display());
        assert_eq!(place(root, "https://cdn.jsdelivr.net/npm/x@1/+esm", Kind::Script), root.join("cdn.jsdelivr.net").join("npm").join("x@1").join("+esm.js"));
    }

    /// A page with an import map, a module that imports from the folder it maps, a style sheet whose fonts come from another CDN, and an address that is no
    /// CDN's: each CDN file fetched once into its place, each pointing at the others, the page at them, and the other address not asked for (the report says
    /// it did not load); checked again, nothing is fetched.
    #[test]
    fn a_pages_cdn_files_are_kept_and_pointed_at_and_kept_files_are_not_fetched_again() {
        let temp = crate::in_container::tempdir().unwrap();
        let html = r#"<html><head><link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Noto+Sans"><script type="importmap">{"imports":{"three":"https://unpkg.com/three@0.160.0/build/three.module.js","three/addons/":"https://unpkg.com/three@0.160.0/examples/jsm/"}}</script></head><body><script src="https://example.com/track.js"></script><script type="module">import * as THREE from 'three'; import { OrbitControls } from 'three/addons/controls/OrbitControls.js';</script></body></html>"#;
        let also = ["https://unpkg.com/three@0.160.0/examples/jsm/controls/OrbitControls.js".to_string()];
        let mut asked: Vec<String> = Vec::new();
        let mut get = |address: &str| -> Result<Got, String> {
            asked.push(address.to_string());
            let (content_type, body) = match address {
                "https://unpkg.com/three@0.160.0/build/three.module.js" => ("application/javascript", "export const REVISION = '160';"),
                "https://unpkg.com/three@0.160.0/examples/jsm/controls/OrbitControls.js" => ("application/javascript", "import { REVISION } from '../../../build/three.module.js'; export class OrbitControls {}"),
                "https://fonts.googleapis.com/css2?family=Noto+Sans" => ("text/css", "@font-face { src: url(https://fonts.gstatic.com/s/noto/v1/a.woff2) format('woff2'); }"),
                "https://fonts.gstatic.com/s/noto/v1/a.woff2" => ("font/woff2", "wOF2"),
                _ => return Ok(Got { status: 404, location: None, content_type: None, body: Vec::new() }),
            };
            Ok(Got { status: 200, location: None, content_type: Some(content_type.into()), body: body.as_bytes().to_vec() })
        };
        let fetched = for_page(html, &also, temp.path(), &mut get);
        let root = temp.path().join(FOLDER);
        let three = root.join("unpkg.com").join("three@0.160.0").join("build").join("three.module.js");
        assert!(three.is_file());
        assert!(fetched.html.contains(&format!("\"{}\"", crate::page_check::file_url(&three))), "{}", fetched.html);
        assert!(fetched.html.contains(&format!("\"{}/\"", crate::page_check::file_url(&root.join("unpkg.com").join("three@0.160.0").join("examples").join("jsm")))), "the folder an import map names");
        let orbit = std::fs::read_to_string(root.join("unpkg.com").join("three@0.160.0").join("examples").join("jsm").join("controls").join("OrbitControls.js")).unwrap();
        assert!(orbit.contains(&crate::page_check::file_url(&three)), "{orbit}");
        let css = std::fs::read_dir(root.join("fonts.googleapis.com")).unwrap().flatten().next().unwrap().path();
        assert!(std::fs::read_to_string(&css).unwrap().contains("file:///"), "the font's address in the style sheet");
        assert_eq!(fetched.files, 4);
        assert_eq!(asked.len(), 4, "and nothing of another host: {asked:?}");
        assert!(!asked.iter().any(|address| address.contains("example.com")));
        // (checked again: all kept)
        let mut none = |address: &str| -> Result<Got, String> { panic!("fetched again: {address}") };
        let again = for_page(html, &also, temp.path(), &mut none);
        assert_eq!(again.files, 4);
        assert_eq!(again.html, fetched.html);
    }

    /// The index is in the sandbox's temp, and a program there can write it: an entry naming an address off the CDNs is not fetched, and one whose file
    /// climbs out of the folder is not used.
    #[test]
    fn what_the_index_says_is_fetched_and_used_only_as_any_address_and_file_are() {
        let temp = crate::in_container::tempdir().unwrap();
        let root = temp.path().join(FOLDER);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(temp.path().join("x.js"), "outside").unwrap();
        let index = r#"{"files": {"https://unpkg.com/a.js": ["..\\x.js", ["http://127.0.0.1:9/x", "https://unpkg.com/b.js"]]}}"#;
        std::fs::write(root.join("index.json"), index).unwrap();
        let html = r#"<script src="https://unpkg.com/a.js"></script>"#;
        let mut asked: Vec<String> = Vec::new();
        let mut get = |address: &str| -> Result<Got, String> {
            asked.push(address.to_string());
            Ok(Got { status: 200, location: None, content_type: Some("text/javascript".into()), body: b"window.a = 1;".to_vec() })
        };
        let fetched = for_page(html, &[], temp.path(), &mut get);
        assert!(!asked.iter().any(|address| address.contains("127.0.0.1")), "{asked:?}");
        assert_eq!(asked, ["https://unpkg.com/a.js"], "the entry's file was not used, so it was fetched again");
        assert!(!fetched.html.contains(&crate::page_check::file_url(&temp.path().join("x.js"))), "{}", fetched.html);
        assert!(get_refuses_this_machine());
    }

    /// The fetch itself refuses what is not a CDN, and a name that leads to this machine or its network.
    fn get_refuses_this_machine() -> bool {
        let cancel = AtomicBool::new(false);
        get("http://127.0.0.1:9/x.js", &cancel).is_err_and(|why| why.contains("not one of the CDNs"))
    }

    /// A folder under the CDN folder made a junction to a place outside (as a program in the sandbox could make one): nothing is written there.
    #[cfg(windows)]
    #[test]
    fn a_junction_in_the_cdn_folder_is_not_written_through() {
        let temp = crate::in_container::tempdir().unwrap();
        let outside = crate::in_container::tempdir().unwrap();
        let root = temp.path().join(FOLDER);
        std::fs::create_dir_all(&root).unwrap();
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(root.join("unpkg.com")).arg(outside.path()).output().unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stdout));
        let html = r#"<script src="https://unpkg.com/a.js"></script>"#;
        let mut get = |_: &str| -> Result<Got, String> { Ok(Got { status: 200, location: None, content_type: Some("text/javascript".into()), body: b"window.a = 1;".to_vec() }) };
        for_page(html, &[], temp.path(), &mut get);
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0, "nothing written outside");
    }

    #[test]
    fn a_redirect_is_followed_on_the_cdns_and_not_off_them() {
        let temp = crate::in_container::tempdir().unwrap();
        let html = r#"<script src="https://unpkg.com/lib"></script><script src="https://unpkg.com/away"></script>"#;
        let mut get = |address: &str| -> Result<Got, String> {
            Ok(match address {
                "https://unpkg.com/lib" => Got { status: 302, location: Some("/lib@1.2.3/dist/lib.js".into()), content_type: None, body: Vec::new() },
                "https://unpkg.com/lib@1.2.3/dist/lib.js" => Got { status: 200, location: None, content_type: Some("text/javascript".into()), body: b"window.lib = 1;".to_vec() },
                _ => Got { status: 302, location: Some("https://example.com/x.js".into()), content_type: None, body: Vec::new() },
            })
        };
        let fetched = for_page(html, &[], temp.path(), &mut get);
        assert_eq!(fetched.loaded, ["https://unpkg.com/lib"]);
        assert!(fetched.refused.iter().any(|(address, why)| address == "https://unpkg.com/away" && why.contains("example.com")), "{:?}", fetched.refused);
        assert!(temp.path().join(FOLDER).join("unpkg.com").join("lib.js").is_file(), "kept by the address the page asked for");
    }
}
