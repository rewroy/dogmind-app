//! The Files view of the panel on the right, drawn and driven: the list of rows `files::Tree` gives, its pointer and keys, its menu, and what it asks of the rest of the app (a preview, an
//! app, Explorer, the composer). The list is an owner-drawn list box, so that the arrows, Home and End, the first letter of a name and a screen reader's reading of its items are the system's;
//! the rows are drawn here in the app's own paint (through DirectWrite like the rest).

use super::*;
use crate::files::{self, Mark, Row, Toggled};

/// A row of the menu is this tall (unscaled pixels).
pub(super) const MENU_ROW: i32 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MenuDoes {
    Open,
    Explorer,
    Copy,
    Mention,
}

/// What the menu of a row offers, in order: open, show in Explorer, copy the path, mention it in the message.
pub(super) const MENU_ITEMS: [(MenuDoes, &str); 4] = [(MenuDoes::Open, "Open"), (MenuDoes::Explorer, "Show in Explorer"), (MenuDoes::Copy, "Copy the path"), (MenuDoes::Mention, "Mention in the message")];

/// The first row's depth is indented this much more than its folder's chevron; each depth below it, this much again (unscaled pixels).
const INDENT: i32 = 14;

/// What a row says to a screen reader, and to the list's search by the first letters of a name: its name first.
pub(super) fn spoken(row: &Row) -> String {
    match &row.kind {
        files::RowKind::Folder { open } => tf("{0}, folder, {1}", &[&row.name, &if *open { t("expanded") } else { t("collapsed") }]),
        files::RowKind::File => match row.mark {
            Mark::Changed(added, removed) => tf("{0}, changed by this session: {1} lines added, {2} removed", &[&row.name, &added, &removed]),
            _ => row.name.clone(),
        },
        files::RowKind::Reading => t("Reading…").to_string(),
        files::RowKind::Empty => t("Empty").to_string(),
        files::RowKind::Problem(why) => tf("This folder could not be read: {0}", &[why]),
        files::RowKind::More(count) => tf("{0} more…", &[count]),
    }
}

/// Where a row's open button is, at the right end of a file's row.
fn open_button(area: RECT, dpi: u32) -> RECT {
    RECT { left: area.right - scale(dpi, ds::size::ROW_ACTION), right: area.right - scale(dpi, ds::space::XS), top: area.top + scale(dpi, ds::space::HAIR), bottom: area.bottom - scale(dpi, ds::space::HAIR) }
}

/// What a key does to the tree, from the row it is on. `Open`: the folder of the row opens or closes, or the file shows; `Parent`: the row of the folder this one is in; `Next`: the row under.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum KeyDoes {
    Toggle,
    Parent,
    Next,
    Act,
    OpenInApp,
    Refresh,
    Nothing,
}

/// What a key does on a row of this kind. Pure, so that what the arrows do is said once and tested: Right opens a closed folder and goes into an open one; Left closes an open folder and
/// otherwise goes up to the folder; Enter and Space open a folder or show a file, and with Ctrl a file is opened in its own app.
pub(super) fn key_does(key: u16, control: bool, row: Option<&Row>) -> KeyDoes {
    let folder_open = row.and_then(|row| match row.kind {
        files::RowKind::Folder { open } => Some(open),
        _ => None,
    });
    match key {
        VK_RIGHT => match folder_open {
            Some(false) => KeyDoes::Toggle,
            Some(true) => KeyDoes::Next,
            None => KeyDoes::Nothing,
        },
        VK_LEFT => match folder_open {
            Some(true) => KeyDoes::Toggle,
            _ if row.is_some_and(|row| row.depth > 0) => KeyDoes::Parent,
            _ => KeyDoes::Nothing,
        },
        VK_RETURN | VK_SPACE if control && row.is_some_and(Row::is_file) => KeyDoes::OpenInApp,
        VK_RETURN | VK_SPACE if row.is_some() => KeyDoes::Act,
        VK_F5 => KeyDoes::Refresh,
        _ => KeyDoes::Nothing,
    }
}

impl App {
    /// The panel shows the tree: it is the chat, the panel is there, and no preview is in its place.
    pub(super) fn files_shown(&self) -> bool {
        self.panel == Panel::Chat && self.right_width > 0 && self.preview_file.is_none() && self.settings.right_files
    }

    /// Which of the panel's two views, and its choice kept.
    pub(super) unsafe fn set_right_view(&mut self, files: bool) {
        if self.settings.right_files == files {
            return;
        }
        self.settings.right_files = files;
        self.save();
        self.layout();
        if files && self.files_shown() {
            SetFocus(self.tree_list);
        }
    }

