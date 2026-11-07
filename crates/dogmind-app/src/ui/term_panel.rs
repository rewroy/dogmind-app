//! The Terminal view of the panel on the right: the person's own shells, as tabs, each a window drawn in the app's own paint. This file is the view's page and tabs, its one window (which
//! shows the terminal of the tab chosen, and takes the keys, the pointer and an IME), the menus, and what it asks of the rest of the app; the shells themselves are `term`.
//!
//! A terminal is the person's, with the person's own rights: nothing it shows goes to the model, and nothing here is given to the agent.

use std::time::Instant;

use super::files_panel::{MenuFor, MenuItem};
use super::*;
use crate::render::{self, TermCursor, TermCursorShape, TermFont, TermGround, TermLine, TermLineKind, TermRun, TermScene};
use crate::term::keymap::{self, Keyed, Mods};
use crate::term::palette;
use crate::term::screen::{Paint, SelectBy, Shape as CursorShape, Snapshot, Underline};
use crate::term::shells::{self, Shell};
use crate::term::{self, Terminal, Wake, MOST_TERMINALS};
use windows_sys::Win32::UI::Input::Ime::*;

pub(super) const TERM_CLASS: &str = "DogTerm";

// What a test of the running app does to the terminals (a debug build, told to).
#[cfg(debug_assertions)]
mod hooks;
#[cfg(debug_assertions)]
pub(super) use hooks::{hooks_on, HOOK};

#[cfg(test)]
mod tests;

mod access;

/// What a reader of the screen is given: the terminal that is shown, as text.
unsafe fn document() -> access::Document {
    match app() {
        Some(app) => app.terminal_document(),
        None => access::Document::default(),
    }
}

/// The strip of tabs is this high (unscaled pixels), and a tab at most this wide, and at least this wide while there is room.
const TAB_HEIGHT: i32 = 28;
const TAB_MOST: i32 = 150;
const TAB_CLOSE: i32 = 22;
/// The cells keep this far from the window's edge.
const PAD: i32 = 6;
/// The face, the first of these that this PC has.
const FACES: [&str; 2] = ["Cascadia Mono", "Consolas"];
/// How many lines a notch of the wheel goes back.
const WHEEL_LINES: i32 = 3;
const TIMER_SCROLL: usize = 1;
const TIMER_ACCESS: usize = 2;

/// What a key of the window is, as `take_key` has it: the terminal's and taken, left to the system to make a character of, or the window's own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TermKey {
    Taken,
    Dispatch,
    Pass,
}

/// The windows of the Terminal view, and the terminals themselves.
pub(super) struct TermView {
    pub window: HWND,
    tab_buttons: Vec<HWND>,
    close_buttons: Vec<HWND>,
    new_button: HWND,
    pub shells_button: HWND,
    start_button: HWND,
    pub start_shells: HWND,
    again_button: HWND,
    pub rename_field: HWND,
    terminals: Vec<Terminal>,
    active: Option<u64>,
    next_id: u64,
    renaming: Option<u64>,
    font: Option<TermFont>,
    font_dpi: u32,
    /// How many cells the window has, which a terminal started now begins with.
    cells: (u16, u16),
    /// The shells the menu that is open lists, in its order.
    shells_listed: Vec<Shell>,
    /// What the tip of each tab says, as last told to the tool tips.
    tab_tips: Vec<String>,
    /// The first half of a character that is two UTF-16 units.
    high_surrogate: Option<u16>,
    selecting: bool,
    clicks: (Option<Instant>, (usize, usize), u8),
    tab_clicked: Option<(u64, Instant)>,
    /// A test's focus: what the window draws as it has, with the real one not touched.
    forced_focus: bool,
    /// Where the window stands in its parent, last.
    area: (i32, i32, i32, i32),
    /// What was last drawn: the terminal, and a sum of the cells (for a test to say that what is on screen is what the screen holds).
    painted: (u64, u64),
    /// The window's element for a reader of the screen, once one has asked, and what it was last told: a sum of the text and where the cursor was; a moment is being waited for.
    element: Option<windows::Win32::UI::Accessibility::IRawElementProviderSimple>,
    told: (u64, usize),
    told_soon: bool,
}

impl TermView {
    pub(super) unsafe fn new(parent: HWND, backdrop: u32) -> TermView {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide(TERM_CLASS);
        let class = WNDCLASSW { style: CS_DBLCLKS, lpfnWndProc: Some(term_procedure), hInstance: instance, lpszClassName: class_name.as_ptr(), hCursor: LoadCursorW(std::ptr::null_mut(), IDC_IBEAM), ..std::mem::zeroed() };
        RegisterClassW(&class);
        let title = wide("Terminal");
        let window = CreateWindowExW(0, class_name.as_ptr(), title.as_ptr(), WS_CHILD | WS_CLIPSIBLINGS | WS_TABSTOP, 0, 0, 0, 0, parent, std::ptr::null_mut(), instance, std::ptr::null());
        let button = |id: isize, text: &str, kind: ButtonKind| make_button(parent, id, text, kind, backdrop);
        TermView {
            window,
            tab_buttons: (0..MOST_TERMINALS as isize).map(|at| button(ID_TERM_TAB + at, "", ButtonKind::Segment)).collect(),
            close_buttons: (0..MOST_TERMINALS as isize).map(|at| button(ID_TERM_CLOSE + at, "x", ButtonKind::Icon)).collect(),
            new_button: button(ID_TERM_NEW, "plus", ButtonKind::Icon),
            shells_button: button(ID_TERM_SHELLS, "chevron-down", ButtonKind::Icon),
            start_button: button(ID_TERM_START, "", ButtonKind::Primary),
            start_shells: button(ID_TERM_START_SHELLS, "chevron-down|", ButtonKind::Secondary),
            again_button: button(ID_TERM_AGAIN, "", ButtonKind::Secondary),
            rename_field: control(parent, "EDIT", "", WS_BORDER | ES_AUTOHSCROLL as u32, 0, ID_TERM_RENAME),
            terminals: Vec::new(),
            active: None,
            next_id: 1,
            renaming: None,
            font: None,
            font_dpi: 0,
            cells: (80, 24),
            shells_listed: Vec::new(),
            tab_tips: Vec::new(),
            high_surrogate: None,
            selecting: false,
            clicks: (None, (0, 0), 0),
            tab_clicked: None,
            forced_focus: false,
            area: (0, 0, 0, 0),
            painted: (0, 0),
            element: None,
            told: (0, 0),
            told_soon: false,
        }
    }

    /// Every window of the view, but the one the keys go to.
    fn others(&self) -> Vec<HWND> {
        let mut all = self.tab_buttons.clone();
        all.extend(&self.close_buttons);
        all.extend([self.new_button, self.shells_button, self.start_button, self.start_shells, self.again_button]);
        all
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.terminals.iter().position(|terminal| terminal.id == id)
    }

    fn active_terminal(&self) -> Option<&Terminal> {
        self.active.and_then(|id| self.terminals.iter().find(|terminal| terminal.id == id))
    }

    /// Which tab a button is, when it is one.
    pub(super) fn tab_of(&self, window: HWND) -> Option<usize> {
        self.tab_buttons.iter().position(|button| *button == window).filter(|at| *at < self.terminals.len())
    }

    /// The ids of the terminals, in the order of the tabs.
    pub(super) fn ids(&self) -> Vec<u64> {
        self.terminals.iter().map(|terminal| terminal.id).collect()
    }

}

/// Whether a key opens a terminal: Ctrl and the backquote, with Shift where the layout has the backquote on a key of its own, and not with Alt (Ctrl and Alt together are AltGr).
pub(super) fn opens_a_terminal(vk: u16, mods: Mods) -> bool {
    mods.ctrl && !mods.alt && vk == VK_OEM_3
}

