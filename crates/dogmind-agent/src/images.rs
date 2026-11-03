//! Pictures in a conversation. The bytes are files in the session's own folder and a message names them: a session file stays small, and
//! a picture goes out the same bytes every time, as the local engine reuses its prefix by each picture's digest. A picture is looked at
//! when it is attached, made smaller only if the provider allows no larger, once, and never encoded again after that.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat, ImageReader};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::message::{ImageRef, Message};

/// What the local engine takes, in bytes and in pixels: nothing past this is read by anyone, so nothing past it is decoded.
const MOST_BYTES: usize = 32 << 20;
const MOST_PIXELS: u64 = 64 << 20;
/// How many times a picture still too heavy is made smaller, by three quarters.
const SHRINKS: u32 = 6;
const JPEG_QUALITY: u8 = 85;

// What a person reads when a file is refused. English here as all the agent's words; `lang` in the app has each in Japanese.
pub const NOT_A_PICTURE: &str = "This file is not a picture that can be attached: PNG, JPEG, WebP and GIF can.";
pub const DAMAGED: &str = "This picture cannot be read: the file is damaged or cut short.";
pub const TOO_LARGE: &str = "This picture is too large to attach: at most 32 MB and 64 million pixels.";
pub const UNREADABLE_FILE: &str = "This file could not be opened.";
pub const NOT_SAVED: &str = "The picture could not be saved with the session, so nothing was sent.";
/// Every message above, for the test that each has its Japanese.
pub const WORDS: [&str; 5] = [NOT_A_PICTURE, DAMAGED, TOO_LARGE, UNREADABLE_FILE, NOT_SAVED];

/// What a provider takes of a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimit {
    /// The longest side, in pixels.
    pub side: u32,
    pub bytes: usize,
}

impl ImageLimit {
    /// The services that make a picture smaller themselves take much, and the local engine does too.
    pub const WIDE: ImageLimit = ImageLimit { side: 8000, bytes: 20 << 20 };
}

/// A picture that has been looked at and can be sent: it holds its bytes, so that a pasted one needs no file and a file may go away after attaching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}

fn media_type_of(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::WebP => Some("image/webp"),
        ImageFormat::Gif => Some("image/gif"),
        _ => None,
    }
}

fn extension_of(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

/// The whole picture, as a check that it is one: a GIF's first frame.
fn decode(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, String> {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MOST_PIXELS * 8);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    reader.decode().map_err(|_| DAMAGED.to_string())
}

/// A picture from what was pasted or dropped. A plain message when it cannot be one.
pub fn attach_bytes(name: &str, bytes: Vec<u8>) -> Result<Attachment, String> {
    if bytes.len() > MOST_BYTES {
        return Err(TOO_LARGE.to_string());
    }
    let format = image::guess_format(&bytes).map_err(|_| NOT_A_PICTURE.to_string())?;
    let media_type = media_type_of(format).ok_or(NOT_A_PICTURE)?;
    let picture = decode(&bytes, format)?;
    if u64::from(picture.width()) * u64::from(picture.height()) > MOST_PIXELS {
        return Err(TOO_LARGE.to_string());
    }
    Ok(Attachment { name: name.to_string(), media_type: media_type.to_string(), width: picture.width(), height: picture.height(), bytes })
}

/// A picture from a file. A plain message when it cannot be one.
pub fn attach_file(path: &Path) -> Result<Attachment, String> {
    let size = std::fs::metadata(path).map_err(|_| UNREADABLE_FILE.to_string())?.len();
    if size > MOST_BYTES as u64 {
        return Err(TOO_LARGE.to_string());
    }
    let bytes = std::fs::read(path).map_err(|_| UNREADABLE_FILE.to_string())?;
    attach_bytes(&path.file_name().map_or(String::new(), |name| name.to_string_lossy().into_owned()), bytes)
}

/// By its first bytes, whatever the file is called.
pub fn is_picture(path: &Path) -> bool {
    let mut head = [0u8; 32];
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let Ok(count) = file.read(&mut head) else { return false };
    image::guess_format(&head[..count]).ok().and_then(media_type_of).is_some()
}

/// Copies of what the person attached, as they were, in a folder commands can read: a model can put one in the workspace for a page to show,
/// which it could not from the session's own folder. Named by the session and their place in the message, with a name a command line takes
/// as it is. Copies older than a week go as new ones come.
pub fn copies_for_commands(folder: &Path, session: &str, pictures: &[Attachment]) -> Vec<PathBuf> {
    if std::fs::create_dir_all(folder).is_err() {
        return Vec::new();
    }
    let week = std::time::Duration::from_secs(7 * 24 * 3600);
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        if entry.metadata().ok().and_then(|data| data.modified().ok()).and_then(|when| when.elapsed().ok()).is_some_and(|age| age > week) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let plain = |text: &str| text.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '-' }).collect::<String>();
    let mut written = Vec::new();
    for (at, picture) in pictures.iter().enumerate() {
        let stem = Path::new(&picture.name).file_stem().map(|stem| plain(&stem.to_string_lossy())).filter(|stem| stem.chars().any(|c| c.is_ascii_alphanumeric())).unwrap_or_else(|| "picture".to_string());
        let path = folder.join(format!("{}-{}-{stem}.{}", plain(session), at + 1, extension_of(&picture.media_type)));
        if std::fs::write(&path, &picture.bytes).is_ok() {
            written.push(path);
        }
    }
    written
}

