//! The transcript as a windowless RichEdit, drawn through Direct2D and DirectWrite on the CPU: so its emoji are in colour, as in the rest of
//! the window. A window of its own class holds the text services and hands them every message it is sent, as a RichEdit window does with its
//! own: what the app sends it, and its subclass, stay as they were.
//!
//! The host's side of it (`ITextHost2`) is written out here by hand. The windows crate has the interface, but with no IID: the text services ask
//! for it by its own, and a host that does not answer to that is drawn with GDI.

use std::cell::Cell;
use std::ffi::c_void;

use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory1, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, ClientToScreen, EndPaint, GetDC, GetSysColor, InvalidateRect, ReleaseDC, ScreenToClient, UpdateWindow, COLOR_WINDOW, HBITMAP, HDC, HRGN, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::Controls::{EnableScrollBar, SetScrollInfo, SetScrollPos, ShowScrollBar};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const S_OK: i32 = 0;
const S_FALSE: i32 = 1;
const E_NOTIMPL: i32 = 0x8000_4001_u32 as i32;
const E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_ITEXTHOST: GUID = GUID::from_u128(0x13e670f4_1a5a_11cf_abeb_00aa00b65ea1);
const IID_ITEXTHOST2: GUID = GUID::from_u128(0x13e670f5_1a5a_11cf_abeb_00aa00b65ea1);
const IID_ITEXTSERVICES2: GUID = GUID::from_u128(0x8d33f741_cf58_11ce_a89d_00aa006cadc5);

// TXTBIT_
const RICHTEXT: u32 = 0x1;
const MULTILINE: u32 = 0x2;
const READONLY: u32 = 0x4;
const HIDESELECTION: u32 = 0x20;
const SAVESELECTION: u32 = 0x40;
const WORDWRAP: u32 = 0x400;
const DISABLEDRAG: u32 = 0x1000;
const BACKSTYLECHANGE: u32 = 0x4000;
const SCROLLBARCHANGE: u32 = 0x10000;
const CLIENTRECTCHANGE: u32 = 0x100000;
/// The bits that only say something changed, from VIEWINSETCHANGE to CLIENTRECTCHANGE: set when they are sent.
const CHANGES: u32 = 0x1F_E000;
const D2DDWRITE: u32 = 0x100_0000;
const PROPERTIES: u32 = RICHTEXT | MULTILINE | READONLY | HIDESELECTION | SAVESELECTION | WORDWRAP | DISABLEDRAG | D2DDWRITE;

// The edit styles RichEdit reads, some of them on the bits of others.
const ES_MULTILINE: u32 = 0x4;
const ES_NOOLEDRAGDROP: u32 = 0x8;
const ES_AUTOVSCROLL: u32 = 0x40;
const ES_AUTOHSCROLL: u32 = 0x80;
const ES_NOHIDESEL: u32 = 0x100;
const ES_READONLY: u32 = 0x800;
const ES_DISABLENOSCROLL: u32 = 0x2000;
const ES_SAVESEL: u32 = 0x8000;

const EM_GETRECT: u32 = 0xB2;
const EM_SETRECT: u32 = 0xB3;
const EM_SETRECTNP: u32 = 0xB4;
const EM_SETREADONLY: u32 = 0xCF;
const EM_SETBKGNDCOLOR: u32 = WM_USER + 67;

pub const CLASS: &str = "DogText";

/// `ITextHost2`'s table, in the order of TextServ.h.
#[repr(C)]
struct HostTable {
    query_interface: unsafe extern "system" fn(*mut Host, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut Host) -> u32,
    release: unsafe extern "system" fn(*mut Host) -> u32,
    get_dc: unsafe extern "system" fn(*mut Host) -> HDC,
    release_dc: unsafe extern "system" fn(*mut Host, HDC) -> i32,
    show_scroll_bar: unsafe extern "system" fn(*mut Host, i32, i32) -> i32,
    enable_scroll_bar: unsafe extern "system" fn(*mut Host, i32, i32) -> i32,
    set_scroll_range: unsafe extern "system" fn(*mut Host, i32, i32, i32, i32) -> i32,
    set_scroll_pos: unsafe extern "system" fn(*mut Host, i32, i32, i32) -> i32,
    invalidate_rect: unsafe extern "system" fn(*mut Host, *const RECT, i32),
    view_change: unsafe extern "system" fn(*mut Host, i32),
    create_caret: unsafe extern "system" fn(*mut Host, HBITMAP, i32, i32) -> i32,
    show_caret: unsafe extern "system" fn(*mut Host, i32) -> i32,
    set_caret_pos: unsafe extern "system" fn(*mut Host, i32, i32) -> i32,
    set_timer: unsafe extern "system" fn(*mut Host, u32, u32) -> i32,
    kill_timer: unsafe extern "system" fn(*mut Host, u32),
    scroll_window_ex: unsafe extern "system" fn(*mut Host, i32, i32, *const RECT, *const RECT, HRGN, *mut RECT, u32),
    set_capture: unsafe extern "system" fn(*mut Host, i32),
    set_focus: unsafe extern "system" fn(*mut Host),
    set_cursor: unsafe extern "system" fn(*mut Host, HCURSOR, i32),
    screen_to_client: unsafe extern "system" fn(*mut Host, *mut POINT) -> i32,
    client_to_screen: unsafe extern "system" fn(*mut Host, *mut POINT) -> i32,
    activate: unsafe extern "system" fn(*mut Host, *mut i32) -> i32,
    deactivate: unsafe extern "system" fn(*mut Host, i32) -> i32,
    get_client_rect: unsafe extern "system" fn(*mut Host, *mut RECT) -> i32,
    get_view_inset: unsafe extern "system" fn(*mut Host, *mut RECT) -> i32,
    get_char_format: unsafe extern "system" fn(*mut Host, *mut *const c_void) -> i32,
    get_para_format: unsafe extern "system" fn(*mut Host, *mut *const c_void) -> i32,
    get_sys_color: unsafe extern "system" fn(*mut Host, i32) -> u32,
    get_back_style: unsafe extern "system" fn(*mut Host, *mut i32) -> i32,
    get_max_length: unsafe extern "system" fn(*mut Host, *mut u32) -> i32,
    get_scroll_bars: unsafe extern "system" fn(*mut Host, *mut u32) -> i32,
    get_password_char: unsafe extern "system" fn(*mut Host, *mut u16) -> i32,
    get_accelerator_pos: unsafe extern "system" fn(*mut Host, *mut i32) -> i32,
    get_extent: unsafe extern "system" fn(*mut Host, *mut SIZE) -> i32,
    on_char_format_change: unsafe extern "system" fn(*mut Host, *const c_void) -> i32,
    on_para_format_change: unsafe extern "system" fn(*mut Host, *const c_void) -> i32,
    get_property_bits: unsafe extern "system" fn(*mut Host, u32, *mut u32) -> i32,
    notify: unsafe extern "system" fn(*mut Host, u32, *mut c_void) -> i32,
    imm_get_context: unsafe extern "system" fn(*mut Host) -> *mut c_void,
    imm_release_context: unsafe extern "system" fn(*mut Host, *mut c_void),
    get_selection_bar_width: unsafe extern "system" fn(*mut Host, *mut i32) -> i32,
    // ITextHost2
    is_double_click_pending: unsafe extern "system" fn(*mut Host) -> i32,
    get_window: unsafe extern "system" fn(*mut Host, *mut HWND) -> i32,
    set_foreground_window: unsafe extern "system" fn(*mut Host) -> i32,
    get_palette: unsafe extern "system" fn(*mut Host) -> *mut c_void,
    get_east_asian_flags: unsafe extern "system" fn(*mut Host, *mut i32) -> i32,
    set_cursor2: unsafe extern "system" fn(*mut Host, HCURSOR, i32) -> HCURSOR,
    free_text_services_notification: unsafe extern "system" fn(*mut Host),
    get_edit_style: unsafe extern "system" fn(*mut Host, u32, *mut u32) -> i32,
    get_window_styles: unsafe extern "system" fn(*mut Host, *mut u32, *mut u32) -> i32,
    show_drop_caret: unsafe extern "system" fn(*mut Host, i32, HDC, *const RECT) -> i32,
    destroy_caret: unsafe extern "system" fn(*mut Host) -> i32,
    get_horz_extent: unsafe extern "system" fn(*mut Host, *mut i32) -> i32,
}

/// The formats the text starts in, before the app sets its own: what the text services ask the host for, and tell it of when they change.
/// Sized for CHARFORMAT2W and PARAFORMAT2, whichever they give.
#[repr(C, align(4))]
struct Format<const N: usize>([u8; N]);

/// The host: what the text services are told of the window, and the window's way to them.
#[repr(C)]
struct Host {
    table: *const HostTable,
    refs: Cell<u32>,
    window: HWND,
    services: Cell<*mut c_void>,
    /// The text's ITextDocument, once asked for: emoji are set in their face through it.
    document: Cell<*mut c_void>,
    /// While emoji are given their face: TOM writes nothing to a read-only text, its formats included.
    unlocked: Cell<bool>,
    /// The window's element in UI Automation, once it is asked for: what a screen reader reads (`access`).
    access: std::cell::RefCell<Option<windows::Win32::UI::Accessibility::IRawElementProviderSimple>>,
    /// What that element is called, in English (`lang::t` says it in the person's language): the window's own name would be all of its text.
    name: Cell<&'static str>,
    /// The text's rectangle, as EM_SETRECT gave it: the window's client area where none was given.
    view: Cell<Option<RECT>>,
    /// The page's colour, around the text's rectangle as within it.
    background: Cell<u32>,
    char_format: std::cell::UnsafeCell<Format<116>>,
    para_format: std::cell::UnsafeCell<Format<188>>,
    target: std::cell::RefCell<Option<(ID2D1Factory1, ID2D1DCRenderTarget)>>,
}

