//! The Timeline view of the panel on the right, drawn and driven: at the top, always in view, what the model and the app are doing now and where the work is heading; under it the path the session
//! took, turn by turn, in rows that scroll. What it says is decided in `timeline_rows` without a window; here the session's files are read (on another thread: a long session takes a while), the
//! rows are measured and drawn, and the pointer and the keys are answered.
//!
//! The files have what has ended and the events say what is going on, so the two are put together at each paint: a read is asked for as an answer, a call or a turn ends, and between two reads
//! `timeline_rows::Live` keeps what the events said.

use std::time::Instant;

use super::*;
use crate::steps::{self, Placed};
use crate::timeline_rows::{self as path, Key, PersonWait, Progress, Source};
use dogmind_agent::timeline::Timeline;

pub(super) const TIMELINE_CLASS: &str = "DogTimeline";

/// The rows keep this far from the window's edge (unscaled pixels), which puts them level with the rest of the panel.
const PAD: i32 = 10;
/// A level of the path stands this much in from the one above it.
const LEVEL: i32 = 14;
/// What the chevron of a row and the icon after it take, before its words.
const MARK: i32 = 18;
/// The most lines of its own that what is going on now, and a thought, take.
const MOST_LINES: i32 = 3;
/// The lines the line of what goes on now takes at the most.
const NOW_LINES: i32 = 2;

/// Words that fit `tall` in `width`: as they are, or as many of them as fit with an ellipsis after.
unsafe fn within_lines(font: HFONT, words: &str, width: i32, tall: i32) -> String {
    if text_height(font, words, width) <= tall {
        return words.to_string();
    }
    let chars: Vec<char> = words.chars().collect();
    let (mut fits, mut over) = (0, chars.len());
    // (the most characters that fit, found by halves: a line of what goes on is redrawn every second)
    while over - fits > 1 {
        let middle = (fits + over) / 2;
        let tried: String = chars[..middle].iter().collect::<String>() + "…";
        if text_height(font, &tried, width) <= tall {
            fits = middle;
        } else {
            over = middle;
        }
    }
    chars[..fits].iter().collect::<String>().trim_end().to_string() + "…"
}
/// A row of the path that is a line of small words, and a card's inner space.
const CARD_PAD: i32 = 10;

/// The Timeline view's window, and what it shows.
pub(super) struct TimelineView {
    pub window: HWND,
    /// The field to look for something in the path, and what is looked for there (in small letters).
    pub search: HWND,
    query: String,
    pub live: path::Live,
    timeline: Option<Timeline>,
    /// Why the last read failed, if it did.
    error: Option<String>,
    /// A read is under way (on another thread), and another was asked for meanwhile.
    reading: bool,
    again: bool,
    /// What happened while the view was out of sight: it is read when it comes back.
    stale: bool,
    /// The first request of a turn is when its first lines are in the files.
    first_request_due: bool,
    /// What the person chose to open or shut, and the row the keys are on.
    opened: HashMap<Key, bool>,
    selected: Option<Key>,
    /// What the rows were last made from: they are made again only when it changes.
    made_from: Option<(u64, u64, path::Inputs, u64)>,
    /// Changes with the files that were read, and with the choices.
    serial: u64,
    opened_serial: u64,
    rows: Vec<path::Row>,
    generation: u64,
    placed: Vec<Placed>,
    total: i32,
    laid_for: Option<(i32, u32, u64)>,
    /// How tall the part that stays at the top was when it was last drawn.
    head_height: i32,
    pub scroll: i32,
    /// The view stays at the end while it grows, until it is moved away from it.
    follow: bool,
    /// Where on the thumb the pointer holds it.
    pub thumb_grab: Option<i32>,
    /// The part of each turn's time its band shows, where the person chose one; and a drag across a band under way (its turn, and where it began and is).
    spans: std::collections::BTreeMap<u64, (u64, u64)>,
    band_drag: Option<(u64, i32, i32)>,
    /// When the window was last drawn for the clock, and whether more has come since than that has shown.
    ticked: Instant,
    head_dirty: bool,
}

impl TimelineView {
    pub(super) unsafe fn new(parent: HWND) -> TimelineView {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide(TIMELINE_CLASS);
        let class = WNDCLASSW { lpfnWndProc: Some(timeline_procedure), hInstance: instance, lpszClassName: class_name.as_ptr(), hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW), ..std::mem::zeroed() };
        RegisterClassW(&class);
        let title = wide("Timeline");
        let window = CreateWindowExW(0, class_name.as_ptr(), title.as_ptr(), WS_CHILD | WS_CLIPSIBLINGS | WS_TABSTOP, 0, 0, 0, 0, parent, std::ptr::null_mut(), instance, std::ptr::null());
        // (a child of the window, as the panel's other fields are: what it tells comes to the window's own procedure)
        let search = control(parent, "EDIT", "", WS_TABSTOP | ES_AUTOHSCROLL as u32, 0, 0);
        TimelineView {
            window,
            search,
            query: String::new(),
            live: path::Live::new(Instant::now()),
            timeline: None,
            error: None,
            reading: false,
            again: false,
            stale: true,
            first_request_due: false,
            opened: HashMap::new(),
            selected: None,
            made_from: None,
            serial: 0,
            opened_serial: 0,
            rows: Vec::new(),
            generation: 0,
            placed: Vec::new(),
            total: 0,
            laid_for: None,
            head_height: 0,
            scroll: 0,
            follow: true,
            thumb_grab: None,
            spans: std::collections::BTreeMap::new(),
            band_drag: None,
            ticked: Instant::now(),
            head_dirty: false,
        }
    }
}

/// What a paint is given besides the rows: what is going on, and where the keys are.
struct Frame {
    now: path::Now,
    selected: Option<Key>,
}

/// The colour of words, by whose they are.
fn words_color(tone: path::Tone) -> u32 {
    match tone {
        path::Tone::Person | path::Tone::Model => theme::text(),
        path::Tone::Derived | path::Tone::Muted => theme::muted(),
        path::Tone::App | path::Tone::Plain => theme::soft(),
        path::Tone::Good => theme::good(),
        path::Tone::Bad => theme::bad(),
    }
}

/// The colour of the mark before a row.
fn mark_color(tone: path::Tone) -> u32 {
    match tone {
        path::Tone::Model => theme::accent(),
        path::Tone::Person => theme::accent_hover(),
        path::Tone::App => theme::warn(),
        path::Tone::Good => theme::good(),
        path::Tone::Bad => theme::bad(),
        path::Tone::Derived | path::Tone::Plain | path::Tone::Muted => theme::faint(),
    }
}

fn who_icon(who: path::Who) -> &'static str {
    match who {
        path::Who::Model => "cpu",
        path::Who::App => "settings",
        path::Who::Person => "circle-question-mark",
        path::Who::Idle => "paw-print",
    }
}

unsafe fn fill(device: HDC, area: RECT, color: u32) {
    let brush = CreateSolidBrush(color);
    FillRect(device, &area, brush);
    DeleteObject(brush);
}

impl App {
    /// The panel shows the Timeline view.
    pub(super) fn timeline_shown(&self) -> bool {
        self.view_shown(RightView::Timeline) && self.steps.is_none()
    }

