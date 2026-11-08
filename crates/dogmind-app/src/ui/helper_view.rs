//! An `explore` helper in the window: its card in the conversation, which says as the helper works what it is on and can stop it alone, and its steps, shown in the panel on the right in place
//! of what the panel showed, from the file the agent kept. What the card says and what the page holds is decided in `cards` and `steps`, without a window; here it is written, drawn and pressed.

use super::*;
use crate::cards::HelperState;
use crate::steps::{self, Block, Piece};

/// A helper at work: when the agent last told how it was going, and the second its card last showed.
pub(super) struct HelperLive {
    pub told_at: std::time::Instant,
    pub shown_second: u64,
}

/// The steps of a helper, as the panel on the right shows them.
pub(super) struct StepsView {
    /// The helper's task as a line, and how it went.
    pub title: String,
    pub facts: String,
    pub blocks: Vec<Block>,
    /// Where the blocks stand at a width and a scale, and how tall the page is: found when the page is drawn at a width it has not been laid out for.
    placed: Vec<steps::Placed>,
    total: i32,
    laid_for: (i32, u32),
    pub scroll: i32,
    /// Where on the thumb the pointer holds it.
    pub thumb_grab: Option<i32>,
}

/// The space between the blocks of the page (unscaled pixels), and the card's rows.
const BLOCK_GAP: i32 = 12;
const CALL_ROW: i32 = 22;

impl App {
    fn helper_card(&self, call: &str) -> Option<&Card> {
        self.shown.iter().rev().find_map(|item| match item {
            Shown::Card(card) if card.helper_call() == Some(call) => Some(card),
            _ => None,
        })
    }

    fn helper_card_mut(&mut self, call: &str) -> Option<&mut Card> {
        self.shown.iter_mut().rev().find_map(|item| match item {
            Shown::Card(card) if card.helper_call() == Some(call) => Some(card),
            _ => None,
        })
    }

    /// Whether the conversation has a card for the helper of this call.
    pub(super) fn has_helper_card(&self, call: &str) -> bool {
        self.helper_card(call).is_some()
    }

    /// The helper of this call has begun: the call's row, which was begun with the call, gives way to its card.
    pub(super) unsafe fn helper_begin(&mut self, id: String, task: String, file: Option<String>) {
        let number = file.as_deref().and_then(|name| session::helper_number(Path::new(name)));
        let _ = self.take_run_entry(&id);
        self.helpers_live.insert(id.clone(), HelperLive { told_at: std::time::Instant::now(), shown_second: 0 });
        if test_log_on() {
            test_log(&format!("helper {id} begins <{task}> file={file:?}"));
        }
        self.write_card(&Card::helper_begun(&id, &task, number));
    }

    /// The helper called a tool.
    pub(super) unsafe fn helper_step_told(&mut self, id: &str, step: u32, most: u32, tool: &str, summary: &str, tokens: u64, elapsed: std::time::Duration) {
        let Some(card) = self.helper_card_mut(id) else { return };
        card.helper_step(step, most, tool, summary, tokens, elapsed.as_millis() as u64);
        if let Some(live) = self.helpers_live.get_mut(id) {
            live.told_at = std::time::Instant::now();
        }
        if test_log_on() {
            test_log(&format!("helper {id} step {step} of {most} {tool} <{summary}> tokens={tokens}"));
        }
        self.helper_refresh(id);
    }

    /// The helper is over: its card is written again as what it came to, which has other rows than a card at work.
    pub(super) unsafe fn helper_end_told(&mut self, id: &str, how: &agent::HelperEnd, steps: u32, files: u32, tokens: u64, elapsed: std::time::Duration, number: Option<u32>) {
        self.helpers_live.remove(id);
        let Some(card) = self.helper_card_mut(id) else { return };
        card.helper_ended(how, steps, files, tokens, elapsed.as_millis() as u64, number);
        if test_log_on() {
            test_log(&format!("helper {id} ends {how:?} steps={steps} files={files} tokens={tokens}"));
        }
        self.reflow();
    }