static TABLE: HostTable = HostTable {
    query_interface,
    add_ref,
    release,
    get_dc,
    release_dc,
    show_scroll_bar,
    enable_scroll_bar,
    set_scroll_range,
    set_scroll_pos,
    invalidate_rect,
    view_change,
    create_caret,
    show_caret,
    set_caret_pos,
    set_timer,
    kill_timer,
    scroll_window_ex,
    set_capture,
    set_focus,
    set_cursor,
    screen_to_client,
    client_to_screen,
    activate,
    deactivate,
    get_client_rect,
    get_view_inset,
    get_char_format,
    get_para_format,
    get_sys_color,
    get_back_style,
    get_max_length,
    get_scroll_bars,
    get_password_char,
    get_accelerator_pos,
    get_extent,
    on_char_format_change,
    on_para_format_change,
    get_property_bits,
    notify,
    imm_get_context,
    imm_release_context,
    get_selection_bar_width,
    is_double_click_pending,
    get_window,
    set_foreground_window,
    get_palette,
    get_east_asian_flags,
    set_cursor2,
    free_text_services_notification,
    get_edit_style,
    get_window_styles,
    show_drop_caret,
    destroy_caret,
    get_horz_extent,
};

/// A slot of the text services' table: `ITextServices2`, in the order of TextServ.h.
mod slot {
    pub const RELEASE: usize = 2;
    pub const SEND_MESSAGE: usize = 3;
    pub const DRAW: usize = 4;
    pub const GET_VSCROLL: usize = 6;
    pub const SET_CURSOR: usize = 7;
    pub const IN_PLACE_ACTIVATE: usize = 9;
    pub const IN_PLACE_DEACTIVATE: usize = 10;
    pub const UI_ACTIVATE: usize = 11;
    pub const UI_DEACTIVATE: usize = 12;
    pub const PROPERTY_BITS_CHANGE: usize = 19;
    pub const DRAW_D2D: usize = 22;
}

impl Host {
    fn new(window: HWND) -> Host {
        let mut char_format = Format([0u8; 116]);
        let mut para_format = Format([0u8; 188]);
        // CHARFORMAT2W: size, mask, effects, height in twips, offset, colour, charset, pitch, face. Segoe UI at 11 points, in the window's colour.
        let put = |bytes: &mut [u8], at: usize, value: &[u8]| bytes[at..at + value.len()].copy_from_slice(value);
        put(&mut char_format.0, 0, &116u32.to_le_bytes());
        put(&mut char_format.0, 4, &0xF800_003Fu32.to_le_bytes()); // CFM_ALL
        put(&mut char_format.0, 8, &0x4000_0000u32.to_le_bytes()); // CFE_AUTOCOLOR
        put(&mut char_format.0, 12, &220i32.to_le_bytes());
        char_format.0[24] = 1; // DEFAULT_CHARSET
        for (index, unit) in "Segoe UI".encode_utf16().enumerate() {
            put(&mut char_format.0, 26 + 2 * index, &unit.to_le_bytes());
        }
        // PARAFORMAT: size, mask (PFM_ALL), numbering, effects, indents, offset, alignment (left), one tab of half an inch.
        put(&mut para_format.0, 0, &156u32.to_le_bytes());
        put(&mut para_format.0, 4, &0x8001_003Fu32.to_le_bytes());
        put(&mut para_format.0, 24, &1u16.to_le_bytes());
        put(&mut para_format.0, 26, &1i16.to_le_bytes());
        put(&mut para_format.0, 28, &720i32.to_le_bytes());
        Host {
            table: &TABLE,
            refs: Cell::new(1),
            window,
            services: Cell::new(std::ptr::null_mut()),
            document: Cell::new(std::ptr::null_mut()),
            unlocked: Cell::new(false),
            access: std::cell::RefCell::new(None),
            name: Cell::new("Conversation"),
            view: Cell::new(None),
            background: Cell::new(unsafe { GetSysColor(COLOR_WINDOW) }),
            char_format: std::cell::UnsafeCell::new(char_format),
            para_format: std::cell::UnsafeCell::new(para_format),
            target: std::cell::RefCell::new(None),
        }
    }

    unsafe fn slot<F: Copy>(&self, index: usize) -> F {
        let table = *(self.services.get() as *const *const usize);
        std::mem::transmute_copy(&*table.add(index))
    }

    /// A message to the text services: what they say of it, and its result.
    unsafe fn send(&self, message: u32, wparam: WPARAM, lparam: LPARAM) -> (i32, LRESULT) {
        let call: unsafe extern "system" fn(*mut c_void, u32, WPARAM, LPARAM, *mut LRESULT) -> i32 = self.slot(slot::SEND_MESSAGE);
        let mut result = 0;
        let said = call(self.services.get(), message, wparam, lparam, &mut result);
        (said, result)
    }

    unsafe fn call0(&self, index: usize) -> i32 {
        let call: unsafe extern "system" fn(*mut c_void) -> i32 = self.slot(index);
        call(self.services.get())
    }

    /// The text's rectangle has changed: the services are told, and activated in place again over it, which is what they lay out from.
    unsafe fn rect_changed(&self) {
        self.bits_changed(CLIENTRECTCHANGE);
        let activate: unsafe extern "system" fn(*mut c_void, *const RECT) -> i32 = self.slot(slot::IN_PLACE_ACTIVATE);
        let rect = self.text_rect();
        activate(self.services.get(), &rect);
    }

    unsafe fn bits_changed(&self, mask: u32) {
        let call: unsafe extern "system" fn(*mut c_void, u32, u32) -> i32 = self.slot(slot::PROPERTY_BITS_CHANGE);
        call(self.services.get(), mask, self.property_bits() & mask | (mask & CHANGES));
    }

    fn style(&self) -> u32 {
        unsafe { GetWindowLongPtrW(self.window, GWL_STYLE) as u32 }
    }

    fn property_bits(&self) -> u32 {
        let style = self.style();
        let mut bits = RICHTEXT | D2DDWRITE;
        if style & ES_MULTILINE != 0 {
            bits |= MULTILINE;
            if style & (WS_HSCROLL | ES_AUTOHSCROLL) == 0 {
                bits |= WORDWRAP;
            }
        }
        if style & ES_READONLY != 0 && !self.unlocked.get() {
            bits |= READONLY;
        }
        if style & ES_NOHIDESEL == 0 {
            bits |= HIDESELECTION;
        }
        if style & ES_SAVESEL != 0 {
            bits |= SAVESELECTION;
        }
        if style & ES_NOOLEDRAGDROP != 0 {
            bits |= DISABLEDRAG;
        }
        bits
    }

    fn client(&self) -> RECT {
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe { GetClientRect(self.window, &mut client) };
        client
    }

    /// Where the text is laid out: the rectangle EM_SETRECT gave, or all of the window.
    fn text_rect(&self) -> RECT {
        self.view.get().unwrap_or_else(|| self.client())
    }

