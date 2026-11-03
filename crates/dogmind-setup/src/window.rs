//! The one small window, in the app's look: its colours by the theme Windows is set to, its type, a bar and two buttons, and on the first page the folder with a button that changes it.

use std::path::Path;
use std::result::Result::Ok;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_DISABLED, ODS_FOCUS, ODS_SELECTED};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::install::{self, Failure, Install, Progress, Stage};
use crate::pack::Package;
use crate::place::{self, Refusal};
use crate::system::{self, wide, Running};
use crate::Options;

const WM_WORKER: u32 = WM_APP + 1;
const TIMER_POLL: usize = 1;
/// The id of the button that changes the folder, beside 1 and 2 of the other two (what a dialog's Enter and Escape press).
const CHANGE: isize = 10;
/// How long a copy that was asked has to close, before the person is asked again.
const CLOSING: Duration = Duration::from_secs(20);

const LOGO: &[u8] = include_bytes!("../../../assets/logo.a8");

pub const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

/// The app's colours, in its two themes.
#[derive(Clone, Copy)]
struct Colours {
    background: u32,
    raised: u32,
    line: u32,
    text: u32,
    muted: u32,
    accent: u32,
    on_accent: u32,
    bad: u32,
    dark: bool,
}

const DARK: Colours = Colours { background: rgb(33, 32, 30), raised: rgb(58, 56, 53), line: rgb(66, 64, 60), text: rgb(236, 233, 227), muted: rgb(158, 154, 146), accent: rgb(224, 138, 91), on_accent: rgb(30, 22, 16), bad: rgb(232, 120, 112), dark: true };
const LIGHT: Colours = Colours { background: rgb(250, 249, 246), raised: rgb(235, 232, 225), line: rgb(221, 217, 209), text: rgb(36, 34, 31), muted: rgb(104, 100, 93), accent: rgb(193, 98, 55), on_accent: rgb(255, 251, 247), bad: rgb(186, 58, 48), dark: false };

fn windows_is_light() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let (key, name) = (wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"), wide("AppsUseLightTheme"));
    let (mut value, mut size) = (0u32, 4u32);
    // SAFETY: the names end in a zero, and the value is as long as the call is told.
    let done = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), std::ptr::addr_of_mut!(value).cast(), &mut size) };
    done == 0 && value != 0
}

fn argb(color: u32) -> u32 {
    0xFF00_0000 | ((color & 0xFF) << 16) | (color & 0xFF00) | ((color >> 16) & 0xFF)
}

fn scale(dpi: u32, value: i32) -> i32 {
    (i64::from(value) * i64::from(dpi) / 96) as i32
}

unsafe fn rounded(device: HDC, area: RECT, radius: i32, fill: Option<u32>, border: Option<u32>) {
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

/// The dog, in the theme's text colour: how much ink each pixel holds.
unsafe fn logo(device: HDC, x: i32, y: i32, size: i32, ink: u32) {
    let (width, height) = (i32::from(u16::from_le_bytes([LOGO[0], LOGO[1]])), i32::from(u16::from_le_bytes([LOGO[2], LOGO[3]])));
    let (shown_width, shown_height) = if width >= height { (size, size * height / width) } else { (size * width / height, size) };
    let colour = (argb(ink) & 0x00FF_FFFF).to_le_bytes();
    let pixels: Vec<u8> = LOGO[4..].iter().flat_map(|&alpha| [colour[0], colour[1], colour[2], alpha]).collect();
    let mut graphics = std::ptr::null_mut();
    if GdipCreateFromHDC(device, &mut graphics) != 0 {
        return;
    }
    GdipSetInterpolationMode(graphics, 7); // InterpolationModeHighQualityBicubic
    GdipSetPixelOffsetMode(graphics, 4); // PixelOffsetModeHalf
    let mut image = std::ptr::null_mut();
    if GdipCreateBitmapFromScan0(width, height, width * 4, 0x0026_200A, pixels.as_ptr(), &mut image) == 0 { // PixelFormat32bppARGB
        GdipDrawImageRectI(graphics, image.cast(), x + (size - shown_width) / 2, y + (size - shown_height) / 2, shown_width, shown_height);
        GdipDisposeImage(image.cast());
    }
    GdipDeleteGraphics(graphics);
}

/// Words in the person's language: Japanese where Windows is set to it.
pub fn japanese(options: &Options) -> bool {
    if options.test {
        if let Ok(language) = std::env::var("DOGMIND_SETUP_LANG") {
            return language == "ja";
        }
    }
    // SAFETY: it takes nothing, and only reads a setting.
    (unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() }) & 0x3FF == 0x11
}

/// Where a page puts the folder it shows.
struct FolderRow {
    label: RECT,
    field: RECT,
    change: Option<RECT>,
    notice: RECT,
}