/// The folder a session keeps its pictures in: beside its file, and deleted with it.
pub fn folder_of(session: &Path) -> PathBuf {
    session.with_extension("images")
}

/// Where a message's picture is on this computer, to show it.
pub fn file_of(session: &Path, image: &ImageRef) -> PathBuf {
    folder_of(session).join(&image.file)
}

/// As the provider will take it: the very bytes when they fit, and otherwise smaller, as a PNG, or as a JPEG when that is what it was or a PNG is still too heavy.
fn fitted(picture: &Attachment, limit: ImageLimit) -> Result<(Vec<u8>, String, u32, u32), String> {
    if picture.width.max(picture.height) <= limit.side && picture.bytes.len() <= limit.bytes {
        return Ok((picture.bytes.clone(), picture.media_type.clone(), picture.width, picture.height));
    }
    let format = image::guess_format(&picture.bytes).map_err(|_| NOT_A_PICTURE.to_string())?;
    let mut shown = decode(&picture.bytes, format)?;
    if shown.width().max(shown.height()) > limit.side {
        shown = shown.resize(limit.side, limit.side, image::imageops::FilterType::Lanczos3);
    }
    let encode = |image: &DynamicImage, as_jpeg: bool| -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        if as_jpeg {
            let flat = image.to_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY).encode_image(&flat).map_err(|_| DAMAGED.to_string())?;
        } else {
            image.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).map_err(|_| DAMAGED.to_string())?;
        }
        Ok(out)
    };
    let mut as_jpeg = picture.media_type == "image/jpeg";
    let mut bytes = encode(&shown, as_jpeg)?;
    if bytes.len() > limit.bytes && !as_jpeg {
        as_jpeg = true;
        bytes = encode(&shown, true)?;
    }
    for _ in 0..SHRINKS {
        if bytes.len() <= limit.bytes {
            break;
        }
        shown = shown.resize(shown.width() * 3 / 4, shown.height() * 3 / 4, image::imageops::FilterType::Lanczos3);
        bytes = encode(&shown, true)?;
    }
    if bytes.len() > limit.bytes {
        return Err(TOO_LARGE.to_string());
    }
    Ok((bytes, if as_jpeg { "image/jpeg" } else { "image/png" }.to_string(), shown.width(), shown.height()))
}

/// Into the session's folder, by what is in it: the same picture twice is one file, so it stays the same bytes.
pub(crate) fn save(session: &Path, picture: &Attachment, limit: ImageLimit) -> Result<ImageRef, String> {
    let (bytes, media_type, width, height) = fitted(picture, limit)?;
    let digest: String = Sha256::digest(&bytes).iter().take(16).map(|byte| format!("{byte:02x}")).collect();
    let file = format!("{digest}.{}", extension_of(&media_type));
    let folder = folder_of(session);
    let target = folder.join(&file);
    if !target.exists() {
        std::fs::create_dir_all(&folder).map_err(|_| NOT_SAVED.to_string())?;
        let partial = target.with_extension("partial");
        std::fs::write(&partial, &bytes).map_err(|_| NOT_SAVED.to_string())?;
        std::fs::rename(&partial, &target).map_err(|_| NOT_SAVED.to_string())?;
    }
    Ok(ImageRef { file, media_type, width, height })
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const LETTERS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let bits = (u32::from(group[0]) << 16) | (u32::from(*group.get(1).unwrap_or(&0)) << 8) | u32::from(*group.get(2).unwrap_or(&0));
        out.push(char::from(LETTERS[(bits >> 18) as usize & 63]));
        out.push(char::from(LETTERS[(bits >> 12) as usize & 63]));
        out.push(if group.len() > 1 { char::from(LETTERS[(bits >> 6) as usize & 63]) } else { '=' });
        out.push(if group.len() > 2 { char::from(LETTERS[bits as usize & 63]) } else { '=' });
    }
    out
}