    /// The text services for this host, made in the D2D mode it asks for. False where Msftedit has none to give.
    unsafe fn start(&self) -> bool {
        type Create = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> i32;
        let library = LoadLibraryW(crate::paint::wide("Msftedit.dll").as_ptr());
        if library.is_null() {
            return false;
        }
        let Some(create) = GetProcAddress(library, c"CreateTextServices".as_ptr().cast()) else { return false };
        let create: Create = std::mem::transmute(create);
        let mut unknown: *mut c_void = std::ptr::null_mut();
        if create(std::ptr::null_mut(), self as *const Host as *mut c_void, &mut unknown) < 0 || unknown.is_null() {
            return false;
        }
        let table = *(unknown as *const *const usize);
        let query: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32 = std::mem::transmute(*table);
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*table.add(slot::RELEASE));
        let mut services: *mut c_void = std::ptr::null_mut();
        let found = query(unknown, &IID_ITEXTSERVICES2, &mut services);
        release(unknown);
        if found < 0 || services.is_null() {
            return false;
        }
        self.services.set(services);
        let activate: unsafe extern "system" fn(*mut c_void, *const RECT) -> i32 = self.slot(slot::IN_PLACE_ACTIVATE);
        let rect = self.text_rect();
        activate(services, &rect);
        true
    }

    unsafe fn stop(&self) {
        let document = self.document.replace(std::ptr::null_mut());
        if !document.is_null() {
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*(*(document as *const *const usize)).add(slot::RELEASE));
            release(document);
        }
        let services = self.services.replace(std::ptr::null_mut());
        if services.is_null() {
            return;
        }
        let table = *(services as *const *const usize);
        let deactivate: unsafe extern "system" fn(*mut c_void) -> i32 = std::mem::transmute(*table.add(slot::IN_PLACE_DEACTIVATE));
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*table.add(slot::RELEASE));
        deactivate(services);
        release(services);
    }

    unsafe fn target(&self) -> Option<ID2D1DCRenderTarget> {
        let mut slot = self.target.borrow_mut();
        if slot.is_none() {
            // (the newest there is: a target from it draws colour glyphs, where the text services ask it to)
            let factory: ID2D1Factory1 = match D2D1CreateFactory::<windows::Win32::Graphics::Direct2D::ID2D1Factory7>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) {
                Ok(newest) => newest.cast().ok()?,
                Err(_) => D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?,
            };
            let properties = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let target = factory.CreateDCRenderTarget(&properties).ok()?;
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            *slot = Some((factory, target));
        }
        slot.as_ref().map(|(_, target)| target.clone())
    }

    /// The window's pixels in `dirty`, on `device`: the page, and the text over it. With GDI where Direct2D cannot.
    ///
    /// The target is bound over `dirty` only, and moved by as much: so a line that comes in costs that line, not the window. It is at the
    /// window's DPI, as the text services take theirs from it: their pixels are then the window's, and so are the places they give.
    unsafe fn draw(&self, device: HDC, dirty: RECT) {
        let client = self.client();
        let dirty = RECT { left: dirty.left.max(client.left), top: dirty.top.max(client.top), right: dirty.right.min(client.right), bottom: dirty.bottom.min(client.bottom) };
        if dirty.right <= dirty.left || dirty.bottom <= dirty.top {
            return;
        }
        let text = self.text_rect();
        let bounds = windows::Win32::Foundation::RECTL { left: text.left, top: text.top, right: text.right, bottom: text.bottom };
        let mut update = dirty;
        if let Some(target) = self.target() {
            let area = windows::Win32::Foundation::RECT { left: dirty.left, top: dirty.top, right: dirty.right, bottom: dirty.bottom };
            if target.BindDC(windows::Win32::Graphics::Gdi::HDC(device), &area).is_ok() {
                let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(self.window).max(96) as f32;
                target.SetDpi(dpi, dpi);
                target.BeginDraw();
                let dip = 96.0 / dpi;
                target.SetTransform(&windows_numerics::Matrix3x2::translation(-dirty.left as f32 * dip, -dirty.top as f32 * dip));
                let page = self.background.get();
                target.Clear(Some(&D2D1_COLOR_F { r: (page & 0xFF) as f32 / 255.0, g: ((page >> 8) & 0xFF) as f32 / 255.0, b: ((page >> 16) & 0xFF) as f32 / 255.0, a: 1.0 }));
                let call: unsafe extern "system" fn(*mut c_void, *mut c_void, *const windows::Win32::Foundation::RECTL, *mut RECT, i32) -> i32 = self.slot(slot::DRAW_D2D);
                let plain = color::on(target.as_raw());
                let drawn = call(self.services.get(), target.as_raw(), &bounds, &mut update, 0);
                color::off(target.as_raw(), plain);
                let ended = target.EndDraw(None, None);
                if ended.is_err() {
                    *self.target.borrow_mut() = None;
                }
                if drawn >= 0 && ended.is_ok() {
                    return;
                }
            }
        }
        // TxDraw, on the device: DVASPECT_CONTENT, TXTVIEW_ACTIVE.
        type Draw = unsafe extern "system" fn(*mut c_void, u32, i32, *mut c_void, *mut c_void, HDC, HDC, *const windows::Win32::Foundation::RECTL, *const c_void, *mut RECT, isize, u32, i32) -> i32;
        let call: Draw = self.slot(slot::DRAW);
        call(self.services.get(), 1, 0, std::ptr::null_mut(), std::ptr::null_mut(), device, std::ptr::null_mut(), &bounds, std::ptr::null(), &mut update, 0, 0, 0);
    }
}

unsafe fn host<'a>(this: *mut Host) -> &'a Host {
    &*this
}

