//! What a test of the running app can do to the terminals without a keyboard, a pointer, a focus or a clipboard: a debug build, with `DOGMIND_TEST_TERMINAL` set, takes a command as a
//! `WM_COPYDATA` message (a line of words, as `COPYDATASTRUCT` carries it) and answers with its result, or writes what it was asked to read into a file it was given the path of.
//!
//! - `new [shell]`: a terminal, in that shell or the one last chosen; the answer is its id (0: none).
//! - `input id text`: the text, with `\r`, `\n`, `\t`, `\e`, `\\` and `\xNN` as escapes, to the shell, as written bytes; `type id text` the same as the characters a keyboard would make.
//! - `paste id text`: the same as a paste; `select id` the tab; `close id` the tab's cross; `tab index` a press on a tab (two of them soon after rename it); `rename name` what is typed
//!   in the field a tab is renamed in, kept.
//! - `drawn id`: whether what the window last drew of the terminal is what its screen holds now.
//! - `colors id path`: each character in view that is not a space, with its two colours as the shell said them.
//! - `grid id path`, `view id path`, `cells id path`, `info id path`, `list path`: the text of the screen and what was kept above it, of what is in view, where each character stands,
//!   what is known of the terminal, and the terminals in the order of the tabs.
//! - `selection id row col row col`, `selected id path`, `scroll id lines`: the selection, what it holds, and the view moved back.
//! - `focus on|off`: the window draws as if it had the keys; `menu shells|terminal|choose n|close`.
//! - `shot path`: a picture of the window as it draws itself, its child windows over it, written as a PNG. A window on a station with no display (a command's in the
//!   sandbox) is on no screen: the system has nothing of it to give, PrintWindow gives an empty picture and a copy of its device fails, so a test there takes this.

use std::sync::OnceLock;

use super::*;

/// The `dwData` of the message.
pub(in crate::ui) const HOOK: usize = 0x5445_524D;

/// Whether this process takes the commands.
pub(in crate::ui) fn hooks_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("DOGMIND_TEST_TERMINAL").is_some())
}

/// The text with its escapes made.
fn unescaped(text: &str) -> String {
    let mut made = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            made.push(c);
            continue;
        }
        match chars.next() {
            Some('r') => made.push('\r'),
            Some('n') => made.push('\n'),
            Some('t') => made.push('\t'),
            Some('e') => made.push('\x1b'),
            Some('\\') => made.push('\\'),
            Some('x') => {
                let digits: String = chars.by_ref().take(2).collect();
                made.push(u8::from_str_radix(&digits, 16).map_or('?', char::from));
            }
            Some(other) => {
                made.push('\\');
                made.push(other);
            }
            None => made.push('\\'),
        }
    }
    made
}

/// A window and the windows in it, drawn by themselves (`WM_PRINTCLIENT`) into a bitmap of the top one's client area: each child over its parent, the lowest of
/// a parent's children first.
unsafe fn own_picture(window: HWND) -> Result<image::RgbaImage, String> {
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(window, &mut client);
    let (width, height) = (client.right - client.left, client.bottom - client.top);
    if width <= 0 || height <= 0 {
        return Err("the window has no client area".to_string());
    }
    let mut all = vec![0u8; (width * height * 4) as usize];
    drawn_into(window, window, &mut all, width, height)?;
    // (BGRA from the bitmap, its alpha left as the drawing left it: what was drawn is all there is)
    let mut picture = image::RgbaImage::new(width as u32, height as u32);
    for (at, pixel) in picture.pixels_mut().enumerate() {
        let bgra = &all[at * 4..at * 4 + 4];
        *pixel = image::Rgba([bgra[2], bgra[1], bgra[0], 255]);
    }
    Ok(picture)
}