/// What a page says and what can be done on it.
#[derive(Default, Clone)]
struct View {
    heading: String,
    detail: String,
    small: String,
    fraction: Option<f64>,
    primary: Option<String>,
    secondary: Option<String>,
    problem: bool,
    /// The folder, in a box of its own under the words, when this page is where it is shown; and the words of the button that changes it, when it can be.
    folder: Option<String>,
    change: Option<String>,
    /// Under the folder: what was done with the person's choice, or why it cannot be used (`true`).
    notice: Option<(String, bool)>,
}

/// What the first page says of the folder besides the folder.
enum Notice {
    /// What was chosen holds other things, so the install goes in a folder of its own inside it.
    InSubfolder,
    Refused(Refusal),
}

enum Message {
    Progress(Progress),
    Installed(bool),
    Failed(Failure),
    Stopped,
    Removed(Result<Vec<String>, String>),
}

#[derive(Clone, Copy, PartialEq)]
enum After {
    Install,
    OpenRemoval,
}

enum Page {
    /// Nothing can be done here: words and a way out.
    Refused(String, String),
    Ready { notice: Option<Notice> },
    CloseFirst { running: Vec<Running>, asked: bool, then: After },
    Closing { since: Instant, then: After },
    Working { progress: Option<Progress>, stopping: bool, update: bool },
    Failed { failure: Failure },
    Installed { update: bool },
    AppWontStart(String),
    Removing { fraction: f64 },
    Gone { left: Vec<String> },
    RemoveFailed(String),
}

struct App {
    options: Options,
    japanese: bool,
    version: String,
    previous: Option<String>,
    window: HWND,
    primary: HWND,
    secondary: HWND,
    change: HWND,
    /// An install was there when this began: it is updated where it is, and the folder is not for the person to change.
    fixed: bool,
    dpi: u32,
    colours: Colours,
    fonts: [HFONT; 3],
    page: Page,
    view: View,
    sender: Sender<Message>,
    messages: Receiver<Message>,
    stop: Arc<AtomicBool>,
    exit_code: i32,
    /// When the app's page is what removes it: nothing is left to do but leave.
    auto_tries: u32,
}

static APP: AtomicIsize = AtomicIsize::new(0);

unsafe fn app() -> Option<&'static mut App> {
    (APP.load(Ordering::Relaxed) as *mut App).as_mut()
}

fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

impl App {
    fn w(&self, english: &str, japanese: &str) -> String {
        (if self.japanese { japanese } else { english }).to_string()
    }

    fn folder(&self) -> &Path {
        &self.options.folder
    }

    fn notice_words(&self, notice: &Notice) -> String {
        match notice {
            Notice::InSubfolder => self.w("That folder is not empty, so it goes in a Dogmind folder inside it.", "選んだフォルダは空ではないため、その中の Dogmind フォルダにインストールします。"),
            Notice::Refused(Refusal::SettingsFolder) => self.w(
                "That is where the app keeps your settings and sessions (%LOCALAPPDATA%\\DogmindApp). Choose another folder.",
                "そこはアプリの設定とセッションの保存先 (%LOCALAPPDATA%\\DogmindApp) です。別のフォルダを選んでください。",
            ),
            Notice::Refused(Refusal::NotWritable(why)) => {
                if self.japanese {
                    format!("管理者権限なしでは、そこにファイルを作れません ({why})。ドキュメントや別のドライブなど、ご自分のフォルダを選んでください。")
                } else {
                    format!("No file can be made there without an administrator ({why}). Choose a folder of your own, in Documents or on another drive.")
                }
            }
        }
    }