unsafe extern "system" fn query_interface(this: *mut Host, iid: *const GUID, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return E_NOINTERFACE;
    }
    if [IID_IUNKNOWN, IID_ITEXTHOST, IID_ITEXTHOST2].contains(&*iid) {
        *out = this.cast();
        add_ref(this);
        return S_OK;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

/// The window owns the host, and lets it go after the text services: these only count.
unsafe extern "system" fn add_ref(this: *mut Host) -> u32 {
    let host = host(this);
    host.refs.set(host.refs.get() + 1);
    host.refs.get()
}

unsafe extern "system" fn release(this: *mut Host) -> u32 {
    let host = host(this);
    host.refs.set(host.refs.get().saturating_sub(1));
    host.refs.get()
}

unsafe extern "system" fn get_dc(this: *mut Host) -> HDC {
    GetDC(host(this).window)
}

unsafe extern "system" fn release_dc(this: *mut Host, device: HDC) -> i32 {
    ReleaseDC(host(this).window, device)
}

unsafe extern "system" fn show_scroll_bar(this: *mut Host, bar: i32, show: i32) -> i32 {
    ShowScrollBar(host(this).window, bar as _, show)
}

unsafe extern "system" fn enable_scroll_bar(this: *mut Host, bar: i32, arrows: i32) -> i32 {
    EnableScrollBar(host(this).window, bar as _, arrows as _)
}

/// The range, with the page the text services have now: they give only the range, and a bar's thumb is the page's share of it.
unsafe extern "system" fn set_scroll_range(this: *mut Host, bar: i32, least: i32, most: i32, redraw: i32) -> i32 {
    let host = host(this);
    let mut info: SCROLLINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
    info.fMask = SIF_RANGE | SIF_PAGE;
    if host.style() & ES_DISABLENOSCROLL != 0 {
        info.fMask |= SIF_DISABLENOSCROLL;
    }
    if bar == SB_VERT as i32 && !host.services.get().is_null() {
        let call: unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32, *mut i32, *mut i32, *mut i32) -> i32 = host.slot(slot::GET_VSCROLL);
        let mut page = 0;
        call(host.services.get(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut page, std::ptr::null_mut());
        info.nPage = page.max(0) as u32;
    }
    info.nMin = least;
    info.nMax = most;
    SetScrollInfo(host.window, bar as _, &info, redraw);
    1
}

unsafe extern "system" fn set_scroll_pos(this: *mut Host, bar: i32, position: i32, redraw: i32) -> i32 {
    SetScrollPos(host(this).window, bar as _, position, redraw);
    1
}

unsafe extern "system" fn invalidate_rect(this: *mut Host, area: *const RECT, erase: i32) {
    InvalidateRect(host(this).window, area, erase);
}

unsafe extern "system" fn view_change(this: *mut Host, update: i32) {
    if update != 0 {
        UpdateWindow(host(this).window);
    }
}

unsafe extern "system" fn create_caret(this: *mut Host, bitmap: HBITMAP, width: i32, height: i32) -> i32 {
    CreateCaret(host(this).window, bitmap, width, height)
}

unsafe extern "system" fn show_caret(this: *mut Host, show: i32) -> i32 {
    if show != 0 { ShowCaret(host(this).window) } else { HideCaret(host(this).window) }
}

unsafe extern "system" fn set_caret_pos(_this: *mut Host, x: i32, y: i32) -> i32 {
    SetCaretPos(x, y)
}

unsafe extern "system" fn set_timer(this: *mut Host, id: u32, milliseconds: u32) -> i32 {
    i32::from(SetTimer(host(this).window, id as usize, milliseconds, None) != 0)
}

unsafe extern "system" fn kill_timer(this: *mut Host, id: u32) {
    KillTimer(host(this).window, id as usize);
}

unsafe extern "system" fn scroll_window_ex(this: *mut Host, dx: i32, dy: i32, scroll: *const RECT, clip: *const RECT, region: HRGN, update: *mut RECT, flags: u32) {
    ScrollWindowEx(host(this).window, dx, dy, scroll, clip, region, update, flags as _);
}

unsafe extern "system" fn set_capture(this: *mut Host, capture: i32) {
    if capture != 0 {
        SetCapture(host(this).window);
    } else {
        ReleaseCapture();
    }
}

unsafe extern "system" fn set_focus(this: *mut Host) {
    SetFocus(host(this).window);
}

unsafe extern "system" fn set_cursor(_this: *mut Host, cursor: HCURSOR, _text: i32) {
    SetCursor(cursor);
}

unsafe extern "system" fn screen_to_client(this: *mut Host, point: *mut POINT) -> i32 {
    ScreenToClient(host(this).window, point)
}

unsafe extern "system" fn client_to_screen(this: *mut Host, point: *mut POINT) -> i32 {
    ClientToScreen(host(this).window, point)
}

unsafe extern "system" fn activate(_this: *mut Host, old: *mut i32) -> i32 {
    if !old.is_null() {
        *old = 0;
    }
    S_OK
}

unsafe extern "system" fn deactivate(_this: *mut Host, _new: i32) -> i32 {
    S_OK
}

unsafe extern "system" fn get_client_rect(this: *mut Host, area: *mut RECT) -> i32 {
    *area = host(this).text_rect();
    S_OK
}

/// None: the text's rectangle is the client one the services are given.
unsafe extern "system" fn get_view_inset(_this: *mut Host, inset: *mut RECT) -> i32 {
    *inset = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    S_OK
}

unsafe extern "system" fn get_char_format(this: *mut Host, format: *mut *const c_void) -> i32 {
    *format = host(this).char_format.get().cast();
    S_OK
}

unsafe extern "system" fn get_para_format(this: *mut Host, format: *mut *const c_void) -> i32 {
    *format = host(this).para_format.get().cast();
    S_OK
}

/// The window's colour is the page's, as EM_SETBKGNDCOLOR set it: a windowless RichEdit is told it by its host, not by that message.
unsafe extern "system" fn get_sys_color(this: *mut Host, index: i32) -> u32 {
    if index == COLOR_WINDOW {
        return host(this).background.get();
    }
    GetSysColor(index as _)
}

unsafe extern "system" fn get_back_style(_this: *mut Host, style: *mut i32) -> i32 {
    *style = 1; // TXTBACK_OPAQUE
    S_OK
}

unsafe extern "system" fn get_max_length(_this: *mut Host, length: *mut u32) -> i32 {
    *length = u32::MAX; // INFINITE: EM_EXLIMITTEXT says
    S_OK
}

unsafe extern "system" fn get_scroll_bars(this: *mut Host, bars: *mut u32) -> i32 {
    *bars = host(this).style() & (WS_VSCROLL | WS_HSCROLL | ES_AUTOVSCROLL | ES_AUTOHSCROLL | ES_DISABLENOSCROLL);
    S_OK
}

unsafe extern "system" fn get_password_char(_this: *mut Host, _character: *mut u16) -> i32 {
    S_FALSE
}

unsafe extern "system" fn get_accelerator_pos(_this: *mut Host, position: *mut i32) -> i32 {
    *position = -1;
    S_OK
}

/// Not given: the text is at its own size, not zoomed to an extent.
unsafe extern "system" fn get_extent(_this: *mut Host, _extent: *mut SIZE) -> i32 {
    E_NOTIMPL
}

/// The default format, kept for when the services ask again.
unsafe extern "system" fn on_char_format_change(this: *mut Host, format: *const c_void) -> i32 {
    keep_format(format, host(this).char_format.get().cast(), 116);
    S_OK
}

unsafe extern "system" fn on_para_format_change(this: *mut Host, format: *const c_void) -> i32 {
    keep_format(format, host(this).para_format.get().cast(), 188);
    S_OK
}

unsafe fn keep_format(given: *const c_void, kept: *mut u8, room: usize) {
    if given.is_null() || given == kept.cast_const().cast() {
        return;
    }
    let size = (*(given as *const u32) as usize).min(room);
    std::ptr::copy(given as *const u8, kept, size);
}

unsafe extern "system" fn get_property_bits(this: *mut Host, mask: u32, bits: *mut u32) -> i32 {
    *bits = host(this).property_bits() & mask;
    S_OK
}

/// To the parent, as a RichEdit window says them: those with a structure as WM_NOTIFY, filled in as from this window, and the rest as WM_COMMAND.
unsafe extern "system" fn notify(this: *mut Host, code: u32, data: *mut c_void) -> i32 {
    let host = host(this);
    let parent = GetParent(host.window);
    if parent.is_null() {
        return S_OK;
    }
    let id = GetDlgCtrlID(host.window);
    // (EN_CHANGE comes with a CHANGENOTIFY, which a window's parent is not given: below 0x700 a notification is a command, whatever comes with it)
    if code >= 0x0700 && !data.is_null() {
        let header = data as *mut windows_sys::Win32::UI::Controls::NMHDR;
        (*header).hwndFrom = host.window;
        (*header).idFrom = id as usize;
        (*header).code = code;
        if SendMessageW(parent, WM_NOTIFY, id as WPARAM, data as LPARAM) != 0 {
            return S_FALSE;
        }
    } else if code < 0x0700 {
        SendMessageW(parent, WM_COMMAND, ((code as usize) << 16) | (id as usize & 0xFFFF), host.window as LPARAM);
    }
    S_OK
}

/// The window's input context: what is composed with an IME goes into the text through it.
unsafe extern "system" fn imm_get_context(this: *mut Host) -> *mut c_void {
    windows_sys::Win32::UI::Input::Ime::ImmGetContext(host(this).window)
}

unsafe extern "system" fn imm_release_context(this: *mut Host, context: *mut c_void) {
    windows_sys::Win32::UI::Input::Ime::ImmReleaseContext(host(this).window, context);
}

unsafe extern "system" fn get_selection_bar_width(_this: *mut Host, width: *mut i32) -> i32 {
    *width = 0;
    S_OK
}

unsafe extern "system" fn is_double_click_pending(this: *mut Host) -> i32 {
    let mut message: MSG = std::mem::zeroed();
    PeekMessageW(&mut message, host(this).window, WM_LBUTTONDBLCLK, WM_LBUTTONDBLCLK, PM_NOREMOVE | PM_NOYIELD)
}

unsafe extern "system" fn get_window(this: *mut Host, window: *mut HWND) -> i32 {
    *window = host(this).window;
    S_OK
}

/// Left alone: nothing here takes the foreground from what the person is in.
unsafe extern "system" fn set_foreground_window(_this: *mut Host) -> i32 {
    S_OK
}

unsafe extern "system" fn get_palette(_this: *mut Host) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "system" fn get_east_asian_flags(_this: *mut Host, flags: *mut i32) -> i32 {
    *flags = 0;
    S_OK
}

unsafe extern "system" fn set_cursor2(_this: *mut Host, cursor: HCURSOR, _text: i32) -> HCURSOR {
    SetCursor(cursor)
}

unsafe extern "system" fn free_text_services_notification(_this: *mut Host) {}

unsafe extern "system" fn get_edit_style(_this: *mut Host, _item: u32, data: *mut u32) -> i32 {
    *data = 0;
    S_OK
}

unsafe extern "system" fn get_window_styles(this: *mut Host, style: *mut u32, extended: *mut u32) -> i32 {
    *style = host(this).style();
    *extended = GetWindowLongPtrW(host(this).window, GWL_EXSTYLE) as u32;
    S_OK
}

unsafe extern "system" fn show_drop_caret(_this: *mut Host, _show: i32, _device: HDC, _area: *const RECT) -> i32 {
    E_NOTIMPL
}

unsafe extern "system" fn destroy_caret(_this: *mut Host) -> i32 {
    DestroyCaret();
    S_OK
}

unsafe extern "system" fn get_horz_extent(_this: *mut Host, extent: *mut i32) -> i32 {
    *extent = 0;
    S_OK
}

/// The class, made once: its windows keep their host in their first extra slot.
pub unsafe fn register(instance: windows_sys::Win32::Foundation::HINSTANCE) {
    let name = crate::paint::wide(CLASS);
    let class = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(procedure),
        cbWndExtra: std::mem::size_of::<usize>() as i32,
        hInstance: instance,
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_IBEAM),
        lpszClassName: name.as_ptr(),
        ..std::mem::zeroed()
    };
    RegisterClassW(&class);
}

/// Whether the transcript, the composer and the panels' document are made this way: yes, unless DOGMIND_TEXTVIEW says 0 (Msftedit's
/// windows, drawn with GDI, as they were).
pub fn wanted() -> bool {
    std::env::var("DOGMIND_TEXTVIEW").map_or(true, |value| value != "0")
}

/// What a window of this class is called in UI Automation, in English. Nothing for one of another class.
pub unsafe fn set_name(window: HWND, name: &'static str) {
    let host = GetWindowLongPtrW(window, 0) as *const Host;
    if !host.is_null() && is_of_class(window) {
        (*host).name.set(name);
    }
}

/// (a test's way to see Japanese composed in a window of this class, with no keyboard: this message, registered, sent to it)
#[cfg(debug_assertions)]
fn test_ime_message() -> u32 {
    static MESSAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe { RegisterWindowMessageW(crate::paint::wide("DogmindTestIme").as_ptr()) })
}

/// The window takes the focus of its own thread, and its input context composes にほんご: shown as it is composed (0), settled (1), or
/// given up (2).
#[cfg(debug_assertions)]
unsafe fn test_ime(window: HWND, step: WPARAM) {
    use windows_sys::Win32::UI::Input::Ime::{ImmGetContext, ImmNotifyIME, ImmReleaseContext, ImmSetCompositionStringW, ImmSetOpenStatus, CPS_CANCEL, CPS_COMPLETE, NI_COMPOSITIONSTR, SCS_SETSTR};
    SetFocus(window);
    let context = ImmGetContext(window);
    if context.is_null() {
        return;
    }
    match step {
        0 => {
            ImmSetOpenStatus(context, 1);
            let text: Vec<u16> = "にほんご".encode_utf16().collect();
            ImmSetCompositionStringW(context, SCS_SETSTR, text.as_ptr().cast(), (text.len() * 2) as u32, std::ptr::null(), 0);
        }
        1 => {
            ImmNotifyIME(context, NI_COMPOSITIONSTR, CPS_COMPLETE, 0);
        }
        _ => {
            ImmNotifyIME(context, NI_COMPOSITIONSTR, CPS_CANCEL, 0);
        }
    }
    ImmReleaseContext(window, context);
}

unsafe fn is_of_class(window: HWND) -> bool {
    let mut class = [0u16; 16];
    let length = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32).max(0) as usize;
    String::from_utf16_lossy(&class[..length]) == CLASS
}

