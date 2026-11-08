//! How the window looks: its colours and type, antialiased shapes through GDI+, and the buttons, which draw themselves.

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::design::{radius, size, DIM_ALPHA};
use crate::{design, icon};

pub const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

/// The colours, by theme: dark and light. Every use asks, so that the whole window can change at once.
pub mod theme {
    use super::rgb;
    use std::sync::atomic::{AtomicBool, Ordering};

    static LIGHT: AtomicBool = AtomicBool::new(false);

    pub fn is_light() -> bool {
        LIGHT.load(Ordering::Relaxed)
    }

    pub fn set_light(light: bool) {
        LIGHT.store(light, Ordering::Relaxed);
    }

    /// What Windows is set to for apps. Dark where it does not say.
    pub fn system_is_light() -> bool {
        use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
        let (key, name) = (super::wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"), super::wide("AppsUseLightTheme"));
        let (mut value, mut size) = (0u32, 4u32);
        // SAFETY: the names end in a zero, and the value is as long as the call is told.
        let done = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), std::ptr::addr_of_mut!(value).cast(), &mut size) };
        done == 0 && value != 0
    }

    macro_rules! palette {
        ($($name:ident: $dark:expr, $light:expr;)*) => {
            $(pub fn $name() -> u32 { if is_light() { $light } else { $dark } })*

            /// The same colour in the other theme, for what was kept as a number.
            pub fn translate(color: u32, to_light: bool) -> u32 {
                $(if color == (if to_light { $dark } else { $light }) { return if to_light { $light } else { $dark }; })*
                color
            }
        };
    }

    palette! {
        background: rgb(33, 32, 30), rgb(250, 249, 246);
        sidebar: rgb(25, 24, 23), rgb(241, 239, 234);
        surface: rgb(45, 44, 41), rgb(255, 255, 255);
        raised: rgb(58, 56, 53), rgb(235, 232, 225);
        line: rgb(66, 64, 60), rgb(221, 217, 209);
        text: rgb(236, 233, 227), rgb(36, 34, 31);
        soft: rgb(205, 201, 194), rgb(62, 59, 54);
        muted: rgb(158, 154, 146), rgb(104, 100, 93);
        faint: rgb(134, 130, 123), rgb(118, 114, 106);
        accent: rgb(224, 138, 91), rgb(193, 98, 55);
        accent_hover: rgb(236, 156, 112), rgb(172, 84, 44);
        on_accent: rgb(30, 22, 16), rgb(255, 251, 247);
        good: rgb(140, 190, 130), rgb(56, 128, 66);
        warn: rgb(226, 190, 120), rgb(150, 102, 16);
        bad: rgb(232, 120, 112), rgb(186, 58, 48);
        bad_hover: rgb(240, 138, 130), rgb(166, 48, 40);
        code: rgb(206, 196, 178), rgb(84, 72, 54);
        // Not darker: on a cell of about 51 in each channel or less, RichEdit ignores the colour of the text, and a diff loses its reds and greens.
        code_back: rgb(56, 55, 52), rgb(240, 238, 232);
        // A changed line's ground: only a shade from the page, for its words to keep their colours.
        added_back: rgb(38, 54, 40), rgb(226, 243, 228);
        removed_back: rgb(62, 40, 38), rgb(251, 230, 226);
        user_back: rgb(58, 54, 49), rgb(236, 232, 224);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_colour_kept_as_a_number_finds_itself_in_the_other_theme() {
            let (dark, light) = (rgb(58, 54, 49), rgb(236, 232, 224));
            assert_eq!(translate(dark, true), light);
            assert_eq!(translate(light, false), dark);
            assert_eq!(translate(rgb(1, 2, 3), true), rgb(1, 2, 3), "one of no theme stays");
        }

        #[test]
        fn a_block_is_light_enough_to_keep_its_colours() {
            set_light(false);
            let back = code_back();
            assert!([back & 0xFF, (back >> 8) & 0xFF, (back >> 16) & 0xFF].iter().all(|channel| *channel >= 52), "{back:#x}");
        }

        /// WCAG's contrast of two colours: 1 is none, 21 is black on white.
        fn contrast(a: u32, b: u32) -> f64 {
            let light = |color: u32| {
                let channel = |shift: u32| {
                    let value = f64::from((color >> shift) & 0xFF) / 255.0;
                    if value <= 0.03928 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
                };
                0.2126 * channel(0) + 0.7152 * channel(8) + 0.0722 * channel(16) + 0.05
            };
            let (a, b) = (light(a), light(b));
            a.max(b) / a.min(b)
        }

        #[test]
        fn what_is_written_can_be_read_in_both_themes() {
            for (light, faint, muted, text, grounds) in [
                (false, rgb(134, 130, 123), rgb(158, 154, 146), rgb(236, 233, 227), [rgb(33, 32, 30), rgb(25, 24, 23), rgb(45, 44, 41)]),
                (true, rgb(118, 114, 106), rgb(104, 100, 93), rgb(36, 34, 31), [rgb(250, 249, 246), rgb(241, 239, 234), rgb(255, 255, 255)]),
            ] {
                for ground in grounds {
                    assert!(contrast(text, ground) >= 7.0, "text, light {light}: {}", contrast(text, ground));
                    assert!(contrast(muted, ground) >= 4.5, "muted, light {light}: {}", contrast(muted, ground));
                    // (for what is only a hint: dates, placeholders, an empty panel's words)
                    assert!(contrast(faint, ground) >= 3.5, "faint, light {light}: {}", contrast(faint, ground));
                }
            }
        }
    }
}

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn mul_div(value: i32, by: i32, over: i32) -> i32 {
    ((i64::from(value) * i64::from(by) + i64::from(over) / 2) / i64::from(over)) as i32
}

pub fn scale(dpi: u32, value: i32) -> i32 {
    mul_div(value, dpi as i32, 96)
}

pub fn rect(x: i32, y: i32, width: i32, height: i32) -> RECT {
    RECT { left: x, top: y, right: x + width, bottom: y + height }
}