    /// What the window knows of the present besides the events: whether a turn is at work, what the person is asked, and how much waits in the background.
    fn timeline_inputs(&self) -> path::Inputs {
        let wait = if let Some(approval) = &self.pending {
            Some(PersonWait::Approval { tool: approval.tool.clone(), summary: approval.summary.clone() })
        } else if self.question.is_some() {
            Some(PersonWait::Question)
        } else if self.plan_review.is_some() {
            Some(PersonWait::Plan)
        } else {
            None
        };
        // (the time now, for the band of the turn at work to reach)
        let wall_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|since| since.as_millis() as i64);
        path::Inputs { running: self.running, wait, background: self.waiting.commands.len() + self.waiting.waits.len(), wall_ms, query: self.timeline.query.clone(), spans: self.timeline.spans.clone() }
    }

    /// Nothing of the view is on screen: its window goes, and the keys go back to the composer if it had them.
    pub(super) unsafe fn timeline_hidden(&mut self) {
        let had_the_keys = [self.timeline.window, self.timeline.search].contains(&GetFocus());
        show(self.timeline.window, false);
        show(self.timeline.search, false);
        if had_the_keys {
            self.focus_composer();
        }
    }

    /// The view from `top` to the foot of the window, with the panel's margins; read again if anything happened while it was out of sight.
    pub(super) unsafe fn layout_timeline_view(&mut self, shapes: &mut Vec<Shape>, x: i32, width: i32, top: i32, bottom: i32) {
        let s = |value: i32| scale(self.dpi, value);
        let margin = s(ds::size::RIGHT_GAP);
        // The field to look for something in the path, over it, in a field drawn as the panel's others are.
        let field = rect(x, top + s(ds::space::XS), width, s(ds::size::FIELD_TEXT) + 2 * s(ds::size::FIELD_INSET));
        shapes.push(Shape::Round { area: field, radius: s(ds::radius::SMALL), fill: Some(theme::surface()), border: Some(theme::line()) });
        place(self.timeline.search, field.left + s(ds::space::S), field.top + s(ds::size::FIELD_INSET), (field.right - field.left) - 2 * s(ds::space::S), s(ds::size::FIELD_TEXT));
        show(self.timeline.search, true);
        let top = field.bottom + s(ds::space::XS);
        // (as wide as the files' list: a little way from the panel's edge)
        place(self.timeline.window, x - margin + s(ds::space::S), top, width + 2 * margin - 2 * s(ds::space::S), (bottom - top).max(s(60)));
        let was_hidden = IsWindowVisible(self.timeline.window) == 0;
        show(self.timeline.window, true);
        if was_hidden || self.timeline.stale {
            self.timeline_request();
        }
        // (a person asked or answered, or a turn began or ended: what the window knows has changed)
        if self.timeline.made_from.as_ref().is_some_and(|made| made.2 != self.timeline_inputs()) {
            InvalidateRect(self.timeline.window, std::ptr::null(), 0);
        }
    }

    /// What is looked for has changed: the path shows only what has it, at once (it is read from what is already there).
    pub(super) unsafe fn timeline_query_changed(&mut self) {
        self.timeline.query = get_text(self.timeline.search).trim().to_lowercase();
        self.timeline.scroll = 0;
        self.timeline.follow = self.timeline.query.is_empty();
        InvalidateRect(self.timeline.window, std::ptr::null(), 0);
    }

    /// The session on screen is another: nothing of the last is its.
    pub(super) unsafe fn timeline_session_changed(&mut self) {
        let at = Instant::now();
        self.timeline.live.reset(at);
        self.timeline.timeline = None;
        self.timeline.error = None;
        self.timeline.opened.clear();
        self.timeline.spans.clear();
        self.timeline.band_drag = None;
        self.timeline.selected = None;
        self.timeline.serial += 1;
        self.timeline.follow = true;
        self.timeline.scroll = 0;
        self.timeline.stale = true;
        self.timeline.first_request_due = false;
        // (calls the session had under way while it was out of sight, which no event said to this)
        let running: Vec<(String, String, String)> = self.tools_running.iter().map(|(id, (tool, summary))| (id.clone(), tool.clone(), summary.clone())).collect();
        for (id, tool, summary) in running {
            self.timeline.live.seed_running(&id, &tool, &summary, at);
        }
        if self.timeline_shown() {
            self.timeline_request();
        }
        InvalidateRect(self.timeline.window, std::ptr::null(), 0);
    }

    /// What an event says: kept for the rows, and a read of the files asked for when something has ended that they have.
    pub(super) unsafe fn timeline_event(&mut self, event: &Event) {
        let version = self.timeline.live.version();
        let changed = self.timeline.live.note(event, Instant::now());
        match event {
            Event::TurnStarted => self.timeline.first_request_due = true,
            Event::RequestStarted if std::mem::take(&mut self.timeline.first_request_due) => self.timeline_wanted(),
            Event::AssistantEnded | Event::ToolEnded { .. } | Event::TurnEnded { .. } => self.timeline_wanted(),
            // (the session has its file now, but this window does not know its name yet: the next layout reads it)
            Event::SessionFile(_) => self.timeline.stale = true,
            _ => {}
        }
        if !self.timeline_shown() {
            return;
        }
        if self.timeline.live.version() != version {
            InvalidateRect(self.timeline.window, std::ptr::null(), 0);
        } else if changed {
            // (the figures of a phase, and the thought: drawn at the next turn of the clock)
            self.timeline.head_dirty = true;
        }
    }

    /// The person's words, as the turn that begins next will have them.
    pub(super) fn timeline_will_ask(&mut self, words: &str) {
        self.timeline.live.will_ask(words);
    }

    /// Something has ended that the files have: they are read again now, or when the view is next in sight.
    unsafe fn timeline_wanted(&mut self) {
        if self.timeline_shown() {
            self.timeline_request();
        } else {
            self.timeline.stale = true;
        }
    }

    /// The session's files are read, on another thread.
    unsafe fn timeline_request(&mut self) {
        let Some(session) = self.current_session.clone() else { return };
        if self.timeline.reading {
            self.timeline.again = true;
            return;
        }
        self.timeline.reading = true;
        self.timeline.stale = false;
        self.timeline.serial += 1;
        let poster = self.poster.clone();
        std::thread::spawn(move || {
            let result = dogmind_agent::timeline::read(&session);
            poster.send(Note::Timeline { session, result });
        });
    }

    /// A read has come, for the session that is on screen still.
    pub(super) unsafe fn timeline_came(&mut self, session: PathBuf, result: Result<Timeline, String>) {
        self.timeline.reading = false;
        self.timeline.serial += 1;
        if self.current_session.as_ref() == Some(&session) {
            match result {
                Ok(timeline) => {
                    self.timeline.live.timeline_came(timeline.turns.len());
                    self.timeline.timeline = Some(timeline);
                    self.timeline.error = None;
                }
                // (what was read before stays, and the view says it is not up to date)
                Err(error) => self.timeline.error = Some(error),
            }
        }
        if std::mem::take(&mut self.timeline.again) || self.timeline.stale {
            if self.timeline_shown() {
                self.timeline_request();
            }
        }
        if self.timeline_shown() {
            InvalidateRect(self.timeline.window, std::ptr::null(), 0);
        }
    }

    /// With the turn at work, once a second: the seconds count on, and the figures of a phase and the thought that came since the last draw are drawn at most four times a second.
    pub(super) unsafe fn timeline_tick(&mut self) {
        if !self.timeline_shown() {
            return;
        }
        let since = self.timeline.ticked.elapsed().as_millis();
        if since >= 1000 || (self.timeline.head_dirty && since >= 250) {
            self.timeline.ticked = Instant::now();
            self.timeline.head_dirty = false;
            InvalidateRect(self.timeline.window, std::ptr::null(), 0);
        }
    }

    /// The rows, made again if what they are made from has changed.
    unsafe fn timeline_rebuild(&mut self, inputs: &path::Inputs, at: Instant) {
        let made = (self.timeline.live.version(), self.timeline.serial, inputs.clone(), self.timeline.opened_serial);
        if self.timeline.made_from.as_ref() == Some(&made) {
            return;
        }
        let note = |text: String| vec![path::Row { kind: path::Kind::Note, tone: path::Tone::Muted, text, ..path::Row::default() }];
        let rows = match (&self.timeline.timeline, &self.timeline.error) {
            (None, Some(error)) => note(tf("The session could not be read: {0}", &[error])),
            (None, None) if (self.timeline.reading || self.timeline.stale) && self.current_session.is_some() => note(t("Reading the session…").to_string()),
            _ => path::rows(self.timeline.timeline.as_ref(), &self.timeline.live, inputs, &self.timeline.opened, at),
        };
        self.timeline.rows = rows;
        self.timeline.made_from = Some(made);
        self.timeline.generation += 1;
    }

    /// Where each row stands at this width, and how tall the path is: found again only when the width, the scale or the rows are not what they were found for.
    unsafe fn timeline_lay_out(&mut self, width: i32, frame: &Frame) {
        let key = (width, self.dpi, self.timeline.generation);
        if self.timeline.laid_for == Some(key) {
            return;
        }
        let mut placed = Vec::with_capacity(self.timeline.rows.len());
        let mut top = 0;
        for (at, row) in self.timeline.rows.iter().enumerate() {
            let height = self.timeline_row(None, row, 0, 0, width, at == 0, frame);
            placed.push(Placed { top, height });
            top += height;
        }
        self.timeline.placed = placed;
        self.timeline.total = top;
        self.timeline.laid_for = Some(key);
    }

    /// The most the path can be scrolled in the window it is in.
    fn timeline_room(&self, view: i32) -> i32 {
        steps::most_scroll(self.timeline.total, view)
    }

    pub(super) unsafe fn timeline_to(&mut self, scroll: i32) {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.timeline.window, &mut client);
        let room = self.timeline_room(client.bottom - self.timeline.head_height);
        self.timeline.scroll = scroll.clamp(0, room);
        self.timeline.follow = self.timeline.scroll >= room;
        InvalidateRect(self.timeline.window, std::ptr::null(), 0);
    }

    /// The wheel (`across`: the one that turns sideways) at a point of the screen: the path scrolls; over a turn's band, with Ctrl the band's time is
    /// drawn out or in around the pointer, and with Shift, or turned sideways, it moves along its turn.
    pub(super) unsafe fn timeline_wheel(&mut self, across: bool, wparam: WPARAM, lparam: LPARAM) {
        let notches = f64::from(((wparam >> 16) & 0xFFFF) as u16 as i16) / 120.0;
        // (MK_SHIFT, MK_CONTROL)
        let (shift, control) = (wparam & 0x0004 != 0, wparam & 0x0008 != 0);
        if across || shift || control {
            let mut at = POINT { x: (lparam & 0xFFFF) as i16 as i32, y: ((lparam >> 16) & 0xFFFF) as i16 as i32 };
            ScreenToClient(self.timeline.window, &mut at);
            if let Some(turn) = self.timeline_row_at(at.y).map(|row| &self.timeline.rows[row]).filter(|row| row.kind == path::Kind::Band).map(|row| row.turn) {
                // (turned sideways, to the right is later)
                self.timeline_band_wheel(turn, at.x, if across { -notches } else { notches }, control && !across);
                return;
            }
        }
        if across {
            return;
        }
        self.timeline_to(self.timeline.scroll - (notches * 3.0) as i32 * scale(self.dpi, ds::size::TEXT_ROW));
    }

    /// Where a band row's band lies across the window: its left and its width (a row's, at its indent).
    unsafe fn timeline_band_across(&self, indent: u8) -> (i32, i32) {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.timeline.window, &mut client);
        let level = scale(self.dpi, LEVEL) * i32::from(indent);
        (scale(self.dpi, PAD) + level, (self.timeline_content_width(client.right) - level).max(1))
    }

    /// The open turn's band row, and the time under a point across it (milliseconds after the turn began, within what the band shows).
    unsafe fn timeline_band_time(&self, turn: u64, x: i32) -> Option<(&path::Row, u64)> {
        let row = self.timeline.rows.iter().find(|row| row.kind == path::Kind::Band && row.turn == turn)?;
        let (left, width) = self.timeline_band_across(row.indent);
        let (from, to) = row.span.unwrap_or((0, row.whole));
        Some((row, from + (f64::from((x - left).clamp(0, width)) / f64::from(width) * (to - from) as f64).round() as u64))
    }

    /// The band's time drawn out (`zoom`, by the turn of the wheel) around the time under the pointer, or moved along its turn by a fifth of what it shows a notch.
    unsafe fn timeline_band_wheel(&mut self, turn: u64, x: i32, notches: f64, zoom: bool) {
        let Some((row, under)) = self.timeline_band_time(turn, x) else { return };
        let whole = row.whole;
        let (from, to) = row.span.unwrap_or((0, whole));
        let shown = (to - from) as f64;
        let span = if zoom {
            let length = (shown * 0.8f64.powf(notches)).max(path::SHORTEST_SPAN as f64);
            let begin = (under as f64 - (under - from) as f64 * length / shown).max(0.0);
            (begin as u64, (begin + length) as u64)
        } else {
            // (the wheel turned up, or sideways to the left, is earlier)
            let moved = (from as f64 - notches * shown / 5.0).clamp(0.0, (whole - (to - from)) as f64) as u64;
            (moved, moved + (to - from))
        };
        self.timeline_span(turn, path::span_inside(span, whole));
    }

    /// The part of a turn's time its band shows (all of it, with None), the band staying where it stands in the window.
    unsafe fn timeline_span(&mut self, turn: u64, span: Option<(u64, u64)>) {
        let anchor = move |row: &path::Row| row.kind == path::Kind::Band && row.turn == turn;
        self.timeline_keep(&anchor, &mut |view| {
            match span {
                Some(span) => view.spans.insert(turn, span),
                None => view.spans.remove(&turn),
            };
        });
    }

    /// Where the row under a point of the window is, if the point is in the path.
    fn timeline_row_at(&self, y: i32) -> Option<usize> {
        if y < self.timeline.head_height {
            return None;
        }
        let content = y - self.timeline.head_height + self.timeline.scroll;
        let at = self.timeline.placed.partition_point(|row| row.top + row.height <= content);
        (at < self.timeline.rows.len()).then_some(at)
    }

    /// The row with a key under a point, for the pointer to be a hand over.
    pub(super) fn timeline_key_at(&self, y: i32) -> Option<Key> {
        self.timeline_row_at(y).and_then(|at| self.timeline.rows[at].key)
    }

    /// A press of the pointer: on a turn's band it begins a drag across it (`timeline_release` ends it); on a row that opens, that row opens or shuts,
    /// and the keys are on it.
    pub(super) unsafe fn timeline_click(&mut self, x: i32, y: i32) {
        if let Some(turn) = self.timeline_row_at(y).map(|at| &self.timeline.rows[at]).filter(|row| row.kind == path::Kind::Band).map(|row| row.turn) {
            SetCapture(self.timeline.window);
            self.timeline.band_drag = Some((turn, x, x));
            return;
        }
        if let Some(key) = self.timeline_key_at(y) {
            self.timeline.selected = Some(key);
            self.timeline_toggle(key, None);
        }
    }

    /// The pointer moved with its button down: a drag across a band follows it.
    pub(super) unsafe fn timeline_drag(&mut self, x: i32) {
        if let Some((_, _, now)) = self.timeline.band_drag.as_mut() {
            *now = x;
            InvalidateRect(self.timeline.window, std::ptr::null(), 0);
        }
    }

    /// The button let go: a drag across a band shows the part of its time it went over; a press that hardly moved goes to the step of that time.
    pub(super) unsafe fn timeline_release(&mut self) {
        let Some((turn, began, now)) = self.timeline.band_drag.take() else { return };
        ReleaseCapture();
        if (now - began).abs() < scale(self.dpi, DRAG_FROM) {
            self.timeline_band_click(turn, began);
            return;
        }
        let (Some((row, one)), Some((_, other))) = (self.timeline_band_time(turn, began), self.timeline_band_time(turn, now)) else { return };
        let whole = row.whole;
        self.timeline_span(turn, path::span_inside((one.min(other), one.max(other)), whole));
    }

    /// From where the wait was to what it was: the time under the pointer, as the band was drawn, is in a step, which is opened and brought to the top
    /// of the view.
    unsafe fn timeline_band_click(&mut self, turn: u64, x: i32) {
        let Some((_, ms)) = self.timeline_band_time(turn, x) else { return };
        let Some(place) = self.timeline.timeline.as_ref().and_then(|timeline| timeline.turns.iter().find(|path| path.turn == turn)).and_then(|path| path::step_at(path, ms)) else { return };
        let key = Key::Step(turn, place);
        self.timeline.selected = Some(key);
        self.timeline_toggle(key, Some(true));
        if let Some(placed) = self.timeline.rows.iter().position(|row| row.key == Some(key)).and_then(|row| self.timeline.placed.get(row).copied()) {
            self.timeline_to(placed.top - scale(self.dpi, ds::space::S));
        }
        InvalidateRect(self.timeline.window, std::ptr::null(), 0);
    }

    /// A row opened or shut (`open`: which, or the other of what it is), the row staying where it stands in the window and not the content under it.
    /// The row under a band that shows a part of its turn's time shows all of it again.
    unsafe fn timeline_toggle(&mut self, key: Key, open: Option<bool>) {
        if let Key::Whole(turn) = key {
            if open.is_none() {
                self.timeline.selected = Some(Key::Turn(turn));
                self.timeline_span(turn, None);
            }
            return;
        }
        let Some(now_open) = self.timeline.rows.iter().find(|row| row.key == Some(key)).map(|row| row.open) else { return };
        let wanted = open.unwrap_or(!now_open);
        if wanted == now_open {
            return;
        }
        self.timeline_keep(&|row: &path::Row| row.key == Some(key), &mut |view| {
            view.opened.insert(key, wanted);
            view.opened_serial += 1;
        });
    }

    /// The rows made again after a choice (`change`), the row `anchor` finds staying where it stands in the window and not the content under it.
    unsafe fn timeline_keep(&mut self, anchor: &dyn Fn(&path::Row) -> bool, change: &mut dyn FnMut(&mut TimelineView)) {
        let place = |view: &TimelineView| view.rows.iter().position(|row| anchor(row)).and_then(|at| view.placed.get(at).map(|placed| placed.top - view.scroll));
        let before = place(&self.timeline);
        change(&mut self.timeline);
        let inputs = self.timeline_inputs();
        self.timeline_rebuild(&inputs, Instant::now());
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.timeline.window, &mut client);
        let width = self.timeline_content_width(client.right);
        let frame = Frame { now: self.timeline.live.now(&inputs, Instant::now()), selected: self.timeline.selected };
        self.timeline_lay_out(width, &frame);
        if let (Some(before), Some(after)) = (before, place(&self.timeline)) {
            self.timeline.scroll += after - before;
        }
        let room = self.timeline_room(client.bottom - self.timeline.head_height);
        self.timeline.scroll = self.timeline.scroll.clamp(0, room);
        self.timeline.follow = self.timeline.follow && self.timeline.scroll >= room;
        InvalidateRect(self.timeline.window, std::ptr::null(), 0);
    }

    fn timeline_content_width(&self, client_width: i32) -> i32 {
        (client_width - 2 * scale(self.dpi, PAD)).max(scale(self.dpi, 120))
    }

    /// A key on the path: the arrows move between the turns and the steps, Enter and Space open or shut the one the keys are on, and the page keys scroll.
    pub(super) unsafe fn timeline_key(&mut self, key: u16) -> bool {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(self.timeline.window, &mut client);
        let view = (client.bottom - self.timeline.head_height).max(0);
        let selected = self.timeline.selected;
        match key {
            VK_UP | VK_DOWN => self.timeline_move(key == VK_DOWN, view),
            VK_RETURN | VK_SPACE => {
                if let Some(selected) = selected {
                    self.timeline_toggle(selected, None);
                }
            }
            VK_RIGHT | VK_LEFT => {
                if let Some(selected) = selected {
                    self.timeline_toggle(selected, Some(key == VK_RIGHT));
                }
            }
            VK_PRIOR => self.timeline_to(self.timeline.scroll - view * 9 / 10),
            VK_NEXT => self.timeline_to(self.timeline.scroll + view * 9 / 10),
            VK_HOME => self.timeline_to(0),
            VK_END => self.timeline_to(i32::MAX / 2),
            _ => return false,
        }
        true
    }

    /// The keys go to the next row that opens (the previous one, with `down` false); with none yet they begin at the end.
    unsafe fn timeline_move(&mut self, down: bool, view: i32) {
        let keyed = path::keyed(&self.timeline.rows);
        if keyed.is_empty() {
            return;
        }
        let at = self.timeline.selected.and_then(|key| keyed.iter().position(|row| self.timeline.rows[*row].key == Some(key)));
        let next = match at {
            Some(at) if down => (at + 1).min(keyed.len() - 1),
            Some(at) => at.saturating_sub(1),
            None => keyed.len() - 1,
        };
        let row = keyed[next];
        self.timeline.selected = self.timeline.rows[row].key;
        // (brought into view, as little as it takes)
        if let Some(placed) = self.timeline.placed.get(row).copied() {
            let scroll = self.timeline.scroll;
            if placed.top < scroll {
                self.timeline_to(placed.top);
            } else if placed.top + placed.height > scroll + view {
                self.timeline_to(placed.top + placed.height - view);
            } else {
                InvalidateRect(self.timeline.window, std::ptr::null(), 0);
            }
        }
    }

    /// The window: the path in view, then over it what stays at the top, and the thumb at the edge.
    pub(super) unsafe fn paint_timeline(&mut self, device: HDC, client: RECT) {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        fill(device, client, theme::sidebar());
        let at = Instant::now();
        let inputs = self.timeline_inputs();
        self.timeline_rebuild(&inputs, at);
        let width = self.timeline_content_width(client.right);
        let frame = Frame { now: self.timeline.live.now(&inputs, at), selected: self.timeline.selected.filter(|_| GetFocus() == self.timeline.window) };
        let heading = path::heading(self.timeline.timeline.as_ref(), &self.timeline.live, &inputs);
        let head = self.timeline_head(None, 0, 0, width, &frame.now, heading.as_ref());
        self.timeline.head_height = head;
        self.timeline_lay_out(width, &frame);
        let view = (client.bottom - head).max(0);
        let room = self.timeline_room(view);
        self.timeline.scroll = if self.timeline.follow { room } else { self.timeline.scroll.clamp(0, room) };
        let (rows, placed, scroll) = (&self.timeline.rows, &self.timeline.placed, self.timeline.scroll);
        for at in steps::in_view(placed, scroll, view) {
            self.timeline_row(Some(device), &rows[at], s(PAD), head + placed[at].top - scroll, width, at == 0, &frame);
        }
        // What stays at the top covers what has scrolled under it.
        fill(device, rect(0, 0, client.right, head), theme::sidebar());
        self.timeline_head(Some(device), s(PAD), 0, width, &frame.now, heading.as_ref());
        fill(device, rect(0, head - s(ds::space::PIXEL), client.right, s(ds::space::PIXEL)), theme::line());
        if self.timeline.total > view && view > 0 {
            let tall = thumb_height(view, view, self.timeline.total, s(ds::space::XXL));
            let top = head + thumb_offset(view - tall, scroll, self.timeline.total - view);
            let color = if self.timeline.thumb_grab.is_some() { theme::muted() } else { theme::line() };
            rounded(device, rect(client.right - s(ds::space::S), top, s(ds::size::THUMB), tall), s(ds::size::THUMB) / 2, Some(color), None);
        }
    }

    /// What stays at the top: what is going on now, and under it where the work is heading. Its height; drawn at (`x`, `y`) when there is a device.
    unsafe fn timeline_head(&self, device: Option<HDC>, x: i32, y: i32, width: i32, now: &path::Now, heading: Option<&path::HeadingView>) -> i32 {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let fonts = &self.fonts;
        let (line, mark) = (s(ds::size::TEXT_LINE), s(MARK + 4));
        let mut used = s(ds::space::S);
        // Now: its name, whose it is and for how long, and under them what it is.
        if let Some(device) = device {
            let caption = t("Now");
            draw_text(device, caption, rect(x, y + used, width, line), fonts.strong, theme::muted(), ONE_LINE);
            let who = now.who.word();
            if !who.is_empty() {
                let across = text_width(fonts.strong, caption) + s(ds::space::S);
                draw_text(device, who, rect(x + across, y + used, width - across, line), fonts.small, theme::muted(), ONE_LINE);
            }
            if let Some(seconds) = now.seconds.filter(|seconds| *seconds >= path::SECONDS_SHOWN_FROM) {
                draw_text(device, &crate::cards::duration_words(seconds), rect(x, y + used, width, line), fonts.small, theme::muted(), DT_RIGHT | ONE_LINE);
            }
        }
        used += line;
        let text_width_in = width - mark;
        let one = text_height(fonts.body, "A", text_width_in).max(1);
        // (two lines at the most, cut with an ellipsis: a long command is the rest of a line, not the panel's height)
        let words = within_lines(fonts.body, &now.text, text_width_in, one * NOW_LINES);
        let tall = text_height(fonts.body, &words, text_width_in).clamp(one, one * NOW_LINES);
        if let Some(device) = device {
            icon::draw(device, who_icon(now.who), rect(x, y + used, s(MARK), one), s(ds::size::ICON), if now.who == path::Who::Idle { theme::faint() } else { mark_color(who_tone(now.who)) });
            draw_text(device, &words, rect(x + mark, y + used, text_width_in, tall), fonts.body, theme::text(), DT_WORDBREAK);
        }
        used += tall + s(ds::space::S);
        let Some(heading) = heading else { return used + s(ds::space::XS) };
        // Heading to: whose it is, and what it is.
        if let Some(device) = device {
            fill(device, rect(x, y + used, width, s(ds::space::PIXEL)), theme::line());
        }
        used += s(ds::space::S);
        if let Some(device) = device {
            let caption = t("Heading to");
            draw_text(device, caption, rect(x, y + used, width, line), fonts.strong, theme::muted(), ONE_LINE);
            let across = text_width(fonts.strong, caption) + s(ds::space::S);
            draw_text(device, &heading.caption, rect(x + across, y + used, width - across, line), fonts.small, theme::muted(), ONE_LINE);
        }
        used += line;
        if heading.before > 0 {
            if let Some(device) = device {
                draw_text(device, &tf("{0} earlier", &[&heading.before]), rect(x + mark, y + used, width - mark, line), fonts.small, theme::faint(), ONE_LINE);
            }
            used += line;
        }
        for item in &heading.items {
            if heading.source == Source::Thought {
                // The model's own words, verbatim, in quotation marks.
                let quoted = format!("\u{201C}{}\u{201D}", item.text);
                let width_in = width - mark;
                let one = text_height(fonts.small, "A", width_in).max(1);
                let tall = text_height(fonts.small, &quoted, width_in).clamp(one, one * MOST_LINES);
                if let Some(device) = device {
                    icon::draw(device, "cpu", rect(x, y + used, s(MARK), one), s(ds::size::ICON_SMALL), theme::accent());
                    draw_text(device, &quoted, rect(x + mark, y + used, width_in, tall), fonts.small, theme::soft(), DT_WORDBREAK);
                }
                used += tall + s(ds::space::XS);
                continue;
            }
            if let Some(device) = device {
                self.progress_mark(device, item.progress, rect(x, y + used, s(MARK), line));
                let (color, font) = match item.progress {
                    Progress::Done => (theme::faint(), fonts.small),
                    Progress::Doing => (theme::text(), fonts.small),
                    _ => (theme::muted(), fonts.small),
                };
                draw_text(device, &item.text, rect(x + mark, y + used, width - mark, line), font, color, ONE_LINE);
            }
            used += line;
        }
        if heading.after > 0 {
            if let Some(device) = device {
                draw_text(device, &tf("and {0} more", &[&heading.after]), rect(x + mark, y + used, width - mark, line), fonts.small, theme::faint(), ONE_LINE);
            }
            used += line;
        }
        used + s(ds::space::XS)
    }

    /// The mark before an item of a plan or a list of tasks: a check for what is done, a filled dot for what is in hand, a ring for the rest.
    unsafe fn progress_mark(&self, device: HDC, progress: Progress, area: RECT) {
        let s = |value: i32| scale(self.dpi, value);
        let dot = s(9);
        let ring = |fill: Option<u32>, border: Option<u32>| {
            let (cx, cy) = ((area.left + area.right) / 2 - s(ds::space::XS), (area.top + area.bottom) / 2);
            rounded(device, rect(cx - dot / 2, cy - dot / 2, dot, dot), dot / 2, fill, border);
        };
        match progress {
            Progress::Done => icon::draw(device, "check", RECT { right: area.right - s(ds::space::XS), ..area }, s(ds::size::ICON_SMALL), theme::good()),
            Progress::Doing => ring(Some(theme::accent()), None),
            Progress::Blocked => icon::draw(device, "lock", RECT { right: area.right - s(ds::space::XS), ..area }, s(ds::size::ICON_SMALL), theme::warn()),
            Progress::Pending | Progress::None => ring(None, Some(theme::faint())),
        }
    }

    /// One row of the path with its left edge at `x` and its top at `y`, `width` wide: drawn when there is a device, and its height either way (what is measured is what is drawn).
    unsafe fn timeline_row(&self, device: Option<HDC>, row: &path::Row, x: i32, y: i32, width: i32, first: bool, frame: &Frame) -> i32 {
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let fonts = &self.fonts;
        let level = s(LEVEL) * i32::from(row.indent);
        let (left, room) = (x + level, width - level);
        let outline = |device: HDC, area: RECT| {
            if row.key.is_some() && row.key == frame.selected {
                rounded(device, area, s(ds::radius::SMALL), None, Some(theme::accent()));
            }
        };
        match row.kind {
            path::Kind::Turn if !row.open => {
                let (gap, tall) = (if first { 0 } else { s(ds::space::S) }, s(ds::size::TEXT_ROW));
                if let Some(device) = device {
                    let top = y + gap;
                    outline(device, rect(x, top, width, tall));
                    icon::draw(device, row.icon, rect(x, top, s(MARK), tall), s(ds::size::ICON_SMALL), theme::muted());
                    let right = text_width(fonts.small, &row.right);
                    draw_text(device, &row.right, rect(x + width - right - s(ds::space::XS), top, right + s(ds::space::XS), tall), fonts.small, words_color(row.right_tone), DT_RIGHT | ONE_LINE);
                    draw_text(device, &row.text, rect(x + s(MARK), top, (width - s(MARK) - right - s(ds::space::M)).max(0), tall), fonts.body, words_color(row.tone), ONE_LINE);
                }
                gap + tall
            }
            path::Kind::Turn => {
                // The turn's request: a card of the person's, or of the app's for a turn nobody sent, with what it is said to be over their words.
                let (gap, pad, line) = (if first { 0 } else { s(ds::space::S) }, s(CARD_PAD), s(ds::size::TEXT_LINE));
                let inner = width - 2 * pad;
                let words = if row.text.is_empty() { 0 } else { text_height(fonts.body, &row.text, inner) + s(ds::space::HAIR) };
                let tall = s(ds::space::S) + line + words + s(ds::space::S);
                if let Some(device) = device {
                    let top = y + gap;
                    rounded(device, rect(x, top, width, tall), s(ds::radius::CONTROL), Some(if row.tone == path::Tone::App { theme::raised() } else { theme::user_back() }), None);
                    outline(device, rect(x, top, width, tall));
                    let label_top = top + s(ds::space::S);
                    icon::draw(device, row.icon, rect(x + s(ds::space::XS), label_top, s(MARK), line), s(ds::size::ICON_SMALL), theme::muted());
                    let right = text_width(fonts.small, &row.right);
                    draw_text(device, &row.right, rect(x + width - pad - right, label_top, right, line), fonts.small, words_color(row.right_tone), DT_RIGHT | ONE_LINE);
                    draw_text(device, &row.label, rect(x + pad + s(ds::space::S), label_top, (inner - right - s(ds::space::M)).max(0), line), fonts.small, theme::muted(), ONE_LINE);
                    if words > 0 {
                        draw_text(device, &row.text, rect(x + pad, label_top + line, inner, words), fonts.body, theme::text(), DT_WORDBREAK);
                    }
                }
                gap + tall
            }
            path::Kind::Step | path::Kind::Live => {
                let live = row.kind == path::Kind::Live;
                let one = s(ds::size::TEXT_ROW);
                // The words of the model are wrapped; what is made from its call is one line, as it is the call's own; the step in progress is one line that counts on.
                let font = if matches!(row.tone, path::Tone::App | path::Tone::Bad) { fonts.small } else { fonts.body };
                let right = if live { frame.now.seconds.filter(|seconds| *seconds >= path::SECONDS_SHOWN_FROM).map(crate::cards::duration_words).unwrap_or_default() } else { row.right.clone() };
                let right_width = if right.is_empty() { 0 } else { text_width(fonts.small, &right) + s(ds::space::M) };
                let text_left = left + 2 * s(MARK);
                let mut text_room = (room - 2 * s(MARK) - right_width).max(s(40));
                // (where it is said to come from, if there is room for that and for enough of the words)
                let tag_width = if row.tag.is_empty() { 0 } else { text_width(fonts.small, &row.tag) + s(ds::space::M) };
                let words = if live { format!("{}  {}", row.label, frame.now.text) } else { row.text.clone() };
                let single = live || row.tone == path::Tone::Derived;
                // (beside short words it stands right after them; beside long ones it takes its room from them, as long as enough of them is left)
                let words_width = if single { text_width(font, &words) } else { i32::MAX };
                let tag_shown = tag_width > 0 && (words_width.saturating_add(tag_width) <= text_room || text_room - tag_width >= s(120));
                if tag_shown {
                    text_room -= tag_width;
                }
                let tag_left = text_left + words_width.min(text_room) + s(ds::space::XS);
                let tall = if single { one } else { (text_height(font, &words, text_room) + s(ds::space::S)).max(one) };
                if let Some(device) = device {
                    if row.current {
                        rounded(device, rect(x, y, width, tall), s(ds::radius::SMALL), Some(theme::raised()), None);
                        fill(device, rect(x, y + s(ds::space::XS), s(3), tall - 2 * s(ds::space::XS)), theme::accent());
                    }
                    outline(device, rect(x, y, width, tall));
                    if row.key.is_some() {
                        icon::draw(device, if row.open { "chevron-down" } else { "chevron-right" }, rect(left, y, s(MARK), one), s(ds::size::ICON_SMALL), theme::muted());
                    }
                    let icon_name = if live { who_icon(frame.now.who) } else { row.icon };
                    let icon_tone = if live { who_tone(frame.now.who) } else { row.tone };
                    icon::draw(device, icon_name, rect(left + s(MARK), y, s(MARK), one), s(ds::size::ICON_SMALL), mark_color(icon_tone));
                    if single {
                        draw_text(device, &words, rect(text_left, y, text_room, tall), font, words_color(if live { path::Tone::Plain } else { row.tone }), ONE_LINE);
                    } else {
                        draw_text(device, &words, rect(text_left, y + s(ds::space::XS), text_room, tall - s(ds::space::XS)), font, words_color(row.tone), DT_WORDBREAK);
                    }
                    if tag_shown {
                        draw_text(device, &row.tag, rect(tag_left, y, tag_width, one), fonts.small, theme::faint(), ONE_LINE);
                    }
                    if right_width > 0 {
                        draw_text(device, &right, rect(x + width - right_width, y, right_width - s(ds::space::XS), one), fonts.small, words_color(if live { path::Tone::Muted } else { row.right_tone }), DT_RIGHT | ONE_LINE);
                    }
                }
                tall
            }
            path::Kind::Call => {
                let tall = s(ds::size::TEXT_LINE);
                if let Some(device) = device {
                    icon::draw(device, row.icon, rect(left + s(ds::space::HAIR), y, s(MARK), tall), s(ds::size::ICON_SMALL), mark_color(row.tone));
                    let right = text_width(fonts.small, &row.right);
                    if right > 0 {
                        draw_text(device, &row.right, rect(x + width - right - s(ds::space::XS), y, right + s(ds::space::XS), tall), fonts.small, words_color(row.right_tone), DT_RIGHT | ONE_LINE);
                    }
                    let name = text_width(fonts.small, &row.label);
                    let text_left = left + s(MARK + 4);
                    let room_for = (x + width - text_left - right - s(ds::space::M)).max(0);
                    draw_text(device, &row.label, rect(text_left, y, name.min(room_for), tall), fonts.small, theme::muted(), ONE_LINE);
                    if !row.text.is_empty() && room_for > name + s(ds::space::S) {
                        draw_text(device, &row.text, rect(text_left + name + s(ds::space::S), y, room_for - name - s(ds::space::S), tall), fonts.small, theme::soft(), ONE_LINE);
                    }
                }
                tall
            }
            path::Kind::Fact => {
                let tall = s(ds::size::TEXT_LINE);
                if let Some(device) = device {
                    let text_left = left + s(MARK + 4);
                    let label = (room * 4 / 10).min(s(150));
                    draw_text(device, &row.label, rect(text_left, y, label, tall), fonts.small, theme::muted(), ONE_LINE);
                    draw_text(device, &row.text, rect(text_left + label + s(ds::space::S), y, (x + width - text_left - label - s(ds::space::S)).max(0), tall), fonts.small, theme::soft(), ONE_LINE);
                }
                tall
            }
            path::Kind::Caption => {
                let tall = s(ds::size::TEXT_LINE) + s(ds::space::XS);
                if let Some(device) = device {
                    // (a caption that opens what is folded under it has the chevron a step has)
                    if row.key.is_some() {
                        icon::draw(device, if row.open { "chevron-down" } else { "chevron-right" }, rect(left, y + s(ds::space::XS), s(MARK + 4), s(ds::size::TEXT_LINE)), s(ds::size::ICON_SMALL), theme::muted());
                    }
                    outline(device, rect(x, y, width, tall));
                    draw_text(device, &row.text, rect(left + s(MARK + 4), y + s(ds::space::XS), (room - s(MARK + 4)).max(0), s(ds::size::TEXT_LINE)), fonts.small, theme::muted(), ONE_LINE);
                }
                tall
            }
            path::Kind::Code => {
                let tall = s(ds::size::DIFF_ROW);
                if let Some(device) = device {
                    let band_left = left + s(MARK + 4);
                    // (the panel is the sidebar's colour, and a block of code in the light theme is nearly it: white sets it off, as it does in the helper's steps)
                    fill(device, rect(band_left, y, (x + width - band_left).max(0), tall), if theme::is_light() { theme::surface() } else { theme::code_back() });
                    let color = match row.tone {
                        path::Tone::Bad => theme::bad(),
                        path::Tone::Muted => theme::muted(),
                        _ => theme::code(),
                    };
                    draw_text(device, &row.text, rect(band_left + s(ds::space::S), y, (x + width - band_left - 2 * s(ds::space::S)).max(0), tall), fonts.code, color, ONE_LINE);
                }
                tall
            }
            path::Kind::Note => {
                let tall = text_height(fonts.small, &row.text, width) + s(ds::space::L);
                if let Some(device) = device {
                    draw_text(device, &row.text, rect(x, y + s(ds::space::S), width, tall - s(ds::space::S)), fonts.small, words_color(row.tone), DT_WORDBREAK);
                }
                tall
            }
            path::Kind::Band => self.timeline_band(device, row, left, y, room),
            path::Kind::Span => {
                // Which part of the turn's time the band over it shows, and at the right the press that shows all of it, as a link is drawn.
                let tall = s(ds::size::TEXT_LINE) + s(ds::space::XS);
                if let Some(device) = device {
                    outline(device, rect(x, y, width, tall));
                    icon::draw(device, row.icon, rect(left, y, s(MARK), tall), s(ds::size::ICON_SMALL), theme::muted());
                    let right = text_width(fonts.small, &row.right);
                    draw_text(device, &row.right, rect(x + width - right - s(ds::space::XS), y, right, tall), fonts.small, theme::accent(), DT_RIGHT | ONE_LINE);
                    draw_text(device, &row.text, rect(left + s(MARK), y, (room - s(MARK) - right - s(ds::space::M)).max(0), tall), fonts.small, theme::muted(), ONE_LINE);
                }
                tall
            }
        }
    }

    /// An open turn's band: the model's requests in one lane and the calls in the one under it, each part where it lay in the turn's time, a mark over
    /// where the app took a step of its own, and under them what each colour is (only those the band has).
    unsafe fn timeline_band(&self, device: Option<HDC>, row: &path::Row, x: i32, y: i32, width: i32) -> i32 {
        use dogmind_agent::timeline::Lane;
        let Some(band) = &row.band else { return 0 };
        let dpi = self.dpi;
        let s = |value: i32| scale(dpi, value);
        let fonts = &self.fonts;
        let (lane, gap, tick) = (s(BAND_LANE), s(BAND_GAP), s(BAND_TICK));
        let has_calls = band.parts.iter().chain(&row.live_part).any(|part| part.lane == Lane::Calls);
        let top = y + s(ds::space::XS) + if band.marks.is_empty() { 0 } else { tick + s(ds::space::HAIR) };
        let lanes_tall = lane + if has_calls { gap + lane } else { 0 };
        let length = band.length.max(1) as f64;
        let place = |ms: u64| x + ((ms as f64 / length) * f64::from(width)).round() as i32;
        if let Some(device) = device {
            // The track under each lane: the time nothing of it was going on.
            fill(device, rect(x, top, width, lane), theme::code_back());
            if has_calls {
                fill(device, rect(x, top + lane + gap, width, lane), theme::code_back());
            }
            for (part, live) in band.parts.iter().map(|part| (part, false)).chain(row.live_part.iter().map(|part| (part, true))) {
                let lane_top = if part.lane == Lane::Calls { top + lane + gap } else { top };
                // (a surface gap between neighbours; a part too short for a pixel still shows as one)
                let (from, to) = (place(part.from), place(part.to));
                let area = rect(from, lane_top, (to - from - s(ds::space::HAIR)).max(s(1)), lane);
                draw_part(device, part.part, area, s(BAND_STRIPE));
                if live {
                    // (the part going on now: its end is now, and it says so with a line at it)
                    fill(device, rect(area.right, lane_top - s(ds::space::HAIR), s(ds::space::HAIR), lane + 2 * s(ds::space::HAIR)), theme::text());
                }
                if part.part == dogmind_agent::timeline::Part::Rereading {
                    // (read from the start: the wait a person most wants to find, marked over its lane as well as by its colour)
                    draw_text(device, "!", rect(from, top - tick - s(ds::space::HAIR), (to - from).max(s(8)), tick + s(ds::space::HAIR)), fonts.small, part_color(part.part), ONE_LINE);
                }
            }
            for &at in &band.marks {
                let middle = place(at);
                fill(device, rect(middle - s(1), top - tick, s(2), tick), theme::muted());
            }
            // A drag across it under way: the part it has gone over, framed.
            if let Some((_, began, now)) = self.timeline.band_drag.filter(|(turn, began, now)| *turn == row.turn && (now - began).abs() >= s(DRAG_FROM)) {
                let (from, to) = (began.min(now).clamp(x, x + width), began.max(now).clamp(x, x + width));
                rounded(device, rect(from, top - s(ds::space::XS), (to - from).max(s(2)), lanes_tall + 2 * s(ds::space::XS)), s(ds::radius::SMALL), None, Some(theme::accent()));
            }
        }
        let mut used = top - y + lanes_tall + s(ds::space::S);
        // What the colours are: a chip and its words, as many on a line as fit.
        let (parts, marked) = path::band_key(band, row.live_part.as_ref());
        let chip = s(BAND_CHIP);
        let line = s(ds::size::TEXT_LINE);
        let mut across = 0;
        let mut entries: Vec<(Option<dogmind_agent::timeline::Part>, &str)> = parts.iter().map(|part| (Some(*part), path::part_words(*part))).collect();
        if marked {
            entries.push((None, t("Dogmind's own steps")));
        }
        for (part, words) in entries {
            let wide = chip + s(ds::space::XS) + text_width(fonts.small, words) + s(ds::space::M);
            if across > 0 && across + wide > width {
                across = 0;
                used += line;
            }
            if let Some(device) = device {
                let middle = y + used + (line - chip) / 2;
                match part {
                    Some(part) => draw_part(device, part, rect(x + across, middle, chip, chip), s(BAND_STRIPE)),
                    None => fill(device, rect(x + across + chip / 2 - s(1), middle, s(2), chip), theme::muted()),
                }
                draw_text(device, words, rect(x + across + chip + s(ds::space::XS), y + used, wide - chip, line), fonts.small, theme::muted(), ONE_LINE);
            }
            across += wide;
        }
        used + line + s(ds::space::S)
    }
}