unsafe extern "system" fn procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let this = GetWindowLongPtrW(window, 0) as *mut Host;
    match message {
        WM_NCCREATE => {
            SetWindowLongPtrW(window, 0, Box::into_raw(Box::new(Host::new(window))) as isize);
            return DefWindowProcW(window, message, wparam, lparam);
        }
        WM_CREATE => return if !this.is_null() && (*this).start() { 0 } else { -1 },
        WM_NCDESTROY => {
            if !this.is_null() {
                SetWindowLongPtrW(window, 0, 0);
                (*this).stop();
                drop(Box::from_raw(this));
            }
            return DefWindowProcW(window, message, wparam, lparam);
        }
        _ => {}
    }
    if this.is_null() || (*this).services.get().is_null() {
        return DefWindowProcW(window, message, wparam, lparam);
    }
    let host = &*this;
    #[cfg(debug_assertions)]
    if message == test_ime_message() {
        test_ime(window, wparam);
        return 0;
    }
    match message {
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let device = BeginPaint(window, &mut paint);
            host.draw(device, paint.rcPaint);
            EndPaint(window, &paint);
            return 0;
        }
        WM_PRINTCLIENT => {
            host.draw(wparam as HDC, host.client());
            return 0;
        }
        WM_ERASEBKGND => return 1,
        WM_GETOBJECT if lparam as i32 == windows::Win32::UI::Accessibility::UiaRootObjectId => {
            if let Some(root) = access::root(host) {
                return access::give(window, wparam, lparam, &root);
            }
        }
        WM_DESTROY => access::let_go(host),
        WM_SIZE => {
            host.rect_changed();
            InvalidateRect(window, std::ptr::null(), 0);
            return 0;
        }
        WM_SETCURSOR if wparam as HWND == window && (lparam & 0xFFFF) as u32 == HTCLIENT => {
            let mut point = POINT { x: 0, y: 0 };
            GetCursorPos(&mut point);
            ScreenToClient(window, &mut point);
            // OnTxSetCursor: DVASPECT_CONTENT, no device, the text's rectangle.
            let call: unsafe extern "system" fn(*mut c_void, u32, i32, *mut c_void, *mut c_void, HDC, HDC, *const RECT, i32, i32) -> i32 = host.slot(slot::SET_CURSOR);
            let rect = host.text_rect();
            call(host.services.get(), 1, -1, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &rect, point.x, point.y);
            return 1;
        }
        WM_SETFOCUS => {
            host.call0(slot::UI_ACTIVATE);
        }
        WM_KILLFOCUS => {
            let (_, result) = host.send(message, wparam, lparam);
            host.call0(slot::UI_DEACTIVATE);
            return result;
        }
        EM_SETRECT | EM_SETRECTNP => {
            let given = lparam as *const RECT;
            let view = if given.is_null() {
                None
            } else if wparam == 1 {
                // (relative to the one there is)
                let (now, by) = (host.text_rect(), *given);
                Some(RECT { left: now.left + by.left, top: now.top + by.top, right: now.right + by.right, bottom: now.bottom + by.bottom })
            } else {
                Some(*given)
            };
            host.view.set(view);
            host.rect_changed();
            if message == EM_SETRECT {
                InvalidateRect(window, std::ptr::null(), 0);
            }
            return 0;
        }
        EM_GETRECT => {
            if lparam != 0 {
                *(lparam as *mut RECT) = host.text_rect();
            }
            return 0;
        }
        EM_SETBKGNDCOLOR => {
            let was = host.background.replace(if wparam != 0 { GetSysColor(COLOR_WINDOW) } else { lparam as u32 });
            host.bits_changed(BACKSTYLECHANGE);
            InvalidateRect(window, std::ptr::null(), 0);
            return was as LRESULT;
        }
        WM_DPICHANGED_AFTERPARENT => {
            InvalidateRect(window, std::ptr::null(), 0);
            return 0;
        }
        EM_SETREADONLY => {
            let style = host.style();
            let style = if wparam != 0 { style | ES_READONLY } else { style & !ES_READONLY };
            SetWindowLongPtrW(window, GWL_STYLE, style as isize);
            host.bits_changed(READONLY);
            return 1;
        }
        WM_STYLECHANGED => {
            host.bits_changed(PROPERTIES | SCROLLBARCHANGE);
        }
        WM_SETREDRAW => {
            // (the services hold their drawing; the window, as any, keeps its visible bit off meanwhile)
            host.send(message, wparam, lparam);
            return DefWindowProcW(window, message, wparam, lparam);
        }
        _ => {}
    }
    let touched = emoji::touched(host, message, wparam);
    let (said, result) = host.send(message, wparam, lparam);
    if let Some(touched) = touched {
        emoji::in_their_face(host, touched);
    }
    if said == S_FALSE {
        DefWindowProcW(window, message, wparam, lparam)
    } else {
        result
    }
}


/// Colour glyphs, where the text services draw outlines only: for the time they draw, the target's table is a copy of its own whose
/// DrawGlyphRun asks DirectWrite for the run's colour layers and draws those, as Direct2D does for text it is given to lay out. The copy is
/// the target's (one for its class, made once); everything else in it is the target's own.
mod color {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows::core::Interface;
    use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
    use windows::Win32::Graphics::Direct2D::{ID2D1Brush, ID2D1DeviceContext4, ID2D1RenderTarget, ID2D1RenderTarget_Vtbl, ID2D1SolidColorBrush, D2D1_COLOR_BITMAP_GLYPH_SNAP_OPTION_DEFAULT};
    use windows::Win32::Graphics::DirectWrite::*;
    use windows_numerics::{Matrix3x2, Vector2};

    type Glyphs = unsafe extern "system" fn(*mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *mut c_void, DWRITE_MEASURING_MODE);

    const SLOT: usize = std::mem::offset_of!(ID2D1RenderTarget_Vtbl, DrawGlyphRun) / std::mem::size_of::<usize>();
    /// The slots copied: the DC render target's 58, and a few after them.
    const COPIED: usize = 64;

    /// DrawGlyphRun as the target has it.
    static PLAIN: AtomicUsize = AtomicUsize::new(0);

