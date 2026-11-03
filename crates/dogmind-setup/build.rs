//! The icon, and a manifest that says this program runs as the person who started it. Without that Windows would see `Setup` in its name and ask for an administrator.

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

/// The version resource: what Explorer's Details, Task Manager and Installed apps call the program, and its version. Without it they
/// call it by its file's name.
fn version_resource(description: &str, internal: &str, original: &str) -> Vec<u8> {
    fn pad(out: &mut Vec<u8>) {
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    fn node(key: &str, value: &[u8], value_length: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![0, 0];
        out.extend_from_slice(&value_length.to_le_bytes());
        out.extend_from_slice(&u16::from(text).to_le_bytes());
        for unit in key.encode_utf16().chain([0]) {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        if !value.is_empty() {
            pad(&mut out);
            out.extend_from_slice(value);
        }
        for child in children {
            pad(&mut out);
            out.extend_from_slice(child);
        }
        let length = out.len() as u16;
        out[..2].copy_from_slice(&length.to_le_bytes());
        out
    }
    let string = |key: &str, text: &str| {
        let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        node(key, &bytes, units.len() as u16, true, &[])
    };
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let parts: Vec<u32> = version.split('.').map(|part| part.parse().unwrap_or(0)).chain(std::iter::repeat(0)).take(3).collect();
    let (high, low) = ((parts[0] << 16) | parts[1], parts[2] << 16);
    let mut fixed = Vec::new();
    // VS_FIXEDFILEINFO: signature, structure version, file and product versions, flags' mask and flags, Windows NT, an application
    for word in [0xFEEF_04BDu32, 0x0001_0000, high, low, high, low, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0] {
        fixed.extend_from_slice(&word.to_le_bytes());
    }
    let strings = [("CompanyName", "rewroy"), ("FileDescription", description), ("FileVersion", version.as_str()), ("InternalName", internal), ("OriginalFilename", original), ("ProductName", "Dogmind"), ("ProductVersion", version.as_str())].map(|(key, text)| string(key, text));
    // (English, United States; Unicode)
    let table = node("040904b0", &[], 0, true, &strings);
    let file_info = node("StringFileInfo", &[], 0, true, &[table]);
    let translation = node("Translation", &0x04b0_0409u32.to_le_bytes(), 4, false, &[]);
    let var_info = node("VarFileInfo", &[], 0, true, &[translation]);
    node("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[file_info, var_info])
}

/// The empty entry every .res starts with, for one with no icon in it.
fn empty_res() -> Vec<u8> {
    let mut out = vec![0, 0, 0, 0, 32, 0, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0];
    out.resize(32, 0);
    out
}
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/app.ico");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let mut res = std::fs::read("../../assets/app.ico").map_or_else(|_| empty_res(), |ico| icon_resources(&ico));
        res.extend(resource(16, 1, &version_resource("Dogmind Setup", "dogmind-setup", "DogmindSetup.exe"))); // RT_VERSION
        let path = std::path::Path::new(&std::env::var("OUT_DIR").expect("cargo sets it")).join("setup.res");
        std::fs::write(&path, res).expect("the build folder can be written");
        println!("cargo:rustc-link-arg-bins={}", path.display());
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='asInvoker' uiAccess='false'");
    }
}