/// A lane of the band, its gap to the next, the room over it for a mark, a chip of its key, and the stripes of a part drawn in tone on tone.
const BAND_LANE: i32 = 10;
const BAND_GAP: i32 = 4;
const BAND_TICK: i32 = 7;
const BAND_CHIP: i32 = 10;
const BAND_STRIPE: i32 = 4;
/// How far the pointer goes across a band, held down, before it is a drag and not a press.
const DRAG_FROM: i32 = 4;

/// The colours of the band's parts: three hues that stay apart for every kind of colour vision (blue the prompt read, orange the model thinking, aqua it
/// writing), a neutral for waiting and for calls, and the status colours, each with its word in the key, for what went wrong or waits for the person.
fn part_color(part: dogmind_agent::timeline::Part) -> u32 {
    use dogmind_agent::timeline::Part;
    let light = theme::is_light();
    let pick = |dark: u32, bright: u32| if light { bright } else { dark };
    match part {
        Part::Waiting => pick(rgb(96, 93, 88), rgb(196, 192, 185)),
        Part::Reading | Part::Growing | Part::Parking => pick(rgb(0x39, 0x87, 0xe5), rgb(0x2a, 0x78, 0xd6)),
        Part::Rereading | Part::Person => rgb(0xfa, 0xb2, 0x19),
        Part::Thinking | Part::Generating => pick(rgb(0xd9, 0x59, 0x26), rgb(0xeb, 0x68, 0x34)),
        Part::Writing => pick(rgb(0x19, 0x9e, 0x70), rgb(0x1b, 0xaf, 0x7a)),
        Part::Call => pick(rgb(150, 146, 138), rgb(150, 146, 138)),
        Part::CallFailed => rgb(0xd0, 0x3b, 0x3b),
    }
}