/// `window` drawn into `all` (the top window's client area, `width` by `height`, BGRA rows from the top) where it lies in `top`, and then its visible children.
unsafe fn drawn_into(top: HWND, window: HWND, all: &mut [u8], width: i32, height: i32) -> Result<(), String> {
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(window, &mut client);
    let (w, h) = (client.right, client.bottom);
    let mut corner = POINT { x: 0, y: 0 };
    MapWindowPoints(window, top, &mut corner, 1);
    if w > 0 && h > 0 {
        let screen = GetDC(std::ptr::null_mut());
        let device = CreateCompatibleDC(screen);
        ReleaseDC(std::ptr::null_mut(), screen);
        if device.is_null() {
            return Err("a device to draw in".to_string());
        }
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(device, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if bitmap.is_null() || bits.is_null() {
            DeleteDC(device);
            return Err("a bitmap to draw in".to_string());
        }
        let before = SelectObject(device, bitmap);
        SendMessageW(window, WM_PRINTCLIENT, device as WPARAM, (PRF_CLIENT | PRF_ERASEBKGND) as LPARAM);
        GdiFlush();
        let drawn = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
        for row in 0..h {
            let y = corner.y + row;
            if y < 0 || y >= height {
                continue;
            }
            let (from, to) = (corner.x.max(0), (corner.x + w).min(width));
            if from >= to {
                continue;
            }
            let source = ((row * w + (from - corner.x)) * 4) as usize;
            let target = ((y * width + from) * 4) as usize;
            let length = ((to - from) * 4) as usize;
            all[target..target + length].copy_from_slice(&drawn[source..source + length]);
        }
        SelectObject(device, before);
        DeleteObject(bitmap);
        DeleteDC(device);
    }
    // (the topmost child is the first; the lowest is drawn first, so that what lies over it is over it in the picture too)
    let mut children = Vec::new();
    let mut child = GetWindow(window, GW_CHILD);
    while !child.is_null() {
        // (its own mark, not IsWindowVisible: a window started hidden, as a test's is, has every child of it not visible by that)
        if GetWindowLongPtrW(child, GWL_STYLE) as u32 & WS_VISIBLE != 0 {
            children.push(child);
        }
        child = GetWindow(child, GW_HWNDNEXT);
    }
    for child in children.into_iter().rev() {
        drawn_into(top, child, all, width, height)?;
    }
    Ok(())
}

impl App {
    fn terminal_by_id(&self, id: u64) -> Option<&Terminal> {
        self.term.terminals.iter().find(|terminal| terminal.id == id)
    }

    pub(in crate::ui) unsafe fn terminal_hook(&mut self, command: &str) -> isize {
        let (verb, rest) = command.split_once(' ').unwrap_or((command, ""));
        let (first, after) = rest.split_once(' ').unwrap_or((rest, ""));
        let id: u64 = first.parse().unwrap_or(0);
        let wrote = |path: &str, text: &str| isize::from(std::fs::write(path, text).is_ok());
        match verb {
            "new" => self.terminal_new(if rest.is_empty() { None } else { Some(rest) }).map_or(0, |id| id as isize),
            "input" => self.terminal_by_id(id).map_or(0, |terminal| {
                terminal.write(unescaped(after).as_bytes());
                1
            }),
            "type" => {
                self.terminal_select(id);
                for unit in unescaped(after).encode_utf16() {
                    self.terminal_char(unit, false);
                }
                1
            }
            "paste" => {
                self.terminal_select(id);
                self.terminal_paste(Some(unescaped(after)));
                1
            }
            "select" => {
                self.terminal_select(id);
                1
            }
            "close" => {
                self.terminal_close(id);
                1
            }
            "tab" => {
                self.terminal_tab_pressed(first.parse().unwrap_or(usize::MAX));
                1
            }
            "rename" => {
                if self.term.renaming.is_none() {
                    return 0;
                }
                set_text(self.term.rename_field, rest);
                self.terminal_rename_end(true);
                1
            }
            "grid" => self.terminal_by_id(id).map_or(0, |terminal| wrote(after, &terminal.with_screen(|screen| screen.text()))),
            "view" => self.terminal_by_id(id).map_or(0, |terminal| wrote(after, &terminal.with_screen(|screen| screen.visible_lines()).join("\n"))),
            "cells" => self.terminal_by_id(id).map_or(0, |terminal| {
                let cells = terminal.with_screen(|screen| screen.cells_in_view());
                wrote(after, &cells.iter().map(|(row, col, ch, width)| format!("{row}\t{col}\t{ch}\t{width}\n")).collect::<String>())
            }),
            // The runs of text the window would draw of this terminal now, each with its row, its column and its colour: what is drawn is made of these.
            "runs" => self.terminal_by_id(id).map_or(0, |terminal| {
                let snapshot = terminal.with_screen(|screen| screen.snapshot());
                let scene = scene_of(&snapshot, true, theme::is_light(), scale(self.dpi, PAD), (self.term.area.2, self.term.area.3), self.dpi);
                wrote(after, &scene.runs.iter().map(|run| format!("{}\t{}\t{:06X}\t{}\n", run.row, run.col, run.color, run.text)).collect::<String>())
            }),
            // 1 where what the window last drew of this terminal is what its screen holds now.
            "paintnote" => wrote(rest, &self.term.paint_note),
            "drawn" => self.terminal_by_id(id).map_or(0, |terminal| {
                let now = terminal.with_screen(|screen| snapshot_sum(&screen.snapshot()));
                isize::from(self.term.painted == (terminal.id, now))
            }),
            "colors" => self.terminal_by_id(id).map_or(0, |terminal| {
                let snapshot = terminal.with_screen(|screen| screen.snapshot());
                let mut text = String::new();
                for row in 0..snapshot.rows {
                    for col in 0..snapshot.cols {
                        let glyph = &snapshot.cells[row * snapshot.cols + col];
                        if glyph.ch != ' ' && glyph.width > 0 {
                            text.push_str(&format!("{row}\t{col}\t{}\t{:?}\t{:?}\t{}\t{}\n", glyph.ch, glyph.fg, glyph.bg, glyph.bold, glyph.inverse));
                        }
                    }
                }
                wrote(after, &text)
            }),
            "info" => self.terminal_by_id(id).map_or(0, |terminal| {
                let (size, offset, history, modes, selected) = terminal.with_screen(|screen| (screen.size(), screen.offset(), screen.history(), screen.modes(), screen.has_selection()));
                let processes: Vec<String> = terminal.processes().iter().map(u32::to_string).collect();
                let text = format!(
                    "pid={}\nprocesses={}\nexit={}\nover={}\ncols={}\nrows={}\noffset={offset}\nhistory={history}\nname={}\nshell={}\nfolder={}\nactive={}\nselected={selected}\nbracketed={}\napplication_cursor={}\nalternate={}\nwindow={:?}\nshown={}\ncell={}\n",
                    terminal.pid(),
                    processes.join(","),
                    terminal.exit_code().map_or("none".to_string(), |code| code.to_string()),
                    terminal.is_over(),
                    size.0,
                    size.1,
                    terminal.name,
                    terminal.shell.id,
                    terminal.folder.display(),
                    self.term.active == Some(terminal.id),
                    modes.bracketed_paste,
                    modes.application_cursor,
                    modes.alternate_screen,
                    self.term.area,
                    self.terminal_shown(),
                    self.term.font.as_ref().map_or("none".to_string(), |font| format!("{}x{}", font.cell_width, font.cell_height)),
                );
                wrote(after, &text)
            }),
            "list" => {
                let text: String = self.term.terminals.iter().map(|terminal| format!("{}\t{}\t{}\n", terminal.id, terminal.name, self.term.active == Some(terminal.id))).collect();
                wrote(rest, &text)
            }
            "selection" => {
                let numbers: Vec<usize> = after.split_whitespace().filter_map(|number| number.parse().ok()).collect();
                let (Some(terminal), [r1, c1, r2, c2]) = (self.terminal_by_id(id), numbers.as_slice()) else { return 0 };
                terminal.with_screen(|screen| {
                    screen.select_begin(*c1, *r1, false, SelectBy::Characters);
                    screen.select_extend(*c2, *r2, true);
                });
                InvalidateRect(self.term.window, std::ptr::null(), 0);
                1
            }
            "selected" => self.terminal_by_id(id).map_or(0, |terminal| wrote(after, &terminal.with_screen(|screen| screen.selected_text()).unwrap_or_default())),
            "scroll" => {
                let lines: i32 = after.trim().parse().unwrap_or(0);
                let done = self.terminal_by_id(id).map(|terminal| terminal.with_screen(|screen| screen.scroll(lines)));
                InvalidateRect(self.term.window, std::ptr::null(), 0);
                isize::from(done.is_some())
            }
            "focus" => {
                self.term.forced_focus = rest == "on";
                InvalidateRect(self.term.window, std::ptr::null(), 0);
                1
            }
            "menu" => {
                match rest.split_once(' ').unwrap_or((rest, "")) {
                    ("terminal", _) => self.terminal_context_menu((scale(self.dpi, 40), scale(self.dpi, 40))),
                    ("choose", number) => self.files_menu_choose(number.trim().parse().unwrap_or(usize::MAX)),
                    _ => self.files_menu_close(true),
                }
                1
            }
            // The words of the tip of a window of the app, named by its handle as a number, in the language now: what the pointer resting on it shows.
            "tip" => {
                let window = first.parse::<isize>().unwrap_or(0) as HWND;
                let mut text = vec![0u16; 512];
                let mut tool: TTTOOLINFOW = std::mem::zeroed();
                tool.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
                tool.hwnd = GetParent(window);
                tool.uId = window as usize;
                tool.lpszText = text.as_mut_ptr();
                SendMessageW(self.tooltip, TTM_GETTEXTW, text.len(), std::ptr::addr_of_mut!(tool) as LPARAM);
                let end = text.iter().position(|unit| *unit == 0).unwrap_or(text.len());
                wrote(after, &String::from_utf16_lossy(&text[..end]))
            }
            "shot" => isize::from(own_picture(self.window).and_then(|picture| picture.save(rest.trim()).map_err(|error| error.to_string())).is_ok()),
            // The window is drawn now, so that a picture taken after it is of what the screen holds and not of what the system last kept of the window.
            "repaint" => {
                RedrawWindow(self.window, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN);
                1
            }
            _ => 0,
        }
    }
}