    fn view_of_page(&self) -> View {
        let folder = self.folder().display().to_string();
        match &self.page {
            Page::Refused(heading, detail) => View { heading: heading.clone(), detail: detail.clone(), secondary: Some(self.w("Close", "閉じる")), problem: true, ..View::default() },
            Page::Ready { notice } => {
                let page = match &self.previous {
                    Some(previous) => View {
                        heading: if self.japanese { format!("Dogmind {previous} を {} に更新", self.version) } else { format!("Update Dogmind {previous} to {}", self.version) },
                        detail: self.w("This replaces the program in the folder below. Your settings, sessions and models are kept.", "下のフォルダのプログラムを置き換えます。設定、セッション、モデルはそのまま残ります。"),
                        primary: Some(self.w("Update", "更新")),
                        secondary: Some(self.w("Cancel", "キャンセル")),
                        ..View::default()
                    },
                    None => View {
                        heading: if self.japanese { format!("Dogmind {} をインストール", self.version) } else { format!("Install Dogmind {}", self.version) },
                        detail: self.w("It goes in the folder below, for you alone: no administrator is needed. It gets a place in the Start menu and in Installed apps.", "下のフォルダに、あなた専用としてインストールします。管理者権限は不要です。スタート メニューと「インストールされているアプリ」に登録します。"),
                        primary: Some(self.w("Install", "インストール")),
                        secondary: Some(self.w("Cancel", "キャンセル")),
                        ..View::default()
                    },
                };
                View {
                    folder: Some(folder),
                    change: (!self.fixed).then(|| self.w("Change...", "変更...")),
                    notice: notice.as_ref().map(|notice| (self.notice_words(notice), matches!(notice, Notice::Refused(_)))),
                    ..page
                }
            }
            Page::CloseFirst { running, asked, then } => {
                let mut names: Vec<&str> = running.iter().map(|one| one.name.as_str()).collect();
                names.sort_unstable();
                names.dedup();
                let names = names.join(", ");
                let goal = if *then == After::Install { self.w("before its files can be replaced", "ファイルを置き換える前に") } else { self.w("before it can be removed", "削除の前に") };
                View {
                    heading: self.w("Dogmind is open", "Dogmind が開いています"),
                    detail: if *asked {
                        if self.japanese { format!("閉じませんでした ({names})。作業中ならウィンドウにそう表示されています。終わったらトレイのアイコンから終了して、もう一度試してください。") } else { format!("It has not closed ({names}). If it is working, its window says so: quit it from the tray icon when it is done, then try again.") }
                    } else if self.japanese {
                        format!("{goal}閉じる必要があります ({names})。閉じるようアプリに依頼します。")
                    } else {
                        format!("It has to close {goal} ({names}). This asks it to.")
                    },
                    primary: Some(if *asked { self.w("Try again", "もう一度試す") } else { self.w("Ask it to close", "閉じるよう依頼") }),
                    secondary: Some(self.w("Cancel", "キャンセル")),
                    ..View::default()
                }
            }
            Page::Closing { .. } => View { heading: self.w("Waiting for it to close...", "閉じるのを待っています..."), secondary: Some(self.w("Cancel", "キャンセル")), ..View::default() },
            Page::Working { progress, stopping, update } => {
                let stage = progress.as_ref().map(|progress| progress.stage.clone());
                let small = progress.as_ref().map_or(String::new(), |progress| match progress.stage {
                    Stage::Checking => format!("{}: {}", self.w("Checking", "確認中"), progress.file),
                    Stage::Unpacking => format!("{}: {}", self.w("Unpacking", "展開中"), progress.file),
                    Stage::Placing => format!("{}: {}", self.w("Putting in place", "配置中"), progress.file),
                    Stage::Finishing => self.w("Finishing", "仕上げ中"),
                });
                let can_stop = !matches!(stage, Some(Stage::Placing | Stage::Finishing)) && !stopping;
                View {
                    heading: if *stopping { self.w("Stopping...", "中止しています...") } else if *update { self.w("Updating...", "更新しています...") } else { self.w("Installing...", "インストールしています...") },
                    small,
                    fraction: Some(progress.as_ref().map_or(0.0, |progress| progress.fraction)),
                    secondary: can_stop.then(|| self.w("Cancel", "キャンセル")),
                    ..View::default()
                }
            }
            Page::Failed { failure } => {
                let why = match failure {
                    Failure::InUse(file) => if self.japanese { format!("{file} が別のプログラムで使用中です。そのプログラムを閉じてください。") } else { format!("{file} is in use by another program. Close it.") },
                    Failure::TooLittleRoom { needed, free } => if self.japanese { format!("このドライブには {} の空きが必要で、{} しかありません。空きを作ってください。", gigabytes(*needed), gigabytes(*free)) } else { format!("This needs {} free on that drive, which has {}. Make room.", gigabytes(*needed), gigabytes(*free)) },
                    Failure::Said(words) => words.clone(),
                    Failure::Stopped => String::new(),
                };
                View {
                    heading: self.w("It did not finish", "完了できませんでした"),
                    detail: format!("{why} {}", self.w("What was done stays: trying again, or running this again, goes on from there.", "ここまでの作業は残っています。もう一度試すか、これをもう一度実行すれば続きから進みます。")),
                    primary: Some(self.w("Try again", "もう一度試す")),
                    secondary: Some(self.w("Close", "閉じる")),
                    problem: true,
                    ..View::default()
                }
            }
            Page::Installed { update } => View {
                heading: if *update { self.w("Dogmind is updated", "Dogmind を更新しました") } else { self.w("Dogmind is ready", "Dogmind の準備ができました") },
                detail: self.w("You can start it from the Start menu. To remove it: Settings > Apps > Installed apps, or Settings > General in the app.", "スタート メニューから起動できます。削除するには: Windows の設定 > アプリ > インストールされているアプリ、またはアプリの 設定 > 一般。"),
                primary: Some(self.w("Start it", "起動する")),
                secondary: Some(self.w("Close", "閉じる")),
                ..View::default()
            },
            Page::AppWontStart(why) => View {
                heading: self.w("The app cannot be started", "アプリを起動できません"),
                detail: if self.japanese { format!("設定や保存したキーの削除はアプリが行います ({why})。プログラムだけを削除すると、それらは %LOCALAPPDATA%\\DogmindApp と資格情報マネージャーに残ります。") } else { format!("It is the app that takes away its settings and saved keys ({why}). Removing only the program leaves those in %LOCALAPPDATA%\\DogmindApp and Credential Manager.") },
                primary: Some(self.w("Remove the program only", "プログラムだけ削除")),
                secondary: Some(self.w("Cancel", "キャンセル")),
                problem: true,
                ..View::default()
            },
            Page::Removing { fraction } => View { heading: self.w("Removing the program...", "プログラムを削除しています..."), fraction: Some(*fraction), ..View::default() },
            Page::Gone { left } => View {
                heading: self.w("Dogmind is removed", "Dogmind を削除しました"),
                detail: if left.is_empty() { String::new() } else if self.japanese { format!("{folder} には、これがインストールしたのではないものが残っています: {}", left.join(", ")) } else { format!("What it did not put there stays in {folder}: {}", left.join(", ")) },
                secondary: Some(self.w("Close", "閉じる")),
                ..View::default()
            },
            Page::RemoveFailed(words) => View {
                heading: self.w("Not everything could be removed", "すべては削除できませんでした"),
                detail: words.clone(),
                primary: Some(self.w("Try again", "もう一度試す")),
                secondary: Some(self.w("Close", "閉じる")),
                problem: true,
                ..View::default()
            },
        }
    }