    /// `/files`: the panel is there with the tree in it, or it says why it cannot be.
    pub(super) unsafe fn show_files_view(&mut self) {
        if self.panel != Panel::Chat {
            self.set_panel(Panel::Chat);
        }
        if self.preview_file.is_some() {
            self.close_preview();
        }
        if !self.settings.right_panel {
            self.settings.right_panel = true;
        }
        self.settings.right_files = true;
        self.save();
        self.layout();
        if self.right_width == 0 {
            self.say(t("The window is too narrow for the panel: make it wider."));
        } else {
            SetFocus(self.tree_list);
        }
    }

    /// The tree is on screen at this place: its folder is the session's workspace, what the disk says is watched for, and the list is up to date.
    pub(super) unsafe fn files_place(&mut self, left: i32, top: i32, width: i32, height: i32) {
        if self.workspace.as_os_str().is_empty() || !self.workspace.is_dir() {
            show(self.tree_list, false);
            return;
        }
        if self.tree_for.as_ref() != Some(&self.workspace) {
            // (another folder: a tree and a watcher of its own)
            self.tree = files::Tree::new();
            self.tree_for = Some(self.workspace.clone());
            self.tree_rows.clear();
            self.tree_version = None;
            self.tree_selected = None;
            self.tree_watcher = None;
            self.tree_hover = None;
            self.files_marks();
            SendMessageW(self.tree_list, LB_RESETCONTENT, 0, 0);
        }
        if self.tree_dpi != self.dpi {
            // (each row's height was asked for as it was added)
            self.tree_dpi = self.dpi;
            self.tree_rows.clear();
            self.tree_version = None;
        }
        // (a layout asks for this a good deal oftener than the place changes: a window put where it is already is drawn again for nothing)
        if IsWindowVisible(self.tree_list) == 0 || child_area(self.tree_list, self.window) != (left, top, width, height) {
            place(self.tree_list, left, top, width, height);
            show(self.tree_list, true);
        }
        if self.tree.root_is_unread() {
            self.tree.root_reading();
            self.files_read(String::new());
        }
        if self.tree_watcher.is_none() {
            let (poster, root) = (self.poster.clone(), self.workspace.clone());
            self.tree_watcher = files::Watcher::start(&self.workspace, move |changed| poster.send(Note::WorkspaceChanged { root: root.clone(), changed }));
            if std::mem::take(&mut self.tree_stale) {
                // (it was not watched while it was out of sight: what is open is read again)
                self.files_changed_on_disk(&self.workspace.clone(), files::Changed { everything: true, folders: Vec::new() });
            }
        }
        self.files_rebuild();
    }

    /// The tree is not on screen: it is not watched, and what it shows is read again when it comes back.
    pub(super) unsafe fn files_hidden(&mut self) {
        self.files_menu_close(false);
        show(self.tree_list, false);
        if self.tree_watcher.take().is_some() {
            self.tree_stale = true;
        }
    }

    /// A folder is read, on another thread: a large one takes a while, and the window goes on.
    pub(super) unsafe fn files_read(&self, path: String) {
        let (poster, root) = (self.poster.clone(), self.workspace.clone());
        let disk = files::disk_path(&root, &path);
        if test_log_on() {
            test_log(&format!("files read of <{path}> begins"));
        }
        std::thread::spawn(move || {
            let result = files::read_folder(&disk);
            poster.send(Note::FolderRead { root, path, result });
        });
    }

    /// A read has come, for a tree that is still of that folder.
    pub(super) unsafe fn files_read_came(&mut self, root: PathBuf, path: String, result: Result<Vec<files::Entry>, String>) {
        if root != self.workspace {
            return;
        }
        if test_log_on() {
            test_log(&format!("files read of <{path}> came: {}", result.as_ref().map_or_else(|why| why.clone(), |entries| format!("{} entries", entries.len()))));
        }
        self.tree.read_came(&path, result);
        self.files_rebuild();
    }

    /// The disk says these folders changed: the ones that are open are read again.
    pub(super) unsafe fn files_changed_on_disk(&mut self, root: &Path, changed: files::Changed) {
        if root != self.workspace {
            return;
        }
        if test_log_on() {
            test_log(&format!("files changed on disk: {:?} everything={}", changed.folders, changed.everything));
        }
        for path in self.tree.changed_on_disk(&changed.folders, changed.everything) {
            self.files_read(path);
        }
    }