/// A picture as a provider is sent it.
pub(crate) struct Loaded {
    pub media_type: String,
    pub data: String,
}

impl Loaded {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

/// A message's pictures read from their files, in order, and its text with a word for any that has gone: the conversation goes on without it.
pub(crate) fn loaded(message: &Message, folder: Option<&Path>) -> (Vec<Loaded>, String) {
    let mut text = message.content.clone().unwrap_or_default();
    let mut pictures = Vec::new();
    for image in &message.images {
        // (a file with nothing in it, as a power cut can leave: a service refuses the empty picture, and with it every later request of this conversation)
        match folder.and_then(|folder| std::fs::read(folder.join(&image.file)).ok()).filter(|bytes| !bytes.is_empty()) {
            Some(bytes) => pictures.push(Loaded { media_type: image.media_type.clone(), data: base64(&bytes) }),
            None => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&gone_note(&image.file));
            }
        }
    }
    (pictures, text)
}

fn gone_note(file: &str) -> String {
    format!("[A picture that was here, {file}, is no longer on this computer, so it could not be shown.]")
}

/// What stands in a message for a picture no longer sent. The file stays.
pub(crate) fn note_for(file: &str, why: &str) -> String {
    format!("[A picture that was here, {file}, is not shown: {why}.]")
}