    unsafe fn refresh(&mut self) {
        self.view = self.view_of_page();
        for (button, words) in [(self.primary, self.view.primary.clone()), (self.secondary, self.view.secondary.clone()), (self.change, self.view.change.clone())] {
            set_text(button, words.as_deref().unwrap_or(""));
            ShowWindow(button, if words.is_some() { SW_SHOWNA } else { SW_HIDE });
        }
        self.place_buttons();
        InvalidateRect(self.window, std::ptr::null(), 0);
        InvalidateRect(self.primary, std::ptr::null(), 0);
        InvalidateRect(self.secondary, std::ptr::null(), 0);
        InvalidateRect(self.change, std::ptr::null(), 0);
        if self.options.auto {
            self.press_by_itself();
        }
    }

    /// `--auto` under the test switch: what a person would press, so that a script needs no pointer or keyboard, and a code says how it ended.
    unsafe fn press_by_itself(&mut self) {
        match &self.page {
            // (a folder that was refused stays refused: nothing here chooses another, so it ends)
            Page::Ready { notice: Some(Notice::Refused(_)) } => self.finish(1),
            Page::Ready { .. } | Page::CloseFirst { asked: false, .. } | Page::AppWontStart(_) => {
                PostMessageW(self.window, WM_COMMAND, 1, 0);
            }
            Page::CloseFirst { asked: true, .. } => {
                self.auto_tries += 1;
                if self.auto_tries > 2 {
                    self.finish(3);
                } else {
                    PostMessageW(self.window, WM_COMMAND, 1, 0);
                }
            }
            Page::Failed { .. } | Page::RemoveFailed(_) | Page::Refused(..) => self.finish(1),
            Page::Installed { .. } | Page::Gone { .. } => self.finish(0),
            _ => {}
        }
    }

    unsafe fn finish(&mut self, code: i32) {
        self.exit_code = code;
        DestroyWindow(self.window);
    }

    unsafe fn place_buttons(&self) {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.window, &mut client);
        let s = |value: i32| scale(self.dpi, value);
        let (height, margin, gap) = (s(34), s(24), s(8));
        let y = client.bottom - margin - height;
        let mut right = client.right - margin;
        for (button, words) in [(self.secondary, &self.view.secondary), (self.primary, &self.view.primary)] {
            if let Some(words) = words {
                let width = self.text_width(self.fonts[2], words) + s(44);
                let width = width.max(s(96));
                SetWindowPos(button, std::ptr::null_mut(), right - width, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE);
                right -= width + gap;
            }
        }
        if self.view.change.is_some() {
            if let Some(at) = self.folder_row().change {
                SetWindowPos(self.change, std::ptr::null_mut(), at.left, at.top, at.right - at.left, at.bottom - at.top, SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
    }

    /// Where the folder goes on a page that shows it, under the words: its label, its box, the button that changes it beside the box, and the notice under both.
    unsafe fn folder_row(&self) -> FolderRow {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.window, &mut client);
        let s = |value: i32| scale(self.dpi, value);
        let (margin, gap, height) = (s(24), s(8), s(36));
        let device = GetDC(self.window);
        let top = self.draw_text(device, false);
        ReleaseDC(self.window, device);
        let label = RECT { left: margin, top, right: client.right - margin, bottom: top + s(22) };
        let mut field = RECT { left: margin, top: label.bottom, right: client.right - margin, bottom: label.bottom + height };
        let change = self.view.change.as_ref().map(|words| {
            let width = (self.text_width(self.fonts[2], words) + s(36)).max(s(96));
            let button = RECT { left: field.right - width, ..field };
            field.right = button.left - gap;
            button
        });
        let notice = RECT { left: margin, top: field.bottom + s(8), right: client.right - margin, bottom: client.bottom - s(60) };
        FolderRow { label, field, change, notice }
    }

    unsafe fn text_width(&self, font: HFONT, text: &str) -> i32 {
        let units: Vec<u16> = text.encode_utf16().collect();
        let device = GetDC(self.window);
        let previous = SelectObject(device, font);
        let mut size = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(device, units.as_ptr(), units.len() as i32, &mut size);
        SelectObject(device, previous);
        ReleaseDC(self.window, device);
        size.cx
    }

    unsafe fn make_fonts(&mut self) {
        let dpi = self.dpi as i32;
        let (face, strong) = if self.japanese { ("Yu Gothic UI", "Yu Gothic UI Semibold") } else { ("Segoe UI Variable Text", "Segoe UI Variable Text Semibold") };
        let make = |face: &str, points: i32, weight: i32| {
            let name = wide(face);
            CreateFontW(-(points * dpi / 72), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, name.as_ptr())
        };
        for font in self.fonts {
            if !font.is_null() {
                DeleteObject(font);
            }
        }
        self.fonts = [make(strong, 13, 600), make(face, 10, 400), make(strong, 10, 600)];
    }

    /// The heading beside the dog and its words under both, drawn (or, with `draw` false, only measured): where the page goes on from, in the window.
    unsafe fn draw_text(&self, device: HDC, draw: bool) -> i32 {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.window, &mut client);
        let colours = self.colours;
        let s = |value: i32| scale(self.dpi, value);
        let (margin, dog) = (s(24), s(44));
        let across = client.right - margin * 2;
        let left = margin + dog + s(14);
        let view = &self.view;
        let measuring = if draw { 0 } else { DT_CALCRECT };
        let mut y = margin;
        y += draw_words(device, &view.heading, RECT { left, top: y + s(4), right: left + across - dog - s(14), bottom: y + dog + s(8) }, self.fonts[0], if view.problem { colours.bad } else { colours.text }, DT_WORDBREAK | measuring);
        y = y.max(margin + dog + s(14));
        if !view.detail.is_empty() {
            y += draw_words(device, &view.detail, RECT { left: margin, top: y, right: margin + across, bottom: client.bottom - s(70) }, self.fonts[1], colours.muted, DT_WORDBREAK | DT_EDITCONTROL | measuring) + s(10);
        }
        y
    }