    /// The files the session changed, from the answers' own record of them: marked in the tree.
    pub(super) unsafe fn files_marks(&mut self) {
        self.tree.set_changes(self.turn_changes.values().flatten().map(|file| (file.path.as_str(), file.added, file.removed)));
        self.files_rebuild();
    }

    /// An answer changed these files: they are marked, and the folders they are in are read again now, without waiting for the watcher's quiet moment.
    pub(super) unsafe fn files_turn_changed(&mut self, changed: &[FileChange]) {
        self.files_marks();
        let folders: Vec<String> = changed.iter().map(|file| files::parent_of(&file.path.replace('\\', "/")).to_string()).collect();
        let root = self.workspace.clone();
        self.files_changed_on_disk(&root, files::Changed { everything: false, folders });
    }

    /// The list is made to say what the tree's rows are now, where it stood and with the same row selected. What has not changed is left alone, as it would flicker.
    pub(super) unsafe fn files_rebuild(&mut self) {
        // (a layout asks for this a good deal oftener than the tree changes)
        if self.tree_version == Some(self.tree.version()) && SendMessageW(self.tree_list, LB_GETCOUNT, 0, 0) == self.tree_rows.len() as isize {
            return;
        }
        self.tree_version = Some(self.tree.version());
        let rows = self.tree.rows();
        if rows == self.tree_rows && SendMessageW(self.tree_list, LB_GETCOUNT, 0, 0) == rows.len() as isize {
            return;
        }
        let top = SendMessageW(self.tree_list, LB_GETTOPINDEX, 0, 0).max(0) as usize;
        redraw(self.tree_list, false);
        SendMessageW(self.tree_list, LB_RESETCONTENT, 0, 0);
        for row in &rows {
            let words = wide(&spoken(row));
            SendMessageW(self.tree_list, LB_ADDSTRING, 0, words.as_ptr() as LPARAM);
        }
        let selected = self.tree_selected.as_ref().and_then(|path| rows.iter().position(|row| row.path == *path && !row.name.is_empty()));
        if let Some(at) = selected {
            SendMessageW(self.tree_list, LB_SETCURSEL, at, 0);
        } else {
            self.tree_selected = None;
        }
        SendMessageW(self.tree_list, LB_SETTOPINDEX, top.min(rows.len().saturating_sub(1)), 0);
        self.tree_rows = rows;
        redraw(self.tree_list, true);
        InvalidateRect(self.tree_list, std::ptr::null(), 0);
        if test_log_on() {
            let height = if self.tree_rows.is_empty() { 0 } else { SendMessageW(self.tree_list, LB_GETITEMHEIGHT, 0, 0) };
            let listed: Vec<String> = self.tree_rows.iter().take(400).map(|row| format!("{}|{}|{:?}", row.path, row.depth, row.kind)).collect();
            test_log(&format!("files rows top={} height={} count={} selected={:?} :: {}", SendMessageW(self.tree_list, LB_GETTOPINDEX, 0, 0), height, self.tree_rows.len(), self.tree_selected, listed.join(" ;; ")));
        }
    }

    /// The row the list has selected.
    fn tree_cursor(&self) -> Option<usize> {
        // SAFETY: a message to a window of this app.
        let at = unsafe { SendMessageW(self.tree_list, LB_GETCURSEL, 0, 0) };
        (at >= 0).then_some(at as usize).filter(|at| *at < self.tree_rows.len())
    }

    /// The selection moved, by key or by pointer: it is the row the keys are on.
    pub(super) unsafe fn files_selection_changed(&mut self) {
        self.tree_selected = self.tree_cursor().and_then(|at| self.tree_rows.get(at)).filter(|row| !row.name.is_empty()).map(|row| row.path.clone());
        if test_log_on() {
            test_log(&format!("files selected <{}>", self.tree_selected.as_deref().unwrap_or("")));
        }
    }

    /// Where a row's file or folder is on this PC.
    fn tree_disk_path(&self, row: &Row) -> PathBuf {
        files::disk_path(&self.workspace, &row.path)
    }