/// Whether two areas share any pixel.
pub fn overlaps(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

fn argb(color: u32) -> u32 {
    0xFF00_0000 | ((color & 0xFF) << 16) | (color & 0xFF00) | ((color >> 16) & 0xFF)
}

pub unsafe fn start_gdiplus() {
    let input = GdiplusStartupInput { GdiplusVersion: 1, DebugEventCallback: 0, SuppressBackgroundThread: 0, SuppressExternalCodecs: 0 };
    let mut token = 0usize;
    GdiplusStartup(&mut token, &input, std::ptr::null_mut());
}

/// A rounded rectangle, antialiased.
pub unsafe fn rounded(device: HDC, area: RECT, radius: i32, fill: Option<u32>, border: Option<u32>) {
    if crate::render::rounded(device, area, radius, fill, border) {
        return;
    }
    let mut graphics = std::ptr::null_mut();
    if GdipCreateFromHDC(device, &mut graphics) != 0 {
        return;
    }
    GdipSetSmoothingMode(graphics, 4); // SmoothingModeAntiAlias
    let mut path = std::ptr::null_mut();
    GdipCreatePath(0, &mut path);
    let (x, y) = (area.left as f32, area.top as f32);
    let (w, h) = ((area.right - area.left - 1) as f32, (area.bottom - area.top - 1) as f32);
    let d = (radius * 2) as f32;
    GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
    GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
    GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(path);
    if let Some(color) = fill {
        let mut brush = std::ptr::null_mut();
        GdipCreateSolidFill(argb(color), &mut brush);
        GdipFillPath(graphics, brush.cast(), path);
        GdipDeleteBrush(brush.cast());
    }
    if let Some(color) = border {
        let mut pen = std::ptr::null_mut();
        GdipCreatePen1(argb(color), 1.0, 2, &mut pen); // UnitPixel
        GdipDrawPath(graphics, pen, path);
        GdipDeletePen(pen);
    }
    GdipDeletePath(path);
    GdipDeleteGraphics(graphics);
}

/// What is in this area, darkened as it is behind a dialog.
pub unsafe fn dim(device: HDC, area: RECT) {
    if crate::render::dim(device, area, DIM_ALPHA) {
        return;
    }
    let mut graphics = std::ptr::null_mut();
    if GdipCreateFromHDC(device, &mut graphics) != 0 {
        return;
    }
    let mut brush = std::ptr::null_mut();
    GdipCreateSolidFill(u32::from(DIM_ALPHA) << 24, &mut brush);
    GdipFillRectangleI(graphics, brush.cast(), area.left, area.top, area.right - area.left, area.bottom - area.top);
    GdipDeleteBrush(brush.cast());
    GdipDeleteGraphics(graphics);
}

/// A speech bubble drawn like the dog: one thick line with round ends, whose underside flows out into a tail like a dog's.
pub unsafe fn bubble(device: HDC, area: RECT, radius: i32, tip: (i32, i32), fill: u32, line: u32, thickness: f32) {
    if bubble_by_render(device, area, radius, tip, fill, line, thickness) {
        return;
    }
    let mut graphics = std::ptr::null_mut();
    if GdipCreateFromHDC(device, &mut graphics) != 0 {
        return;
    }
    GdipSetSmoothingMode(graphics, 4); // SmoothingModeAntiAlias
    let mut path = std::ptr::null_mut();
    GdipCreatePath(0, &mut path);
    let inset = thickness / 2.0;
    let (x, y) = (area.left as f32 + inset, area.top as f32 + inset);
    let (w, h) = ((area.right - area.left) as f32 - thickness, (area.bottom - area.top) as f32 - thickness);
    let (r, d) = (radius as f32, (radius * 2) as f32);
    let (right, bottom) = (x + w, y + h);
    let (tip_x, tip_y) = (tip.0 as f32, tip.1 as f32);
    // A short tail, leaning to the right with a slight bend and a round end, like a dog's. Each side leaves the underside along
    // it, so there is no corner there. `tip` is its far end.
    let half = r * 0.46;
    let root = (tip_x - half * 0.95).clamp(x + r + half, right - r - half);
    let drop = tip_y - bottom;
    let end = thickness * 0.55;
    let (inner, outer) = ((tip_x + end * 0.1, tip_y - end * 0.9), (tip_x - end * 0.9, tip_y + end * 0.2));
    GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    GdipAddPathArc(path, right - d, y, d, d, 270.0, 90.0);
    GdipAddPathArc(path, right - d, bottom - d, d, d, 0.0, 90.0);
    GdipAddPathLine(path, right - r, bottom, root + half, bottom);
    GdipAddPathBezier(path, root + half, bottom, root + half * 0.45, bottom, inner.0 - end * 0.8, inner.1 - drop * 0.25, inner.0, inner.1);
    GdipAddPathBezier(path, inner.0, inner.1, tip_x + end, tip_y - end * 0.2, tip_x, tip_y + end, outer.0, outer.1);
    GdipAddPathBezier(path, outer.0, outer.1, root - half * 0.05, tip_y, root - half * 0.6, bottom + drop * 0.45, root - half, bottom);
    GdipAddPathLine(path, root - half, bottom, x + r, bottom);
    GdipAddPathArc(path, x, bottom - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(path);
    let mut brush = std::ptr::null_mut();
    GdipCreateSolidFill(argb(fill), &mut brush);
    GdipFillPath(graphics, brush.cast(), path);
    GdipDeleteBrush(brush.cast());
    let mut pen = std::ptr::null_mut();
    GdipCreatePen1(argb(line), thickness, 2, &mut pen); // UnitPixel
    GdipSetPenLineJoin(pen, 2); // LineJoinRound: the tip comes out round
    GdipDrawPath(graphics, pen, path);
    GdipDeletePen(pen);
    GdipDeletePath(path);
    GdipDeleteGraphics(graphics);
}

/// The same bubble through Direct2D: its pieces as GDI+ was given them, in an area that holds the tail as well. False: not drawn.
unsafe fn bubble_by_render(device: HDC, area: RECT, radius: i32, tip: (i32, i32), fill: u32, line: u32, thickness: f32) -> bool {
    use crate::render::Piece::*;
    let pad = thickness.ceil() as i32 + 2;
    let bounds = RECT { left: area.left.min(tip.0) - pad, top: area.top - pad, right: area.right.max(tip.0) + pad, bottom: area.bottom.max(tip.1) + pad };
    let (ox, oy) = (bounds.left as f32, bounds.top as f32);
    let inset = thickness / 2.0;
    let (x, y) = (area.left as f32 + inset - ox, area.top as f32 + inset - oy);
    let (w, h) = ((area.right - area.left) as f32 - thickness, (area.bottom - area.top) as f32 - thickness);
    let (r, d) = (radius as f32, (radius * 2) as f32);
    let (right, bottom) = (x + w, y + h);
    let (tip_x, tip_y) = (tip.0 as f32 - ox, tip.1 as f32 - oy);
    let half = r * 0.46;
    let root = (tip_x - half * 0.95).clamp(x + r + half, right - r - half);
    let drop = tip_y - bottom;
    let end = thickness * 0.55;
    let (inner, outer) = ((tip_x + end * 0.1, tip_y - end * 0.9), (tip_x - end * 0.9, tip_y + end * 0.2));
    let pieces = [
        Arc(x, y, d, d, 180.0, 90.0),
        Arc(right - d, y, d, d, 270.0, 90.0),
        Arc(right - d, bottom - d, d, d, 0.0, 90.0),
        Line(root + half, bottom),
        Curve(root + half * 0.45, bottom, inner.0 - end * 0.8, inner.1 - drop * 0.25, inner.0, inner.1),
        Curve(tip_x + end, tip_y - end * 0.2, tip_x, tip_y + end, outer.0, outer.1),
        Curve(root - half * 0.05, tip_y, root - half * 0.6, bottom + drop * 0.45, root - half, bottom),
        Line(x + r, bottom),
        Arc(x, bottom - d, d, d, 90.0, 90.0),
        Close,
    ];
    crate::render::figure(device, bounds, &pieces, Some(fill), Some((line, thickness)))
}

/// How wide a line of this type is.
/// A ring with a bright arc on it, the arc's start at `degrees`.
pub unsafe fn spinner(device: HDC, area: RECT, degrees: f32, ring: u32, arc: u32, thickness: f32) {
    ring_with_arc(device, area, degrees, 100.0, ring, arc, thickness);
}

/// A ring with a bright arc on it, from `degrees` and this far round.
pub unsafe fn ring_with_arc(device: HDC, area: RECT, degrees: f32, sweep: f32, ring: u32, arc: u32, thickness: f32) {
    {
        use crate::render::Piece::*;
        let inset = thickness / 2.0;
        let (width, height) = ((area.right - area.left) as f32 - thickness, (area.bottom - area.top) as f32 - thickness);
        let circle = [Circle(inset + width / 2.0, inset + height / 2.0, width.min(height) / 2.0)];
        if crate::render::figure(device, area, &circle, None, Some((ring, thickness))) {
            if sweep > 0.0 {
                crate::render::figure(device, area, &[Arc(inset, inset, width, height, degrees, sweep)], None, Some((arc, thickness)));
            }
            return;
        }
    }
    let mut graphics = std::ptr::null_mut();
    if GdipCreateFromHDC(device, &mut graphics) != 0 {
        return;
    }
    GdipSetSmoothingMode(graphics, 4); // SmoothingModeAntiAlias
    let inset = thickness / 2.0;
    let (x, y) = (area.left as f32 + inset, area.top as f32 + inset);
    let (width, height) = ((area.right - area.left) as f32 - thickness, (area.bottom - area.top) as f32 - thickness);
    let mut pen = std::ptr::null_mut();
    GdipCreatePen1(argb(ring), thickness, 2, &mut pen); // UnitPixel
    GdipDrawEllipse(graphics, pen, x, y, width, height);
    GdipDeletePen(pen);
    let mut pen = std::ptr::null_mut();
    GdipCreatePen1(argb(arc), thickness, 2, &mut pen);
    GdipSetPenStartCap(pen, LineCapRound);
    GdipSetPenEndCap(pen, LineCapRound);
    if sweep > 0.0 {
        GdipDrawArc(graphics, pen, x, y, width, height, degrees, sweep);
    }
    GdipDeletePen(pen);
    GdipDeleteGraphics(graphics);
}

/// How wide a line of what the window draws itself is: measured as DirectWrite draws it (`width_on` is GDI's, for what a control draws).
pub unsafe fn text_width(font: HFONT, text: &str) -> i32 {
    if let Some((width, _)) = crate::render::measure(font, text, None) {
        return width;
    }
    let device = CreateCompatibleDC(std::ptr::null_mut());
    let width = width_on(device, font, text);
    DeleteDC(device);
    width
}

/// The same, where a device is at hand.
pub unsafe fn width_on(device: HDC, font: HFONT, text: &str) -> i32 {
    let units: Vec<u16> = text.encode_utf16().collect();
    if let Some(partner) = partner_for(font, &units) {
        let across = measured(device, font, partner, &units);
        return *across.last().unwrap_or(&0);
    }
    let previous = SelectObject(device, font);
    let mut size = SIZE { cx: 0, cy: 0 };
    GetTextExtentPoint32W(device, units.as_ptr(), units.len() as i32, &mut size);
    SelectObject(device, previous);
    size.cx
}

/// A path shortened in the middle to what `fits`, so that its start and its last name both show, with an ellipsis between: `C:\\Users\\…\\Local\\Temp`. What fits is shown whole. A path with
/// too few parts to shorten, or that cannot keep its start and its last name, is cut at the end with an ellipsis, as any text is.
pub fn compact_path_with(path: &str, fits: &dyn Fn(&str) -> bool) -> String {
    if fits(path) {
        return path.to_string();
    }
    let end_cut = |text: &str| -> String {
        let mut kept: Vec<char> = text.chars().collect();
        while !kept.is_empty() && !fits(&format!("{}…", kept.iter().collect::<String>())) {
            kept.pop();
        }
        format!("{}…", kept.iter().collect::<String>())
    };
    let separator = if path.contains('\\') || !path.contains('/') { '\\' } else { '/' };
    let parts: Vec<&str> = path.split(separator).collect();
    let join = |head: usize, tail: usize| -> String {
        format!("{}{separator}…{separator}{}", parts[..head].join(&separator.to_string()), parts[parts.len() - tail..].join(&separator.to_string()))
    };
    if parts.len() < 3 || !fits(&join(1, 1)) {
        return end_cut(path);
    }
    let (mut head, mut tail) = (1, 1);
    // (the last name and the start grow in turn while there is room: what says where this is, and what says which drive and folder)
    loop {
        let mut grew = false;
        if head + tail + 1 < parts.len() && fits(&join(head, tail + 1)) {
            tail += 1;
            grew = true;
        }
        if head + tail + 1 < parts.len() && fits(&join(head + 1, tail)) {
            head += 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    join(head, tail)
}

/// A path as it is shown in this width: shortened in the middle where it does not fit.
pub unsafe fn compact_path(font: HFONT, path: &str, width: i32) -> String {
    compact_path_with(path, &|text| text_width(font, text) <= width)
}

/// How tall this text is, wrapped to this width, as the window draws it.
pub unsafe fn text_height(font: HFONT, text: &str, width: i32) -> i32 {
    if let Some((_, height)) = crate::render::measure(font, text, Some(width)) {
        return height;
    }
    let mut units: Vec<u16> = text.encode_utf16().collect();
    let device = CreateCompatibleDC(std::ptr::null_mut());
    if let Some(partner) = partner_for(font, &units) {
        let across = measured(device, font, partner, &units);
        let lines = break_lines(&units, &across, DT_WORDBREAK, width);
        let (rise, fall) = line_metrics(device, font, partner);
        DeleteDC(device);
        return lines.len() as i32 * (rise + fall);
    }
    let previous = SelectObject(device, font);
    let mut area = RECT { left: 0, top: 0, right: width, bottom: 0 };
    DrawTextW(device, units.as_mut_ptr(), units.len() as i32, &mut area, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
    SelectObject(device, previous);
    DeleteDC(device);
    area.bottom
}

// ---- Two faces for one text. Windows's own fallback from a Latin face goes by a list in its registry, on which Tahoma and Meiryo UI come first, and has no way to be told otherwise for
// GDI: so what is Japanese is drawn here, in runs, each in its own face on one baseline. Text with none in it goes to the system as ever.

/// The type of the interface: Windows 11's own, with its optical size for text.
pub const UI_FACE: &str = "Segoe UI Variable Text";
const UI_FACE_STRONG: &str = "Segoe UI Variable Text Semibold";
/// The same at a size for headings. (Windows's name for it is cut off at its 31 characters.)
const UI_FACE_LARGE: &str = "Segoe UI Variable Display Semib";
/// What is Japanese in that type, shipped beside the app in `fonts\\`: a RichEdit is told, and `paint` draws it so.
pub(crate) const BUNDLED_FACE: &str = "BIZ UDGothic";
/// Without them, as a build that was not packaged is: Windows's own.
pub(crate) const FALLBACK_FACE: &str = "Yu Gothic UI";
const FALLBACK_FACE_STRONG: &str = "Yu Gothic UI Semibold";
pub(crate) const BUNDLED_FILES: [&str; 2] = ["BIZUDGothic-Regular.ttf", "BIZUDGothic-Bold.ttf"];

/// Where the fonts are: `fonts\\` beside the program, and for a build that has not been packaged, what fetch_tools.py has fetched.
pub(crate) fn font_folders() -> Vec<std::path::PathBuf> {
    let beside = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|folder| folder.join("fonts")));
    #[cfg(debug_assertions)]
    let cache = std::env::var_os("DOGMIND_TOOLS_CACHE").map(std::path::PathBuf::from).or_else(|| std::env::var_os("LOCALAPPDATA").map(|local| std::path::Path::new(&local).join("DogmindBuild").join("tools-cache")));
    #[cfg(not(debug_assertions))]
    let cache = None;
    beside.into_iter().chain(cache).collect()
}

/// The fonts for this process alone, which installs nothing: its RichEdits are in it and see them, and a browser it starts does not.
/// Whichever face there is, once: everything that names one asks here, so the first asking comes before the first font.
pub fn japanese_face() -> &'static str {
    static FACE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FACE.get_or_init(|| unsafe {
        const FR_PRIVATE: u32 = 0x10;
        // (a test's way to see the fallback on a Windows that has its own copy)
        #[cfg(debug_assertions)]
        if std::env::var_os("DOGMIND_TEST_FALLBACK_FACE").is_some() {
            return FALLBACK_FACE;
        }
        if let Some(folder) = font_folders().into_iter().find(|folder| BUNDLED_FILES.iter().all(|file| folder.join(file).is_file())) {
            for file in BUNDLED_FILES {
                AddFontResourceExW(wide(&folder.join(file).display().to_string()).as_ptr(), FR_PRIVATE, std::ptr::null());
            }
        }
        // (or Windows's own copy, from its Japanese supplemental fonts: the same face)
        if face_is_here(BUNDLED_FACE) { BUNDLED_FACE } else { FALLBACK_FACE }
    })
}

/// The face and weight of what is Japanese and heavy. The bundled face has a bold of its own, and Yu Gothic UI a semibold by name.
fn japanese_strong() -> (&'static str, i32) {
    if japanese_face() == BUNDLED_FACE { (BUNDLED_FACE, 700) } else { (FALLBACK_FACE_STRONG, 600) }
}

/// Whether a character is one the Latin face has none of and Japanese is written in: kana and kanji and their punctuation. Digits, spaces and Latin stay in the Latin face.
pub fn is_japanese(c: char) -> bool {
    matches!(u32::from(c), 0x2E80..=0x2FDF | 0x3000..=0x30FF | 0x31C0..=0x31FF | 0x3200..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

fn unit_is_japanese(units: &[u16], at: usize) -> bool {
    match units[at] {
        // (each half of a pair, by the character they make)
        0xD800..=0xDBFF => char::decode_utf16(units[at..].iter().copied()).next().and_then(Result::ok).is_some_and(is_japanese),
        0xDC00..=0xDFFF => at > 0 && char::decode_utf16(units[at - 1..].iter().copied()).next().and_then(Result::ok).is_some_and(is_japanese),
        unit => char::from_u32(u32::from(unit)).is_some_and(is_japanese),
    }
}

/// The runs of `units`, as ends and whether each is Japanese.
fn runs(units: &[u16]) -> Vec<(std::ops::Range<usize>, bool)> {
    let mut found = Vec::new();
    let mut start = 0;
    for at in 1..=units.len() {
        if at == units.len() || unit_is_japanese(units, at) != unit_is_japanese(units, start) {
            if start < units.len() {
                found.push((start..at, unit_is_japanese(units, start)));
            }
            start = at;
        }
    }
    found
}

/// What cannot begin a line in Japanese.
fn stays_after(unit: u16) -> bool {
    matches!(unit, 0x3001 | 0x3002 | 0xFF0C | 0xFF0E | 0xFF1A | 0xFF1B | 0xFF01 | 0xFF1F | 0x30FB | 0x30FC | 0x3005 | 0x3009 | 0x300B | 0x300D | 0x300F | 0x3011 | 0x3015 | 0xFF09 | 0xFF3D | 0xFF5D | 0x3041 | 0x3043 | 0x3045 | 0x3047 | 0x3049 | 0x3063 | 0x3083 | 0x3085 | 0x3087 | 0x30A1 | 0x30A3 | 0x30A5 | 0x30A7 | 0x30A9 | 0x30C3 | 0x30E3 | 0x30E5 | 0x30E7)
}

/// What cannot end a line in Japanese.
fn stays_before(unit: u16) -> bool {
    matches!(unit, 0x3008 | 0x300A | 0x300C | 0x300E | 0x3010 | 0x3014 | 0xFF08 | 0xFF3B | 0xFF5B)
}

/// Where each line of `units` runs, by how far across each unit ends (`across[at]`: the width of everything before `at`).
fn break_lines(units: &[u16], across: &[i32], flags: u32, width: i32) -> Vec<std::ops::Range<usize>> {
    let wraps = flags & DT_WORDBREAK != 0 && flags & DT_SINGLELINE == 0;
    let mut lines = Vec::new();
    let (mut start, mut at, mut last_break) = (0, 0, None);
    while at < units.len() {
        let unit = units[at];
        if unit == u16::from(b'\n') && flags & DT_SINGLELINE == 0 {
            let end = if at > start && units[at - 1] == u16::from(b'\r') { at - 1 } else { at };
            lines.push(start..end);
            at += 1;
            start = at;
            last_break = None;
            continue;
        }
        if wraps && at > start && across[at + 1] - across[start] > width && !matches!(unit, 0x20 | 0x3000) {
            // Back to the last place a line may break, or here if a word is too long for any line.
            let end = last_break.unwrap_or(at);
            lines.push(start..end);
            start = end;
            while start < units.len() && matches!(units[start], 0x20) {
                start += 1;
            }
            at = start;
            last_break = None;
            continue;
        }
        // The next line may begin after this unit: after a space, or between Japanese characters, as the rules for each allow.
        if let Some(&next) = units.get(at + 1) {
            let japanese_here = unit_is_japanese(units, at) || unit_is_japanese(units, at + 1);
            if !(0xD800..=0xDBFF).contains(&unit) && (unit == 0x20 || (japanese_here && !stays_after(next) && !stays_before(unit))) {
                last_break = Some(at + 1);
            }
        }
        at += 1;
    }
    lines.push(start..units.len());
    lines
}

/// How far across each unit begins, and on the end the width of the whole.
unsafe fn measured(device: HDC, font: HFONT, partner: HFONT, units: &[u16]) -> Vec<i32> {
    let mut across = Vec::with_capacity(units.len() + 1);
    across.push(0);
    let previous = SelectObject(device, font);
    let mut base = 0;
    for (range, japanese) in runs(units) {
        SelectObject(device, if japanese { partner } else { font });
        let piece = &units[range];
        let mut ends = vec![0i32; piece.len()];
        let (mut fitted, mut size) = (0, SIZE { cx: 0, cy: 0 });
        GetTextExtentExPointW(device, piece.as_ptr(), piece.len() as i32, i32::MAX, &mut fitted, ends.as_mut_ptr(), &mut size);
        across.extend(ends.iter().map(|end| base + end));
        base += size.cx;
    }
    SelectObject(device, previous);
    across
}

/// How high above the line and how low under it the two faces go at the most: every line is as tall, so that a text does not jump where Japanese comes into it.
unsafe fn line_metrics(device: HDC, font: HFONT, partner: HFONT) -> (i32, i32) {
    let previous = SelectObject(device, font);
    let mut metrics: TEXTMETRICW = std::mem::zeroed();
    GetTextMetricsW(device, &mut metrics);
    let (mut rise, mut fall) = (metrics.tmAscent, metrics.tmDescent);
    SelectObject(device, partner);
    GetTextMetricsW(device, &mut metrics);
    rise = rise.max(metrics.tmAscent);
    fall = fall.max(metrics.tmDescent);
    SelectObject(device, previous);
    (rise, fall)
}

/// One text in two faces, as `DrawTextW` would lay it out with these flags: wrapping, centring, the ellipses.
unsafe fn draw_mixed(device: HDC, font: HFONT, partner: HFONT, mut units: Vec<u16>, area: RECT, flags: u32) {
    let width = area.right - area.left;
    let mut across = measured(device, font, partner, &units);
    if flags & DT_PATH_ELLIPSIS != 0 && *across.last().unwrap_or(&0) > width {
        // The name at the end stays, and what comes before it gives way from its end.
        let tail = units.iter().rposition(|&unit| unit == u16::from(b'\\')).unwrap_or(0);
        let dots: Vec<u16> = "...".encode_utf16().collect();
        let mut head = tail;
        loop {
            let tried: Vec<u16> = units[..head].iter().chain(&dots).chain(&units[tail..]).copied().collect();
            let wide = measured(device, font, partner, &tried);
            if head == 0 || *wide.last().unwrap_or(&0) <= width {
                units = tried;
                across = wide;
                break;
            }
            head -= 1;
        }
    }
    if flags & DT_END_ELLIPSIS != 0 && *across.last().unwrap_or(&0) > width && (flags & DT_SINGLELINE != 0 || flags & DT_WORDBREAK == 0) {
        let dots = width_on(device, font, "...");
        let mut end = units.len();
        while end > 0 && across[end] + dots > width {
            end -= 1;
        }
        while end > 0 && (0xD800..=0xDBFF).contains(&units[end - 1]) {
            end -= 1;
        }
        units.truncate(end);
        units.extend("...".encode_utf16());
        across = measured(device, font, partner, &units);
    }
    let lines = break_lines(&units, &across, flags, width);
    let (rise, fall) = line_metrics(device, font, partner);
    let tall = rise + fall;
    let top = if flags & DT_SINGLELINE != 0 && flags & DT_VCENTER != 0 {
        area.top + (area.bottom - area.top - tall) / 2
    } else if flags & DT_SINGLELINE != 0 && flags & DT_BOTTOM != 0 {
        area.bottom - tall
    } else {
        area.top
    };
    let was = SetTextAlign(device, TA_BASELINE | TA_LEFT);
    let previous = SelectObject(device, font);
    for (at, line) in lines.iter().enumerate() {
        let y = top + at as i32 * tall;
        if y >= area.bottom {
            break;
        }
        let mut shown = line.clone();
        // (what is left over at the end of a line takes no room)
        while shown.end > shown.start && units[shown.end - 1] == 0x20 && flags & DT_WORDBREAK != 0 {
            shown.end -= 1;
        }
        let this_wide = across[shown.end] - across[shown.start];
        let x = if flags & DT_CENTER != 0 {
            area.left + (width - this_wide) / 2
        } else if flags & DT_RIGHT != 0 {
            area.right - this_wide
        } else {
            area.left
        };
        let clip = RECT { left: area.left, top: y.max(area.top), right: area.right, bottom: (y + tall).min(area.bottom) };
        for (range, japanese) in runs(&units[shown.clone()]) {
            let (start, end) = (shown.start + range.start, shown.start + range.end);
            SelectObject(device, if japanese { partner } else { font });
            ExtTextOutW(device, x + across[start] - across[shown.start], y + rise, ETO_CLIPPED, &clip, units[start..end].as_ptr(), (end - start) as u32, std::ptr::null());
        }
    }
    SelectObject(device, previous);
    SetTextAlign(device, was);
}

/// Each Latin face's Japanese one, from the fonts made in `Fonts::new`.
static PARTNERS: std::sync::Mutex<Vec<(isize, isize)>> = std::sync::Mutex::new(Vec::new());

/// The face that takes what is Japanese in this text, when there is any and this font has one.
fn partner_for(font: HFONT, units: &[u16]) -> Option<HFONT> {
    if !(0..units.len()).any(|at| unit_is_japanese(units, at)) {
        return None;
    }
    PARTNERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().find(|(latin, _)| *latin == font as isize).map(|(_, japanese)| *japanese as HFONT)
}

/// Whether Windows, or this process, has a family of this name. GDI is asked to list it: what a font made with the name says it is
/// will not do, as Japanese Windows calls some faces by their Japanese names (BIZ UDGothic is BIZ UDゴシック), and that is no match.
unsafe fn face_is_here(face: &str) -> bool {
    unsafe extern "system" fn found(_font: *const LOGFONTW, _metrics: *const TEXTMETRICW, _kind: u32, seen: LPARAM) -> i32 {
        *(seen as *mut bool) = true;
        0
    }
    let mut wanted: LOGFONTW = std::mem::zeroed();
    wanted.lfCharSet = DEFAULT_CHARSET as u8;
    for (slot, unit) in wanted.lfFaceName.iter_mut().take(31).zip(face.encode_utf16()) {
        *slot = unit;
    }
    let device = GetDC(std::ptr::null_mut());
    let mut seen = false;
    EnumFontFamiliesExW(device, &wanted, Some(found), std::ptr::addr_of_mut!(seen) as LPARAM, 0);
    ReleaseDC(std::ptr::null_mut(), device);
    seen
}
/// The artwork in the executable, at this size; null where it was built without it.
unsafe fn resource_icon(size: i32) -> HICON {
    LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as *const u16, IMAGE_ICON, size, size, LR_DEFAULTCOLOR) as HICON
}

/// The dog, line drawn, and the name in its own lettering: how much ink each pixel holds, made by `scripts/make_logo.py`.
const LOGO: &[u8] = include_bytes!("../../../assets/logo.a8");
const TITLE: &[u8] = include_bytes!("../../../assets/title.a8");

/// Width and height of one of those.
fn ink_size(ink: &[u8]) -> (i32, i32) {
    (i32::from(u16::from_le_bytes([ink[0], ink[1]])), i32::from(u16::from_le_bytes([ink[2], ink[3]])))
}

/// How high the title stands at this width.
pub fn title_height(width: i32) -> i32 {
    let (full_width, full_height) = ink_size(TITLE);
    mul_div(width, full_height, full_width)
}

/// One of those drawings at this place and size, in the theme's text colour.
unsafe fn draw_ink(device: HDC, source: &[u8], x: i32, y: i32, shown_width: i32, shown_height: i32) {
    let (width, height) = ink_size(source);
    if crate::render::ink(device, &source[4..], width as u32, height as u32, theme::text(), rect(x, y, shown_width, shown_height)) {
        return;
    }
    let ink = (argb(theme::text()) & 0x00FF_FFFF).to_le_bytes();
    let pixels: Vec<u8> = source[4..].iter().flat_map(|&alpha| [ink[0], ink[1], ink[2], alpha]).collect();
    let mut graphics = std::ptr::null_mut();
    if GdipCreateFromHDC(device, &mut graphics) != 0 {
        return;
    }
    GdipSetInterpolationMode(graphics, 7); // InterpolationModeHighQualityBicubic
    GdipSetPixelOffsetMode(graphics, 4); // PixelOffsetModeHalf
    let mut image = std::ptr::null_mut();
    if GdipCreateBitmapFromScan0(width, height, width * 4, 0x0026_200A, pixels.as_ptr(), &mut image) == 0 { // PixelFormat32bppARGB
        GdipDrawImageRectI(graphics, image.cast(), x, y, shown_width, shown_height);
        GdipDisposeImage(image.cast());
    }
    GdipDeleteGraphics(graphics);
}

/// The dog in the square given: it is not the icon, which is a card of its own and looks pasted on.
pub unsafe fn logo(device: HDC, x: i32, y: i32, size: i32) {
    let (width, height) = ink_size(LOGO);
    let (shown_width, shown_height) = if width >= height { (size, mul_div(size, height, width)) } else { (mul_div(size, width, height), size) };
    draw_ink(device, LOGO, x + (size - shown_width) / 2, y + (size - shown_height) / 2, shown_width, shown_height);
}

/// The name, this wide.
pub unsafe fn title_mark(device: HDC, x: i32, y: i32, width: i32) {
    draw_ink(device, TITLE, x, y, width, title_height(width));
}

/// The icon from the executable's resources, or drawn from the same paw where it was built without them.
pub unsafe fn window_icon(size: i32) -> HICON {
    let loaded = resource_icon(size);
    if !loaded.is_null() {
        return loaded;
    }
    let screen = GetDC(std::ptr::null_mut());
    let device = CreateCompatibleDC(screen);
    let color = CreateCompatibleBitmap(screen, size, size);
    let previous = SelectObject(device, color);
    let brush = CreateSolidBrush(theme::sidebar());
    let all = rect(0, 0, size, size);
    FillRect(device, &all, brush);
    DeleteObject(brush);
    rounded(device, all, size / 4, Some(theme::accent()), None);
    icon::draw(device, "paw-print", all, size * 3 / 4, theme::on_accent());
    SelectObject(device, previous);
    let mask = CreateBitmap(size, size, 1, 1, std::ptr::null());
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info);
    DeleteObject(mask);
    DeleteObject(color);
    DeleteDC(device);
    ReleaseDC(std::ptr::null_mut(), screen);
    icon
}

#[derive(Clone, Copy)]
pub struct Fonts {
    pub body: HFONT,
    pub small: HFONT,
    pub strong: HFONT,
    pub title: HFONT,
    /// The name of what a dialog shows.
    pub heading: HFONT,
    /// What the dog says on the first screen.
    pub greeting: HFONT,
    /// Lines of code, where they are drawn here and not by the control.
    pub code: HFONT,
    /// What a plain edit control's words are in.
    pub field: HFONT,
    pub dpi: u32,
}

impl Fonts {
    pub unsafe fn new(dpi: u32) -> Fonts {
        let make = |face: &str, points: i32, weight: i32| {
            let face = wide(face);
            CreateFontW(-mul_div(points, dpi as i32, 72), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr())
        };
        // In two faces; `Segoe UI` in place of the variable one on a Windows that has been kept without it.
        let pair = |face: &str, (japanese, japanese_weight): (&str, i32), points: i32, weight: i32| {
            let mut latin = make(face, points, weight);
            let here = face_is_here(face);
            if !here {
                DeleteObject(latin);
                latin = make(if weight > 400 { "Segoe UI Semibold" } else { "Segoe UI" }, points, weight);
            }
            let partner = make(japanese, points, japanese_weight);
            PARTNERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((latin as isize, partner as isize));
            // (as DirectWrite names it: one family and its weights, where GDI has a name for each weight)
            let family = if !here { "Segoe UI" } else if face == UI_FACE_LARGE { "Segoe UI Variable Display" } else { UI_FACE };
            crate::render::register(latin, crate::render::Face { family: family.to_string(), weight: weight as u32, size: (points * dpi as i32) as f32 / 72.0 });
            latin
        };
        // (for a plain edit control, which has one font: the face its words are mostly in)
        let field = make(if crate::lang::is_japanese() { japanese_face() } else { UI_FACE }, design::points::BODY, 400);
        let (plain, strong) = ((japanese_face(), 400), japanese_strong());
        Fonts {
            body: pair(UI_FACE, plain, design::points::BODY, 400),
            small: pair(UI_FACE, plain, design::points::SMALL, 400),
            strong: pair(UI_FACE_STRONG, strong, design::points::BODY, 600),
            title: pair(UI_FACE_LARGE, strong, design::points::TITLE, 600),
            heading: pair(UI_FACE_STRONG, strong, design::points::HEADING, 600),
            greeting: pair(UI_FACE_LARGE, strong, design::points::GREETING, 600),
            code: {
                let code = make("Consolas", design::points::CODE, 400);
                crate::render::register(code, crate::render::Face { family: "Consolas".into(), weight: 400, size: (design::points::CODE * dpi as i32) as f32 / 72.0 });
                code
            },
            field,
            dpi,
        }
    }
}

pub unsafe fn draw_text(device: HDC, text: &str, area: RECT, font: HFONT, color: u32, flags: u32) {
    if text.is_empty() {
        return; // DrawTextW reads even zero characters
    }
    // Through DirectWrite, with its colour emoji; GDI where it cannot start.
    if crate::render::text(device, text, area, font, color, flags) {
        return;
    }
    let mut units: Vec<u16> = text.encode_utf16().collect();
    let mut target = area;
    SetBkMode(device, TRANSPARENT as _);
    SetTextColor(device, color);
    if let Some(partner) = partner_for(font, &units) {
        draw_mixed(device, font, partner, units, area, flags);
        return;
    }
    let previous = SelectObject(device, font);
    DrawTextW(device, units.as_mut_ptr(), units.len() as i32, &mut target, flags | DT_NOPREFIX);
    SelectObject(device, previous);
}

pub const ONE_LINE: u32 = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS;

// ---- Buttons.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonKind {
    /// The one thing to do here: filled with the accent.
    Primary = 0,
    /// Outlined.
    Secondary = 1,
    /// Only text, until the pointer is on it.
    Ghost = 2,
    /// A small round-ended pill inside a card, with a chevron.
    Chip = 3,
    /// A round accent button holding an icon.
    Round = 4,
    /// One of a few choices side by side, on a frame its parent draws; the one chosen stands out.
    Segment = 5,
    SegmentOn = 6,
    /// Only an icon, named by its text, until the pointer is on it. Quiet: for what sits among other things.
    Icon = 7,
    /// A picture waiting to be sent: its small copy, with a cross on it for the pointer and the Delete key.
    Attached = 8,
    /// A row of a dialog's list of places: an icon and a name.
    Nav = 10,
    NavOn = 11,
    /// A switch.
    Toggle = 12,
    ToggleOn = 13,
    /// A value, with a chevron for what else it can be.
    Select = 14,
    /// Small quiet words that lead somewhere, underlined under the pointer.
    Link = 15,
    /// What cannot be undone.
    Danger = 17,
    /// One of the choices a card puts to the person: its number, its label, what it means, and a mark on the one the model suggests (`choice_text`).
    Choice = 18,
    /// A file of the card of an answer's changes: its name, its lines added and removed and a small bar of them (`file_row_text`).
    FileRow = 19,
    /// The frame a field of a card stands in; the field itself is a control of its own over it.
    Frame = 20,
    /// A control of no size and nothing drawn: what Tab reaches and a screen reader names for something that is drawn elsewhere (the icons in the transcript).
    Hidden = 21,
    /// A line of small quiet words on a card, which nothing is done with: what a helper has just read.
    Note = 22,
    /// A line of the card's own words, in the colour of text: the task of a helper.
    Task = 23,
    /// A line with its words at the left and its figures at the right (the text's two parts are told apart by `PART`): where a helper is, and how long it has taken.
    Status = 24,
}

/// The parts of the text of a button that has more than one are told apart by this.
pub const PART: char = '\u{1F}';

/// The text of a `Choice` button.
pub fn choice_text(number: usize, suggested: &str, label: &str, meaning: &str) -> String {
    format!("{number}{PART}{suggested}{PART}{label}{PART}{meaning}")
}

/// The text of a `FileRow` button: lines added and removed, the parts of its bar that are green and red, and the file's name.
pub fn file_row_text(added: usize, removed: usize, green: usize, red: usize, path: &str) -> String {
    format!("{added}{PART}{removed}{PART}{green}{PART}{red}{PART}{path}")
}

/// What a picture's button shows.
#[derive(Clone)]
pub enum Thumb {
    /// Still being read: it is only a shape, so that the page does not move when it comes.
    Waiting,
    /// A small copy, which whoever made it keeps and frees.
    Ready { bitmap: HBITMAP, width: i32, height: i32 },
    /// In place of a picture: why there is none.
    Missing(String),
}

thread_local! {
    static THUMBS: std::cell::RefCell<std::collections::HashMap<isize, Thumb>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

pub unsafe fn set_thumb(window: HWND, thumb: Thumb) {
    THUMBS.with(|thumbs| thumbs.borrow_mut().insert(window as isize, thumb));
    InvalidateRect(window, std::ptr::null(), 0);
}

/// `WM_COMMAND`'s notification when the cross on a picture was used, in place of the click that opens it.
pub const THUMB_REMOVE: usize = 1;

/// Whether a point in an attached picture is on its cross.
fn on_the_cross(client: RECT, x: i32, y: i32, dpi: u32) -> bool {
    x >= client.right - scale(dpi, 26) && y < scale(dpi, 26)
}

/// A small copy, in rounded corners, its alpha over what is there.
unsafe fn draw_thumb(device: HDC, bitmap: HBITMAP, x: i32, y: i32, width: i32, height: i32, radius: i32) {
    let source = CreateCompatibleDC(device);
    let previous = SelectObject(source, bitmap);
    let round = CreateRoundRectRgn(x, y, x + width + 1, y + height + 1, radius * 2, radius * 2);
    SelectClipRgn(device, round);
    let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: 1 }; // AC_SRC_OVER, AC_SRC_ALPHA
    GdiAlphaBlend(device, x, y, width, height, source, 0, 0, width, height, blend);
    SelectClipRgn(device, std::ptr::null_mut());
    DeleteObject(round);
    SelectObject(source, previous);
    DeleteDC(source);
}

/// A picture's tile in `area` of `device`: its small copy, or the shape that holds its place while it is read, or why there is none. `attached`: a picture waiting to go, in the middle of a
/// square of its own; else one in a message, from the top left of its place, as the text under it is. Shared by the button of an attached picture and by the transcript's own paint,
/// which draws the pictures of its messages itself.
pub unsafe fn draw_tile(device: HDC, area: RECT, thumb: &Thumb, attached: bool, fonts: &Fonts) {
    let s = |value: i32| scale(fonts.dpi, value);
    let radius = s(radius::SMALL);
    match thumb {
        Thumb::Ready { bitmap, width, height } => {
            let (x, y) = if attached { (area.left + (area.right - area.left - width) / 2, area.top + (area.bottom - area.top - height) / 2) } else { (area.left, area.top) };
            if attached {
                rounded(device, area, radius, Some(theme::raised()), None);
            }
            draw_thumb(device, *bitmap, x, y, *width, *height, radius);
        }
        Thumb::Waiting => rounded(device, area, radius, Some(theme::raised()), None),
        Thumb::Missing(words) => {
            rounded(device, area, radius, Some(theme::raised()), Some(theme::line()));
            let across = area.right - area.left - s(design::space::M);
            let tall = text_height(fonts.small, words, across).min(area.bottom - area.top);
            let inside = RECT { left: area.left + s(6), top: area.top + (area.bottom - area.top - tall) / 2, right: area.right - s(6), bottom: area.top + (area.bottom - area.top + tall) / 2 + 1 };
            draw_text(device, words, inside, fonts.small, theme::muted(), DT_CENTER | DT_WORDBREAK | DT_END_ELLIPSIS);
        }
    }
}

const WM_MOUSELEAVE: u32 = 0x02A3;
const FLAG_HOVER: isize = 1 << 8;
const FLAG_PRESSED: isize = 1 << 9;
const BACKDROP_SHIFT: isize = 16;

static BUTTON_FONTS: std::sync::Mutex<Option<SendFonts>> = std::sync::Mutex::new(None);

/// Whether the keys are what is being used: only then does a control show that it has the focus. A dialog opened with the pointer has no ring in it.
static KEYS_IN_USE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A test's way to look at what the pointer does, which a window off the screen never sees: every button drawn as under it. Not in what ships.
pub fn forced_hover() -> bool {
    #[cfg(debug_assertions)]
    {
        static FORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FORCED.get_or_init(|| std::env::var_os("DOGMIND_TEST_HOVER").is_some())
    }
    #[cfg(not(debug_assertions))]
    false
}

/// Whether the keys are what is being used.
pub fn keys_are_in_use() -> bool {
    KEYS_IN_USE.load(std::sync::atomic::Ordering::Relaxed)
}

/// From a key that moves the focus, or a press of the pointer. What has the focus now shows it or stops showing it.
pub unsafe fn keys_in_use(used: bool) {
    if KEYS_IN_USE.swap(used, std::sync::atomic::Ordering::Relaxed) != used {
        let focus = GetFocus();
        if !focus.is_null() {
            InvalidateRect(focus, std::ptr::null(), 0);
        }
    }
}

struct SendFonts(Fonts);

// GDI font handles are process-wide.
unsafe impl Send for SendFonts {}

pub fn set_button_fonts(fonts: Fonts) {
    *BUTTON_FONTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(SendFonts(fonts));
}

unsafe extern "system" fn button_procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state = GetWindowLongPtrW(window, GWLP_USERDATA);
    match message {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let target = BeginPaint(window, &mut paint);
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(window, &mut client);
            let device = CreateCompatibleDC(target);
            let bitmap = CreateCompatibleBitmap(target, client.right, client.bottom);
            let previous = SelectObject(device, bitmap);
            draw_button(window, device, client, state);
            BitBlt(target, 0, 0, client.right, client.bottom, device, 0, 0, SRCCOPY);
            SelectObject(device, previous);
            DeleteObject(bitmap);
            DeleteDC(device);
            EndPaint(window, &paint);
            0
        }
        WM_MOUSEMOVE => {
            if state & FLAG_HOVER == 0 {
                SetWindowLongPtrW(window, GWLP_USERDATA, state | FLAG_HOVER);
                let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: window, dwHoverTime: 0 };
                TrackMouseEvent(&mut track);
                InvalidateRect(window, std::ptr::null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            SetWindowLongPtrW(window, GWLP_USERDATA, state & !FLAG_HOVER & !FLAG_PRESSED);
            InvalidateRect(window, std::ptr::null(), 0);
            0
        }
        // Hidden under the pointer, it gets no leaving: shown again, it would still look as if the pointer were on it.
        WM_SHOWWINDOW if wparam == 0 => {
            SetWindowLongPtrW(window, GWLP_USERDATA, state & !FLAG_HOVER & !FLAG_PRESSED);
            0
        }
        WM_LBUTTONDOWN => {
            SetWindowLongPtrW(window, GWLP_USERDATA, state | FLAG_PRESSED);
            InvalidateRect(window, std::ptr::null(), 0);
            0
        }
        WM_LBUTTONUP => {
            SetWindowLongPtrW(window, GWLP_USERDATA, state & !FLAG_PRESSED);
            InvalidateRect(window, std::ptr::null(), 0);
            if state & FLAG_PRESSED != 0 {
                let id = GetDlgCtrlID(window) as usize;
                let mut client: RECT = std::mem::zeroed();
                GetClientRect(window, &mut client);
                let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
                let crossed = state & 0xFF == ButtonKind::Attached as isize && on_the_cross(client, x, y, GetDpiForWindow(window));
                SendMessageW(GetParent(window), WM_COMMAND, id | if crossed { THUMB_REMOVE << 16 } else { 0 }, window as LPARAM);
            }
            0
        }
        // Reached with Tab: it shows, and Enter or Space presses it.
        WM_SETFOCUS | WM_KILLFOCUS => {
            InvalidateRect(window, std::ptr::null(), 0);
            // (what it stands for is drawn elsewhere, and shows the focus there)
            if state & 0xFF == ButtonKind::Hidden as isize {
                PostMessageW(GetParent(window), crate::ui::WM_PROXY_FOCUS, 0, 0);
            }
            0
        }
        WM_KEYDOWN if matches!(wparam as u16, VK_RETURN | VK_SPACE) && IsWindowEnabled(window) != 0 => {
            let id = GetDlgCtrlID(window) as usize;
            SendMessageW(GetParent(window), WM_COMMAND, id, window as LPARAM);
            0
        }
        WM_KEYDOWN if matches!(wparam as u16, VK_DELETE | VK_BACK) && state & 0xFF == ButtonKind::Attached as isize => {
            let id = GetDlgCtrlID(window) as usize;
            SendMessageW(GetParent(window), WM_COMMAND, id | (THUMB_REMOVE << 16), window as LPARAM);
            0
        }
        WM_DESTROY => {
            THUMBS.with(|thumbs| thumbs.borrow_mut().remove(&(window as isize)));
            0
        }
        WM_ENABLE | WM_SETTEXT => {
            let result = DefWindowProcW(window, message, wparam, lparam);
            InvalidateRect(window, std::ptr::null(), 0);
            result
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// A button's text is its label, or `icon|label` for an icon before it.
unsafe fn draw_button(window: HWND, device: HDC, client: RECT, state: isize) {
    let mut buffer = [0u16; 640];
    let length = GetWindowTextW(window, buffer.as_mut_ptr(), 640);
    let text = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
    let (hover, pressed) = (state & FLAG_HOVER != 0 || forced_hover(), state & FLAG_PRESSED != 0);
    draw_button_face(window, device, client, state & 0xFF, &text, (state >> BACKDROP_SHIFT) as u32, hover, pressed, IsWindowEnabled(window) != 0, GetFocus() == window);
}

/// A button that is no window, drawn where `area` is in `device`: in its own bitmap and put on at once, as a window's own paint is, for what a paint of another window's has to draw in
/// its pixels (the controls of the cards over the transcript). `lit`: the pointer is on it; `ring`: the keys are on it.
pub unsafe fn paint_button(device: HDC, area: RECT, kind: ButtonKind, text: &str, backdrop: u32, lit: bool, pressed: bool, ring: bool) {
    let (width, height) = (area.right - area.left, area.bottom - area.top);
    if width <= 0 || height <= 0 {
        return;
    }
    let memory = CreateCompatibleDC(device);
    let bitmap = CreateCompatibleBitmap(device, width, height);
    let old = SelectObject(memory, bitmap);
    draw_button_face(std::ptr::null_mut(), memory, RECT { left: 0, top: 0, right: width, bottom: height }, kind as isize, text, backdrop, lit, pressed, true, ring);
    BitBlt(device, area.left, area.top, width, height, memory, 0, 0, SRCCOPY);
    SelectObject(memory, old);
    DeleteObject(bitmap);
    DeleteDC(memory);
}

/// What a button looks like: its `kind` and words (`text`), on `backdrop`, with the pointer on it (`hover`), pressed, enabled, and with the keys on it (`focused`).
unsafe fn draw_button_face(window: HWND, device: HDC, client: RECT, kind: isize, text: &str, backdrop: u32, hover: bool, pressed: bool, enabled: bool, focused: bool) {
    let brush = CreateSolidBrush(backdrop);
    FillRect(device, &client, brush);
    DeleteObject(brush);
    // (a text of several parts has its own marks, and is not an icon before a label)
    let (glyph, label) = if text.contains(PART) { (None, text) } else { text.split_once('|').map_or((None, text), |(glyph, label)| (Some(glyph), label)) };
    let height = client.bottom;
    let Some(fonts) = BUTTON_FONTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().map(|fonts| fonts.0) else { return };
    let s = |value: i32| scale(fonts.dpi, value);
    let center = DT_CENTER | ONE_LINE;
    let lit = hover || pressed;
    // A label with an icon before it, the two centred together.
    let centered = |device: HDC, font: HFONT, color: u32| {
        match glyph {
            Some(name) => {
                let size = s(design::space::L);
                let gap = s(design::space::S);
                let total = size + gap + width_on(device, font, label);
                let left = (client.right - total) / 2;
                icon::draw(device, name, rect(left, 0, size, height), size, color);
                draw_text(device, label, RECT { left: left + size + gap, right: client.right, ..client }, font, color, ONE_LINE);
            }
            None => draw_text(device, label, client, font, color, center),
        }
    };
    match kind {
        0 => {
            let fill = if !enabled { theme::raised() } else if lit { theme::accent_hover() } else { theme::accent() };
            rounded(device, client, s(radius::CONTROL), Some(fill), None);
            centered(device, fonts.strong, if enabled { theme::on_accent() } else { theme::muted() });
        }
        1 => {
            // (on a card of the conversation, a well a shade off it, as the choices of a question are)
            rounded(device, client, s(radius::CONTROL), if lit { Some(theme::raised()) } else { (backdrop == theme::code_back()).then(theme::surface) }, Some(theme::line()));
            centered(device, fonts.body, if enabled { theme::text() } else { theme::faint() });
        }
        2 => {
            if lit {
                rounded(device, client, s(radius::CONTROL), Some(theme::surface()), None);
            }
            let color = if lit { theme::text() } else { theme::muted() };
            let mut area = client;
            area.left += s(size::GHOST_INSET);
            if let Some(name) = glyph {
                icon::draw(device, name, rect(area.left, 0, s(design::size::ICON), height), s(design::space::L), color);
                area.left += s(design::space::XL);
            }
            draw_text(device, label, area, fonts.body, color, ONE_LINE);
        }
        3 => {
            if lit {
                rounded(device, client, s(radius::CONTROL), Some(theme::raised()), None);
            }
            let mut area = client;
            area.left += s(design::space::M);
            area.right -= s(size::CHIP_CHEVRON);
            let color = if lit { theme::text() } else { theme::muted() };
            if let Some(name) = glyph {
                icon::draw(device, name, rect(area.left, 0, s(design::size::ICON), height), s(design::size::ICON_SMALL), color);
                area.left += s(21);
            }
            draw_text(device, label, area, fonts.small, color, ONE_LINE);
            // That it is a menu shows at rest, small; the pointer on it brings it up to full size.
            let (chevron, color) = if lit { (design::size::ICON_SMALL, theme::muted()) } else { (design::size::ICON_TINY, theme::faint()) };
            icon::draw(device, "chevron-down", rect(client.right - s(design::space::XL), 0, s(design::size::ICON_SMALL), height), s(chevron), color);
        }
        4 => {
            let fill = if !enabled { theme::raised() } else if lit { theme::accent_hover() } else { theme::accent() };
            rounded(device, client, height / 2, Some(fill), None);
            icon::draw(device, label, client, s(size::ICON_LARGE), if enabled { theme::on_accent() } else { theme::muted() });
        }
        7 => {
            if lit {
                // (on a card, what is a shade up from the card)
                rounded(device, client, s(radius::SMALL), Some(if pressed || backdrop == theme::surface() { theme::raised() } else { theme::surface() }), None);
            }
            let color = if label == "check" { theme::good() } else if lit { theme::text() } else { theme::faint() };
            icon::draw(device, label, client, s(design::size::ICON), color);
        }
        8 => {
            let thumb = THUMBS.with(|thumbs| thumbs.borrow().get(&(window as isize)).cloned()).unwrap_or(Thumb::Waiting);
            draw_tile(device, client, &thumb, true, &fonts);
            if lit || focused {
                let cross = RECT { left: client.right - s(22), top: s(3), right: client.right - s(3), bottom: s(22) };
                rounded(device, cross, s(design::radius::CONTROL), Some(theme::surface()), Some(theme::line()));
                icon::draw(device, "x", cross, s(design::size::ICON_SMALL), theme::text());
            }
        }
        10 | 11 => {
            if kind == 11 || lit {
                rounded(device, client, s(radius::CONTROL), Some(if kind == 11 { theme::raised() } else { theme::surface() }), None);
            }
            let color = if kind == 11 || lit { theme::text() } else { theme::soft() };
            let mut area = client;
            area.left += s(design::space::M);
            if let Some(name) = glyph {
                icon::draw(device, name, rect(area.left, 0, s(size::ICON), height), s(size::ICON), color);
                area.left += s(size::ICON + design::space::S);
            }
            draw_text(device, label, area, fonts.body, color, ONE_LINE);
        }
        12 | 13 => {
            let on = kind == 13;
            let fill = if !enabled { theme::raised() } else if on { if lit { theme::accent_hover() } else { theme::accent() } } else if lit { theme::line() } else { theme::raised() };
            rounded(device, client, height / 2, Some(fill), on.then_some(fill).or(Some(theme::line())));
            let knob = height - 2 * s(size::KNOB_INSET);
            let x = if on { client.right - knob - s(size::KNOB_INSET) } else { s(size::KNOB_INSET) };
            rounded(device, rect(x, s(size::KNOB_INSET), knob, knob), knob / 2, Some(if on { theme::on_accent() } else { theme::soft() }), None);
        }
        14 => {
            rounded(device, client, s(radius::CONTROL), Some(if lit { theme::raised() } else { theme::surface() }), Some(theme::line()));
            let mut area = client;
            area.left += s(design::space::M);
            area.right -= s(size::SELECT_CHEVRON);
            draw_text(device, label, area, fonts.body, if enabled { theme::text() } else { theme::faint() }, ONE_LINE);
            icon::draw(device, "chevron-down", rect(client.right - s(26), 0, s(design::size::ICON_SMALL), height), s(size::ICON_SMALL), theme::muted());
        }
        17 => {
            rounded(device, client, s(radius::CONTROL), Some(if lit { theme::bad_hover() } else { theme::bad() }), None);
            centered(device, fonts.strong, theme::on_accent());
        }
        15 => {
            let color = if lit { theme::text() } else { theme::muted() };
            draw_text(device, label, client, fonts.small, color, center);
            if lit {
                let across = text_width(fonts.small, label);
                let brush = CreateSolidBrush(color);
                let line = rect((client.right - across) / 2, height / 2 + s(9), across, 1.max(s(1)));
                FillRect(device, &line, brush);
                DeleteObject(brush);
            }
        }
        22 => draw_text(device, label, client, fonts.small, theme::muted(), ONE_LINE),
        23 => draw_text(device, label, client, fonts.body, theme::text(), ONE_LINE),
        24 => {
            let mut parts = label.split(PART);
            let (words, figures) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            // (the figures keep their room whole, and the words give way to them)
            let across = text_width(fonts.small, figures);
            draw_text(device, figures, RECT { left: (client.right - across).max(client.left), ..client }, fonts.small, theme::muted(), DT_RIGHT | ONE_LINE);
            draw_text(device, words, RECT { right: (client.right - across - s(design::space::M)).max(client.left), ..client }, fonts.strong, theme::text(), ONE_LINE);
        }
        18 => {
            let mut parts = label.split(PART);
            let (number, suggested, name, meaning) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            // On a card, a well a shade off it; the pointer on it, lit in the accent's frame.
            rounded(device, client, s(radius::CONTROL), Some(if lit { theme::raised() } else { theme::surface() }), Some(if lit { theme::accent() } else { theme::line() }));
            let mut area = client;
            area.left += s(design::space::M);
            area.right -= s(design::space::M);
            // (the number is what picks it from the composer too)
            let badge = s(design::space::L);
            draw_text(device, number, RECT { left: area.left, right: area.left + badge, ..client }, fonts.small, theme::faint(), DT_CENTER | ONE_LINE);
            area.left += badge + s(design::space::S);
            if !suggested.is_empty() {
                let across = text_width(fonts.small, suggested) + 2 * s(design::space::S);
                let pill = rect(area.right - across, (height - s(20)) / 2, across, s(20));
                rounded(device, pill, s(10), None, Some(theme::accent()));
                draw_text(device, suggested, pill, fonts.small, theme::accent(), DT_CENTER | ONE_LINE);
                area.right -= across + s(design::space::S);
            }
            if meaning.is_empty() {
                draw_text(device, name, RECT { left: area.left, right: area.right, top: 0, bottom: height }, fonts.strong, theme::text(), ONE_LINE);
            } else {
                draw_text(device, name, RECT { left: area.left, right: area.right, top: s(7), bottom: height / 2 + s(2) }, fonts.strong, theme::text(), ONE_LINE);
                draw_text(device, meaning, RECT { left: area.left, right: area.right, top: height / 2, bottom: height - s(7) }, fonts.small, theme::muted(), ONE_LINE);
            }
        }
        19 => {
            let mut parts = label.split(PART);
            let mut number = || parts.next().and_then(|part| part.parse::<usize>().ok()).unwrap_or(0);
            let (added, removed, green, red) = (number(), number(), number(), number());
            let path = parts.next().unwrap_or("");
            if lit {
                rounded(device, client, s(radius::SMALL), Some(theme::raised()), None);
            }
            let right = client.right - s(design::space::M);
            let mut left_of_numbers = right;
            // The bar: all its parts, the lines added green, the lines removed red, and what is left of it the card's line.
            if added + removed > 0 {
                let (cell, gap, tall) = (s(5), s(1), s(10));
                let across = crate::cards::BAR_PARTS as i32 * (cell + gap) - gap;
                let top = (height - tall) / 2;
                for part in 0..crate::cards::BAR_PARTS {
                    let color = if part < green { theme::good() } else if part < green + red { theme::bad() } else { theme::line() };
                    rounded(device, rect(right - across + part as i32 * (cell + gap), top, cell, tall), s(1), Some(color), None);
                }
                left_of_numbers = right - across - s(design::space::M);
                let (minus, plus) = (format!("\u{2212}{removed}"), format!("+{added}"));
                let (minus_width, plus_width) = (text_width(fonts.small, &minus), text_width(fonts.small, &plus));
                draw_text(device, &minus, RECT { left: left_of_numbers - minus_width, right: left_of_numbers, top: 0, bottom: height }, fonts.small, theme::bad(), DT_RIGHT | ONE_LINE);
                draw_text(device, &plus, RECT { left: left_of_numbers - minus_width - s(design::space::S) - plus_width, right: left_of_numbers - minus_width - s(design::space::S), top: 0, bottom: height }, fonts.small, theme::good(), DT_RIGHT | ONE_LINE);
                left_of_numbers -= minus_width + plus_width + s(design::space::S) + s(design::space::M);
            }
            draw_text(device, path, RECT { left: client.left + s(design::space::M), right: left_of_numbers, top: 0, bottom: height }, fonts.body, if lit { theme::text() } else { theme::soft() }, (ONE_LINE & !DT_END_ELLIPSIS) | DT_PATH_ELLIPSIS);
        }
        21 => {}
        20 => {
            // The field of a card, which is the composer: a frame with the words that say where to write; lit when the pointer is on it or the composer has the focus.
            rounded(device, client, s(radius::CONTROL), Some(theme::surface()), Some(if lit { theme::accent() } else { theme::line() }));
            let mut area = client;
            area.left += s(design::space::M);
            area.right -= s(design::space::M);
            draw_text(device, label, area, fonts.body, theme::faint(), ONE_LINE);
        }
        selected => {
            if selected == 6 || lit {
                rounded(device, client, s(radius::SMALL), Some(if selected == 6 { theme::raised() } else { theme::surface() }), None);
            }
            draw_text(device, label, client, fonts.body, if selected == 6 { theme::text() } else { theme::muted() }, center);
        }
    }
    if focused && KEYS_IN_USE.load(std::sync::atomic::Ordering::Relaxed) {
        let ring = RECT { left: 1, top: 1, right: client.right - 1, bottom: client.bottom - 1 };
        let corner = match kind {
            4 | 12 | 13 => (height - 2) / 2,
            7..=9 => s(radius::SMALL),
            _ => s(radius::CONTROL),
        };
        // (the one colour that says "here": on what is that colour already, the text's)
        rounded(device, ring, corner, None, Some(if kind == 0 || kind == 4 || kind == 13 || kind == 17 { theme::text() } else { theme::accent() }));
    }
}

pub unsafe fn register_button_class() {
    let name = wide("DogButton");
    let class = WNDCLASSW {
        lpfnWndProc: Some(button_procedure),
        hInstance: GetModuleHandleW(std::ptr::null()),
        lpszClassName: name.as_ptr(),
        // (all of it is drawn again at a new size: a chip follows its words' width, and without this only the new part would be)
        style: CS_HREDRAW | CS_VREDRAW,
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_HAND),
        ..std::mem::zeroed()
    };
    RegisterClassW(&class);
}

pub unsafe fn make_button(parent: HWND, id: isize, text: &str, kind: ButtonKind, backdrop: u32) -> HWND {
    let (class, text) = (wide("DogButton"), wide(text));
    let window = CreateWindowExW(0, class.as_ptr(), text.as_ptr(), WS_CHILD | WS_CLIPSIBLINGS, 0, 0, 0, 0, parent, id as HMENU, GetModuleHandleW(std::ptr::null()), std::ptr::null());
    set_button_style(window, kind, backdrop);
    window
}

/// The kind and backdrop a button is drawn with. One that has them already is left as it is, not drawn again: a layout that runs for every note of progress would have each of its buttons
/// drawn at the rate the notes come.
pub unsafe fn set_button_style(window: HWND, kind: ButtonKind, backdrop: u32) {
    let wanted = kind as isize | ((backdrop as isize) << BACKDROP_SHIFT);
    if GetWindowLongPtrW(window, GWLP_USERDATA) & !(FLAG_HOVER | FLAG_PRESSED) == wanted {
        return;
    }
    SetWindowLongPtrW(window, GWLP_USERDATA, wanted);
    InvalidateRect(window, std::ptr::null(), 0);
}

/// Popup menus and scrollbars in the theme. These entry points are not documented; on a Windows without them nothing changes.
pub unsafe fn prefer_theme(light: bool) {
    let name = wide("uxtheme.dll");
    let library = LoadLibraryW(name.as_ptr());
    if library.is_null() {
        return;
    }
    if let Some(set_mode) = GetProcAddress(library, 135 as _) {
        let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
        set_mode(if light { 3 } else { 2 }); // ForceLight, ForceDark
    }
    if let Some(flush) = GetProcAddress(library, 136 as _) {
        let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
        flush();
    }
}

pub unsafe fn allow_dark(window: HWND, dark: bool) {
    let name = wide("uxtheme.dll");
    let library = LoadLibraryW(name.as_ptr());
    if library.is_null() {
        return;
    }
    if let Some(allow) = GetProcAddress(library, 133 as _) {
        let allow: unsafe extern "system" fn(HWND, i32) -> i32 = std::mem::transmute(allow);
        allow(window, i32::from(dark));
    }
}

/// The backdrop each button keeps, moved to the theme just set.
pub unsafe fn retheme_buttons(parent: HWND, to_light: bool) {
    unsafe extern "system" fn one(window: HWND, to_light: LPARAM) -> BOOL {
        let mut class = [0u16; 32];
        let length = GetClassNameW(window, class.as_mut_ptr(), 32);
        if String::from_utf16_lossy(&class[..length.max(0) as usize]) == "DogButton" {
            let state = GetWindowLongPtrW(window, GWLP_USERDATA);
            let backdrop = theme::translate((state >> BACKDROP_SHIFT) as u32, to_light != 0);
            SetWindowLongPtrW(window, GWLP_USERDATA, (state & ((1 << BACKDROP_SHIFT) - 1)) | ((backdrop as isize) << BACKDROP_SHIFT));
        }
        1
    }
    EnumChildWindows(parent, Some(one), LPARAM::from(to_light));
}

/// Where Tab goes from what has the focus, in this order.
pub fn next_stop(stops: &[HWND], focus: HWND, backwards: bool) -> Option<HWND> {
    if stops.is_empty() {
        return None;
    }
    let count = stops.len();
    let next = match stops.iter().position(|window| *window == focus) {
        Some(at) if backwards => (at + count - 1) % count,
        Some(at) => (at + 1) % count,
        None if backwards => count - 1,
        None => 0,
    };
    Some(stops[next])
}

/// What Tab can reach in a window: what can take input and is on screen, top to bottom and left to right, past the sidebar.
pub unsafe fn tab_stops(parent: HWND, sidebar_right: i32) -> Vec<HWND> {
    unsafe extern "system" fn one(window: HWND, found: LPARAM) -> BOOL {
        let found = &mut *(found as *mut Vec<HWND>);
        found.push(window);
        1
    }
    let mut all: Vec<HWND> = Vec::new();
    EnumChildWindows(parent, Some(one), std::ptr::addr_of_mut!(all) as LPARAM);
    let mut stops: Vec<(i32, i32, HWND)> = Vec::new();
    for window in all {
        if IsWindowVisible(window) == 0 || IsWindowEnabled(window) == 0 {
            continue;
        }
        let mut class = [0u16; 32];
        let length = GetClassNameW(window, class.as_mut_ptr(), 32);
        let name = String::from_utf16_lossy(&class[..length.max(0) as usize]);
        let style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
        let takes_input = match name.as_str() {
            "DogButton" | "ListBox" | "DogTerm" => true,
            "Edit" | "RICHEDIT50W" | crate::textview::CLASS => style & ES_READONLY as u32 == 0,
            _ => false,
        };
        if !takes_input {
            continue;
        }
        let mut area: RECT = std::mem::zeroed();
        GetWindowRect(window, &mut area);
        let mut corner = POINT { x: area.left, y: area.top };
        ScreenToClient(parent, &mut corner);
        if corner.x >= sidebar_right {
            stops.push((corner.y, corner.x, window));
        }
    }
    stops.sort_by_key(|(top, left, _)| (*top, *left));
    stops.into_iter().map(|(_, _, window)| window).collect()
}

#[cfg(test)]
mod path_tests {
    use super::*;

    /// A width counted in characters: every character is one wide, as the test needs.
    fn within(width: usize) -> impl Fn(&str) -> bool {
        move |text| text.chars().count() <= width
    }

    #[test]
    fn a_path_is_shortened_in_the_middle_so_that_its_start_and_its_last_name_show() {
        let path = "C:\\Users\\someone\\AppData\\Local\\Temp\\claude\\data";
        assert_eq!(compact_path_with(path, &within(100)), path, "what fits is whole");
        for width in [14usize, 20, 28, 36, 46] {
            let shown = compact_path_with(path, &within(width));
            assert!(shown.chars().count() <= width, "{shown} at {width}");
            assert!(shown.starts_with("C:\\") && shown.ends_with("data") && shown.contains("\\…\\"), "{shown} at {width}");
        }
        assert_eq!(compact_path_with(path, &within(30)), "C:\\Users\\…\\Temp\\claude\\data");
        assert_eq!(compact_path_with(path, &within(41)), "C:\\Users\\someone\\…\\Local\\Temp\\claude\\data", "as much of the start as there is room for, after the last name");
    }

    #[test]
    fn what_cannot_keep_its_start_and_its_name_is_cut_at_the_end_with_an_ellipsis() {
        assert_eq!(compact_path_with("C:\\VeryLongFolderName\\AnotherVeryLongName", &within(12)), "C:\\VeryLong…");
        assert_eq!(compact_path_with("plain words that are long", &within(10)), "plain wor…");
        assert_eq!(compact_path_with("C:\\x", &within(2)), "C…");
        assert_eq!(compact_path_with("/home/someone/projects/app", &within(20)), "/home/…/projects/app", "a path with slashes keeps them");
        assert_eq!(compact_path_with("日本語のフォルダー名", &within(4)), "日本語…");
    }

    #[test]
    fn the_path_that_ends_a_row_is_whole_in_a_wide_row_and_never_cut_across_a_character() {
        let path = "C:\\Users\\ユーザー\\AppData\\Local\\DogmindApp";
        for width in 6..path.chars().count() + 2 {
            let shown = compact_path_with(path, &within(width));
            assert!(shown.chars().count() <= width.max(2), "{shown} at {width}");
            assert!(shown.chars().all(|c| c != '\u{FFFD}'));
        }
    }
}

#[cfg(test)]
mod tab_tests {
    use super::*;

    #[test]
    fn tab_goes_round_what_can_take_it() {
        let stops: Vec<HWND> = [1usize, 2, 3].map(|at| at as HWND).to_vec();
        assert_eq!(next_stop(&stops, 1 as HWND, false), Some(2 as HWND));
        assert_eq!(next_stop(&stops, 3 as HWND, false), Some(1 as HWND));
        assert_eq!(next_stop(&stops, 1 as HWND, true), Some(3 as HWND));
        assert_eq!(next_stop(&stops, 9 as HWND, false), Some(1 as HWND), "from elsewhere, the first");
        assert_eq!(next_stop(&stops, 9 as HWND, true), Some(3 as HWND));
        assert_eq!(next_stop(&[], 1 as HWND, false), None);
    }
}

/// The window and every control in it, drawn again now: those that keep what they drew do not all see an invalidation of their parent.
pub unsafe fn repaint_all(window: HWND) {
    unsafe extern "system" fn one(child: HWND, _: LPARAM) -> BOOL {
        RedrawWindow(child, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_UPDATENOW);
        1
    }
    RedrawWindow(window, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_UPDATENOW);
    EnumChildWindows(window, Some(one), 0);
}

#[cfg(test)]
mod bundled_face {
    use super::*;

    /// With the fonts fetched: Windows takes them for this process alone.
    #[test]
    fn the_fonts_load_for_this_process() {
        let Some(folder) = font_folders().into_iter().find(|folder| BUNDLED_FILES.iter().all(|file| folder.join(file).is_file())) else { return };
        for file in BUNDLED_FILES {
            let added = unsafe { AddFontResourceExW(wide(&folder.join(file).display().to_string()).as_ptr(), 0x10, std::ptr::null()) };
            assert!(added > 0, "{file}");
        }
    }

    /// A face is there by its English name, whatever name the Windows it runs on gives it; one that is nowhere is not.
    #[test]
    fn a_face_is_found_by_its_english_name() {
        unsafe {
            assert!(face_is_here("Segoe UI"));
            assert!(!face_is_here("No Such Face 7f3a"));
            // (fetched or installed, the Japanese face is found by its English name, which a Japanese Windows does not give back)
            let fetched = font_folders().into_iter().any(|folder| BUNDLED_FILES.iter().all(|file| folder.join(file).is_file()));
            if fetched {
                assert_eq!(japanese_face(), BUNDLED_FACE);
            }
        }
    }

    /// With the fonts fetched: the heavy one is the font's own bold, and not the regular one made thicker.
    #[test]
    fn the_bold_is_the_real_one() {
        if japanese_face() != BUNDLED_FACE {
            return;
        }
        unsafe {
            let (face, weight) = japanese_strong();
            let name = wide(face);
            let font = CreateFontW(-20, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, name.as_ptr());
            let device = CreateCompatibleDC(std::ptr::null_mut());
            let previous = SelectObject(device, font);
            let size = GetOutlineTextMetricsW(device, 0, std::ptr::null_mut());
            let mut bytes = vec![0u32; size as usize / 4 + 1];
            GetOutlineTextMetricsW(device, size, bytes.as_mut_ptr().cast());
            let metrics = &*(bytes.as_ptr() as *const OUTLINETEXTMETRICW);
            let full = bytes.as_ptr().cast::<u8>().add(metrics.otmpFullName as usize).cast::<u16>();
            let units: Vec<u16> = (0..).map(|at| *full.add(at)).take_while(|unit| *unit != 0).collect();
            let full = String::from_utf16_lossy(&units);
            SelectObject(device, previous);
            DeleteDC(device);
            DeleteObject(font);
            assert!(full.contains("Bold"), "{full}");
        }
    }
}

#[cfg(test)]
mod button_style_tests {
    use super::*;

    /// A button that has the style it is asked for is not drawn again, however often the layout asks; one that is given another is.
    #[test]
    fn a_button_is_drawn_again_only_when_its_style_changes() {
        // SAFETY: a window of this test's own, off every screen and never activated, made and ended on this thread.
        unsafe {
            register_button_class();
            let class = wide("DogButton");
            let window = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, class.as_ptr(), wide("Cancel").as_ptr(), WS_POPUP | WS_VISIBLE, -20000, -20000, 120, 40, std::ptr::null_mut(), std::ptr::null_mut(), GetModuleHandleW(std::ptr::null()), std::ptr::null());
            assert!(!window.is_null());
            let wanted_again = |window: HWND| {
                let mut area: RECT = std::mem::zeroed();
                GetUpdateRect(window, &mut area, 0) != 0
            };
            let settle = |window: HWND| {
                UpdateWindow(window);
            };
            set_button_style(window, ButtonKind::Link, 0x202020);
            settle(window);
            assert!(!wanted_again(window), "drawn, and nothing asks for more");
            for _ in 0..5 {
                set_button_style(window, ButtonKind::Link, 0x202020);
                assert!(!wanted_again(window), "the same style is no reason to draw it again");
            }
            // (the pointer on it is not undone by the layout that says what it is)
            SetWindowLongPtrW(window, GWLP_USERDATA, GetWindowLongPtrW(window, GWLP_USERDATA) | FLAG_HOVER);
            set_button_style(window, ButtonKind::Link, 0x202020);
            assert!(GetWindowLongPtrW(window, GWLP_USERDATA) & FLAG_HOVER != 0);
            set_button_style(window, ButtonKind::Secondary, 0x202020);
            assert!(wanted_again(window), "another kind is drawn");
            settle(window);
            set_button_style(window, ButtonKind::Secondary, 0x303030);
            assert!(wanted_again(window), "and another backdrop");
            DestroyWindow(window);
        }
    }
}

#[cfg(test)]
mod two_faces {
    use super::*;

    fn wide16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn a_text_is_cut_where_its_face_changes() {
        let units = wide16("abc 日本語 12");
        let found = runs(&units);
        assert_eq!(found, [(0..4, false), (4..7, true), (7..10, false)]);
        assert!(!is_japanese('a') && !is_japanese('1') && !is_japanese(' ') && !is_japanese('é'));
        for c in ['あ', 'ア', '語', '。', '「', 'Ａ', '𠀋'] {
            assert!(is_japanese(c), "{c}");
        }
        assert_eq!(runs(&wide16("𠀋")), [(0..2, true)], "a pair stays one run");
        assert_eq!(runs(&wide16("😀")), [(0..2, false)], "an emoji is not Japanese");
        assert!(runs(&[]).is_empty());
    }

    /// A character of ten wide, each.
    fn even(units: &[u16]) -> Vec<i32> {
        (0..=units.len() as i32).map(|at| at * 10).collect()
    }

    fn lines_of(text: &str, flags: u32, width: i32) -> Vec<String> {
        let units = wide16(text);
        break_lines(&units, &even(&units), flags, width).into_iter().map(|line| String::from_utf16_lossy(&units[line])).collect()
    }

    #[test]
    fn lines_break_as_each_language_allows() {
        assert_eq!(lines_of("one line", DT_SINGLELINE, 20), ["one line"], "one line stays one");
        assert_eq!(lines_of("two\nlines", 0, 1000), ["two", "lines"]);
        assert_eq!(lines_of("two\r\nlines\n", DT_WORDBREAK, 1000), ["two", "lines", ""]);
        assert_eq!(lines_of("aa bb cc", DT_WORDBREAK, 50), ["aa bb ", "cc"]);
        assert_eq!(lines_of("aa bb cc", DT_WORDBREAK, 30), ["aa ", "bb ", "cc"]);
        assert_eq!(lines_of("abcdefgh", DT_WORDBREAK, 30), ["abc", "def", "gh"], "a word too long for any line");
        assert_eq!(lines_of("日本語日本語", DT_WORDBREAK, 30), ["日本語", "日本語"], "between any two");
        assert_eq!(lines_of("日本語。日本", DT_WORDBREAK, 30), ["日本", "語。日", "本"], "a stop does not begin a line");
        assert_eq!(lines_of("日本「語」", DT_WORDBREAK, 30), ["日本", "「語」"], "an opening mark does not end one");
        assert_eq!(lines_of("use 日本語", DT_WORDBREAK, 30), ["use ", "日本語"], "and where they meet");
        assert_eq!(lines_of("", DT_WORDBREAK, 50), [""]);
    }
}