    unsafe fn paint(&self, device: HDC) {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.window, &mut client);
        let colours = self.colours;
        let s = |value: i32| scale(self.dpi, value);
        let brush = CreateSolidBrush(colours.background);
        FillRect(device, &client, brush);
        DeleteObject(brush);
        let margin = s(24);
        logo(device, margin, margin, s(44), colours.text);
        let across = client.right - margin * 2;
        let mut y = self.draw_text(device, true);
        let view = &self.view;
        if let Some(folder) = &view.folder {
            let row = self.folder_row();
            draw_words(device, &self.w("Install folder", "インストール先"), row.label, self.fonts[1], colours.muted, DT_SINGLELINE);
            rounded(device, row.field, s(8), Some(colours.raised), None);
            draw_words(device, folder, RECT { left: row.field.left + s(12), right: row.field.right - s(12), ..row.field }, self.fonts[1], colours.text, DT_SINGLELINE | DT_VCENTER | DT_PATH_ELLIPSIS);
            if let Some((words, problem)) = &view.notice {
                draw_words(device, words, row.notice, self.fonts[1], if *problem { colours.bad } else { colours.muted }, DT_WORDBREAK | DT_EDITCONTROL);
            }
        }
        if let Some(fraction) = view.fraction {
            let bar = RECT { left: margin, top: y + s(4), right: margin + across, bottom: y + s(10) };
            rounded(device, bar, s(3), Some(colours.raised), None);
            let filled = ((across as f64) * fraction.clamp(0.015, 1.0)) as i32;
            rounded(device, RECT { right: margin + filled, ..bar }, s(3), Some(colours.accent), None);
            y += s(20);
        }
        if !view.small.is_empty() {
            draw_words(device, &view.small, RECT { left: margin, top: y, right: margin + across, bottom: y + s(22) }, self.fonts[1], colours.muted, DT_SINGLELINE | DT_PATH_ELLIPSIS);
        }
    }

    unsafe fn paint_button(&self, item: &DRAWITEMSTRUCT) {
        let device = item.hDC;
        let area = item.rcItem;
        let colours = self.colours;
        let primary = item.hwndItem == self.primary;
        let pressed = item.itemState & ODS_SELECTED != 0;
        let brush = CreateSolidBrush(colours.background);
        FillRect(device, &area, brush);
        DeleteObject(brush);
        let radius = scale(self.dpi, 8);
        let disabled = item.itemState & ODS_DISABLED != 0;
        if primary {
            rounded(device, area, radius, Some(if pressed { colours.raised } else { colours.accent }), None);
        } else {
            rounded(device, area, radius, Some(if pressed { colours.raised } else { colours.background }), Some(colours.line));
        }
        if item.itemState & ODS_FOCUS != 0 {
            let ring = RECT { left: area.left + 2, top: area.top + 2, right: area.right - 2, bottom: area.bottom - 2 };
            rounded(device, ring, radius - 2, None, Some(if primary { colours.on_accent } else { colours.muted }));
        }
        let words = get_text(item.hwndItem);
        let colour = if disabled { colours.muted } else if primary && !pressed { colours.on_accent } else { colours.text };
        draw_words(device, &words, area, self.fonts[2], colour, DT_SINGLELINE | DT_CENTER | DT_VCENTER);
    }

    unsafe fn start_install(&mut self) {
        let update = self.previous.is_some();
        self.stop.store(false, Ordering::Relaxed);
        self.page = Page::Working { progress: None, stopping: false, update };
        self.refresh();
        let (sender, window, stop, options) = (self.sender.clone(), self.window as isize, self.stop.clone(), self.options.clone());
        std::thread::spawn(move || {
            let send = |message: Message| {
                let _ = sender.send(message);
                // SAFETY: a message to a window that has gone is simply lost.
                unsafe { PostMessageW(window as HWND, WM_WORKER, 0, 0) };
            };
            let exe = std::env::current_exe().unwrap_or_default();
            let mut package = match Package::open(&exe) {
                Ok(package) => package,
                Err(error) => return send(Message::Failed(Failure::Said(error.to_string()))),
            };
            let folder = options.folder.clone();
            let shell = crate::shell_for(&options, &folder);
            let stub = exe.clone();
            let write_uninstaller = move |path: &Path| crate::write_stub(&stub, path);
            let job = Install {
                folder: &folder,
                shell: shell.as_ref(),
                free: system::free_bytes(&folder),
                stop: &stop,
                test: options.test,
                write_uninstaller: &write_uninstaller,
                fail_after: options.fail_after,
                slowly: options.slowly,
            };
            let mut last = Instant::now() - Duration::from_secs(1);
            let result = install::install(&mut package, &job, &mut |progress| {
                if last.elapsed() >= Duration::from_millis(50) || progress.stage == Stage::Finishing {
                    last = Instant::now();
                    send(Message::Progress(progress));
                }
            });
            match result {
                Ok(_) => send(Message::Installed(update)),
                Err(Failure::Stopped) => {
                    install::clean_up_waiting(&package, &folder);
                    send(Message::Stopped);
                }
                Err(failure) => send(Message::Failed(failure)),
            }
        });
    }

    unsafe fn start_removal(&mut self) {
        self.page = Page::Removing { fraction: 0.0 };
        self.refresh();
        let (sender, window, options) = (self.sender.clone(), self.window as isize, self.options.clone());
        std::thread::spawn(move || {
            let send = |message: Message| {
                let _ = sender.send(message);
                // SAFETY: a message to a window that has gone is simply lost.
                unsafe { PostMessageW(window as HWND, WM_WORKER, 0, 0) };
            };
            // The app, and what it left running, have to be gone first; a person has seen it close.
            system::wait_for_end(&options.wait_pids, 30_000);
            let folder = options.folder.clone();
            let shell = crate::shell_for(&options, &folder);
            let result = install::remove(&folder, shell.as_ref(), &mut |_| {}).map(|removed| removed.left);
            send(Message::Removed(result));
        });
    }

    /// Who is running, and on to `then` if no one is.
    unsafe fn clear_the_way(&mut self, then: After) {
        let running = system::running_from(self.folder(), !self.options.test);
        if running.is_empty() {
            self.go_on(then);
        } else {
            self.page = Page::CloseFirst { running, asked: false, then };
            self.refresh();
        }
    }

    unsafe fn go_on(&mut self, then: After) {
        match then {
            After::Install => self.start_install(),
            After::OpenRemoval => self.open_the_apps_removal(),
        }
    }

    /// The app's own page does the work and the person's choosing: when it closes, what it leaves behind comes back to this program with `--after-remove`.
    unsafe fn open_the_apps_removal(&mut self) {
        let app = self.folder().join(&self.options.app_name);
        match system::start_program(&app, "--remove", self.options.test) {
            Ok(()) => self.finish(0),
            Err(error) => {
                self.page = Page::AppWontStart(error.to_string());
                self.refresh();
            }
        }
    }

    /// The standard folder picker, started in the folder that is chosen now. What it gives is the folder, or a `Dogmind` folder inside it if it holds other things; and that is where the
    /// install is, and what it is checked against, as soon as it is chosen, so that a person is told here and not after pressing Install.
    unsafe fn on_change(&mut self) {
        if self.fixed || !matches!(self.page, Page::Ready { .. }) {
            return;
        }
        // (the picker runs its own message loop, in which this window's timer and painting go on: nothing is held of `self` through it)
        let (window, start, title) = (self.window, self.folder().to_path_buf(), self.w("Choose the folder for Dogmind", "Dogmind のインストール先のフォルダを選択"));
        let Some(picked) = self.options.pick.clone().or_else(|| system::pick_folder(window, &start, &title)) else { return };
        let chosen = place::choose(&picked);
        let notice = match place::check(&chosen, &place::settings_folders()) {
            Err(refusal) => Some(Notice::Refused(refusal)),
            Ok(()) if chosen != picked => Some(Notice::InSubfolder),
            Ok(()) => None,
        };
        // (a folder that has an install in it is an update of that one)
        self.previous = install::read_manifest(&chosen).map(|manifest| manifest.version);
        self.options.folder = chosen;
        self.page = Page::Ready { notice };
        self.refresh();
    }

    unsafe fn on_primary(&mut self) {
        match &self.page {
            // Before anything is put there: this person can write in the folder, and it is not where their settings are.
            Page::Ready { .. } => match place::check(self.folder(), &place::settings_folders()) {
                Ok(()) => self.clear_the_way(After::Install),
                Err(refusal) => {
                    self.page = Page::Ready { notice: Some(Notice::Refused(refusal)) };
                    self.refresh();
                }
            },
            Page::CloseFirst { running, then, .. } => {
                let (pids, then) = (running.iter().map(|one| one.pid).collect::<Vec<u32>>(), *then);
                system::ask_to_close(&pids);
                self.page = Page::Closing { since: Instant::now(), then };
                self.refresh();
            }
            Page::Failed { .. } => self.clear_the_way(After::Install),
            Page::Installed { .. } => {
                let _ = system::start_program(&self.folder().join(&self.options.app_name), "", self.options.test);
                self.finish(0);
            }
            Page::AppWontStart(_) | Page::RemoveFailed(_) => self.start_removal(),
            _ => {}
        }
    }

    unsafe fn on_secondary(&mut self) {
        match &mut self.page {
            Page::Working { stopping, .. } => {
                *stopping = true;
                self.stop.store(true, Ordering::Relaxed);
                self.refresh();
            }
            Page::Removing { .. } => {}
            Page::Gone { .. } | Page::Installed { .. } => self.finish(0),
            Page::Failed { .. } | Page::RemoveFailed(_) | Page::Refused(..) => self.finish(1),
            _ => self.finish(2),
        }
    }

    unsafe fn on_timer(&mut self) {
        if let Page::Closing { since, then } = &self.page {
            let (since, then) = (*since, *then);
            let running = system::running_from(self.folder(), !self.options.test);
            if running.is_empty() {
                self.go_on(then);
            } else if since.elapsed() >= CLOSING {
                self.page = Page::CloseFirst { running, asked: true, then };
                self.refresh();
            }
        }
    }

    unsafe fn on_worker(&mut self) {
        while let Ok(message) = self.messages.try_recv() {
            match message {
                Message::Progress(new) => {
                    if let Page::Working { progress, .. } = &mut self.page {
                        *progress = Some(new);
                    }
                    self.refresh();
                }
                Message::Installed(update) => {
                    self.page = Page::Installed { update };
                    self.refresh();
                }
                Message::Failed(failure) => {
                    self.page = Page::Failed { failure };
                    self.refresh();
                }
                Message::Stopped => self.finish(2),
                Message::Removed(Ok(left)) => {
                    self.page = Page::Gone { left };
                    self.refresh();
                }
                Message::Removed(Err(words)) => {
                    self.page = Page::RemoveFailed(words);
                    self.refresh();
                }
            }
        }
    }
}