    /// The parts of the row that says where a working helper is, with Stop beside it.
    pub(super) unsafe fn helper_status_parts(&self, call: &str, state: &HelperState, since: u64) -> Vec<CardPart> {
        let s = |value: i32| scale(self.dpi, value);
        let (words, figures) = cards::helper_status(state, since);
        let stop = t("Stop");
        vec![
            CardPart { text: format!("{words}{PART}{figures}"), kind: ButtonKind::Status, act: CardAct::None, width: 0, tip: String::new() },
            CardPart {
                text: stop.to_string(),
                kind: ButtonKind::Secondary,
                act: CardAct::StopHelper(call.to_string()),
                width: text_width(self.fonts.strong, stop) + s(ds::size::BUTTON_PADDING),
                tip: t("Stop only this helper. The conversation goes on without its answer.").to_string(),
            },
        ]
    }

    /// A card at work says what the agent last told, with the time run on: the words of its rows are changed where they stand and only they are drawn again, as the rows keep their height.
    pub(super) unsafe fn helper_refresh(&mut self, call: &str) {
        let Some(Card::Helper { state, .. }) = self.helper_card(call).cloned() else { return };
        let HelperState::Working { ms, .. } = state else { return };
        let since = self.helpers_live.get(call).map_or(0, |live| live.told_at.elapsed().as_millis() as u64);
        let (words, figures) = cards::helper_status(&state, since);
        let recent = cards::helper_recent(&state);
        let mut changed = Vec::new();
        for (index, slot) in self.card_slots.iter_mut().enumerate() {
            let Some((owner, row)) = &slot.owner else { continue };
            if owner != call {
                continue;
            }
            let text = if *row == 0 { format!("{words}{PART}{figures}") } else { recent.get(*row - 1).cloned().unwrap_or_default() };
            if let Some(part) = slot.parts.first_mut().filter(|part| part.text != text) {
                part.text = text;
                changed.push(index);
            }
        }
        if let Some(live) = self.helpers_live.get_mut(call) {
            live.shown_second = (ms + since) / 1000;
        }
        for index in changed {
            self.invalidate_overlay(OverlayKey::Card(index, 0));
        }
    }

    /// Once the clock of a card at work has run to another second, it is drawn again.
    pub(super) unsafe fn helper_tick(&mut self) {
        if self.helpers_live.is_empty() || self.panel != Panel::Chat {
            return;
        }
        let calls: Vec<String> = self.helpers_live.keys().cloned().collect();
        for call in calls {
            let ms = match self.helper_card(&call) {
                Some(Card::Helper { state: HelperState::Working { ms, .. }, .. }) => *ms,
                _ => continue,
            };
            let Some(live) = self.helpers_live.get(&call) else { continue };
            if (ms + live.told_at.elapsed().as_millis() as u64) / 1000 != live.shown_second {
                self.helper_refresh(&call);
            }
        }
    }

    /// The press on a card's Stop: the helper alone is stopped, and its card says so till the agent says it has ended.
    pub(super) unsafe fn helper_stop_pressed(&mut self, call: &str) {
        self.agent.helper_stop.stop(call);
        if let Some(card) = self.helper_card_mut(call) {
            card.helper_stopping();
        }
        if test_log_on() {
            test_log(&format!("helper {call} stop pressed"));
        }
        self.helper_refresh(call);
    }

    /// The turn is over: a card still at work is not.
    pub(super) fn helpers_settle(&mut self) -> bool {
        let mut changed = false;
        for item in &mut self.shown {
            if let Shown::Card(card) = item {
                changed |= card.helper_settled();
            }
        }
        self.helpers_live.clear();
        changed
    }

