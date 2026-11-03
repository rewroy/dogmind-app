//! Pictures that go with a message, on the app's side: which files count as one, a pasted bitmap as a PNG, and small copies to show. A picture is
//! decoded here when it comes into view, on a thread of its own, and only kept at the size it is shown at: a long transcript opens without reading any of them.

use std::path::{Path, PathBuf};

use windows_sys::Win32::Graphics::Gdi::*;

use crate::lang::t;
use crate::paint::scale;

/// How many go with one message.
pub const MOST_IMAGES: usize = 8;

/// The kinds the agent takes. A file of another kind dropped on the window is named in the message, as any other file is.
const EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

pub fn is_image(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// What a paste brings to the composer.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Paste {
    /// Files copied in the Explorer.
    Files,
    /// The control's own job: a copy from a table or a document has a picture of the text as well, and the text is what was meant.
    Text,
    Bitmap,
    Nothing,
}

pub fn paste_kind(has_files: bool, has_text: bool, has_bitmap: bool) -> Paste {
    if has_files {
        Paste::Files
    } else if has_text {
        Paste::Text
    } else if has_bitmap {
        Paste::Bitmap
    } else {
        Paste::Nothing
    }
}

/// Where a pasted picture is kept: a new name each time, as the message's file is read when it is sent, and shown again when the session is opened.
pub fn pasted_path(folder: &Path, stamp: u128) -> PathBuf {
    folder.join("attachments").join(format!("pasted-{stamp}.png"))
}

/// This size kept in proportion, inside the room given, and never larger than it is.
pub fn fit(width: i32, height: i32, room_width: i32, room_height: i32) -> (i32, i32) {
    if width <= 0 || height <= 0 {
        return (room_width.max(1), room_height.max(1));
    }
    let share = (f64::from(room_width) / f64::from(width)).min(f64::from(room_height) / f64::from(height)).min(1.0);
    (((f64::from(width) * share).round() as i32).max(1), ((f64::from(height) * share).round() as i32).max(1))
}

/// How a message's pictures stand in its card: one is large, several are small and in rows. Without decoding them: the room they take is known from their count.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Tiles {
    pub width: i32,
    pub height: i32,
    pub gap: i32,
    pub per_row: i32,
    pub rows: i32,
}

impl Tiles {
    /// `sizes`: what each is, in pixels, where it is known (none is 0 by 0), so that a card is as tall as what is in it. `room`: the pixels across that the card has for them.
    pub fn of(sizes: &[(i32, i32)], room: i32, dpi: u32) -> Tiles {
        let count = (sizes.len() as i32).max(1);
        let gap = scale(dpi, 6);
        let (most_width, most_height) = if count == 1 { (scale(dpi, 280), scale(dpi, 200)) } else { (scale(dpi, 130), scale(dpi, 104)) };
        let most_width = most_width.min(room.max(scale(dpi, 60)));
        let most_height = if count == 1 { most_height } else { most_height.min(most_width) };
        let fitted: Vec<(i32, i32)> = sizes.iter().map(|&(width, height)| fit(width, height, most_width, most_height)).collect();
        let height = fitted.iter().map(|(_, height)| *height).max().unwrap_or(most_height);
        let width = if count == 1 { fitted.first().map_or(most_width, |(width, _)| *width) } else { most_width };
        let per_row = ((room + gap) / (width + gap)).clamp(1, count);
        Tiles { width, height, gap, per_row, rows: (count + per_row - 1) / per_row }
    }

    pub fn total_width(&self) -> i32 {
        self.per_row * self.width + (self.per_row - 1) * self.gap
    }

    pub fn total_height(&self) -> i32 {
        self.rows * self.height + (self.rows - 1) * self.gap
    }

    /// Where the picture at this place in the message stands, from the top left of them all.
    pub fn at(&self, index: usize) -> (i32, i32) {
        let (row, column) = (index as i32 / self.per_row, index as i32 % self.per_row);
        (column * (self.width + self.gap), row * (self.height + self.gap))
    }
}