/// What the terminal's ground is: the panel's own, so that it is of the theme.
fn ground() -> u32 {
    theme::sidebar()
}

/// How a shell that has ended says it did: the code as a number, and as a status where it is one (a program that was stopped or crashed gives such).
fn exit_words(code: u32) -> String {
    if code >= 0xC000_0000 { tf("Exited with code 0x{0}", &[&format!("{code:08X}")]) } else { tf("Exited with code {0}", &[&code]) }
}

impl App {
    /// The words of what has only an icon, when the pointer rests on it, and the name of the terminal's window, in the language now.
    pub(super) unsafe fn terminal_words(&self) {
        set_text(self.term.window, t("Terminal"));
        self.tip(self.term.new_button, t("New terminal"));
        for window in [self.term.shells_button, self.term.start_shells] {
            self.tip(window, t("Choose a shell"));
        }
        for window in &self.term.close_buttons {
            self.tip(*window, t("Close the terminal"));
        }
    }

    /// The panel shows the Terminal view.
    pub(super) fn terminal_shown(&self) -> bool {
        self.view_shown(RightView::Terminal)
    }

    /// What `look` makes of the terminal that is shown on its tab, when there is one. The terminal is borrowed for no longer than that, so that what comes of it can be acted on.
    fn with_active<R>(&self, look: impl FnOnce(&Terminal) -> R) -> Option<R> {
        self.term.active_terminal().map(look)
    }

    /// The face for this screen's scale: made again when the scale changes.
    unsafe fn terminal_font(&mut self) -> Option<TermFont> {
        if self.term.font.is_none() || self.term.font_dpi != self.dpi {
            self.term.font = render::terminal_font(&FACES, ds::points::CODE, self.dpi);
            self.term.font_dpi = self.dpi;
        }
        self.term.font.clone()
    }

    /// Nothing of the view is on screen: its windows go, and the terminals go on.
    pub(super) unsafe fn terminal_hidden(&mut self) {
        if matches!(self.panel_menu.whose, MenuFor::Shells | MenuFor::Terminal) {
            self.files_menu_close(false);
        }
        if self.term.renaming.is_some() {
            self.terminal_rename_end(true);
        }
        let had_the_keys = GetFocus() == self.term.window;
        show(self.term.window, false);
        show(self.term.rename_field, false);
        for window in self.term.others() {
            show(window, false);
        }
        if had_the_keys {
            self.focus_composer();
        }
    }

    /// The Terminal view, from `top` to the foot of the window: the page that says what it is while there is no terminal, and otherwise the tabs, the terminal under them, and, for one that
    /// has ended, what it ended with and the way to a new one.
    pub(super) unsafe fn layout_terminal_view(&mut self, shapes: &mut Vec<Shape>, x: i32, width: i32, top: i32, bottom: i32) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let font = self.terminal_font();
        let margin = s(ds::size::RIGHT_GAP);
        // (as wide as the files' list: a little way from the panel's edge)
        let (left, span) = (x - margin + s(ds::space::S), width + 2 * margin - 2 * s(ds::space::S));
        let strip_top = top + s(ds::space::XS);
        let window_top = strip_top + s(TAB_HEIGHT) + s(ds::space::S);
        let mut window_bottom = bottom - s(ds::space::S);
        let over = self.term.active_terminal().filter(|terminal| terminal.is_over()).map(|terminal| terminal.exit_code().unwrap_or(0));
        let bar = if over.is_some() { s(ds::size::HEAD) } else { 0 };
        window_bottom -= bar;
        if let Some(font) = &font {
            // (what a terminal started now begins with: as many cells as the window will have, with a strip and no bar)
            self.term.cells = cells_in(span, bottom - s(ds::space::S) - window_top, font, dpi);
        }
        if self.term.terminals.is_empty() {
            show(self.term.window, false);
            for window in self.term.tab_buttons.clone().into_iter().chain(self.term.close_buttons.clone()).chain([self.term.new_button, self.term.shells_button, self.term.again_button]) {
                show(window, false);
            }
            self.layout_terminal_page(shapes, x, width, top, bottom);
            return;
        }
        show(self.term.start_button, false);
        show(self.term.start_shells, false);

        // The tabs, and at their end a new terminal and the shells to choose from.
        let (plus, arrow) = (s(ds::size::ICON_BUTTON), s(ds::size::ICON_BUTTON) - s(ds::space::XS));
        let tabs_room = (width - plus - arrow - s(ds::space::XS)).max(s(60));
        let count = self.term.terminals.len() as i32;
        // With room, each tab has its words and its cross. Without, the one shown keeps its words and its cross and the others share what is left, as far as their first letters.
        let roomy = tabs_room / count >= s(72);
        let shown_width = (tabs_room - (count - 1) * s(36)).clamp(s(96), s(TAB_MOST));
        let other_width = ((tabs_room - shown_width) / (count - 1).max(1)).max(s(24));
        let mut at = x;
        let tabs: Vec<(u64, String, String)> = self.term.terminals.iter().map(|terminal| (terminal.id, terminal.name.clone(), format!("{}\n{}", terminal.name, terminal.folder.display()))).collect();
        for (index, (id, name, tip)) in tabs.iter().enumerate() {
            let on = Some(*id) == self.term.active;
            let closes = roomy || on;
            let button = self.term.tab_buttons[index];
            let close = self.term.close_buttons[index];
            let tab = if roomy { (tabs_room / count).min(s(TAB_MOST)) } else if on { shown_width } else { other_width };
            let face = if closes { tab - s(TAB_CLOSE) } else { tab };
            place(button, at, strip_top, face, s(TAB_HEIGHT));
            set_button_style(button, if on { ButtonKind::SegmentOn } else { ButtonKind::Segment }, ground());
            set_text_if_changed(button, name);
            // (the whole name, where the tab cannot show it, and the folder the shell was started in: only a change is passed on, as a tip begun stays while the window is laid out again under it)
            if self.term.tab_tips.get(index) != Some(tip) {
                self.term.tab_tips.resize(index + 1, String::new());
                self.term.tab_tips[index] = tip.clone();
                self.tip(button, tip);
            }
            show(button, true);
            if closes {
                place(close, at + face, strip_top, s(TAB_CLOSE), s(TAB_HEIGHT));
                set_button_style(close, ButtonKind::Icon, ground());
                show(close, true);
            } else {
                show(close, false);
            }
            at += tab;
        }
        for index in tabs.len()..MOST_TERMINALS {
            show(self.term.tab_buttons[index], false);
            show(self.term.close_buttons[index], false);
        }
        let (new_button, shells_button) = (self.term.new_button, self.term.shells_button);
        place(new_button, x + width - plus - arrow, strip_top, plus, s(TAB_HEIGHT));
        place(shells_button, x + width - arrow, strip_top, arrow, s(TAB_HEIGHT));
        for button in [new_button, shells_button] {
            set_button_style(button, ButtonKind::Icon, ground());
            show(button, true);
        }