    /// The helpers at work, for a session that goes out of sight: its agent goes on, and what it says of them is kept with it.
    pub(super) fn take_live_helpers(&mut self) -> Vec<(Card, std::time::Instant)> {
        let live = std::mem::take(&mut self.helpers_live);
        self.shown.iter().filter_map(|item| match item {
            Shown::Card(card @ Card::Helper { state: HelperState::Working { .. }, call, .. }) => live.get(call).map(|live| (card.clone(), live.told_at)),
            _ => None,
        }).collect()
    }

    /// A session at work is on screen again: the cards of its helpers at work are written.
    pub(super) unsafe fn restore_live_helpers(&mut self, helpers: Vec<(Card, std::time::Instant)>) {
        for (card, told_at) in helpers {
            let Some(call) = card.helper_call().map(str::to_string) else { continue };
            self.helpers_live.insert(call, HelperLive { told_at, shown_second: 0 });
            self.write_card(&card);
        }
    }

    // ---- The steps, in the panel on the right.

    /// "Show its steps": the helper's file is read, and its conversation takes the panel's place.
    pub(super) unsafe fn steps_open(&mut self, call: &str) {
        let Some(Card::Helper { task, number: Some(number), state, .. }) = self.helper_card(call).cloned() else { return };
        let Some(parent) = self.current_session.clone() else { return };
        let Some(path) = session::helper_path(&parent, number) else {
            self.say(t("Its steps are not kept: the file is gone."));
            return;
        };
        let messages = match session::read(&path) {
            Ok(messages) => messages,
            Err(error) => {
                self.say(&tf("Its steps could not be read: {0}", &[&error]));
                return;
            }
        };
        let blocks = steps::shown(steps::blocks_of(&messages));
        let facts = cards::helper_outcome(&state).map(|(words, _)| words).unwrap_or_default();
        if self.preview_file.is_some() {
            self.close_preview();
        }
        if test_log_on() {
            test_log(&format!("steps open <{task}> blocks={} calls={}", blocks.len(), steps::blocks_summary(&blocks).0));
        }
        self.steps = Some(StepsView { title: task, facts, blocks, placed: Vec::new(), total: 0, laid_for: (0, 0), scroll: 0, thumb_grab: None });
        self.layout();
        // (a panel needs the width: without it there is nothing to see, and nothing else would say why)
        if self.right_width == 0 {
            self.steps = None;
            self.layout();
            self.say(t("The window is too narrow for the panel: make it wider."));
            return;
        }
        InvalidateRect(self.window, std::ptr::null(), 0);
    }

    /// The steps are put away, and the panel shows what it showed.
    pub(super) unsafe fn steps_close(&mut self) {
        if self.steps.take().is_some() {
            if test_log_on() {
                test_log("steps closed");
            }
            self.layout();
            InvalidateRect(self.window, std::ptr::null(), 0);
        }
    }

    /// The panel with the steps: level with the session's name its own name and the button that closes it, under them the task and how it went, and the page to the foot.
    pub(super) unsafe fn layout_steps(&mut self, shapes: &mut Vec<Shape>, x: i32, width: i32, height: i32) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let bar = s(ds::size::TITLE_STRIP);
        let button = s(ds::size::ROW_ACTION);
        let margin = s(ds::size::RIGHT_GAP);
        shapes.push(Shape::Fill { area: rect(x - margin, bar - s(ds::space::PIXEL), width + 2 * margin, s(ds::space::PIXEL)), color: theme::line() });
        let at = x + width - button;
        SetWindowPos(self.steps_close, std::ptr::null_mut(), at, (bar - button) / 2, button, button, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        shapes.push(Shape::Text { area: rect(x, 0, at - x - s(ds::space::S), bar), text: t("Helper's steps").to_string(), font: FontKind::Strong, color: theme::text(), flags: ONE_LINE });
        let Some(view) = &self.steps else { return };
        let mut y = bar + s(ds::space::S);
        shapes.push(Shape::Text { area: rect(x, y, width, s(ds::size::TEXT_LINE)), text: view.title.clone(), font: FontKind::Small, color: theme::soft(), flags: ONE_LINE });
        y += s(ds::space::XL);
        if !view.facts.is_empty() {
            shapes.push(Shape::Text { area: rect(x, y, width, s(ds::size::TEXT_LINE)), text: view.facts.clone(), font: FontKind::Small, color: theme::muted(), flags: ONE_LINE });
            y += s(ds::space::XL);
        }
        y += s(ds::space::S);
        // (the page to the panel's edges: its blocks keep the margin themselves, and its thumb is at the edge)
        place(self.steps_view, x - margin, y, width + 2 * margin, (height - y).max(s(40)));
        show(self.steps_view, true);
    }