    /// A row in the tree has been drawn: its name, and what the session did to it.
    pub(super) unsafe fn draw_file_row(&self, item: &DRAWITEMSTRUCT) {
        let device = item.hDC;
        FillRect(device, &item.rcItem, self.brushes[1]);
        let Some(row) = self.tree_rows.get(item.itemID as usize) else { return };
        let area = item.rcItem;
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let selected = item.itemState & ODS_SELECTED != 0;
        let hover = self.tree_hover == Some(item.itemID as usize);
        let has_a_name = !row.name.is_empty();
        if has_a_name && (selected || hover) {
            let shade = RECT { left: area.left + s(ds::space::XS), right: area.right - s(ds::space::XS), top: area.top + s(ds::space::PIXEL), bottom: area.bottom - s(ds::space::PIXEL) };
            rounded(device, shade, s(ds::radius::SMALL), Some(if selected { theme::raised() } else { theme::surface() }), None);
        }
        let step = s(INDENT);
        let chevron = s(ds::size::ICON_SMALL);
        let mut x = area.left + s(ds::space::L) + row.depth as i32 * step;
        let mut right = area.right - s(ds::space::M);
        let small = self.fonts.small;
        let text_color = |dim: bool| if dim { theme::faint() } else if selected || row.mark != Mark::None { theme::text() } else { theme::soft() };
        match &row.kind {
            files::RowKind::Folder { open } => {
                icon::draw(device, if *open { "chevron-down" } else { "chevron-right" }, RECT { left: x, right: x + chevron, ..area }, s(ds::size::ICON_SMALL), theme::faint());
                x += chevron + s(ds::space::XS);
                icon::draw(device, if *open { "folder-open" } else { "folder" }, RECT { left: x, right: x + s(ds::size::ICON), ..area }, s(ds::size::ICON), if row.dim { theme::faint() } else { theme::muted() });
                x += s(ds::size::ICON) + s(ds::space::S);
            }
            files::RowKind::File => {
                // (a file lines up with the names of the folders beside it, past the room of a chevron)
                x += chevron + s(ds::space::XS);
                icon::draw(device, "file", RECT { left: x, right: x + s(ds::size::ICON), ..area }, s(ds::size::ICON), if row.dim { theme::faint() } else { theme::muted() });
                x += s(ds::size::ICON) + s(ds::space::S);
            }
            _ => x += chevron + s(ds::space::XS),
        }
        match &row.kind {
            files::RowKind::Folder { .. } | files::RowKind::File => {
                // The open button, on the file under the pointer or selected; and the marks of what the session changed, before it.
                if row.is_file() && (hover || selected) {
                    let button = open_button(area, dpi);
                    icon::draw(device, "external-link", button, s(ds::size::ICON_SMALL), theme::muted());
                    right = button.left - s(ds::space::XS);
                }
                match row.mark {
                    Mark::Changed(added, removed) => {
                        let (minus, plus) = (format!("\u{2212}{removed}"), format!("+{added}"));
                        let (minus_width, plus_width) = (text_width(small, &minus), text_width(small, &plus));
                        if removed > 0 {
                            draw_text(device, &minus, RECT { left: right - minus_width, right, ..area }, small, theme::bad(), DT_RIGHT | ONE_LINE);
                            right -= minus_width + s(ds::space::S);
                        }
                        if added > 0 || removed == 0 {
                            draw_text(device, &plus, RECT { left: right - plus_width, right, ..area }, small, theme::good(), DT_RIGHT | ONE_LINE);
                            right -= plus_width + s(ds::space::S);
                        }
                    }
                    Mark::Inside => {
                        // (something under a closed folder changed: a small dot, where the numbers of a file would be)
                        let dot = s(7);
                        let middle = (area.top + area.bottom) / 2;
                        rounded(device, RECT { left: right - dot, top: middle - dot / 2, right, bottom: middle - dot / 2 + dot }, dot / 2, Some(theme::good()), None);
                        right -= dot + s(ds::space::S);
                    }
                    Mark::None => {}
                }
                draw_text(device, &row.name, RECT { left: x, right: right.max(x), ..area }, small, text_color(row.dim), ONE_LINE);
            }
            files::RowKind::Reading => draw_text(device, t("Reading…"), RECT { left: x, right, ..area }, small, theme::faint(), ONE_LINE),
            files::RowKind::Empty => draw_text(device, t("Empty"), RECT { left: x, right, ..area }, small, theme::faint(), ONE_LINE),
            files::RowKind::Problem(why) => draw_text(device, &tf("This folder could not be read: {0}", &[why]), RECT { left: x, right, ..area }, small, theme::bad(), ONE_LINE),
            files::RowKind::More(count) => {
                let color = if hover || selected { theme::accent_hover() } else { theme::accent() };
                draw_text(device, &tf("{0} more…", &[count]), RECT { left: x, right, ..area }, small, color, ONE_LINE);
            }
        }
        if item.itemState & ODS_FOCUS != 0 && keys_are_in_use() {
            let ring = RECT { left: area.left + s(ds::space::XS) + 1, top: area.top + 1, right: area.right - s(ds::space::XS) - 1, bottom: area.bottom - 1 };
            rounded(device, ring, s(ds::radius::SMALL), None, Some(theme::accent()));
        }
    }