/// A part filled with its colour; growing the K/V, setting a conversation aside and a request's time not split into thinking and writing have stripes of
/// a darker step of it (by their spacing), so that they are told apart from the plain part of the same hue without another colour.
unsafe fn draw_part(device: HDC, part: dogmind_agent::timeline::Part, area: RECT, stripe: i32) {
    use dogmind_agent::timeline::Part;
    let color = part_color(part);
    fill(device, area, color);
    let every = match part {
        Part::Growing | Part::Generating => stripe,
        Part::Parking => stripe * 2,
        _ => return,
    };
    let darker = darken(color);
    let mut at = area.left + every / 2;
    while at < area.right {
        fill(device, RECT { left: at, top: area.top, right: (at + (stripe / 2).max(1)).min(area.right), bottom: area.bottom }, darker);
        at += every;
    }
}

/// A colour two thirds as bright: the stripes' step.
fn darken(color: u32) -> u32 {
    let channel = |shift: u32| ((color >> shift) & 0xff) * 2 / 3;
    channel(0) | (channel(8) << 8) | (channel(16) << 16)
}

/// The tone of the mark of what is going on, by whose it is.
fn who_tone(who: path::Who) -> path::Tone {
    match who {
        path::Who::Model => path::Tone::Model,
        path::Who::App => path::Tone::App,
        path::Who::Person => path::Tone::Person,
        path::Who::Idle => path::Tone::Muted,
    }
}

