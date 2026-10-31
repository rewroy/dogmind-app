//! How the window looks: its colours and type, antialiased shapes through GDI+, and the buttons, which draw themselves.

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::icon;

pub const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

pub const BACKGROUND: u32 = rgb(33, 32, 30);
pub const SIDEBAR: u32 = rgb(25, 24, 23);
pub const SURFACE: u32 = rgb(45, 44, 41);
pub const RAISED: u32 = rgb(58, 56, 53);
pub const LINE: u32 = rgb(66, 64, 60);
pub const TEXT: u32 = rgb(236, 233, 227);
pub const MUTED: u32 = rgb(158, 154, 146);
pub const FAINT: u32 = rgb(112, 109, 103);
pub const ACCENT: u32 = rgb(224, 138, 91);
pub const ACCENT_HOVER: u32 = rgb(236, 156, 112);
pub const ON_ACCENT: u32 = rgb(30, 22, 16);
pub const GOOD: u32 = rgb(140, 190, 130);
pub const WARN: u32 = rgb(226, 190, 120);
pub const BAD: u32 = rgb(232, 120, 112);
pub const CODE: u32 = rgb(206, 196, 178);

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

/// The paw, in a rounded square.
/// The artwork in the executable, at this size; null where it was built without it.
unsafe fn resource_icon(size: i32) -> HICON {
    LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as *const u16, IMAGE_ICON, size, size, LR_DEFAULTCOLOR) as HICON
}

pub unsafe fn logo(device: HDC, x: i32, y: i32, size: i32) {
    let artwork = resource_icon(size);
    if !artwork.is_null() {
        DrawIconEx(device, x, y, artwork, size, size, 0, std::ptr::null_mut(), DI_NORMAL);
        DestroyIcon(artwork);
        return;
    }
    let area = rect(x, y, size, size);
    rounded(device, area, size / 4, Some(ACCENT), None);
    icon::draw(device, "paw-print", area, size * 3 / 4, ON_ACCENT);
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
    let brush = CreateSolidBrush(SIDEBAR);
    let all = rect(0, 0, size, size);
    FillRect(device, &all, brush);
    DeleteObject(brush);
    logo(device, 0, 0, size);
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
    pub dpi: u32,
}

impl Fonts {
    pub unsafe fn new(dpi: u32) -> Fonts {
        let make = |face: &str, points: i32, weight: i32| {
            let face = wide(face);
            CreateFontW(-mul_div(points, dpi as i32, 72), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr())
        };
        Fonts {
            body: make("Yu Gothic UI", 10, 400),
            small: make("Yu Gothic UI", 9, 400),
            strong: make("Yu Gothic UI Semibold", 10, 600),
            title: make("Yu Gothic UI Semibold", 20, 600),
            dpi,
        }
    }
}

pub unsafe fn draw_text(device: HDC, text: &str, area: RECT, font: HFONT, color: u32, flags: u32) {
    if text.is_empty() {
        return; // DrawTextW reads even zero characters
    }
    let mut units: Vec<u16> = text.encode_utf16().collect();
    let mut target = area;
    SetBkMode(device, TRANSPARENT as _);
    SetTextColor(device, color);
    let previous = SelectObject(device, font);
    DrawTextW(device, units.as_mut_ptr(), units.len() as i32, &mut target, flags | DT_NOPREFIX);
    SelectObject(device, previous);
}

pub const ONE_LINE: u32 = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS;

// ---- Buttons.

#[derive(Clone, Copy, PartialEq, Eq)]
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
    /// A pill in a row of choices; selected looks raised.
    Tab = 5,
    TabSelected = 6,
    /// Only an icon, named by its text, until the pointer is on it. Quiet: for what sits among other things.
    Icon = 7,
}

const WM_MOUSELEAVE: u32 = 0x02A3;
const FLAG_HOVER: isize = 1 << 8;
const FLAG_PRESSED: isize = 1 << 9;
const BACKDROP_SHIFT: isize = 16;