        // The terminal itself.
        let rect = (left, window_top, span, (window_bottom - window_top).max(s(60)));
        if self.term.area != rect || IsWindowVisible(self.term.window) == 0 {
            self.term.area = rect;
            place(self.term.window, rect.0, rect.1, rect.2, rect.3);
            show(self.term.window, true);
        }
        if let Some(font) = &font {
            let (cols, rows) = cells_in(rect.2, rect.3, font, dpi);
            self.terminal_fit(cols, rows, font);
        }
        // (what a terminal that has ended says, under it, with the way to another)
        if let Some(code) = over {
            let bar_top = window_bottom + s(ds::space::XS);
            let words = exit_words(code);
            let button_words = t("New terminal");
            let button_width = (text_width(self.fonts.strong, button_words) + s(ds::size::BUTTON_PADDING)).min(width / 2);
            set_text_if_changed(self.term.again_button, button_words);
            place(self.term.again_button, x + width - button_width, bar_top + (bar - s(ds::space::XS) - s(ds::size::CONTROL)) / 2, button_width, s(ds::size::CONTROL));
            show(self.term.again_button, true);
            shapes.push(Shape::Text { area: rect_of(x, bar_top, width - button_width - s(ds::space::S), bar - s(ds::space::XS)), text: words, font: FontKind::Small, color: theme::muted(), flags: ONE_LINE });
        } else {
            show(self.term.again_button, false);
        }
    }

    /// The page of a view with no terminal in it: what it is, how to begin one, and, quietly, what it is not.
    unsafe fn layout_terminal_page(&mut self, shapes: &mut Vec<Shape>, x: i32, width: i32, top: i32, bottom: i32) {
        let s = |value: i32| scale(self.dpi, value);
        let mut y = top + s(ds::space::XL);
        shapes.push(Shape::Icon { name: "terminal", area: rect(x, y, s(ds::size::ICON_LARGE), s(ds::size::ICON_LARGE)), size: s(ds::size::ICON_LARGE), color: theme::muted() });
        y += s(ds::size::ICON_LARGE) + s(ds::space::S);
        shapes.push(Shape::Text { area: rect(x, y, width, s(ds::space::XL)), text: t("Terminal").to_string(), font: FontKind::Strong, color: theme::text(), flags: ONE_LINE });
        y += s(ds::size::QUEUE_STEP);
        let about = t("Your own shell, in this session's folder.");
        let tall = text_height(self.fonts.small, about, width);
        shapes.push(Shape::Text { area: rect(x, y, width, tall), text: about.to_string(), font: FontKind::Small, color: theme::muted(), flags: DT_WORDBREAK });
        y += tall + s(ds::space::L);
        // The button for a new terminal, in the shell last used, and the arrow beside it for the others.
        let words = t("New terminal");
        let button_width = (text_width(self.fonts.strong, words) + s(ds::size::BUTTON_PADDING)).min(width - s(ds::size::CONTROL) - s(ds::space::XS));
        set_text_if_changed(self.term.start_button, words);
        place(self.term.start_button, x, y, button_width, s(ds::size::CONTROL));
        place(self.term.start_shells, x + button_width + s(ds::space::XS), y, s(ds::size::CONTROL), s(ds::size::CONTROL));
        set_button_style(self.term.start_button, ButtonKind::Primary, ground());
        set_button_style(self.term.start_shells, ButtonKind::Secondary, ground());
        show(self.term.start_button, true);
        show(self.term.start_shells, true);
        // At the foot of the page, quietly: what these commands are, and that the model does not see them.
        let rights = t("Commands here run with your own rights, apart from the agent's permissions. Nothing shown here is sent to the model.");
        let tall = text_height(self.fonts.small, rights, width);
        let at = (bottom - s(ds::space::L) - tall).max(y + s(ds::size::CONTROL) + s(ds::space::L));
        shapes.push(Shape::Text { area: rect(x, at, width, tall), text: rights.to_string(), font: FontKind::Small, color: theme::faint(), flags: DT_WORDBREAK });
    }

    /// The window now has `cols` by `rows` cells: every terminal is told, the one that is not shown as well, so that each is as it will be when it is.
    unsafe fn terminal_fit(&mut self, cols: u16, rows: u16, font: &TermFont) {
        self.term.cells = (cols, rows);
        for terminal in &self.term.terminals {
            terminal.with_screen(|screen| screen.set_cell_pixels(font.cell_width as u16, font.cell_height as u16));
            terminal.resize(cols, rows);
        }
    }

    // ---- Starting, choosing and closing.

    /// The view is on screen, as `/terminal` and a new terminal want it: the conversation, the panel there, and the Terminal view in it.
    pub(super) unsafe fn show_terminal_view(&mut self) -> bool {
        if self.panel != Panel::Chat {
            self.set_panel(Panel::Chat);
        }
        if self.preview_file.is_some() {
            self.close_preview();
        }
        if !self.settings.right_panel {
            self.settings.right_panel = true;
        }
        self.settings.right_view = RightView::Terminal;
        self.save();
        self.layout();
        if self.right_width == 0 {
            self.say(t("The window is too narrow for the panel: make it wider."));
            return false;
        }
        true
    }

    /// `/terminal` and Ctrl+`: the view, with a terminal in it: the one there is, or a new one when there is none.
    pub(super) unsafe fn terminal_command(&mut self) {
        if self.show_terminal_view() && self.term.terminals.is_empty() {
            self.terminal_new(None);
        } else if self.terminal_shown() {
            SetFocus(self.term.window);
        }
    }

    /// A new terminal, in the shell named, or in the one last chosen where none is. Its tab is shown and has the keys.
    pub(super) unsafe fn terminal_new(&mut self, shell: Option<&str>) -> Option<u64> {
        if self.term.terminals.len() >= MOST_TERMINALS {
            self.say(&tf("{0} terminals are open: close one to open another.", &[&MOST_TERMINALS]));
            return None;
        }
        if !self.show_terminal_view() {
            return None;
        }
        // (looked for when it is wanted: a shell installed since is there)
        let installed = shells::discover(&shells::Places::from_env());
        let chosen = match shell {
            Some(id) => installed.iter().find(|candidate| candidate.id == id),
            None => shells::chosen(&installed, &self.settings.terminal_shell),
        }
        .cloned();
        let Some(chosen) = chosen else {
            self.say(t("No shell was found on this PC."));
            return None;
        };
        if self.settings.terminal_shell != chosen.id {
            self.settings.terminal_shell = chosen.id.to_string();
            self.save();
        }
        let folder = term::start_folder(&self.workspace, term::home_folder().as_deref());
        let id = self.term.next_id;
        let window = self.window as isize;
        let wake = Arc::new(move |id: u64, what: Wake| {
            // SAFETY: posting to a window that has gone fails and does nothing.
            unsafe { PostMessageW(window as HWND, WM_TERM, id as WPARAM, what as LPARAM) };
        });
        let (cols, rows) = self.term.cells;
        match Terminal::start(id, chosen, &folder, cols, rows, wake) {
            Ok(terminal) => {
                self.term.next_id += 1;
                terminal.with_screen(|screen| screen.set_theme(theme::is_light(), theme::text(), ground()));
                self.term.terminals.push(terminal);
                self.term.active = Some(id);
                self.layout();
                SetFocus(self.term.window);
                InvalidateRect(self.term.window, std::ptr::null(), 0);
                Some(id)
            }
            Err(error) => {
                self.say(&tf("The shell could not be started: {0}", &[&error]));
                None
            }
        }
    }

    /// The tab of this terminal is the one shown, and has the keys.
    pub(super) unsafe fn terminal_select(&mut self, id: u64) {
        if self.term.index_of(id).is_none() {
            return;
        }
        if self.term.active != Some(id) {
            self.term.active = Some(id);
            self.term.selecting = false;
            self.layout();
        }
        InvalidateRect(self.term.window, std::ptr::null(), 0);
        SetFocus(self.term.window);
    }

    /// The tab is closed, and with it the shell and all it started.
    pub(super) unsafe fn terminal_close(&mut self, id: u64) {
        let Some(at) = self.term.index_of(id) else { return };
        if self.term.renaming == Some(id) {
            self.terminal_rename_end(false);
        }
        let gone = self.term.terminals.remove(at);
        if self.term.active == Some(id) {
            self.term.active = self.term.terminals.get(at.min(self.term.terminals.len().saturating_sub(1))).map(|terminal| terminal.id);
        }
        // (its job closes here: the shell and what it started end)
        drop(gone);
        let had_the_keys = GetFocus() == self.term.window;
        self.layout();
        InvalidateRect(self.term.window, std::ptr::null(), 0);
        if self.term.terminals.is_empty() {
            self.focus_composer();
        } else if had_the_keys {
            SetFocus(self.term.window);
        }
    }

    /// The app is ending: every terminal ends, with all that it started.
    pub(super) fn terminals_end(&mut self) {
        self.term.terminals.clear();
        self.term.active = None;
    }

    /// The menu of shells, under the arrow beside the new terminal: what this PC has, with the one last chosen marked. Discovery is of the moment; it starts nothing.
    pub(super) unsafe fn terminal_shells_menu(&mut self, anchor: HWND) {
        let installed = shells::discover(&shells::Places::from_env());
        if installed.is_empty() {
            self.say(t("No shell was found on this PC."));
            return;
        }
        let marked = shells::chosen(&installed, &self.settings.terminal_shell).map(|shell| shell.id);
        let items: Vec<MenuItem> = installed.iter().map(|shell| MenuItem { checked: Some(shell.id) == marked, ..MenuItem::new(t(shell.name)) }).collect();
        self.term.shells_listed = installed;
        // (under the arrow, where the menu of the others is)
        let (left, top, _, height) = child_area(anchor, self.window);
        self.panel_menu_show(items, MenuFor::Shells, (left, top + height));
    }

    /// A shell is chosen from the menu: it is remembered, and opened.
    pub(super) unsafe fn terminal_shell_chosen(&mut self, at: usize) {
        if let Some(id) = self.term.shells_listed.get(at).map(|shell| shell.id) {
            self.terminal_new(Some(id));
        }
    }

    // ---- Names.

    /// The name of a tab is edited where it stands.
    pub(super) unsafe fn terminal_rename_begin(&mut self, id: u64) {
        let Some(at) = self.term.index_of(id) else { return };
        let (x, y, width, height) = child_area(self.term.tab_buttons[at], self.window);
        self.term.renaming = Some(id);
        SendMessageW(self.term.rename_field, EM_SETLIMITTEXT, 40, 0);
        set_text(self.term.rename_field, &self.term.terminals[at].name);
        SetWindowPos(self.term.rename_field, HWND_TOP, x, y, width.max(scale(self.dpi, 96)), height, SWP_SHOWWINDOW);
        SetFocus(self.term.rename_field);
        SendMessageW(self.term.rename_field, EM_SETSEL, 0, -1);
    }

    /// The name is kept as it was typed (an empty one is the shell's name again), or what was typed is put away.
    pub(super) unsafe fn terminal_rename_end(&mut self, keep: bool) {
        let Some(id) = self.term.renaming.take() else { return };
        let typed = get_text(self.term.rename_field).trim().to_string();
        show(self.term.rename_field, false);
        let had_the_keys = GetFocus() == self.term.rename_field;
        if keep {
            if let Some(at) = self.term.index_of(id) {
                let terminal = &mut self.term.terminals[at];
                terminal.renamed = !typed.is_empty();
                terminal.name = if typed.is_empty() { t(terminal.shell.name).to_string() } else { typed };
            }
        }
        self.layout();
        if had_the_keys && self.terminal_shown() {
            SetFocus(self.term.window);
        }
    }

    /// F2 on a tab that has the keys: its name is edited.
    pub(super) unsafe fn terminal_rename_tab_at(&mut self, index: usize) {
        if let Some(id) = self.term.terminals.get(index).map(|terminal| terminal.id) {
            self.terminal_rename_begin(id);
        }
    }

    /// A press on a tab: it is chosen, and a second press soon after is a rename.
    pub(super) unsafe fn terminal_tab_pressed(&mut self, index: usize) {
        let Some(id) = self.term.terminals.get(index).map(|terminal| terminal.id) else { return };
        let twice = self.term.tab_clicked.is_some_and(|(was, at)| was == id && at.elapsed().as_millis() <= u128::from(GetDoubleClickTime()));
        self.term.tab_clicked = (!twice).then(|| (id, Instant::now()));
        if twice {
            self.terminal_rename_begin(id);
        } else {
            self.terminal_select(id);
        }
    }

    // ---- What the terminal tells the window.

    /// A terminal has written something, or has ended.
    pub(super) unsafe fn terminal_wake(&mut self, id: u64, ended: bool) {
        let Some(at) = self.term.index_of(id) else { return };
        let bell = {
            let terminal = &self.term.terminals[at];
            terminal.looked();
            terminal.with_screen(|screen| screen.take_bell())
        };
        if !self.terminal_shown() || self.term.active != Some(id) {
            return;
        }
        if bell {
            windows_sys::Win32::System::Diagnostics::Debug::MessageBeep(0);
        }
        if ended {
            // (the bar that says what it ended with)
            self.layout();
        }
        InvalidateRect(self.term.window, std::ptr::null(), 0);
        self.terminal_changed();
    }

    // ---- Keys.

    /// A key pressed while the terminal has the keys. `Taken`: it was this terminal's, and is done; `Dispatch`: it makes a character, which the system will send as a message (so that
    /// layouts, dead keys, AltGr and an IME are its); `Pass`: it is the window's own (the next session, Alt+F4), and goes on to be that.
    pub(super) unsafe fn terminal_key(&mut self, message: &MSG) -> TermKey {
        let vk = message.wParam as u16;
        let held = |key: u16| GetKeyState(i32::from(key)) < 0;
        let mods = Mods { shift: held(VK_SHIFT), ctrl: held(VK_CONTROL), alt: held(VK_MENU) };
        // What stays the window's. Ctrl+` is a new terminal, wherever the keys are.
        if opens_a_terminal(vk, mods) {
            self.terminal_command_new();
            return TermKey::Taken;
        }
        if (vk == VK_TAB && mods.ctrl) || (mods.ctrl && !mods.shift && !mods.alt && (0x31..=0x39).contains(&vk)) {
            return TermKey::Pass;
        }
        if mods.alt && matches!(vk, VK_F4 | VK_SPACE | VK_TAB | VK_ESCAPE) {
            return TermKey::Dispatch;
        }
        // F6 is the way out of the terminal by keys: every other key is the shell's.
        if vk == VK_F6 && !mods.ctrl && !mods.alt {
            self.focus_composer();
            return TermKey::Taken;
        }
        let Some((over, modes, selected, rows)) = self.with_active(|terminal| {
            let (modes, selected, size) = terminal.with_screen(|screen| (screen.modes(), screen.has_selection(), screen.size()));
            (terminal.exit_code().is_some(), modes, selected, size.1)
        }) else {
            return TermKey::Taken;
        };
        // (one that has ended takes no keys: it never starts again by itself)
        if over {
            return TermKey::Taken;
        }
        // Copy and paste: Ctrl+C copies when something is selected and is the shell's ^C when not; Ctrl+V and Shift+Insert paste; with Shift, Ctrl+C is always a copy.
        let copy_key = (mods.ctrl && vk == 0x43 && (mods.shift || selected)) || (mods.ctrl && !mods.shift && vk == VK_INSERT);
        if copy_key {
            if selected {
                self.terminal_copy();
            }
            return TermKey::Taken;
        }
        if (mods.ctrl && vk == 0x56 && !mods.alt) || (mods.shift && !mods.ctrl && vk == VK_INSERT) {
            self.terminal_paste(None);
            return TermKey::Taken;
        }
        // Scrolling back: Shift with the page keys, as in other terminals, except where the program has the whole screen.
        if mods.shift && !mods.ctrl && !mods.alt && matches!(vk, VK_PRIOR | VK_NEXT) && !modes.alternate_screen {
            let page = i32::from(rows) - 1;
            self.with_active(|terminal| terminal.with_screen(|screen| screen.scroll(if vk == VK_PRIOR { page } else { -page })));
            InvalidateRect(self.term.window, std::ptr::null(), 0);
            return TermKey::Taken;
        }
        match keymap::key(vk, mods, modes.application_cursor) {
            Keyed::Send(bytes) => {
                self.terminal_send(&bytes);
                TermKey::Taken
            }
            Keyed::Text => TermKey::Dispatch,
            Keyed::Nothing => TermKey::Taken,
        }
    }

    /// Bytes to the shell of the terminal that is shown, as the person typed them: what was scrolled back goes to the foot, and what was selected is no longer.
    unsafe fn terminal_send(&mut self, bytes: &[u8]) {
        let sent = self.with_active(|terminal| {
            if terminal.exit_code().is_some() {
                return false;
            }
            terminal.with_screen(|screen| {
                screen.scroll_to_foot();
                screen.select_clear();
            });
            terminal.write(bytes);
            true
        });
        if sent == Some(true) {
            InvalidateRect(self.term.window, std::ptr::null(), 0);
        }
    }

    /// A character the system made of a key, or an IME's result: to the shell as UTF-8, and with Alt an escape before it.
    pub(super) unsafe fn terminal_char(&mut self, unit: u16, alt: bool) {
        let character = match (self.term.high_surrogate.take(), unit) {
            (_, 0xD800..=0xDBFF) => {
                self.term.high_surrogate = Some(unit);
                return;
            }
            (Some(high), 0xDC00..=0xDFFF) => char::decode_utf16([high, unit]).next().and_then(Result::ok),
            (_, 0xDC00..=0xDFFF) => None,
            (_, unit) => char::from_u32(u32::from(unit)),
        };
        if let Some(character) = character {
            self.terminal_send(&keymap::character(character, alt));
        }
    }

    /// Ctrl+` : another terminal, in the shell last chosen.
    pub(super) unsafe fn terminal_command_new(&mut self) {
        self.terminal_new(None);
    }

    // ---- Copy and paste.

    /// What is selected, on the clipboard.
    pub(super) unsafe fn terminal_copy(&mut self) {
        let text = self.with_active(|terminal| terminal.with_screen(|screen| screen.selected_text())).flatten();
        if let Some(text) = text {
            self.copy_to_clipboard(&text);
        }
    }

    /// What is on the clipboard (or `text`, from a test), to the shell: as a paste where the shell asked for that.
    pub(super) unsafe fn terminal_paste(&mut self, text: Option<String>) {
        let Some(text) = text.or_else(|| self.clipboard_text()) else { return };
        let bracketed = self.with_active(|terminal| terminal.with_screen(|screen| screen.modes().bracketed_paste)).unwrap_or(false);
        self.terminal_send(&keymap::paste(&text, bracketed));
    }

    /// The text on the clipboard, if there is any.
    unsafe fn clipboard_text(&self) -> Option<String> {
        if IsClipboardFormatAvailable(13) == 0 || OpenClipboard(self.window) == 0 {
            return None;
        }
        let mut text = None;
        let handle = GetClipboardData(13);
        if !handle.is_null() {
            let memory = GlobalLock(handle) as *const u16;
            if !memory.is_null() {
                let length = (0..).take_while(|at| *memory.add(*at) != 0).count();
                text = Some(String::from_utf16_lossy(std::slice::from_raw_parts(memory, length)));
                GlobalUnlock(handle);
            }
        }
        CloseClipboard();
        text
    }

    // ---- The menu of a right click.

    /// A right click, or the menu key: copy what is selected, paste what is on the clipboard, select all.
    pub(super) unsafe fn terminal_context_menu(&mut self, at: (i32, i32)) {
        let Some((selected, running)) = self.with_active(|terminal| (terminal.with_screen(|screen| screen.has_selection()), terminal.exit_code().is_none())) else { return };
        let can_paste = IsClipboardFormatAvailable(13) != 0 && running;
        let items = vec![MenuItem { enabled: selected, ..MenuItem::new(t("Copy")) }, MenuItem { enabled: can_paste, ..MenuItem::new(t("Paste")) }, MenuItem::new(t("Select all"))];
        let mut point = POINT { x: at.0, y: at.1 };
        ClientToScreen(self.term.window, &mut point);
        ScreenToClient(self.window, &mut point);
        self.panel_menu_show(items, MenuFor::Terminal, (point.x, point.y));
    }

    pub(super) unsafe fn terminal_menu_chosen(&mut self, at: usize) {
        match at {
            0 => self.terminal_copy(),
            1 => self.terminal_paste(None),
            2 => {
                self.with_active(|terminal| terminal.with_screen(|screen| screen.select_all()));
                InvalidateRect(self.term.window, std::ptr::null(), 0);
            }
            _ => {}
        }
    }

    // ---- The pointer.

    /// Which cell of the view a point of the window is in, and whether it is the right half of it.
    fn cell_at(&self, x: i32, y: i32) -> (usize, usize, bool) {
        let Some(font) = &self.term.font else { return (0, 0, false) };
        let pad = scale(self.dpi, PAD);
        let (across, down) = (x - pad, y - pad);
        let (cols, rows) = self.term.cells;
        let col = (across.max(0) / font.cell_width).min(i32::from(cols) - 1).max(0);
        let row = (down.max(0) / font.cell_height).min(i32::from(rows) - 1).max(0);
        (col as usize, row as usize, across.max(0) % font.cell_width >= font.cell_width / 2)
    }

    /// A press on the terminal: the selection begins (a word for a second press in a moment, a line for a third), or where a program has asked for the mouse, it is told.
    pub(super) unsafe fn terminal_pressed(&mut self, x: i32, y: i32, double: bool) {
        SetFocus(self.term.window);
        let (col, row, right) = self.cell_at(x, y);
        let held = |key: u16| GetKeyState(i32::from(key)) < 0;
        let Some((modes, over)) = self.with_active(|terminal| (terminal.with_screen(|screen| screen.modes()), terminal.exit_code().is_some())) else { return };
        if (modes.mouse_click || modes.mouse_drag || modes.mouse_motion) && !held(VK_SHIFT) && !over {
            self.terminal_mouse_report(0, col, row, true);
            self.term.selecting = false;
            SetCapture(self.term.window);
            return;
        }
        let soon = self.term.clicks.0.is_some_and(|at| at.elapsed().as_millis() <= u128::from(GetDoubleClickTime())) && self.term.clicks.1 == (col, row);
        // (the system's second press is a double click's: a word, even where the first was not seen)
        let count = if soon { self.term.clicks.2 % 3 + 1 } else if double { 2 } else { 1 };
        self.term.clicks = (Some(Instant::now()), (col, row), count);
        let by = match count {
            2 => SelectBy::Words,
            3 => SelectBy::Lines,
            _ if held(VK_MENU) => SelectBy::Block,
            _ => SelectBy::Characters,
        };
        self.with_active(|terminal| {
            terminal.with_screen(|screen| {
                if held(VK_SHIFT) && count == 1 && screen.has_selection() {
                    screen.select_extend(col, row, right);
                } else {
                    screen.select_begin(col, row, right, by);
                }
            })
        });
        self.term.selecting = true;
        SetCapture(self.term.window);
        InvalidateRect(self.term.window, std::ptr::null(), 0);
    }

    /// The pointer moves with the button down: the selection goes with it, and past the top or the foot the view goes back or forward.
    pub(super) unsafe fn terminal_dragged(&mut self, x: i32, y: i32) {
        let (col, row, right) = self.cell_at(x, y);
        let Some(modes) = self.with_active(|terminal| terminal.with_screen(|screen| screen.modes())) else { return };
        if !self.term.selecting {
            if modes.mouse_drag || modes.mouse_motion {
                self.terminal_mouse_report(32, col, row, true);
            }
            return;
        }
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.term.window, &mut client);
        if y < 0 || y > client.bottom {
            SetTimer(self.term.window, TIMER_SCROLL, 60, None);
        } else {
            KillTimer(self.term.window, TIMER_SCROLL);
        }
        self.with_active(|terminal| terminal.with_screen(|screen| screen.select_extend(col, row, right)));
        InvalidateRect(self.term.window, std::ptr::null(), 0);
    }

    /// The button is let go: the selection stays; where a program has the mouse, it is told.
    pub(super) unsafe fn terminal_released(&mut self, x: i32, y: i32) {
        KillTimer(self.term.window, TIMER_SCROLL);
        if GetCapture() == self.term.window {
            ReleaseCapture();
        }
        if self.term.selecting {
            self.term.selecting = false;
            return;
        }
        let (col, row, _) = self.cell_at(x, y);
        if self.with_active(|terminal| terminal.with_screen(|screen| screen.modes())).is_some_and(|modes| modes.mouse_click || modes.mouse_drag || modes.mouse_motion) {
            self.terminal_mouse_report(0, col, row, false);
        }
    }

    /// While the button is down past the edge, the view goes on moving, and the selection reaches the line at that edge.
    pub(super) unsafe fn terminal_autoscroll(&mut self) {
        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        ScreenToClient(self.term.window, &mut point);
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.term.window, &mut client);
        let (col, _, _) = self.cell_at(point.x, 0);
        let last_row = usize::from(self.term.cells.1).saturating_sub(1);
        if point.y < 0 {
            self.with_active(|terminal| terminal.with_screen(|screen| {
                screen.scroll(1);
                screen.select_extend(col, 0, false);
            }));
        } else if point.y > client.bottom {
            self.with_active(|terminal| terminal.with_screen(|screen| {
                screen.scroll(-1);
                screen.select_extend(col, last_row, true);
            }));
        }
        InvalidateRect(self.term.window, std::ptr::null(), 0);
    }

    /// A program that asked for the mouse is told of it, in the form it asked for (the SGR form, or the old one where it did not).
    unsafe fn terminal_mouse_report(&self, button: u32, col: usize, row: usize, down: bool) {
        let Some(modes) = self.with_active(|terminal| terminal.with_screen(|screen| screen.modes())) else { return };
        let held = |key: u16| GetKeyState(i32::from(key)) < 0;
        let modifiers = u32::from(held(VK_SHIFT)) * 4 + u32::from(held(VK_MENU)) * 8 + u32::from(held(VK_CONTROL)) * 16;
        let code = button + modifiers;
        let text = if modes.mouse_sgr {
            format!("\x1b[<{code};{};{}{}", col + 1, row + 1, if down { 'M' } else { 'm' })
        } else {
            // (the old form: the button as a byte, and a position that goes no further than a byte does)
            let byte = |value: usize| char::from_u32(32 + value.min(222) as u32 + 1).unwrap_or(' ');
            let released = if down { code } else { 3 };
            format!("\x1b[M{}{}{}", char::from_u32(32 + released).unwrap_or(' '), byte(col), byte(row))
        };
        self.with_active(|terminal| terminal.write(text.as_bytes()));
    }

    /// The wheel: back into what scrolled off (or forward again); to a program that asked for the mouse, as wheel presses; on the alternate screen, as the arrow keys.
    pub(super) unsafe fn terminal_wheel(&mut self, notches: i32, x: i32, y: i32) {
        let Some(modes) = self.with_active(|terminal| terminal.with_screen(|screen| screen.modes())) else { return };
        let lines = notches * WHEEL_LINES;
        let held = |key: u16| GetKeyState(i32::from(key)) < 0;
        if modes.mouse_click || modes.mouse_drag || modes.mouse_motion {
            let (col, row, _) = self.cell_at(x, y);
            for _ in 0..lines.abs() {
                self.terminal_mouse_report(if lines > 0 { 64 } else { 65 }, col, row, true);
            }
        } else if modes.alternate_screen && modes.alternate_scroll && !held(VK_SHIFT) {
            if let Keyed::Send(bytes) = keymap::key(if lines > 0 { VK_UP } else { VK_DOWN }, Mods::default(), modes.application_cursor) {
                for _ in 0..lines.abs() {
                    self.with_active(|terminal| terminal.write(&bytes));
                }
            }
        } else {
            self.with_active(|terminal| terminal.with_screen(|screen| screen.scroll(lines)));
            InvalidateRect(self.term.window, std::ptr::null(), 0);
        }
    }

    // ---- Focus and the IME.

    pub(super) unsafe fn terminal_focus(&mut self, gained: bool) {
        InvalidateRect(self.term.window, std::ptr::null(), 0);
        self.with_active(|terminal| {
            if terminal.exit_code().is_none() && terminal.with_screen(|screen| screen.modes().focus_reports) {
                terminal.write(keymap::focus(gained));
            }
        });
    }

    /// The window an IME composes in stands at the cursor, in the size of the text: a Japanese person typing sees what they type where it will go.
    pub(super) unsafe fn terminal_ime_place(&mut self) {
        let (Some(font), Some((col, row))) = (self.term.font.clone(), self.with_active(|terminal| terminal.with_screen(|screen| screen.cursor_position()))) else { return };
        let pad = scale(self.dpi, PAD);
        let context = ImmGetContext(self.term.window);
        if context.is_null() {
            return;
        }
        let form = COMPOSITIONFORM { dwStyle: CFS_POINT, ptCurrentPos: POINT { x: pad + col as i32 * font.cell_width, y: pad + row as i32 * font.cell_height }, rcArea: RECT { left: 0, top: 0, right: 0, bottom: 0 } };
        ImmSetCompositionWindow(context, &form);
        let mut face: LOGFONTW = std::mem::zeroed();
        face.lfHeight = -font.cell_height;
        face.lfCharSet = 1;
        for (at, unit) in crate::paint::japanese_face().encode_utf16().take(31).enumerate() {
            face.lfFaceName[at] = unit;
        }
        ImmSetCompositionFontW(context, &face);
        ImmReleaseContext(self.term.window, context);
    }

    // ---- Drawing.

    /// What the window shows of the terminal on the tab chosen.
    pub(super) unsafe fn paint_terminal(&mut self, device: HDC, client: RECT) {
        let ground = ground();
        let (Some(font), Some(terminal)) = (self.terminal_font(), self.term.active_terminal()) else {
            let brush = CreateSolidBrush(ground);
            FillRect(device, &client, brush);
            DeleteObject(brush);
            return;
        };
        let focused = GetFocus() == self.term.window || self.term.forced_focus;
        let light = theme::is_light();
        let snapshot = terminal.with_screen(|screen| {
            screen.set_theme(light, theme::text(), ground);
            screen.snapshot()
        });
        let drawn_of = (terminal.id, snapshot_sum(&snapshot));
        let scene = scene_of(&snapshot, focused && terminal.exit_code().is_none(), light, scale(self.dpi, PAD), (client.right, client.bottom), self.dpi);
        if !render::terminal(device, client, &font, &scene) {
            let brush = CreateSolidBrush(ground);
            FillRect(device, &client, brush);
            DeleteObject(brush);
        }
        self.term.painted = drawn_of;
    }

    // ---- What a reader of the screen is told.

    /// The text a reader is given: what is in view, to the last line that has anything (or the cursor's), and where the cursor is in it.
    unsafe fn terminal_document(&self) -> access::Document {
        let Some(font) = self.term.font.clone() else { return access::Document::default() };
        let pad = scale(self.dpi, PAD);
        let mut area: RECT = std::mem::zeroed();
        GetWindowRect(self.term.window, &mut area);
        let shown = self.with_active(|terminal| {
            terminal.with_screen(|screen| {
                let (line, before) = screen.cursor_line();
                (screen.visible_lines(), screen.cursor_position().1, line.chars().count().min(before), screen.offset())
            })
        });
        let Some((mut lines, row, before, offset)) = shown else { return access::Document { origin: (area.left + pad, area.top + pad), cell: (font.cell_width, font.cell_height), ..Default::default() } };
        // (the lines under the last that has anything, and are not the cursor's, are not read out)
        let last = lines.iter().rposition(|line| !line.is_empty()).map_or(0, |last| last.max(if offset == 0 { row } else { 0 }));
        lines.truncate(last + 1);
        let text: Vec<u16> = lines.join("\n").encode_utf16().collect();
        let caret = if offset == 0 && row < lines.len() {
            let before_lines: usize = lines[..row].iter().map(|line| line.encode_utf16().count() + 1).sum();
            let in_line: usize = lines[row].chars().take(before).map(char::len_utf16).sum();
            before_lines + in_line
        } else {
            text.len()
        };
        access::Document { text, caret, origin: (area.left + pad, area.top + pad), cell: (font.cell_width, font.cell_height) }
    }

    /// The window is asked for its element, by a reader: it is made and kept, and its being there is what makes the screen tell it of changes.
    pub(super) unsafe fn terminal_element(&mut self) -> windows::Win32::UI::Accessibility::IRawElementProviderSimple {
        if self.term.element.is_none() {
            self.term.element = Some(access::element(self.term.window));
        }
        self.term.element.clone().unwrap()
    }

    /// The window goes: a reader lets go of the element.
    pub(super) unsafe fn terminal_element_gone(&mut self) {
        if let Some(element) = self.term.element.take() {
            access::let_go(self.term.window, &element);
        }
    }

    /// What a reader is told has changed: the text of the terminal or where its cursor is. A moment after the output, so that a burst is told once.
    unsafe fn terminal_changed(&mut self) {
        if self.term.element.is_some() && !self.term.told_soon {
            self.term.told_soon = true;
            SetTimer(self.term.window, TIMER_ACCESS, 150, None);
        }
    }

    /// The moment is up: a reader that listens is told what changed since it was last.
    pub(super) unsafe fn terminal_tell_reader(&mut self) {
        KillTimer(self.term.window, TIMER_ACCESS);
        self.term.told_soon = false;
        let Some(element) = self.term.element.clone() else { return };
        let document = self.terminal_document();
        let sum = document.text.iter().fold(0u64, |sum, unit| sum.wrapping_mul(31).wrapping_add(u64::from(*unit)));
        let now = (sum, document.caret);
        access::changed(&element, now.0 != self.term.told.0, now.1 != self.term.told.1);
        self.term.told = now;
    }
}