unsafe fn set_text(window: HWND, text: &str) {
    let units = wide(text);
    SetWindowTextW(window, units.as_ptr());
}

unsafe fn get_text(window: HWND) -> String {
    let mut units = [0u16; 256];
    let length = GetWindowTextW(window, units.as_mut_ptr(), units.len() as i32);
    String::from_utf16_lossy(&units[..length.max(0) as usize])
}

unsafe fn draw_words(device: HDC, text: &str, area: RECT, font: HFONT, color: u32, flags: u32) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let mut units: Vec<u16> = text.encode_utf16().collect();
    let mut target = area;
    SetBkMode(device, TRANSPARENT as _);
    SetTextColor(device, color);
    let previous = SelectObject(device, font);
    let tall = DrawTextW(device, units.as_mut_ptr(), units.len() as i32, &mut target, flags | DT_NOPREFIX);
    SelectObject(device, previous);
    tall
}

unsafe extern "system" fn procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    match message {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let screen = BeginPaint(window, &mut paint);
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(window, &mut client);
            let device = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, client.right, client.bottom);
            let previous = SelectObject(device, bitmap);
            app.paint(device);
            BitBlt(screen, 0, 0, client.right, client.bottom, device, 0, 0, SRCCOPY);
            SelectObject(device, previous);
            DeleteObject(bitmap);
            DeleteDC(device);
            EndPaint(window, &paint);
            0
        }
        WM_PRINTCLIENT => {
            app.paint(wparam as HDC);
            0
        }
        WM_DRAWITEM => {
            app.paint_button(&*(lparam as *const DRAWITEMSTRUCT));
            1
        }
        WM_COMMAND => {
            match wparam & 0xFFFF {
                1 => app.on_primary(),
                2 => app.on_secondary(),
                10 => app.on_change(),
                _ => {}
            }
            0
        }
        WM_WORKER => {
            app.on_worker();
            0
        }
        WM_TIMER => {
            app.on_timer();
            0
        }
        WM_CLOSE => {
            app.on_secondary();
            0
        }
        WM_DPICHANGED => {
            app.dpi = (wparam & 0xFFFF) as u32;
            app.make_fonts();
            let rect = &*(lparam as *const RECT);
            SetWindowPos(window, std::ptr::null_mut(), rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top, SWP_NOZORDER | SWP_NOACTIVATE);
            app.place_buttons();
            InvalidateRect(window, std::ptr::null(), 0);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// What the window opens on.
pub enum Start {
    Install { previous: Option<String>, version: String },
    Refuse(String, String),
    /// `--uninstall`: the way cleared, then the app's own page.
    Uninstall,
    /// What the app has left to do once its page is done: the program, from a copy of this outside its folder.
    FinishRemoval,
}

/// The window, until it is done: what the process should end with.
pub fn run(options: Options, start: Start) -> i32 {
    // SAFETY: on this one thread; `APP` is set once, before any message reaches the procedure by it, and used only on this thread.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let input = GdiplusStartupInput { GdiplusVersion: 1, DebugEventCallback: 0, SuppressBackgroundThread: 0, SuppressExternalCodecs: 0 };
        let mut token = 0usize;
        GdiplusStartup(&mut token, &input, std::ptr::null_mut());
        let light = if options.test { std::env::var("DOGMIND_SETUP_THEME").map_or_else(|_| windows_is_light(), |theme| theme == "light") } else { windows_is_light() };
        let colours = if light { LIGHT } else { DARK };
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("DogmindSetup");
        let background = CreateSolidBrush(colours.background);
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            hbrBackground: background,
            hIcon: LoadImageW(instance, 1 as *const u16, IMAGE_ICON, 64, 64, LR_DEFAULTCOLOR) as HICON,
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            ..std::mem::zeroed()
        };
        RegisterClassW(&class);
        let title = wide(if options.test { "Dogmind (test)" } else { "Dogmind" });
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let window = CreateWindowExW(0, class_name.as_ptr(), title.as_ptr(), style, CW_USEDEFAULT, CW_USEDEFAULT, 200, 100, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
        let app_is_a_test = options.test;
        let dpi = GetDpiForWindow(window);
        // (the first page has the folder on it, and what is said of it, under the words)
        let mut area = RECT { left: 0, top: 0, right: scale(dpi, 520), bottom: scale(dpi, if matches!(start, Start::Install { .. }) { 320 } else { 280 }) };
        AdjustWindowRectExForDpi(&mut area, style, 0, 0, dpi);
        // (a test keeps off the person's screen and out of their way, however it was started)
        if app_is_a_test {
            SetWindowPos(window, std::ptr::null_mut(), -20000, 0, area.right - area.left, area.bottom - area.top, SWP_NOZORDER | SWP_NOACTIVATE);
        } else {
            SetWindowPos(window, std::ptr::null_mut(), 0, 0, area.right - area.left, area.bottom - area.top, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        let dark = i32::from(colours.dark);
        DwmSetWindowAttribute(window, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, std::ptr::addr_of!(dark).cast(), 4);
        DwmSetWindowAttribute(window, DWMWA_CAPTION_COLOR as u32, std::ptr::addr_of!(colours.background).cast(), 4);
        let button_class = wide("BUTTON");
        let empty = wide("");
        let button = |id: isize| CreateWindowExW(0, button_class.as_ptr(), empty.as_ptr(), WS_CHILD | WS_TABSTOP | BS_OWNERDRAW as u32, 0, 0, 10, 10, window, id as HMENU, instance, std::ptr::null());
        // (made first, as the tab key goes in the order they are made: the folder's, then the page's)
        let change = button(CHANGE);
        let (primary, secondary) = (button(1), button(2));
        let (sender, messages) = channel();
        let japanese = japanese(&options);
        let (version, previous) = match &start {
            Start::Install { previous, version } => (version.clone(), previous.clone()),
            _ => (String::new(), None),
        };
        let page = match &start {
            Start::Install { .. } => Page::Ready { notice: None },
            Start::Refuse(heading, detail) => Page::Refused(heading.clone(), detail.clone()),
            Start::Uninstall | Start::FinishRemoval => Page::Removing { fraction: 0.0 },
        };
        let fixed = previous.is_some();
        let app = Box::into_raw(Box::new(App { options, japanese, version, previous, window, primary, secondary, change, fixed, dpi, colours, fonts: [std::ptr::null_mut(); 3], page, view: View::default(), sender, messages, stop: Arc::new(AtomicBool::new(false)), exit_code: 0, auto_tries: 0 }));
        APP.store(app as isize, Ordering::Relaxed);
        let app = &mut *app;
        app.make_fonts();
        ShowWindow(window, if app_is_a_test { SW_SHOWNOACTIVATE } else { SW_SHOWDEFAULT });
        SetTimer(window, TIMER_POLL, 250, None);
        match start {
            Start::Uninstall => app.clear_the_way(After::OpenRemoval),
            Start::FinishRemoval => app.start_removal(),
            _ => app.refresh(),
        }
        if !app_is_a_test && GetFocus().is_null() && app.view.primary.is_some() {
            SetFocus(app.primary);
        }
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(window, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        app.exit_code
    }
}