static BUTTON_FONTS: std::sync::Mutex<Option<SendFonts>> = std::sync::Mutex::new(None);

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
                SendMessageW(GetParent(window), WM_COMMAND, id, window as LPARAM);
            }
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
    let kind = state & 0xFF;
    let (hover, pressed, enabled) = (state & FLAG_HOVER != 0, state & FLAG_PRESSED != 0, IsWindowEnabled(window) != 0);
    let backdrop = (state >> BACKDROP_SHIFT) as u32;
    let brush = CreateSolidBrush(backdrop);
    FillRect(device, &client, brush);
    DeleteObject(brush);
    let mut buffer = [0u16; 256];
    let length = GetWindowTextW(window, buffer.as_mut_ptr(), 256);
    let text = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
    let (glyph, label) = text.split_once('|').map_or((None, text.as_str()), |(glyph, label)| (Some(glyph), label));
    let height = client.bottom;
    let Some(fonts) = BUTTON_FONTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().map(|fonts| fonts.0) else { return };
    let s = |value: i32| scale(fonts.dpi, value);
    let center = DT_CENTER | ONE_LINE;
    let lit = hover || pressed;
    // A label with an icon before it, the two centred together.
    let centered = |device: HDC, font: HFONT, color: u32| {
        match glyph {
            Some(name) => {
                let mut measure = client;
                let previous = SelectObject(device, font);
                let mut units: Vec<u16> = label.encode_utf16().collect();
                DrawTextW(device, units.as_mut_ptr(), units.len() as i32, &mut measure, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
                SelectObject(device, previous);
                let size = s(16);
                let gap = s(8);
                let total = size + gap + (measure.right - measure.left);
                let left = (client.right - total) / 2;
                icon::draw(device, name, rect(left, 0, size, height), size, color);
                draw_text(device, label, RECT { left: left + size + gap, right: client.right, ..client }, font, color, ONE_LINE);
            }
            None => draw_text(device, label, client, font, color, center),
        }
    };
    match kind {
        0 => {
            let fill = if !enabled { RAISED } else if lit { ACCENT_HOVER } else { ACCENT };
            rounded(device, client, height / 2, Some(fill), None);
            centered(device, fonts.strong, if enabled { ON_ACCENT } else { MUTED });
        }
        1 => {
            rounded(device, client, height / 2, lit.then_some(RAISED), Some(LINE));
            centered(device, fonts.body, if enabled { TEXT } else { FAINT });
        }
        2 => {
            if lit {
                rounded(device, client, s(8), Some(SURFACE), None);
            }
            let color = if lit { TEXT } else { MUTED };
            let mut area = client;
            area.left += s(10);
            if let Some(name) = glyph {
                icon::draw(device, name, rect(area.left, 0, s(16), height), s(16), color);
                area.left += s(24);
            }
            draw_text(device, label, area, fonts.body, color, ONE_LINE);
        }
        3 => {
            if lit {
                rounded(device, client, height / 2, Some(RAISED), None);
            }
            let mut area = client;
            area.left += s(12);
            area.right -= s(26);
            draw_text(device, label, area, fonts.small, if lit { TEXT } else { MUTED }, ONE_LINE);
            icon::draw(device, "chevron-down", rect(client.right - s(24), 0, s(14), height), s(14), MUTED);
        }
        4 => {
            let fill = if !enabled { RAISED } else if lit { ACCENT_HOVER } else { ACCENT };
            rounded(device, client, height / 2, Some(fill), None);
            icon::draw(device, label, client, s(18), if enabled { ON_ACCENT } else { MUTED });
        }
        7 => {
            if lit {
                rounded(device, client, s(7), Some(if pressed { RAISED } else { SURFACE }), None);
            }
            let color = if label == "check" { GOOD } else if lit { TEXT } else { FAINT };
            icon::draw(device, label, client, s(15), color);
        }
        selected => {
            if selected == 6 || lit {
                rounded(device, client, height / 2, Some(if selected == 6 { RAISED } else { SURFACE }), None);
            }
            draw_text(device, label, client, fonts.body, if selected == 6 { TEXT } else { MUTED }, center);
        }
    }
}

pub unsafe fn register_button_class() {
    let name = wide("DogButton");
    let class = WNDCLASSW {
        lpfnWndProc: Some(button_procedure),
        hInstance: GetModuleHandleW(std::ptr::null()),
        lpszClassName: name.as_ptr(),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_HAND),
        ..std::mem::zeroed()
    };
    RegisterClassW(&class);
}

pub unsafe fn make_button(parent: HWND, id: isize, text: &str, kind: ButtonKind, backdrop: u32) -> HWND {
    let (class, text) = (wide("DogButton"), wide(text));
    let window = CreateWindowExW(0, class.as_ptr(), text.as_ptr(), WS_CHILD, 0, 0, 0, 0, parent, id as HMENU, GetModuleHandleW(std::ptr::null()), std::ptr::null());
    set_button_style(window, kind, backdrop);
    window
}

pub unsafe fn set_button_style(window: HWND, kind: ButtonKind, backdrop: u32) {
    SetWindowLongPtrW(window, GWLP_USERDATA, kind as isize | ((backdrop as isize) << BACKDROP_SHIFT));
    InvalidateRect(window, std::ptr::null(), 0);
}

/// Dark popup menus and scrollbars. These entry points are not documented; on a Windows without them nothing changes.
pub unsafe fn prefer_dark_theme() {
    let name = wide("uxtheme.dll");
    let library = LoadLibraryW(name.as_ptr());
    if library.is_null() {
        return;
    }
    if let Some(set_mode) = GetProcAddress(library, 135 as _) {
        let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
        set_mode(2); // ForceDark
    }
    if let Some(flush) = GetProcAddress(library, 136 as _) {
        let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
        flush();
    }
}

pub unsafe fn allow_dark(window: HWND) {
    let name = wide("uxtheme.dll");
    let library = LoadLibraryW(name.as_ptr());
    if library.is_null() {
        return;
    }
    if let Some(allow) = GetProcAddress(library, 133 as _) {
        let allow: unsafe extern "system" fn(HWND, i32) -> i32 = std::mem::transmute(allow);
        allow(window, 1);
    }
}