/// The content of an OpenAI-style message with pictures: they come first, then the words.
pub(crate) fn openai_content(message: &Message, folder: Option<&Path>) -> Value {
    let (pictures, text) = loaded(message, folder);
    if pictures.is_empty() {
        return Value::String(text);
    }
    let mut parts: Vec<Value> = pictures.iter().map(|picture| json!({"type": "image_url", "image_url": {"url": picture.data_url()}})).collect();
    parts.push(json!({"type": "text", "text": text}));
    Value::Array(parts)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use image::RgbImage;

    #[test]
    fn attached_pictures_are_copied_as_they_were_under_names_a_command_takes() {
        let dir = tempfile::tempdir().unwrap();
        let picture = |name: &str, bytes: &[u8], media_type: &str| Attachment { name: name.into(), bytes: bytes.to_vec(), media_type: media_type.into(), width: 4, height: 3 };
        let folder = dir.path().join("attachments");
        let written = copies_for_commands(&folder, "20261103-151658", &[picture("レウ の写真.JPG", b"jpeg bytes", "image/jpeg"), picture("", b"png bytes", "image/png"), picture("my photo (2).webp", b"webp", "image/webp")]);
        let names: Vec<String> = written.iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["20261103-151658-1-picture.jpg", "20261103-151658-2-picture.png", "20261103-151658-3-my-photo--2-.webp"]);
        assert_eq!(std::fs::read(&written[0]).unwrap(), b"jpeg bytes", "the very bytes, not what a provider was sent");
    }

    pub(crate) fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        // Not flat, so that a picture has a real size to shrink.
        let picture = RgbImage::from_fn(width, height, |x, y| image::Rgb([(x * 7 % 251) as u8, (y * 13 % 241) as u8, ((x ^ y) * 3 % 239) as u8]));
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(picture).write_to(&mut Cursor::new(&mut out), format).unwrap();
        out
    }

    #[test]
    fn a_picture_with_nothing_in_it_goes_as_one_that_has_gone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), "").unwrap();
        let message = Message { images: vec![ImageRef { file: "a.png".into(), media_type: "image/png".into(), width: 1, height: 1 }], ..Message::text("user", "look") };
        let (pictures, text) = loaded(&message, Some(dir.path()));
        assert!(pictures.is_empty(), "a service refuses it, and all that comes after it");
        assert!(text.contains("no longer on this computer"), "{text}");
    }

    #[test]
    fn each_kind_of_picture_is_taken_and_says_what_it_is() {
        for (format, media_type) in [(ImageFormat::Png, "image/png"), (ImageFormat::Jpeg, "image/jpeg"), (ImageFormat::WebP, "image/webp"), (ImageFormat::Gif, "image/gif")] {
            let picture = attach_bytes("a", encoded(40, 30, format)).unwrap();
            assert_eq!((picture.media_type.as_str(), picture.width, picture.height), (media_type, 40, 30));
        }
    }

    #[test]
    fn what_is_no_picture_is_refused_in_plain_words() {
        assert_eq!(attach_bytes("a", b"hello there, this is text".to_vec()).unwrap_err(), NOT_A_PICTURE);
        let mut cut = encoded(200, 200, ImageFormat::Png);
        cut.truncate(cut.len() / 2);
        assert_eq!(attach_bytes("a", cut).unwrap_err(), DAMAGED);
        assert_eq!(attach_bytes("a", vec![0; MOST_BYTES + 1]).unwrap_err(), TOO_LARGE);
        assert_eq!(attach_file(Path::new("no/such/file.png")).unwrap_err(), UNREADABLE_FILE);
    }

    #[test]
    fn a_file_is_told_by_its_bytes_and_keeps_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let (picture, text) = (dir.path().join("named.txt"), dir.path().join("a.png"));
        std::fs::write(&picture, encoded(8, 8, ImageFormat::Png)).unwrap();
        std::fs::write(&text, "not a picture").unwrap();
        assert!(is_picture(&picture) && !is_picture(&text) && !is_picture(&dir.path().join("none")));
        assert_eq!(attach_file(&picture).unwrap().name, "named.txt");
    }

    #[test]
    fn a_picture_that_fits_is_the_same_bytes_and_one_that_does_not_is_made_smaller_once() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let bytes = encoded(300, 100, ImageFormat::Png);
        let picture = attach_bytes("a", bytes.clone()).unwrap();
        let kept = save(&session, &picture, ImageLimit::WIDE).unwrap();
        assert_eq!(std::fs::read(file_of(&session, &kept)).unwrap(), bytes, "never encoded again");
        assert_eq!(save(&session, &picture, ImageLimit::WIDE).unwrap(), kept, "the same picture is the same file");
        // (a PNG encoded again can come out the same: a JPEG never does, nor a GIF as itself)
        for format in [ImageFormat::Jpeg, ImageFormat::Gif, ImageFormat::WebP] {
            let bytes = encoded(64, 64, format);
            let kept = save(&session, &attach_bytes("a", bytes.clone()).unwrap(), ImageLimit::WIDE).unwrap();
            assert_eq!(std::fs::read(file_of(&session, &kept)).unwrap(), bytes, "{format:?}");
        }
        let small = save(&session, &picture, ImageLimit { side: 150, bytes: 20 << 20 }).unwrap();
        assert_eq!((small.width, small.height), (150, 50));
        assert_ne!(small.file, kept.file);
        assert!(attach_file(&file_of(&session, &small)).is_ok(), "what was made is a picture");
    }

    #[test]
    fn a_picture_too_heavy_goes_as_a_jpeg_and_smaller_until_it_fits() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let picture = attach_bytes("a", encoded(600, 600, ImageFormat::Png)).unwrap();
        let most = picture.bytes.len() / 8;
        let kept = save(&session, &picture, ImageLimit { side: 8000, bytes: most }).unwrap();
        assert_eq!(kept.media_type, "image/jpeg");
        assert!(kept.file.ends_with(".jpg"));
        assert!(std::fs::read(file_of(&session, &kept)).unwrap().len() <= most);
    }

    #[test]
    fn base64_is_the_standard_one() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xfb, 0xff, 0xfe]), "+//+");
    }

    #[test]
    fn a_message_goes_pictures_first_and_a_lost_picture_is_said_in_words() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let bytes = encoded(8, 8, ImageFormat::Png);
        let kept = save(&session, &attach_bytes("a", bytes.clone()).unwrap(), ImageLimit::WIDE).unwrap();
        let message = Message { images: vec![kept.clone()], ..Message::text("user", "what is this") };
        let content = openai_content(&message, Some(&folder_of(&session)));
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(content[0]["image_url"]["url"], format!("data:image/png;base64,{}", base64(&bytes)));
        assert_eq!(content[1], json!({"type": "text", "text": "what is this"}));
        std::fs::remove_file(file_of(&session, &kept)).unwrap();
        let content = openai_content(&message, Some(&folder_of(&session)));
        let said = content.as_str().unwrap();
        assert!(said.starts_with("what is this\n") && said.contains("no longer on this computer"), "{said}");
    }
}