    /// A press and release on a row: a folder opens or closes, a file shows in the panel, the row of more adds a page; on the button of a file it opens in its own app.
    pub(super) unsafe fn files_row_clicked(&mut self, at: usize, x: i32) {
        let Some(row) = self.tree_rows.get(at).cloned() else { return };
        self.files_selection_changed();
        if row.is_file() {
            let mut area: RECT = std::mem::zeroed();
            SendMessageW(self.tree_list, LB_GETITEMRECT, at, std::ptr::addr_of_mut!(area) as LPARAM);
            if x >= open_button(area, self.dpi).left {
                self.files_open_in_app(&row);
                return;
            }
        }
        self.files_act(&row, false);
    }

    /// What a row does when it is chosen: a folder opens or closes, a file shows in the panel (or, with `in_app`, in its own app), and the row of more shows more.
    pub(super) unsafe fn files_act(&mut self, row: &Row, in_app: bool) {
        match &row.kind {
            files::RowKind::Folder { .. } => self.files_toggle(&row.path),
            files::RowKind::File if in_app => self.files_open_in_app(row),
            files::RowKind::File => {
                // (what the panel has no way to show is only selected: a double click or the button opens it in its app)
                if preview::can_show(&row.name) {
                    let file = self.tree_disk_path(row);
                    self.open_preview(&file.display().to_string());
                }
            }
            files::RowKind::More(_) => {
                self.tree.more(&row.path);
                self.files_rebuild();
            }
            files::RowKind::Problem(_) => {
                if let Toggled::Read(place) = self.tree.retry(&row.path) {
                    self.files_read(place);
                }
                self.files_rebuild();
            }
            _ => {}
        }
    }

    unsafe fn files_toggle(&mut self, path: &str) {
        // (a row of a folder that failed stands for the folder: pressed, it tries it again)
        if let Toggled::Read(place) = self.tree.toggle(path) {
            self.files_read(place);
        }
        self.files_rebuild();
    }

    /// A double click: a file opens in its own app. (A folder was opened or closed by the first click of it.)
    pub(super) unsafe fn files_row_double_clicked(&mut self, at: usize) {
        if let Some(row) = self.tree_rows.get(at).cloned() {
            if row.is_file() {
                self.files_open_in_app(&row);
            }
        }
    }

    /// A file in its own app; what is a program or a script is shown in Explorer, as the app does not run what it did not write; a folder opens in Explorer.
    pub(super) unsafe fn files_open_in_app(&mut self, row: &Row) {
        let disk = self.tree_disk_path(row);
        let (target, arguments) = if row.is_folder() {
            ("explorer.exe".to_string(), format!("\"{}\"", disk.display()))
        } else if disk.is_file() && cards::opens_in_app(&row.name) {
            (disk.display().to_string(), String::new())
        } else if disk.is_file() {
            ("explorer.exe".to_string(), format!("/select,\"{}\"", disk.display()))
        } else {
            return;
        };
        self.shell_open(&target, &arguments);
    }

    /// Explorer, with the file selected in its folder; a folder is opened.
    unsafe fn files_show_in_explorer(&mut self, row: &Row) {
        let disk = self.tree_disk_path(row);
        let arguments = if row.is_folder() { format!("\"{}\"", disk.display()) } else { format!("/select,\"{}\"", disk.display()) };
        self.shell_open("explorer.exe", &arguments);
    }

    unsafe fn shell_open(&self, target: &str, arguments: &str) {
        let (verb, target, arguments) = (wide("open"), wide(target), wide(arguments));
        ShellExecuteW(self.window, verb.as_ptr(), target.as_ptr(), if arguments.len() > 1 { arguments.as_ptr() } else { std::ptr::null() }, std::ptr::null(), SW_SHOWNORMAL);
    }

    /// `@path` at the end of what is being written, as `@` offers a file: the model is pointed at it.
    unsafe fn files_mention(&mut self, row: &Row) {
        let named = if row.is_folder() { format!("@{}/ ", row.path) } else { format!("@{} ", row.path) };
        let text = wide(&named);
        let before = get_text(self.composer);
        // (a space between it and what is there, unless there is none)
        let separator = if before.is_empty() || before.ends_with(char::is_whitespace) { "" } else { " " };
        let text = if separator.is_empty() { text } else { wide(&format!("{separator}{named}")) };
        SendMessageW(self.composer, EM_SETSEL, usize::MAX, -1);
        SendMessageW(self.composer, EM_REPLACESEL, 1, text.as_ptr() as LPARAM);
        self.focus_composer();
    }