    thread_local! {
        /// Each table of a target's class, and the copy of it that draws in colour.
        static TABLES: RefCell<Vec<(usize, &'static [usize])>> = const { RefCell::new(Vec::new()) };
        static WRITE: RefCell<Option<Option<IDWriteFactory4>>> = const { RefCell::new(None) };
    }

    /// The target's table, swapped for the copy that draws in colour; the one it had, to put back.
    pub unsafe fn on(target: *mut c_void) -> *const usize {
        let old = *(target as *const *const usize);
        let copy = TABLES.with(|tables| {
            let mut tables = tables.borrow_mut();
            if let Some((_, copy)) = tables.iter().find(|(table, _)| *table == old as usize) {
                return copy.as_ptr();
            }
            let mut copy: Vec<usize> = (0..COPIED).map(|index| *old.add(index)).collect();
            PLAIN.store(copy[SLOT], Ordering::Relaxed);
            copy[SLOT] = draw_glyph_run as Glyphs as usize;
            // (kept for good: the text services may hold the target, and call it, after this host is gone)
            let copy: &'static [usize] = Box::leak(copy.into_boxed_slice());
            tables.push((old as usize, copy));
            copy.as_ptr()
        });
        *(target as *mut *const usize) = copy;
        old
    }

    pub unsafe fn off(target: *mut c_void, old: *const usize) {
        *(target as *mut *const usize) = old;
    }

    fn write() -> Option<IDWriteFactory4> {
        WRITE.with(|cell| cell.borrow_mut().get_or_insert_with(|| unsafe { DWriteCreateFactory::<IDWriteFactory4>(DWRITE_FACTORY_TYPE_SHARED).ok() }).clone())
    }

    unsafe extern "system" fn draw_glyph_run(this: *mut c_void, origin: Vector2, run: *const DWRITE_GLYPH_RUN, brush: *mut c_void, mode: DWRITE_MEASURING_MODE) {
        let plain: Glyphs = std::mem::transmute(PLAIN.load(Ordering::Relaxed));
        if !in_colour(this, origin, run, brush, mode, plain) && !in_emoji_face(this, origin, run, brush, mode, plain) {
            plain(this, origin, run, brush, mode);
        }
    }

    thread_local! {
        /// The faces runs came in, each with what its emoji glyphs stand for, where it is Segoe UI Symbol. Each is held, so that no other face
        /// comes to have its address while it is here.
        static FACES: RefCell<Vec<(IDWriteFontFace, Option<Rc<HashMap<u16, u32>>>)>> = const { RefCell::new(Vec::new()) };
        static EMOJI: RefCell<Option<Option<IDWriteFontFace>>> = const { RefCell::new(None) };
    }

    /// The emoji in Segoe UI Symbol, by glyph: what each stands for. None for any other face.
    unsafe fn symbol_emoji(face: &IDWriteFontFace) -> Option<Rc<HashMap<u16, u32>>> {
        FACES.with(|faces| {
            let mut faces = faces.borrow_mut();
            if let Some((_, found)) = faces.iter().find(|(held, _)| held.as_raw() == face.as_raw()) {
                return found.clone();
            }
            let family = face.cast::<IDWriteFontFace3>().ok().and_then(|face| face.GetFamilyNames().ok()).and_then(|names| {
                let mut name = [0u16; 64];
                names.GetString(0, &mut name).ok()?;
                Some(String::from_utf16_lossy(&name[..name.iter().position(|unit| *unit == 0).unwrap_or(64)]))
            });
            let found = (family.as_deref() == Some("Segoe UI Symbol")).then(|| {
                let points: Vec<u32> = (0x2300..=0x2BFF).chain(0x1F000..=0x1FAFF).filter(|point| super::emoji::is_emoji(*point)).collect();
                let mut glyphs = vec![0u16; points.len()];
                let _ = face.GetGlyphIndices(points.as_ptr(), points.len() as u32, glyphs.as_mut_ptr());
                let mut map = HashMap::new();
                for (glyph, point) in glyphs.into_iter().zip(points) {
                    if glyph != 0 {
                        map.entry(glyph).or_insert(point);
                    }
                }
                Rc::new(map)
            });
            if faces.len() >= 16 {
                faces.remove(0);
            }
            faces.push((face.clone(), found.clone()));
            found
        })
    }

    fn emoji_face() -> Option<IDWriteFontFace> {
        EMOJI.with(|cell| {
            cell.borrow_mut()
                .get_or_insert_with(|| unsafe {
                    let write: IDWriteFactory = write()?.cast().ok()?;
                    let mut collection: Option<IDWriteFontCollection> = None;
                    write.GetSystemFontCollection(&mut collection, false).ok()?;
                    let collection = collection?;
                    let (mut index, mut exists) = (0u32, windows::core::BOOL(0));
                    collection.FindFamilyName(windows::core::w!("Segoe UI Emoji"), &mut index, &mut exists).ok()?;
                    if !exists.as_bool() {
                        return None;
                    }
                    let font = collection.GetFontFamily(index).ok()?.GetFirstMatchingFont(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL).ok()?;
                    font.CreateFontFace().ok()
                })
                .clone()
        })
    }

    /// A run in Segoe UI Symbol, where RichEdit sets emoji in a plain text (the composer), and no face can be set on a part of it: its emoji
    /// are drawn from Segoe UI Emoji, in colour, each where the run puts its glyph; the rest of it as it was. False where it has none.
    unsafe fn in_emoji_face(this: *mut c_void, origin: Vector2, run: *const DWRITE_GLYPH_RUN, brush: *mut c_void, mode: DWRITE_MEASURING_MODE, plain: Glyphs) -> bool {
        let run = &*run;
        let count = run.glyphCount as usize;
        // (right to left, or sideways, the places are not simply one after another)
        if count == 0 || run.bidiLevel & 1 != 0 || run.isSideways.as_bool() || run.glyphAdvances.is_null() {
            return false;
        }
        let Some(face) = run.fontFace.as_ref() else { return false };
        let Some(points) = symbol_emoji(face) else { return false };
        let glyphs = std::slice::from_raw_parts(run.glyphIndices, count);
        if !glyphs.iter().any(|glyph| points.contains_key(glyph)) {
            return false;
        }
        let Some(emoji) = emoji_face() else { return false };
        let advances = std::slice::from_raw_parts(run.glyphAdvances, count);
        let mut x = origin.X;
        let mut at = 0;
        while at < count {
            let coloured = points.contains_key(&glyphs[at]);
            let mut end = at;
            let mut width = 0.0;
            while end < count && points.contains_key(&glyphs[end]) == coloured {
                width += advances[end];
                end += 1;
            }
            let place = Vector2 { X: x, Y: origin.Y };
            let mut swapped: Vec<u16> = Vec::new();
            if coloured {
                let wanted: Vec<u32> = glyphs[at..end].iter().map(|glyph| points[glyph]).collect();
                swapped = vec![0u16; wanted.len()];
                if emoji.GetGlyphIndices(wanted.as_ptr(), wanted.len() as u32, swapped.as_mut_ptr()).is_err() {
                    swapped.clear();
                }
            }
            let (font, indices) = if swapped.is_empty() { (face, glyphs[at..].as_ptr()) } else { (&emoji, swapped.as_ptr()) };
            let part = DWRITE_GLYPH_RUN {
                // (borrowed, not held: the part is gone before the run is)
                fontFace: std::mem::transmute_copy(font),
                fontEmSize: run.fontEmSize,
                glyphCount: (end - at) as u32,
                glyphIndices: indices,
                glyphAdvances: advances[at..].as_ptr(),
                glyphOffsets: if run.glyphOffsets.is_null() { std::ptr::null() } else { run.glyphOffsets.add(at) },
                isSideways: run.isSideways,
                bidiLevel: run.bidiLevel,
            };
            if swapped.is_empty() || !in_colour(this, place, &part, brush, mode, plain) {
                plain(this, place, &part, brush, mode);
            }
            x += width;
            at = end;
        }
        true
    }

    /// The run in its colour layers. False where it has none (DWRITE_E_NOCOLOR), and is drawn as it was.
    unsafe fn in_colour(this: *mut c_void, origin: Vector2, run: *const DWRITE_GLYPH_RUN, brush: *mut c_void, mode: DWRITE_MEASURING_MODE, plain: Glyphs) -> bool {
        let (Some(write), Some(target)) = (write(), ID2D1RenderTarget::from_raw_borrowed(&this)) else { return false };
        let mut world = Matrix3x2::default();
        target.GetTransform(&mut world);
        let (mut dpi_x, mut dpi_y) = (96.0f32, 96.0f32);
        target.GetDpi(&mut dpi_x, &mut dpi_y);
        let (x, y) = (dpi_x / 96.0, dpi_y / 96.0);
        let device = DWRITE_MATRIX { m11: world.M11 * x, m12: world.M12 * y, m21: world.M21 * x, m22: world.M22 * y, dx: world.M31 * x, dy: world.M32 * y };
        let formats = DWRITE_GLYPH_IMAGE_FORMATS_TRUETYPE
            | DWRITE_GLYPH_IMAGE_FORMATS_CFF
            | DWRITE_GLYPH_IMAGE_FORMATS_COLR
            | DWRITE_GLYPH_IMAGE_FORMATS_SVG
            | DWRITE_GLYPH_IMAGE_FORMATS_PNG
            | DWRITE_GLYPH_IMAGE_FORMATS_JPEG
            | DWRITE_GLYPH_IMAGE_FORMATS_TIFF
            | DWRITE_GLYPH_IMAGE_FORMATS_PREMULTIPLIED_B8G8R8A8;
        let translated = write.TranslateColorGlyphRun(origin, run, None, formats, mode, Some(&device), 0);
        let Ok(runs) = translated else { return false };
        let context: Option<ID2D1DeviceContext4> = target.cast().ok();
        let mut solid: Option<ID2D1SolidColorBrush> = None;
        while runs.MoveNext().is_ok_and(|more| more.as_bool()) {
            let Ok(layer) = runs.GetCurrentRun() else { break };
            let layer = &*layer;
            let at = Vector2 { X: layer.Base.baselineOriginX, Y: layer.Base.baselineOriginY };
            let glyphs = &layer.Base.glyphRun;
            let format = layer.glyphImageFormat;
            if [DWRITE_GLYPH_IMAGE_FORMATS_PNG, DWRITE_GLYPH_IMAGE_FORMATS_JPEG, DWRITE_GLYPH_IMAGE_FORMATS_TIFF, DWRITE_GLYPH_IMAGE_FORMATS_PREMULTIPLIED_B8G8R8A8].contains(&format) {
                if let Some(context) = &context {
                    context.DrawColorBitmapGlyphRun(format, at, glyphs, layer.measuringMode, D2D1_COLOR_BITMAP_GLYPH_SNAP_OPTION_DEFAULT);
                }
            } else if format == DWRITE_GLYPH_IMAGE_FORMATS_SVG {
                if let Some(context) = &context {
                    context.DrawSvgGlyphRun(at, glyphs, ID2D1Brush::from_raw_borrowed(&brush), None, 0, layer.measuringMode);
                }
            } else {
                // (a layer of the palette's colour, or one in the text's own: 0xFFFF)
                let paint = if layer.Base.paletteIndex == 0xFFFF {
                    brush
                } else {
                    let tone = layer.Base.runColor;
                    let color = D2D1_COLOR_F { r: tone.r, g: tone.g, b: tone.b, a: tone.a };
                    match &solid {
                        Some(made) => made.SetColor(&color),
                        None => solid = target.CreateSolidColorBrush(&color, None).ok(),
                    }
                    solid.as_ref().map_or(brush, |made| made.as_raw())
                };
                plain(this, at, glyphs, paint, layer.measuringMode);
            }
        }
        true
    }
}

/// Emoji in Segoe UI Emoji, whose glyphs have colours: left to itself RichEdit sets them in Segoe UI Symbol, which has outlines only. Done
/// here, over what each message that writes or formats text touched, so that no format the app sets afterwards takes it away.
mod emoji {
    use std::ffi::c_void;

    use windows::core::GUID;
    use windows_sys::Win32::Foundation::WPARAM;
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_SETTEXT, WM_USER};

    use super::{slot, Host};