    /// How tall the page is in the room it has; the blocks are laid out again only when the room or the scale is not the one they were laid out for.
    unsafe fn steps_lay_out(&mut self, width: i32) {
        let dpi = self.dpi;
        let Some(view) = self.steps.as_ref() else { return };
        if view.laid_for == (width, dpi) {
            return;
        }
        let gap = scale(dpi, BLOCK_GAP);
        let (placed, total) = steps::place(&view.blocks, gap, |block| self.steps_block(None, block, 0, 0, width));
        if let Some(view) = self.steps.as_mut() {
            view.placed = placed;
            view.total = total;
            view.laid_for = (width, dpi);
        }
    }

    /// The most the page can be scrolled in the window it is in.
    unsafe fn steps_room(&self) -> i32 {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.steps_view, &mut client);
        self.steps.as_ref().map_or(0, |view| steps::most_scroll(view.total, client.bottom))
    }

    pub(super) unsafe fn steps_to(&mut self, scroll: i32) {
        let room = self.steps_room();
        if let Some(view) = self.steps.as_mut() {
            view.scroll = scroll.clamp(0, room);
        }
        InvalidateRect(self.steps_view, std::ptr::null(), 0);
    }

    pub(super) unsafe fn steps_wheel(&mut self, wparam: WPARAM) {
        let notches = i32::from(((wparam >> 16) & 0xFFFF) as u16 as i16) / 120;
        let scroll = self.steps.as_ref().map_or(0, |view| view.scroll);
        self.steps_to(scroll - notches * 3 * scale(self.dpi, 20));
    }

    /// A key on the page: the arrows and the page keys move it, Esc puts it away.
    pub(super) unsafe fn steps_key(&mut self, key: u16) {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.steps_view, &mut client);
        let scroll = self.steps.as_ref().map_or(0, |view| view.scroll);
        let line = scale(self.dpi, 40);
        match key {
            VK_UP => self.steps_to(scroll - line),
            VK_DOWN => self.steps_to(scroll + line),
            VK_PRIOR => self.steps_to(scroll - client.bottom * 9 / 10),
            VK_NEXT => self.steps_to(scroll + client.bottom * 9 / 10),
            VK_HOME => self.steps_to(0),
            VK_END => self.steps_to(i32::MAX / 2),
            VK_ESCAPE => self.steps_close(),
            _ => {}
        }
    }

    /// The page: the blocks in view, and the thumb at its edge.
    pub(super) unsafe fn paint_steps(&mut self, device: HDC, client: RECT) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let brush = CreateSolidBrush(theme::sidebar());
        FillRect(device, &client, brush);
        DeleteObject(brush);
        let (strip, margin) = (s(ds::space::M), s(ds::size::RIGHT_GAP));
        let width = (client.right - strip - 2 * margin).max(s(120));
        self.steps_lay_out(width);
        // (a window made smaller keeps the view inside the page)
        let room = self.steps_room();
        let Some(view) = self.steps.as_mut() else { return };
        view.scroll = view.scroll.clamp(0, room);
        let Some(view) = self.steps.as_ref() else { return };
        for at in steps::in_view(&view.placed, view.scroll, client.bottom) {
            self.steps_block(Some(device), &view.blocks[at], margin, view.placed[at].top - view.scroll, width);
        }
        if view.total > client.bottom {
            let tall = thumb_height(client.bottom, client.bottom, view.total, s(ds::space::XXL));
            let at = thumb_offset(client.bottom - tall, view.scroll, view.total - client.bottom);
            let color = if view.thumb_grab.is_some() { theme::muted() } else { theme::line() };
            rounded(device, rect(client.right - strip + s(2), at, strip - s(4), tall), (strip - s(4)) / 2, Some(color), None);
        }
    }

    /// One block of the page with its left edge at `x` and its top at `y`, `width` wide: drawn when there is a device, and its height either way (what is measured is what is drawn).
    unsafe fn steps_block(&self, device: Option<HDC>, block: &Block, x: i32, y: i32, width: i32) -> i32 {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let fonts = &self.fonts;
        let wrap = DT_WORDBREAK | DT_NOPREFIX;
        let paragraph = |words: &str, font: HFONT, color: u32, x: i32, y: i32, width: i32| -> i32 {
            let tall = text_height(font, words, width);
            if let Some(device) = device {
                draw_text(device, words, rect(x, y, width, tall), font, color, wrap);
            }
            tall
        };
        match block {
            Block::Task(words) => {
                let pad = s(10);
                let tall = text_height(fonts.body, words, width - 2 * pad) + 2 * pad;
                if let Some(device) = device {
                    rounded(device, rect(x, y, width, tall), s(ds::radius::CONTROL), Some(theme::user_back()), None);
                    draw_text(device, words, rect(x + pad, y + pad, width - 2 * pad, tall - 2 * pad), fonts.body, theme::text(), wrap);
                }
                tall
            }
            Block::Said(pieces) => {
                let mut used = 0;
                for (at, piece) in pieces.iter().enumerate() {
                    if at > 0 {
                        used += s(6);
                    }
                    used += match piece {
                        Piece::Paragraph(words) => paragraph(words, fonts.body, theme::text(), x, y + used, width),
                        Piece::Heading(words) => paragraph(words, fonts.strong, theme::text(), x, y + used, width),
                        Piece::Item(words) => {
                            let indent = s(16);
                            if let Some(device) = device {
                                draw_text(device, "\u{2022}", rect(x + s(3), y + used, indent, s(CALL_ROW)), fonts.body, theme::muted(), DT_NOPREFIX | DT_SINGLELINE);
                            }
                            paragraph(words, fonts.body, theme::text(), x + indent, y + used, width - indent)
                        }
                        Piece::Code(lines) => self.steps_code(device, lines, 0, theme::code(), x, y + used, width),
                    };
                }
                used
            }
            Block::Call { tool, summary, ok, output, cut } => {
                let row = s(CALL_ROW);
                if let Some(device) = device {
                    let (mark, color) = match ok {
                        Some(true) => ("check", theme::good()),
                        Some(false) => ("x", theme::bad()),
                        None => ("square", theme::faint()),
                    };
                    icon::draw(device, mark, rect(x, y, s(ds::space::L), row), s(ds::size::ICON_SMALL), color);
                    let name = t(tool_name(tool));
                    let words = if summary.is_empty() { name.to_string() } else { format!("{name}  {summary}") };
                    draw_text(device, &words, rect(x + s(ds::space::XL), y, width - s(ds::space::XL), row), fonts.small, if *ok == Some(false) { theme::bad() } else { theme::muted() }, ONE_LINE);
                }
                if output.is_empty() && *cut == 0 {
                    return row;
                }
                row + self.steps_code(device, output, *cut, if *ok == Some(false) { theme::bad() } else { theme::code() }, x, y + row, width)
            }
            Block::Note(words) => paragraph(words, fonts.small, theme::faint(), x, y, width),
        }
    }

    /// Lines of code or of what a call brought back, on the shading a block of code has in the conversation, one row each and cut at the width; `cut` more lines are counted in a last row.
    unsafe fn steps_code(&self, device: Option<HDC>, lines: &[String], cut: usize, color: u32, x: i32, y: i32, width: i32) -> i32 {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let (row, pad) = (s(ds::size::DIFF_ROW), s(8));
        let rows = lines.len() + usize::from(cut > 0);
        let tall = rows as i32 * row + 2 * pad;
        if let Some(device) = device {
            // (the panel is the sidebar's colour, and a block of code in the light theme is nearly it: white sets it off, as it does a button here)
            let back = if theme::is_light() { theme::surface() } else { theme::code_back() };
            rounded(device, rect(x, y, width, tall), s(ds::radius::SMALL), Some(back), None);
            let flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX;
            for (at, line) in lines.iter().enumerate() {
                draw_text(device, &line.replace('\t', "    "), rect(x + pad, y + pad + at as i32 * row, width - 2 * pad, row), self.fonts.code, color, flags);
            }
            if cut > 0 {
                draw_text(device, &tf("… {0} more lines", &[&cut]), rect(x + pad, y + pad + lines.len() as i32 * row, width - 2 * pad, row), self.fonts.small, theme::muted(), flags);
            }
        }
        tall
    }
}