/// The window of the view: its paint, the wheel, the keys, and a drag of its thumb.
unsafe extern "system" fn timeline_procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(window, message, wparam, lparam) };
    let (x, y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(window, &mut client);
    match message {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let target = BeginPaint(window, &mut paint);
            if client.right > 0 && client.bottom > 0 {
                let device = CreateCompatibleDC(target);
                let bitmap = CreateCompatibleBitmap(target, client.right, client.bottom);
                let previous = SelectObject(device, bitmap);
                app.paint_timeline(device, client);
                BitBlt(target, 0, 0, client.right, client.bottom, device, 0, 0, SRCCOPY);
                SelectObject(device, previous);
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            EndPaint(window, &paint);
            0
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            app.timeline_wheel(message == WM_MOUSEHWHEEL, wparam, lparam);
            0
        }
        // (the arrows and the page keys come to it for itself)
        WM_GETDLGCODE => (DLGC_WANTARROWS | DLGC_WANTCHARS) as LRESULT,
        WM_CHAR => 0,
        WM_KEYDOWN => {
            if app.timeline_key(wparam as u16) {
                0
            } else {
                DefWindowProcW(window, message, wparam, lparam)
            }
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            InvalidateRect(window, std::ptr::null(), 0);
            0
        }
        WM_SETCURSOR if (lparam & 0xFFFF) as u32 == HTCLIENT => {
            let mut at = POINT { x: 0, y: 0 };
            GetCursorPos(&mut at);
            ScreenToClient(window, &mut at);
            if at.x < client.right - scale(app.dpi, ds::space::M) && app.timeline_key_at(at.y).is_some() {
                SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
                return 1;
            }
            // (over a band: a part of its time can be chosen by a drag)
            if at.x < client.right - scale(app.dpi, ds::space::M) && app.timeline_row_at(at.y).is_some_and(|row| app.timeline.rows[row].kind == path::Kind::Band) {
                SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_CROSS));
                return 1;
            }
            DefWindowProcW(window, message, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            SetFocus(window);
            if x >= client.right - scale(app.dpi, ds::space::M) {
                let (total, scroll, head) = (app.timeline.total, app.timeline.scroll, app.timeline.head_height);
                let view = client.bottom - head;
                if total > view && view > 0 {
                    SetCapture(window);
                    let tall = thumb_height(view, view, total, scale(app.dpi, ds::space::XXL));
                    let at = head + thumb_offset(view - tall, scroll, total - view);
                    // (on the thumb it goes with the pointer; off it, the thumb comes to the pointer first)
                    let grab = if y >= at && y < at + tall { y - at } else { tall / 2 };
                    app.timeline.thumb_grab = Some(grab);
                    app.timeline_to((i64::from(y - grab - head) * i64::from(total - view) / i64::from((view - tall).max(1))) as i32);
                }
            } else {
                app.timeline_click(x, y);
            }
            0
        }
        WM_MOUSEMOVE if wparam & 1 != 0 => {
            // MK_LBUTTON
            if let Some(grab) = app.timeline.thumb_grab {
                let (total, head) = (app.timeline.total, app.timeline.head_height);
                let view = client.bottom - head;
                let tall = thumb_height(view, view, total, scale(app.dpi, ds::space::XXL));
                app.timeline_to((i64::from(y - grab - head) * i64::from((total - view).max(0)) / i64::from((view - tall).max(1))) as i32);
            } else {
                app.timeline_drag(x);
            }
            0
        }
        WM_LBUTTONUP => {
            if app.timeline.thumb_grab.take().is_some() {
                ReleaseCapture();
                InvalidateRect(window, std::ptr::null(), 0);
            }
            app.timeline_release();
            0
        }
        // (the pointer taken by another window: a drag across a band ends with nothing chosen)
        WM_CAPTURECHANGED => {
            if app.timeline.band_drag.take().is_some() {
                InvalidateRect(window, std::ptr::null(), 0);
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}