    const EM_REPLACESEL: u32 = 0xC2;
    const EM_EXGETSEL: u32 = WM_USER + 52;
    const EM_SETCHARFORMAT: u32 = WM_USER + 68;
    const EM_STREAMIN: u32 = WM_USER + 73;
    const EM_GETTEXTRANGE: u32 = WM_USER + 75;
    const EM_GETTEXTLENGTHEX: u32 = WM_USER + 95;
    const EM_SETTEXTEX: u32 = WM_USER + 97;
    const EM_GETOLEINTERFACE: u32 = WM_USER + 60;
    const EM_GETTEXTMODE: u32 = WM_USER + 90;
    // ITextDocument::Undo's counts that hold the undo list, and let it go on
    const TOM_SUSPEND: i32 = -9_999_995;
    const TOM_RESUME: i32 = -9_999_994;
    const IID_ITEXTDOCUMENT: GUID = GUID::from_u128(0x8cc497c0_a1df_11ce_8098_00aa0047be5d);
    const FACE: &str = "Segoe UI Emoji";
    /// How far before what was written the text is looked at: a sequence (a ZWJ, a skin tone) may be written in two pieces.
    const BEFORE: i32 = 16;

    /// What a message is about to touch.
    pub enum Touched {
        /// From here to where the caret is after it.
        From(i32),
        Selection,
        All,
    }

    unsafe fn selection(host: &Host) -> (i32, i32) {
        let mut range = [0i32; 2];
        host.send(EM_EXGETSEL, 0, range.as_mut_ptr() as isize);
        (range[0], range[1])
    }

    /// A plain text (TM_PLAINTEXT) has one format for all of it: a face set on its emoji would be set on every word. Its emoji take their
    /// colours where they are drawn (`color::in_emoji_face`).
    unsafe fn is_plain(host: &Host) -> bool {
        host.send(EM_GETTEXTMODE, 0, 0).1 & 1 != 0
    }

    pub unsafe fn touched(host: &Host, message: u32, wparam: WPARAM) -> Option<Touched> {
        if !matches!(message, EM_REPLACESEL | EM_SETTEXTEX | EM_STREAMIN | WM_SETTEXT | EM_SETCHARFORMAT) || is_plain(host) {
            return None;
        }
        match message {
            EM_REPLACESEL => Some(Touched::From(selection(host).0)),
            // SETTEXTEX: ST_SELECTION; STREAMIN: SFF_SELECTION
            EM_SETTEXTEX if wparam != 0 && *(wparam as *const u32) & 2 != 0 => Some(Touched::From(selection(host).0)),
            EM_STREAMIN if wparam & 0x8000 != 0 => Some(Touched::From(selection(host).0)),
            EM_SETTEXTEX | EM_STREAMIN | WM_SETTEXT => Some(Touched::All),
            // SCF_ALL, SCF_SELECTION
            EM_SETCHARFORMAT if wparam & 4 != 0 => Some(Touched::All),
            EM_SETCHARFORMAT if wparam & 1 != 0 => Some(Touched::Selection),
            _ => None,
        }
    }

    pub unsafe fn in_their_face(host: &Host, touched: Touched) {
        let (first, last) = match touched {
            Touched::From(first) => (first, selection(host).1),
            Touched::Selection => selection(host),
            Touched::All => {
                // GTL_NUMCHARS | GTL_PRECISE, in UTF-16
                let how = [8u32 | 2, 1200];
                (0, host.send(EM_GETTEXTLENGTHEX, how.as_ptr() as WPARAM, 0).1 as i32)
            }
        };
        if last <= first {
            return;
        }
        let first = (first - BEFORE).max(0);
        let mut units = vec![0u16; (last - first + 1) as usize];
        #[repr(C)]
        struct TextRange {
            first: i32,
            last: i32,
            text: *mut u16,
        }
        let mut asked = TextRange { first, last, text: units.as_mut_ptr() };
        let got = host.send(EM_GETTEXTRANGE, 0, std::ptr::addr_of_mut!(asked) as isize).1.clamp(0, (last - first) as isize) as usize;
        let runs = runs(&units[..got]);
        if runs.is_empty() {
            return;
        }
        let document = document(host);
        if document.is_null() {
            return;
        }
        let face = windows::core::BSTR::from(FACE);
        // (a face is no edit the person made: Ctrl+Z does not take it back first)
        let undo: unsafe extern "system" fn(*mut c_void, i32, *mut i32) -> i32 = std::mem::transmute(*(*(document as *const *const usize)).add(22));
        undo(document, TOM_SUSPEND, std::ptr::null_mut());
        host.unlocked.set(true);
        host.bits_changed(super::READONLY);
        for (start, end) in runs {
            set_face(document, first + start as i32, first + end as i32, &face);
        }
        host.unlocked.set(false);
        host.bits_changed(super::READONLY);
        undo(document, TOM_RESUME, std::ptr::null_mut());
    }

    unsafe fn document(host: &Host) -> *mut c_void {
        if host.document.get().is_null() {
            let mut ole: *mut c_void = std::ptr::null_mut();
            host.send(EM_GETOLEINTERFACE, 0, std::ptr::addr_of_mut!(ole) as isize);
            if !ole.is_null() {
                let table = *(ole as *const *const usize);
                let query: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32 = std::mem::transmute(*table);
                let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*table.add(slot::RELEASE));
                let mut document: *mut c_void = std::ptr::null_mut();
                if query(ole, &IID_ITEXTDOCUMENT, &mut document) >= 0 {
                    host.document.set(document);
                }
                release(ole);
            }
        }
        host.document.get()
    }

    /// ITextDocument::Range, ITextRange::GetFont, ITextFont::SetName: no selection moves, and nothing scrolls.
    unsafe fn set_face(document: *mut c_void, first: i32, last: i32, face: &windows::core::BSTR) {
        let call = |object: *mut c_void, index: usize| *(*(object as *const *const usize)).add(index);
        let range_of: unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> i32 = std::mem::transmute(call(document, 24));
        let mut range: *mut c_void = std::ptr::null_mut();
        if range_of(document, first, last, &mut range) < 0 || range.is_null() {
            return;
        }
        let font_of: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 = std::mem::transmute(call(range, 18));
        let mut font: *mut c_void = std::ptr::null_mut();
        if font_of(range, &mut font) >= 0 && !font.is_null() {
            let set_name: unsafe extern "system" fn(*mut c_void, *const u16) -> i32 = std::mem::transmute(call(font, 37));
            set_name(font, std::mem::transmute_copy(face));
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(call(font, slot::RELEASE));
            release(font);
        }
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(call(range, slot::RELEASE));
        release(range);
    }

    /// Emoji presented as such without a selector, in the Basic Multilingual Plane (Unicode's Emoji_Presentation).
    const PRESENTED: &[(u32, u32)] = &[
        (0x231A, 0x231B), (0x23E9, 0x23EC), (0x23F0, 0x23F0), (0x23F3, 0x23F3), (0x25FD, 0x25FE), (0x2614, 0x2615), (0x2648, 0x2653),
        (0x267F, 0x267F), (0x2693, 0x2693), (0x26A1, 0x26A1), (0x26AA, 0x26AB), (0x26BD, 0x26BE), (0x26C4, 0x26C5), (0x26CE, 0x26CE),
        (0x26D4, 0x26D4), (0x26EA, 0x26EA), (0x26F2, 0x26F3), (0x26F5, 0x26F5), (0x26FA, 0x26FA), (0x26FD, 0x26FD), (0x2705, 0x2705),
        (0x270A, 0x270B), (0x2728, 0x2728), (0x274C, 0x274C), (0x274E, 0x274E), (0x2753, 0x2755), (0x2757, 0x2757), (0x2795, 0x2797),
        (0x27B0, 0x27B0), (0x27BF, 0x27BF), (0x2B1B, 0x2B1C), (0x2B50, 0x2B50), (0x2B55, 0x2B55),
    ];

    /// An emoji by itself, with no selector after it.
    pub fn is_emoji(point: u32) -> bool {
        (0x1F000..=0x1FAFF).contains(&point) || PRESENTED.iter().any(|(low, high)| (*low..=*high).contains(&point))
    }

    fn begins(point: u32, next: Option<u32>) -> bool {
        next == Some(0xFE0F) || is_emoji(point)
    }

    /// What goes on with the emoji before it: a joiner and what it joins, a selector, a keycap, a skin tone, a tag.
    fn goes_on(point: u32) -> bool {
        matches!(point, 0x200D | 0xFE0F | 0x20E3 | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F)
    }

    /// The emoji in these UTF-16 units, as runs of them: where each begins and ends, in units.
    pub fn runs(units: &[u16]) -> Vec<(usize, usize)> {
        // (a low surrogate at the start is the end of something cut off: skipped)
        let skip = usize::from(units.first().is_some_and(|unit| (0xDC00..0xE000).contains(unit)));
        let mut points: Vec<(usize, u32)> = Vec::new();
        let mut at = skip;
        for decoded in char::decode_utf16(units[skip..].iter().copied()) {
            let point = decoded.map_or(0xFFFD, u32::from);
            points.push((at, point));
            at += if point > 0xFFFF { 2 } else { 1 };
        }
        let end_of = |index: usize| points.get(index).map_or(units.len(), |(at, _)| *at);
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut index = 0;
        while index < points.len() {
            let (at, point) = points[index];
            let next = points.get(index + 1).map(|(_, point)| *point);
            if next == Some(0xFE0E) {
                // (asked to be text)
                index += 2;
                continue;
            }
            // (a joiner at the start goes on from what came before it, which was written earlier)
            if !begins(point, next) && !(index == 0 && goes_on(point)) {
                index += 1;
                continue;
            }
            let mut after = index + 1;
            let flag = (0x1F1E6..=0x1F1FF).contains(&point);
            while let Some(&(_, following)) = points.get(after) {
                if goes_on(following) {
                    after += 1;
                    if following == 0x200D && after < points.len() {
                        after += 1;
                    }
                } else if flag && after == index + 1 && (0x1F1E6..=0x1F1FF).contains(&following) {
                    after += 1;
                } else {
                    break;
                }
            }
            let (start, end) = (at, end_of(after));
            match runs.last_mut() {
                Some(last) if last.1 == start => last.1 = end,
                _ => runs.push((start, end)),
            }
            index = after;
        }
        runs
    }

    #[cfg(test)]
    mod tests {
        use super::runs;

        fn units(text: &str) -> Vec<u16> {
            text.encode_utf16().collect()
        }

        #[test]
        fn emoji_are_found_with_what_goes_on_with_them() {
            assert_eq!(runs(&units("abc")), vec![]);
            // 👋 is two units; ✨ one, presented as emoji by itself
            assert_eq!(runs(&units("hi 👋 x ✨")), vec![(3, 5), (8, 9)]);
            // a family, joined: one run
            let family = units("👨\u{200D}👩\u{200D}👧");
            assert_eq!(runs(&family), vec![(0, family.len())]);
            // a heart with its selector, a keycap, and a check mark left as text
            assert_eq!(runs(&units("❤\u{FE0F}")), vec![(0, 2)]);
            assert_eq!(runs(&units("1\u{FE0F}\u{20E3}")), vec![(0, 3)]);
            assert_eq!(runs(&units("✓ ✔")), vec![]);
            // asked to be text
            assert_eq!(runs(&units("☺\u{FE0E}")), vec![]);
            // a flag is two regional indicators
            assert_eq!(runs(&units("🇯🇵!")), vec![(0, 4)]);
            // the end of a pair cut off at the start, and a joiner that goes on from before
            assert_eq!(runs(&[0xDC4B, u16::from(b'a')]), vec![]);
            assert_eq!(runs(&units("\u{200D}👧")), vec![(0, 3)]);
        }
    }
}