    /// The menu of a row, a little list that stands over the window at `at` (in the tree list's own pixels): open, show in Explorer, copy the path, and mention it in the message. It is
    /// a window of the app's and drawn in its paint, as the rest is, so that it is of its theme and size, and a reader and the keys have it as a list.
    pub(super) unsafe fn files_menu(&mut self, index: usize, at: (i32, i32)) {
        let Some(row) = self.tree_rows.get(index).cloned().filter(|row| !row.name.is_empty()) else { return };
        SendMessageW(self.tree_list, LB_SETCURSEL, index, 0);
        self.files_selection_changed();
        self.tree_menu_row = Some(row);
        self.tree_menu_hover = None;
        SendMessageW(self.tree_menu, LB_RESETCONTENT, 0, 0);
        let mut widest = 0;
        for (_, words) in MENU_ITEMS {
            let words = t(words);
            widest = widest.max(text_width(self.fonts.body, words));
            let units = wide(words);
            SendMessageW(self.tree_menu, LB_ADDSTRING, 0, units.as_ptr() as LPARAM);
        }
        let s = |value: i32| scale(self.dpi, value);
        let (width, height) = ((widest + 2 * s(ds::space::L) + 2).max(s(190)), MENU_ITEMS.len() as i32 * s(MENU_ROW) + 2);
        // (in the window's pixels, and kept inside it)
        let mut point = POINT { x: at.0, y: at.1 };
        ClientToScreen(self.tree_list, &mut point);
        ScreenToClient(self.window, &mut point);
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.window, &mut client);
        let (x, y) = (point.x.min(client.right - width - s(ds::space::XS)).max(0), point.y.min(client.bottom - height - s(ds::space::XS)).max(0));
        SetWindowPos(self.tree_menu, HWND_TOP, x, y, width, height, SWP_SHOWWINDOW);
        SendMessageW(self.tree_menu, LB_SETCURSEL, 0, 0);
        SetFocus(self.tree_menu);
        InvalidateRect(self.tree_menu, std::ptr::null(), 0);
    }

    /// The menu is put away, and the keys go back to the tree unless they went somewhere else.
    pub(super) unsafe fn files_menu_close(&mut self, refocus: bool) {
        if self.tree_menu_row.take().is_none() {
            return;
        }
        self.tree_menu_hover = None;
        show(self.tree_menu, false);
        if refocus {
            SetFocus(self.tree_list);
        }
    }

    /// An item of the menu is chosen: what it says is done to the row it is of, and the menu goes.
    pub(super) unsafe fn files_menu_choose(&mut self, at: usize) {
        let Some(row) = self.tree_menu_row.clone() else { return };
        self.files_menu_close(true);
        match MENU_ITEMS.get(at).map(|(what, _)| *what) {
            Some(MenuDoes::Open) => self.files_open_in_app(&row),
            Some(MenuDoes::Explorer) => self.files_show_in_explorer(&row),
            Some(MenuDoes::Copy) => {
                let disk = self.tree_disk_path(&row);
                self.copy_to_clipboard(&disk.display().to_string());
            }
            Some(MenuDoes::Mention) => self.files_mention(&row),
            None => {}
        }
    }

    /// A row of the menu, drawn: its words, lit under the pointer or the keys, and a hair of a frame round all of them.
    pub(super) unsafe fn draw_menu_row(&self, item: &DRAWITEMSTRUCT) {
        let device = item.hDC;
        let (area, dpi) = (item.rcItem, self.dpi);
        let s = |value: i32| scale(dpi, value);
        FillRect(device, &area, self.brushes[2]);
        let Some((_, words)) = MENU_ITEMS.get(item.itemID as usize) else { return };
        if item.itemState & ODS_SELECTED != 0 {
            let shade = RECT { left: area.left + s(ds::space::XS), right: area.right - s(ds::space::XS), top: area.top + s(ds::space::HAIR), bottom: area.bottom - s(ds::space::HAIR) };
            rounded(device, shade, s(ds::radius::SMALL), Some(theme::raised()), None);
        }
        draw_text(device, t(words), RECT { left: area.left + s(ds::space::L), right: area.right - s(ds::space::L), ..area }, self.fonts.body, theme::text(), ONE_LINE);
        // (the frame: each row draws its own share, the first the top and the last the foot)
        let brush = CreateSolidBrush(theme::line());
        let last = item.itemID as usize + 1 == MENU_ITEMS.len();
        let edges = [
            (true, RECT { left: area.left, top: area.top, right: area.left + 1, bottom: area.bottom }),
            (true, RECT { left: area.right - 1, top: area.top, right: area.right, bottom: area.bottom }),
            (item.itemID == 0, RECT { left: area.left, top: area.top, right: area.right, bottom: area.top + 1 }),
            (last, RECT { left: area.left, top: area.bottom - 1, right: area.right, bottom: area.bottom }),
        ];
        for (drawn, edge) in edges {
            if drawn {
                FillRect(device, &edge, brush);
            }
        }
        DeleteObject(brush);
    }

    /// A key on the list: what the arrows and Enter do to a tree. `true`: it was this tree's.
    pub(super) unsafe fn files_key(&mut self, key: u16) -> bool {
        let control = GetKeyState(i32::from(VK_CONTROL)) < 0 || GetKeyState(i32::from(VK_SHIFT)) < 0;
        let at = self.tree_cursor();
        let row = at.and_then(|at| self.tree_rows.get(at)).cloned();
        match key_does(key, control, row.as_ref()) {
            KeyDoes::Toggle => {
                if let Some(row) = row {
                    self.files_toggle(&row.path);
                }
            }
            KeyDoes::Parent => {
                if let Some(parent) = at.and_then(|at| files::parent_row(&self.tree_rows, at)) {
                    SendMessageW(self.tree_list, LB_SETCURSEL, parent, 0);
                    self.files_selection_changed();
                }
            }
            KeyDoes::Next => {
                if let Some(next) = at.map(|at| at + 1).filter(|next| *next < self.tree_rows.len()) {
                    SendMessageW(self.tree_list, LB_SETCURSEL, next, 0);
                    self.files_selection_changed();
                }
            }
            KeyDoes::Act => {
                if let Some(row) = row {
                    self.files_act(&row, false);
                }
            }
            KeyDoes::OpenInApp => {
                if let Some(row) = row {
                    self.files_act(&row, true);
                }
            }
            KeyDoes::Refresh => {
                let root = self.workspace.clone();
                self.files_changed_on_disk(&root, files::Changed { everything: true, folders: Vec::new() });
            }
            KeyDoes::Nothing => return false,
        }
        true
    }

    /// Where the menu of the selected row goes when the key for it is pressed: under its name.
    pub(super) unsafe fn files_menu_from_key(&mut self) {
        let Some(at) = self.tree_cursor() else { return };
        let mut area: RECT = std::mem::zeroed();
        SendMessageW(self.tree_list, LB_GETITEMRECT, at, std::ptr::addr_of_mut!(area) as LPARAM);
        self.files_menu(at, (area.left + scale(self.dpi, 40), area.bottom));
    }

    /// The tabs of the panel, and the row under a point of the window, for a press.
    pub(super) fn right_tab_at(&self, x: i32, y: i32) -> Option<bool> {
        self.right_tab_hits.iter().find(|(area, _)| x >= area.left && x < area.right && y >= area.top && y < area.bottom).map(|(_, files)| *files)
    }
}