/// A small copy, top down, in blue, green, red and alpha, premultiplied.
pub struct Decoded {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Undecoded {
    Gone,
    Unreadable,
}

/// A small copy of a file, for a room this large.
pub fn decode(path: &Path, room_width: i32, room_height: i32) -> Result<Decoded, Undecoded> {
    let bytes = std::fs::read(path).map_err(|_| Undecoded::Gone)?;
    decode_bytes(&bytes, room_width, room_height)
}

/// (a GIF's first frame)
pub fn decode_bytes(bytes: &[u8], room_width: i32, room_height: i32) -> Result<Decoded, Undecoded> {
    let picture = image::load_from_memory(bytes).map_err(|_| Undecoded::Unreadable)?;
    let (width, height) = fit(picture.width() as i32, picture.height() as i32, room_width, room_height);
    let shown = if (width as u32, height as u32) == (picture.width(), picture.height()) { picture } else { picture.thumbnail_exact(width as u32, height as u32) };
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for pixel in shown.to_rgba8().pixels() {
        let [red, green, blue, alpha] = pixel.0;
        let over = |channel: u8| (u32::from(channel) * u32::from(alpha) / 255) as u8;
        pixels.extend_from_slice(&[over(blue), over(green), over(red), alpha]);
    }
    Ok(Decoded { width, height, pixels })
}

/// What a bitmap, as the clipboard holds one, has in it: its width, its height and its pixels, top down, in blue, green and red. Quick, for
/// while the clipboard is held; making a PNG of them is not.
pub unsafe fn bitmap_pixels(bitmap: HBITMAP) -> Option<(i32, i32, Vec<u8>)> {
    let mut header: BITMAP = std::mem::zeroed();
    if GetObjectW(bitmap, std::mem::size_of::<BITMAP>() as i32, std::ptr::addr_of_mut!(header).cast()) == 0 || header.bmWidth <= 0 || header.bmHeight <= 0 {
        return None;
    }
    let (width, height) = (header.bmWidth, header.bmHeight);
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width;
    info.bmiHeader.biHeight = -height; // from the top down
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let device = CreateCompatibleDC(std::ptr::null_mut());
    let lines = GetDIBits(device, bitmap, 0, height as u32, pixels.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS);
    DeleteDC(device);
    (lines > 0).then_some((width, height, pixels))
}

/// Those pixels as a PNG. Their alpha is not read: a screenshot's is all zeros.
pub fn png_of(width: i32, height: i32, pixels: &[u8]) -> Result<Vec<u8>, String> {
    let mut rgba = Vec::with_capacity(pixels.len());
    for pixel in pixels.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    let picture = image::RgbaImage::from_raw(width as u32, height as u32, rgba).ok_or_else(|| t("The picture on the clipboard could not be read.").to_string())?;
    let mut out = Vec::new();
    picture.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).map_err(|_| t("The picture on the clipboard could not be read.").to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_is_told_by_its_name_whatever_its_case() {
        for name in ["a.png", "b.JPG", "c.jpeg", "d.GiF", "C:\\x y\\f.webp"] {
            assert!(is_image(Path::new(name)), "{name}");
        }
        for name in ["a.txt", "png", "a.png.exe", "a", "b.bmp"] {
            assert!(!is_image(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn a_paste_prefers_files_then_text_then_a_bitmap() {
        assert_eq!(paste_kind(true, true, true), Paste::Files);
        assert_eq!(paste_kind(false, true, true), Paste::Text, "a table copied from a spreadsheet");
        assert_eq!(paste_kind(false, false, true), Paste::Bitmap, "a screenshot");
        assert_eq!(paste_kind(false, false, false), Paste::Nothing);
    }

    #[test]
    fn a_picture_keeps_its_proportions_and_is_not_blown_up() {
        assert_eq!(fit(1000, 500, 200, 200), (200, 100));
        assert_eq!(fit(500, 1000, 200, 200), (100, 200));
        assert_eq!(fit(50, 40, 200, 200), (50, 40));
        assert_eq!(fit(10000, 1, 100, 100), (100, 1), "at least a pixel");
        assert_eq!(fit(0, 0, 100, 80), (100, 80));
    }

    #[test]
    fn pictures_stand_in_rows_that_fit_the_card() {
        let one = Tiles::of(&[(1000, 500)], 2000, 96);
        assert_eq!((one.width, one.height, one.per_row, one.rows), (280, 140, 1, 1), "the card is as tall as the picture");
        let unknown = Tiles::of(&[(0, 0)], 2000, 96);
        assert_eq!((unknown.width, unknown.height), (280, 200));
        let small = Tiles::of(&[(32, 32)], 2000, 96);
        assert_eq!((small.width, small.height), (32, 32), "it is not blown up");
        let one_narrow = Tiles::of(&[(1000, 500)], 150, 96);
        assert!(one_narrow.width <= 150, "{one_narrow:?}");
        let five = Tiles::of(&[(0, 0); 5], 410, 96);
        assert_eq!((five.per_row, five.rows), (3, 2));
        assert_eq!(five.at(0), (0, 0));
        assert_eq!(five.at(4), (136, 110));
        assert!(five.total_width() <= 410);
        assert_eq!(five.total_height(), 2 * 104 + 6);
        let two = Tiles::of(&[(1000, 100), (1000, 200)], 2000, 96);
        assert_eq!((two.per_row, two.rows, two.height), (2, 1, 26), "no empty column, and as tall as the tallest");
        assert_eq!(Tiles::of(&[(0, 0); 3], 10, 96).per_row, 1, "whatever the room, it is one at least");
    }

    #[test]
    fn a_pasted_bitmap_becomes_a_png_the_agent_takes() {
        unsafe {
            // What the clipboard holds after a screenshot: a device-independent bitmap, here 8 by 6 of one colour, its alpha all zeros.
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = 8;
            info.bmiHeader.biHeight = -6;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(std::ptr::null_mut(), &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            assert!(!bitmap.is_null());
            for at in 0..8 * 6 {
                *(bits as *mut u32).add(at) = 0x00_20_40_80; // blue 0x80, green 0x40, red 0x20
            }
            let (width, height, pixels) = bitmap_pixels(bitmap).unwrap();
            DeleteObject(bitmap);
            assert_eq!((width, height, pixels.len()), (8, 6, 8 * 6 * 4));
            let png = png_of(width, height, &pixels).unwrap();
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            let copy = decode_bytes(&png, 100, 100).unwrap();
            assert_eq!((copy.width, copy.height), (8, 6));
            assert_eq!(&copy.pixels[..4], &[0x80, 0x40, 0x20, 0xFF], "opaque, in the colour it had");
            let small = decode_bytes(&png, 4, 4).unwrap();
            assert_eq!((small.width, small.height, small.pixels.len()), (4, 3, 4 * 3 * 4));
            let taken = dogmind_agent::images::attach_bytes("pasted.png", png).unwrap();
            assert_eq!((taken.media_type.as_str(), taken.width, taken.height), ("image/png", 8, 6));
        }
    }

    #[test]
    fn a_file_that_went_or_is_no_picture_says_which() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(decode(&dir.path().join("gone.png"), 10, 10).err(), Some(Undecoded::Gone));
        let broken = dir.path().join("broken.png");
        std::fs::write(&broken, b"not a picture").unwrap();
        assert_eq!(decode(&broken, 10, 10).err(), Some(Undecoded::Unreadable));
    }
}