/// The transcript in UI Automation, as a RichEdit window has it: a document whose text a screen reader can read and move through. A
/// windowless RichEdit makes that element itself (`IRicheditWindowlessAccessibility`); the host gives it a place (a site) under an element
/// of its own for the window, which UI Automation is given for it.
// (Windows's names for UI Automation's constants, which are matched on there)
#[allow(non_upper_case_globals)]
mod access {
    use std::ffi::c_void;

    use windows::core::{implement, Error, Interface, Result, GUID};
    use windows_core::IUnknownImpl;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Com::SAFEARRAY;
    use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement};
    use windows::Win32::System::Variant::{VARIANT, VT_I4};
    use windows::Win32::UI::Accessibility::*;

    use super::{slot, Host};

    const IID_WINDOWLESS_ACCESSIBILITY: GUID = GUID::from_u128(0x983e572d_20cd_460b_9104_83111592dd10);

    /// The window's element: a pane named for what it holds, whose one child is the text.
    #[implement(IRawElementProviderSimple, IRawElementProviderFragment, IRawElementProviderFragmentRoot)]
    struct Window {
        window: HWND,
        text: IRawElementProviderFragment,
    }

    /// Where the text's element stands: under the window's.
    #[implement(IRawElementProviderWindowlessSite)]
    struct Site {
        window: HWND,
    }

    fn handle(window: windows_sys::Win32::Foundation::HWND) -> HWND {
        HWND(window)
    }

    /// The window's element, made the first time it is asked for. None where the text services make none.
    pub unsafe fn root(host: &Host) -> Option<IRawElementProviderSimple> {
        if let Some(root) = host.access.borrow().as_ref() {
            return Some(root.clone());
        }
        let services = host.services.get();
        let table = *(services as *const *const usize);
        let query: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32 = std::mem::transmute(*table);
        let mut maker: *mut c_void = std::ptr::null_mut();
        if query(services, &IID_WINDOWLESS_ACCESSIBILITY, &mut maker) < 0 || maker.is_null() {
            return None;
        }
        let maker_table = *(maker as *const *const usize);
        // CreateProvider(site, provider)
        let create: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> i32 = std::mem::transmute(*maker_table.add(3));
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*maker_table.add(slot::RELEASE));
        let site: IRawElementProviderWindowlessSite = Site { window: handle(host.window) }.into();
        let mut made: *mut c_void = std::ptr::null_mut();
        let said = create(maker, site.as_raw(), &mut made);
        release(maker);
        if said < 0 || made.is_null() {
            return None;
        }
        let text = IRawElementProviderSimple::from_raw(made);
        let text: IRawElementProviderFragment = text.cast().ok()?;
        let root: IRawElementProviderSimple = Window { window: handle(host.window), text }.into();
        *host.access.borrow_mut() = Some(root.clone());
        Some(root)
    }

    pub unsafe fn give(window: windows_sys::Win32::Foundation::HWND, wparam: windows_sys::Win32::Foundation::WPARAM, lparam: windows_sys::Win32::Foundation::LPARAM, root: &IRawElementProviderSimple) -> isize {
        let LRESULT(result) = UiaReturnRawElementProvider(handle(window), WPARAM(wparam), LPARAM(lparam), root);
        result
    }

    /// As the window goes: UI Automation lets go of its elements, and they of the text.
    pub unsafe fn let_go(host: &Host) {
        let root = host.access.borrow_mut().take();
        if let Some(root) = root {
            UiaReturnRawElementProvider(handle(host.window), WPARAM(0), LPARAM(0), None::<&IRawElementProviderSimple>);
            let _ = UiaDisconnectProvider(&root);
        }
    }

    /// The window's element as a fragment, from the host the window keeps: for the site, which holds only the window.
    unsafe fn window_element(window: HWND) -> Option<IRawElementProviderFragment> {
        let host = windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(window.0, 0) as *const Host;
        if host.is_null() {
            return None;
        }
        (*host).access.borrow().as_ref().and_then(|root| root.cast().ok())
    }

    impl IRawElementProviderSimple_Impl for Window_Impl {
        fn ProviderOptions(&self) -> Result<ProviderOptions> {
            Ok(ProviderOptions_ServerSideProvider | ProviderOptions_UseComThreading)
        }

        fn GetPatternProvider(&self, _pattern: UIA_PATTERN_ID) -> Result<windows::core::IUnknown> {
            Err(Error::empty())
        }

        /// Its name, where the window's own would be all of its text.
        fn GetPropertyValue(&self, property: UIA_PROPERTY_ID) -> Result<VARIANT> {
            Ok(match property {
                UIA_NamePropertyId => {
                    let host = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(self.window.0, 0) } as *const Host;
                    let name = if host.is_null() { "Conversation" } else { unsafe { (*host).name.get() } };
                    VARIANT::from(crate::lang::t(name))
                }
                UIA_ControlTypePropertyId => VARIANT::from(UIA_PaneControlTypeId.0),
                _ => VARIANT::default(),
            })
        }

        fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
            unsafe { UiaHostProviderFromHwnd(self.window) }
        }
    }

    impl IRawElementProviderFragment_Impl for Window_Impl {
        fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
            match direction {
                NavigateDirection_FirstChild | NavigateDirection_LastChild => Ok(self.text.clone()),
                _ => Err(Error::empty()),
            }
        }

        /// None: a window's element has the window's.
        fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
            Ok(std::ptr::null_mut())
        }

        fn BoundingRectangle(&self) -> Result<UiaRect> {
            let mut area = windows::Win32::Foundation::RECT::default();
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(self.window.0, std::ptr::addr_of_mut!(area).cast()) };
            Ok(UiaRect { left: area.left.into(), top: area.top.into(), width: (area.right - area.left).into(), height: (area.bottom - area.top).into() })
        }

        fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
            Ok(std::ptr::null_mut())
        }

        fn SetFocus(&self) -> Result<()> {
            unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(self.window.0) };
            Ok(())
        }

        fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
            Ok(self.to_interface())
        }
    }

    impl IRawElementProviderFragmentRoot_Impl for Window_Impl {
        fn ElementProviderFromPoint(&self, _x: f64, _y: f64) -> Result<IRawElementProviderFragment> {
            Ok(self.text.clone())
        }

        fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
            Ok(self.text.clone())
        }
    }

    impl IRawElementProviderWindowlessSite_Impl for Site_Impl {
        fn GetAdjacentFragment(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
            match direction {
                NavigateDirection_Parent => unsafe { window_element(self.window) }.ok_or_else(Error::empty),
                _ => Err(Error::empty()),
            }
        }

        /// What the text's own ids are put after: the window's element's, which UI Automation fills in.
        fn GetRuntimeIdPrefix(&self) -> Result<*mut SAFEARRAY> {
            unsafe {
                let array = SafeArrayCreateVector(VT_I4, 0, 2);
                if array.is_null() {
                    return Err(Error::from_hresult(windows::Win32::Foundation::E_OUTOFMEMORY));
                }
                for (index, value) in [UiaAppendRuntimeId as i32, 1].iter().enumerate() {
                    SafeArrayPutElement(array, &(index as i32), (value as *const i32).cast())?;
                }
                Ok(array)
            }
        }
    }
}