/// The menu's own window procedure: the pointer lights the row it is on and a press on a row chooses it; Esc and a click elsewhere put it away, as does the keys' going to another control.
pub(super) unsafe extern "system" fn menu_subclass(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let base = std::mem::transmute::<isize, WNDPROC>(app.list_procedure);
    let row_under = |lparam: LPARAM| {
        let found = SendMessageW(window, LB_ITEMFROMPOINT, 0, lparam);
        ((found >> 16) & 0xFFFF == 0).then_some((found & 0xFFFF) as usize).filter(|at| *at < MENU_ITEMS.len())
    };
    match message {
        WM_MOUSEMOVE => {
            if let Some(at) = row_under(lparam).filter(|at| Some(*at) != app.tree_menu_hover) {
                app.tree_menu_hover = Some(at);
                SendMessageW(window, LB_SETCURSEL, at, 0);
            }
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => 0,
        WM_LBUTTONUP => {
            match row_under(lparam) {
                Some(at) => app.files_menu_choose(at),
                None => app.files_menu_close(true),
            }
            0
        }
        WM_RBUTTONUP | WM_MOUSEWHEEL => 0,
        WM_KEYDOWN if wparam as u16 == VK_ESCAPE => {
            app.files_menu_close(true);
            0
        }
        WM_KEYDOWN if matches!(wparam as u16, VK_RETURN | VK_SPACE) => {
            let at = SendMessageW(window, LB_GETCURSEL, 0, 0);
            if at >= 0 {
                app.files_menu_choose(at as usize);
            }
            0
        }
        WM_CHAR => 0,
        WM_KILLFOCUS => {
            // (the keys went to another control, or the window lost them: it is put away, and takes none back)
            let result = CallWindowProcW(base, window, message, wparam, lparam);
            app.files_menu_close(false);
            result
        }
        _ => CallWindowProcW(base, window, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: files::RowKind, depth: usize, name: &str) -> Row {
        Row { path: name.to_string(), name: name.to_string(), depth, kind, dim: false, mark: Mark::None }
    }

    #[test]
    fn right_opens_a_closed_folder_goes_into_an_open_one_and_does_nothing_on_a_file() {
        let closed = row(files::RowKind::Folder { open: false }, 0, "src");
        let open = row(files::RowKind::Folder { open: true }, 0, "src");
        let file = row(files::RowKind::File, 1, "main.rs");
        assert_eq!(key_does(VK_RIGHT, false, Some(&closed)), KeyDoes::Toggle);
        assert_eq!(key_does(VK_RIGHT, false, Some(&open)), KeyDoes::Next);
        assert_eq!(key_does(VK_RIGHT, false, Some(&file)), KeyDoes::Nothing);
        assert_eq!(key_does(VK_RIGHT, false, None), KeyDoes::Nothing);
    }

    #[test]
    fn left_closes_an_open_folder_and_otherwise_goes_up_to_the_folder_a_row_is_in() {
        let open = row(files::RowKind::Folder { open: true }, 0, "src");
        let closed_inside = row(files::RowKind::Folder { open: false }, 1, "ui");
        let file_inside = row(files::RowKind::File, 1, "main.rs");
        let file_at_the_top = row(files::RowKind::File, 0, "README.md");
        assert_eq!(key_does(VK_LEFT, false, Some(&open)), KeyDoes::Toggle);
        assert_eq!(key_does(VK_LEFT, false, Some(&closed_inside)), KeyDoes::Parent);
        assert_eq!(key_does(VK_LEFT, false, Some(&file_inside)), KeyDoes::Parent);
        assert_eq!(key_does(VK_LEFT, false, Some(&file_at_the_top)), KeyDoes::Nothing, "there is no folder above a row at the top");
    }

    #[test]
    fn enter_and_space_choose_a_row_and_with_control_open_a_file_in_its_app() {
        let folder = row(files::RowKind::Folder { open: false }, 0, "src");
        let file = row(files::RowKind::File, 0, "a.md");
        let more = row(files::RowKind::More(40), 0, "");
        for key in [VK_RETURN, VK_SPACE] {
            assert_eq!(key_does(key, false, Some(&folder)), KeyDoes::Act);
            assert_eq!(key_does(key, false, Some(&file)), KeyDoes::Act);
            assert_eq!(key_does(key, true, Some(&file)), KeyDoes::OpenInApp);
            assert_eq!(key_does(key, true, Some(&folder)), KeyDoes::Act, "a folder has no app: it opens");
            assert_eq!(key_does(key, false, Some(&more)), KeyDoes::Act);
            assert_eq!(key_does(key, false, None), KeyDoes::Nothing);
        }
        assert_eq!(key_does(VK_F5, false, None), KeyDoes::Refresh);
        assert_eq!(key_does(VK_DOWN, false, Some(&file)), KeyDoes::Nothing, "the arrows up and down are the list's own");
    }

    #[test]
    fn what_a_reader_says_of_a_row_begins_with_its_name_and_tells_the_rest() {
        let mut changed = row(files::RowKind::File, 0, "app.py");
        changed.mark = Mark::Changed(11, 2);
        assert_eq!(spoken(&row(files::RowKind::File, 0, "notes.md")), "notes.md");
        assert!(spoken(&changed).starts_with("app.py") && spoken(&changed).contains("11") && spoken(&changed).contains('2'));
        let folder = spoken(&row(files::RowKind::Folder { open: true }, 0, "src"));
        assert!(folder.starts_with("src") && folder.contains("folder"));
        assert!(spoken(&row(files::RowKind::More(38), 0, "")).contains("38"));
        assert!(spoken(&row(files::RowKind::Problem("Access is denied.".into()), 0, "")).contains("Access is denied."));
    }
}