/// A sum of what a snapshot holds: its characters, colours and marks, its cursor and how far back it is. Two that are the same to the eye have the same.
pub(super) fn snapshot_sum(snapshot: &Snapshot) -> u64 {
    let mut sum: u64 = 0xcbf2_9ce4_8422_2325;
    let mut put = |value: u64| {
        sum ^= value;
        sum = sum.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for glyph in &snapshot.cells {
        put(u64::from(glyph.ch as u32));
        put(match glyph.fg {
            Paint::Default => 1,
            Paint::Indexed(index) => 2 + u64::from(index),
            Paint::Rgb(color) => 1000 + u64::from(color),
        });
        put(match glyph.bg {
            Paint::Default => 1,
            Paint::Indexed(index) => 2 + u64::from(index),
            Paint::Rgb(color) => 1000 + u64::from(color),
        });
        put(u64::from(glyph.bold) | u64::from(glyph.inverse) << 1 | u64::from(glyph.selected) << 2 | u64::from(glyph.width) << 3 | (glyph.underline as u64) << 6);
    }
    put(snapshot.cursor.map_or(0, |cursor| 1 + cursor.col as u64 * 1000 + cursor.row as u64));
    put(snapshot.offset as u64);
    sum
}

/// How many cells a window of this size has, with the face's cell and the padding the cells keep from the edge.
fn cells_in(width: i32, height: i32, font: &TermFont, dpi: u32) -> (u16, u16) {
    let pad = scale(dpi, PAD);
    let cols = ((width - 2 * pad) / font.cell_width.max(1)).clamp(2, 400);
    let rows = ((height - 2 * pad) / font.cell_height.max(1)).clamp(1, 200);
    (cols as u16, rows as u16)
}

fn rect_of(x: i32, y: i32, width: i32, height: i32) -> RECT {
    RECT { left: x, top: y, right: x + width.max(0), bottom: y + height.max(0) }
}

/// The colour a cell's paint is, in the theme: the sixteen are the theme's, the rest xterm's, the text and the ground the theme's own, and what a program set is what it set.
fn color_of(paint: Paint, snapshot: &Snapshot, light: bool, text: bool) -> u32 {
    match paint {
        Paint::Default if text => snapshot.default_fg.unwrap_or_else(theme::text),
        Paint::Default => snapshot.default_bg.unwrap_or_else(ground),
        Paint::Indexed(index) => snapshot.indexed.get(usize::from(index)).copied().flatten().unwrap_or_else(|| palette::indexed(light, usize::from(index))),
        Paint::Rgb(color) => color,
    }
}

/// The cells of a snapshot as what is drawn: each cell's two colours (bold in a plain colour brighter, inverse swapped, dim between the two, the selection in the theme's accent, and text
/// held 4.5 to 1 against its ground), grounds and text in runs, the lines of underline and strike, and the cursor.
fn scene_of(snapshot: &Snapshot, focused: bool, light: bool, pad: i32, area: (i32, i32), dpi: u32) -> TermScene {
    let ground = snapshot.default_bg.unwrap_or_else(ground);
    let selection = mix(ground, theme::accent(), if light { 0.35 } else { 0.42 });
    let mut scene = TermScene { ground, origin: (pad, pad), grounds: Vec::new(), runs: Vec::new(), lines: Vec::new(), cursor: None, thumb: None };
    // The thumb of the lines kept above: how much of all there is is in view, and where.
    if snapshot.history > 0 {
        let (track, width) = (area.1 - 2 * pad, scale(dpi, ds::size::THUMB) - scale(dpi, ds::space::XS) / 2);
        let height = (track * snapshot.rows as i32 / (snapshot.rows + snapshot.history) as i32).max(scale(dpi, ds::space::XXL)).min(track);
        let top = pad + (track - height) * (snapshot.history - snapshot.offset.min(snapshot.history)) as i32 / snapshot.history as i32;
        let right = area.0 - scale(dpi, ds::space::HAIR) - 1;
        scene.thumb = Some(((right - width, top, right, top + height), if snapshot.offset > 0 { theme::muted() } else { theme::line() }));
    }
    let colors = |glyph: &crate::term::screen::Glyph| {
        let mut text_paint = glyph.fg;
        if let (true, Paint::Indexed(index)) = (glyph.bold, glyph.fg) {
            if index < 8 {
                text_paint = Paint::Indexed(index + 8);
            }
        }
        let (mut fg, mut bg) = (color_of(text_paint, snapshot, light, true), color_of(glyph.bg, snapshot, light, false));
        if glyph.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if glyph.selected {
            bg = selection;
        }
        if glyph.dim {
            fg = mix(bg, fg, 0.6);
        }
        let blank = glyph.ch == ' ' && glyph.marks.is_none();
        let fg = if glyph.hidden { bg } else if blank { fg } else { palette::with_contrast(fg, bg, palette::LEAST_CONTRAST) };
        (fg, bg)
    };
    for row in 0..snapshot.rows {
        let cells = &snapshot.cells[row * snapshot.cols..(row + 1) * snapshot.cols];
        let mut run: Option<TermRun> = None;
        let mut ground_run: Option<TermGround> = None;
        let mut line_run: Option<TermLine> = None;
        let flush = |scene: &mut TermScene, run: &mut Option<TermRun>| {
            if let Some(mut done) = run.take() {
                let kept = done.text.trim_end_matches(' ').chars().count();
                if kept > 0 {
                    done.text.truncate(done.text.trim_end_matches(' ').len());
                    done.cells = kept.min(done.cells);
                    scene.runs.push(done);
                }
            }
        };
        for (col, glyph) in cells.iter().enumerate() {
            if glyph.width == 0 {
                continue;
            }
            let span = usize::from(glyph.width).max(1);
            let (fg, bg) = colors(glyph);
            // The ground, where it is not the terminal's.
            if bg != ground {
                match &mut ground_run {
                    Some(open) if open.color == bg && open.col + open.cells == col => open.cells += span,
                    _ => {
                        if let Some(done) = ground_run.take() {
                            scene.grounds.push(done);
                        }
                        ground_run = Some(TermGround { row, col, cells: span, color: bg });
                    }
                }
            } else if let Some(done) = ground_run.take() {
                scene.grounds.push(done);
            }
            // Underline and strike, in a run of the same kind.
            let kinds = [
                match glyph.underline {
                    Underline::None => None,
                    Underline::Single => Some(TermLineKind::Underline),
                    Underline::Double => Some(TermLineKind::Double),
                    Underline::Curly => Some(TermLineKind::Curly),
                    Underline::Dotted => Some(TermLineKind::Dotted),
                    Underline::Dashed => Some(TermLineKind::Dashed),
                },
                glyph.strike.then_some(TermLineKind::Strike),
            ];
            let line_color = glyph.underline_color.map_or(fg, |paint| color_of(paint, snapshot, light, true));
            for kind in kinds.into_iter().flatten() {
                match &mut line_run {
                    Some(open) if open.kind == kind && open.color == line_color && open.col + open.cells == col && open.row == row => open.cells += span,
                    _ => {
                        if let Some(done) = line_run.take() {
                            scene.lines.push(done);
                        }
                        line_run = Some(TermLine { row, col, cells: span, color: line_color, kind });
                    }
                }
            }
            if kinds.iter().all(Option::is_none) {
                if let Some(done) = line_run.take() {
                    scene.lines.push(done);
                }
            }
            // The text: narrow plain characters of one style run together; anything wider or odder is drawn by itself, in its own cells.
            let blank = glyph.ch == ' ' && glyph.marks.is_none();
            let plain = (' '..='~').contains(&glyph.ch) && glyph.marks.is_none() && glyph.width == 1;
            if plain {
                match &mut run {
                    Some(open) if open.color == fg && open.bold == glyph.bold && open.italic == glyph.italic && open.col + open.cells == col => {
                        open.text.push(glyph.ch);
                        open.cells += 1;
                    }
                    _ if blank => {
                        // (a blank of another style ends the run, and is drawn as nothing)
                        flush(&mut scene, &mut run);
                    }
                    _ => {
                        flush(&mut scene, &mut run);
                        run = Some(TermRun { row, col, cells: 1, text: glyph.ch.to_string(), color: fg, bold: glyph.bold, italic: glyph.italic });
                    }
                }
            } else {
                flush(&mut scene, &mut run);
                if !blank {
                    scene.runs.push(TermRun { row, col, cells: span, text: glyph.text(), color: fg, bold: glyph.bold, italic: glyph.italic });
                }
            }
        }
        flush(&mut scene, &mut run);
        if let Some(done) = ground_run.take() {
            scene.grounds.push(done);
        }
        if let Some(done) = line_run.take() {
            scene.lines.push(done);
        }
    }
    if let Some(cursor) = snapshot.cursor {
        let under = snapshot.cells.get(cursor.row * snapshot.cols + cursor.col);
        let cells = under.map_or(1, |glyph| usize::from(glyph.width).max(1));
        let shape = if !focused {
            TermCursorShape::Hollow
        } else {
            match cursor.shape {
                CursorShape::Underline => TermCursorShape::Underline,
                CursorShape::Beam => TermCursorShape::Beam,
                CursorShape::Hollow => TermCursorShape::Hollow,
                CursorShape::Block => TermCursorShape::Block,
            }
        };
        let hidden = under.filter(|glyph| !(glyph.ch == ' ' && glyph.marks.is_none())).map(|glyph| TermRun { row: cursor.row, col: cursor.col, cells, text: glyph.text(), color: theme::on_accent(), bold: glyph.bold, italic: glyph.italic });
        scene.cursor = Some((TermCursor { row: cursor.row, col: cursor.col, cells, shape, color: theme::accent() }, hidden));
    }
    scene
}

/// The terminal's own window: what it draws, and the keys, the pointer, the wheel and the IME it takes.
pub(super) unsafe extern "system" fn term_procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
    match message {
        WM_ERASEBKGND => 1,
        // A reader of the screen asks for what the window is: a document, with its text.
        WM_GETOBJECT if lparam as i32 == windows::Win32::UI::Accessibility::UiaRootObjectId => {
            let element = app.terminal_element();
            access::give(window, wparam, lparam, &element)
        }
        WM_DESTROY => {
            app.terminal_element_gone();
            0
        }
        WM_TIMER if wparam == TIMER_ACCESS => {
            app.terminal_tell_reader();
            0
        }
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let target = BeginPaint(window, &mut paint);
            let mut client: RECT = std::mem::zeroed();
            GetClientRect(window, &mut client);
            if client.right > 0 && client.bottom > 0 {
                let device = CreateCompatibleDC(target);
                let bitmap = CreateCompatibleBitmap(target, client.right, client.bottom);
                let previous = SelectObject(device, bitmap);
                app.paint_terminal(device, client);
                BitBlt(target, 0, 0, client.right, client.bottom, device, 0, 0, SRCCOPY);
                SelectObject(device, previous);
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            EndPaint(window, &paint);
            0
        }
        // (every key is the terminal's: Tab and the arrows are for a shell, and not for a dialog's moving between controls)
        WM_GETDLGCODE => (DLGC_WANTALLKEYS | DLGC_WANTCHARS | DLGC_WANTARROWS | DLGC_WANTTAB) as LRESULT,
        WM_SETFOCUS => {
            app.terminal_focus(true);
            0
        }
        WM_KILLFOCUS => {
            app.terminal_focus(false);
            0
        }
        WM_CHAR => {
            app.terminal_char(wparam as u16, false);
            0
        }
        // Alt and a character: what the layout makes of it, with an escape before it. (Alt and Space is the window's menu.)
        WM_SYSCHAR if wparam as u16 != u16::from(b' ') => {
            app.terminal_char(wparam as u16, true);
            0
        }
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION => {
            app.terminal_ime_place();
            DefWindowProcW(window, message, wparam, lparam)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            app.terminal_pressed(x, y, message == WM_LBUTTONDBLCLK);
            0
        }
        WM_MOUSEMOVE if wparam & 1 != 0 && GetCapture() == window => {
            app.terminal_dragged(x, y);
            0
        }
        WM_LBUTTONUP => {
            app.terminal_released(x, y);
            0
        }
        WM_CAPTURECHANGED => {
            KillTimer(window, TIMER_SCROLL);
            app.term.selecting = false;
            0
        }
        WM_TIMER if wparam == TIMER_SCROLL => {
            app.terminal_autoscroll();
            0
        }
        WM_RBUTTONUP => {
            SetFocus(window);
            app.terminal_context_menu((x, y));
            0
        }
        WM_CONTEXTMENU => {
            // (from the menu key: at the cursor's cell, as the point is not the pointer's)
            let (col, row) = app.term.active_terminal().map_or((0, 0), |terminal| terminal.with_screen(|screen| screen.cursor_position()));
            let (cell_width, cell_height) = app.term.font.as_ref().map_or((8, 16), |font| (font.cell_width, font.cell_height));
            let pad = scale(app.dpi, PAD);
            app.terminal_context_menu((pad + col as i32 * cell_width, pad + (row as i32 + 1) * cell_height));
            0
        }
        WM_MOUSEWHEEL => {
            let notches = ((wparam >> 16) & 0xFFFF) as u16 as i16 as i32 / 120;
            // (the wheel's point is the screen's)
            let mut point = POINT { x, y };
            ScreenToClient(window, &mut point);
            app.terminal_wheel(notches, point.x, point.y);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}
