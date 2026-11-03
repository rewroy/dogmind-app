//! Common Controls 6 for the themed scrollbars, as a manifest in the executable, and its icon as a resource. The linker takes
//! a compiled resource file directly, so nothing beyond it is needed, and that file's format is short enough to write here.

/// One resource as a `.res` file holds it, numbered both in type and in name.
fn resource(kind: u16, id: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 36);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    for number in [0xFFFF, kind, 0xFFFF, id] {
        out.extend_from_slice(&u16::to_le_bytes(number));
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // data version
    out.extend_from_slice(&0x1030u16.to_le_bytes()); // moveable, pure, discardable
    out.extend_from_slice(&0x0409u16.to_le_bytes()); // language
    out.extend_from_slice(&[0u8; 8]); // version, characteristics
    out.extend_from_slice(data);
    out.resize(out.len().next_multiple_of(4), 0);
    out
}

/// An `.ico`, as the resources that `LoadImage` and Explorer read: each image, and a directory that numbers them.
fn icon_resources(ico: &[u8]) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    let word = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]);
    let long = |at: usize| u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]) as usize;
    let count = word(4);
    // The empty entry every .res starts with.
    let mut out = vec![0, 0, 0, 0, 32, 0, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0];
    out.resize(32, 0);
    let mut group = vec![0, 0, 1, 0];
    group.extend_from_slice(&count.to_le_bytes());
    for image in 0..count {
        let entry = 6 + 16 * image as usize;
        out.extend(resource(RT_ICON, image + 1, &ico[long(entry + 12)..long(entry + 12) + long(entry + 8)]));
        group.extend_from_slice(&ico[entry..entry + 12]);
        group.extend_from_slice(&(image + 1).to_le_bytes());
    }
    out.extend(resource(RT_GROUP_ICON, 1, &group));
    out
}

/// Which source this is, for /status: the commit, whether the tree differed from it, and when it was built. A person comparing builds needs more than the version number.
fn build_id() -> String {
    let git = |args: &[&str]| std::process::Command::new("git").args(args).output().ok().filter(|output| output.status.success()).map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    let commit = git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_else(|| "no commit".to_string());
    let changes = git(&["--no-optional-locks", "status", "--porcelain"]).is_some_and(|status| !status.is_empty());
    let built = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    format!("{commit}{}, built at {built}", if changes { " with changes" } else { "" })
}

fn main() {
    println!("cargo:rustc-env=DOGMIND_APP_BUILD={}", build_id());
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../dogmind-agent/src");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/app.ico");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // `scripts/make_icon.py` makes it from the artwork.
        if let Ok(ico) = std::fs::read("../../assets/app.ico") {
            let path = std::path::Path::new(&std::env::var("OUT_DIR").expect("cargo sets it")).join("app.res");
            std::fs::write(&path, icon_resources(&ico)).expect("the build folder can be written");
            println!("cargo:rustc-link-arg-bins={}", path.display());
        }
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'");
    }
}