/// The page of a helper's steps: its paint, the wheel, and a drag of its thumb.
pub(super) unsafe extern "system" fn steps_procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(window, &mut client);
    let strip = scale(app.dpi, ds::space::M);
    match message {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let target = BeginPaint(window, &mut paint);
            if client.right > 0 && client.bottom > 0 {
                let device = CreateCompatibleDC(target);
                let bitmap = CreateCompatibleBitmap(target, client.right, client.bottom);
                let previous = SelectObject(device, bitmap);
                app.paint_steps(device, client);
                BitBlt(target, 0, 0, client.right, client.bottom, device, 0, 0, SRCCOPY);
                SelectObject(device, previous);
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            EndPaint(window, &paint);
            0
        }
        WM_MOUSEWHEEL => {
            app.steps_wheel(wparam);
            0
        }
        // (arrows and page keys come to it for itself)
        WM_GETDLGCODE => (DLGC_WANTARROWS | DLGC_WANTCHARS) as LRESULT,
        WM_KEYDOWN => {
            app.steps_key(wparam as u16);
            0
        }
        WM_LBUTTONDOWN => {
            SetFocus(window);
            SetCapture(window);
            let (total, scroll) = app.steps.as_ref().map_or((0, 0), |view| (view.total, view.scroll));
            if x >= client.right - strip && total > client.bottom {
                let tall = thumb_height(client.bottom, client.bottom, total, scale(app.dpi, ds::space::XXL));
                let at = thumb_offset(client.bottom - tall, scroll, total - client.bottom);
                // (on the thumb it goes with the pointer; off it, the thumb comes to the pointer first)
                let grab = if y >= at && y < at + tall { y - at } else { tall / 2 };
                if let Some(view) = app.steps.as_mut() {
                    view.thumb_grab = Some(grab);
                }
                app.steps_to((i64::from(y - grab) * i64::from(total - client.bottom) / i64::from((client.bottom - tall).max(1))) as i32);
            }
            0
        }
        WM_MOUSEMOVE if wparam & 1 != 0 => {
            // MK_LBUTTON
            let (total, grab) = app.steps.as_ref().map_or((0, None), |view| (view.total, view.thumb_grab));
            if let Some(grab) = grab {
                let tall = thumb_height(client.bottom, client.bottom, total, scale(app.dpi, ds::space::XXL));
                app.steps_to((i64::from(y - grab) * i64::from((total - client.bottom).max(0)) / i64::from((client.bottom - tall).max(1))) as i32);
            }
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            if let Some(view) = app.steps.as_mut() {
                if view.thumb_grab.take().is_some() {
                    InvalidateRect(window, std::ptr::null(), 0);
                }
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}
