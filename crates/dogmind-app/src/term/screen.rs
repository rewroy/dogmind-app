//! A terminal's screen: the grid of cells and the lines that scrolled off it, the cursor, the selection, and the parsing of what a shell writes into all of them (alacritty's `Term` and the
//! parser of `vte`). This wrapper is what the window sees: a copy of the visible cells to draw (`snapshot`), text to read, and the modes a program has set.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{viewport_to_point, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb, StdSyncHandler};

use super::palette;

/// What the terminal says that is not on the screen, kept for the screen to look at after each feed.
#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Vec<Event>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(event);
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// A colour of a cell, as the shell said it: the theme's and xterm's palette are the painter's to look up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Paint {
    /// The text colour, or the ground, of the theme.
    Default,
    Indexed(u8),
    Rgb(u32),
}

/// How a cell is underlined.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Underline {
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Debug)]
pub struct Glyph {
    pub ch: char,
    /// What combines with it (accents, joiners), where it has any.
    pub marks: Option<String>,
    pub fg: Paint,
    pub bg: Paint,
    pub bold: bool,
    pub italic: bool,
    pub dim: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strike: bool,
    pub underline: Underline,
    /// The colour of its underline, where a program gave one.
    pub underline_color: Option<Paint>,
    /// 1, or 2 for a wide character (the cell after it is 0: it is covered). A line's end that a wide character did not fit is 0 as well.
    pub width: u8,
    pub selected: bool,
}

impl Glyph {
    /// The characters it shows, with what combines with them.
    pub fn text(&self) -> String {
        let mut text = self.ch.to_string();
        text.extend(self.marks.iter().flat_map(|marks| marks.chars()));
        text
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    Block,
    Underline,
    Beam,
    Hollow,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    pub col: usize,
    pub row: usize,
    pub shape: Shape,
    pub blinking: bool,
}

/// What the window draws of the screen, copied while the reader waits and drawn after.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub cols: usize,
    pub rows: usize,
    /// `rows` by `cols`, a row after a row.
    pub cells: Vec<Glyph>,
    pub cursor: Option<Cursor>,
    /// How many lines the view is scrolled back from the foot.
    pub offset: usize,
    /// The lines kept above the screen.
    pub history: usize,
    /// What the text colour and the ground are, where a program has set them (OSC 10 and 11).
    pub default_fg: Option<u32>,
    pub default_bg: Option<u32>,
    /// The colours of the 256 that a program has set by number (OSC 4).
    pub indexed: Vec<Option<u32>>,
}

/// The modes a program has set that decide what the window sends.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Modes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub alternate_screen: bool,
    /// The wheel is the arrow keys on the alternate screen.
    pub alternate_scroll: bool,
    pub focus_reports: bool,
    pub mouse_click: bool,
    pub mouse_drag: bool,
    pub mouse_motion: bool,
    pub mouse_sgr: bool,
}

pub struct Screen {
    term: Term<Listener>,
    parser: Processor<StdSyncHandler>,
    events: Listener,
    bell: bool,
    light: bool,
    foreground: u32,
    background: u32,
    cell_px: (u16, u16),
}

fn rgb_of(color: u32) -> Rgb {
    Rgb { r: (color & 0xFF) as u8, g: ((color >> 8) & 0xFF) as u8, b: ((color >> 16) & 0xFF) as u8 }
}

fn color_of(rgb: Rgb) -> u32 {
    u32::from(rgb.r) | (u32::from(rgb.g) << 8) | (u32::from(rgb.b) << 16)
}

impl Screen {
    pub fn new(cols: u16, rows: u16, history: usize) -> Screen {
        let events = Listener::default();
        let config = Config { scrolling_history: history, ..Config::default() };
        let term = Term::new(config, &Size { cols: usize::from(cols.max(1)), rows: usize::from(rows.max(1)) }, events.clone());
        Screen { term, parser: Processor::new(), events, bell: false, light: false, foreground: palette::indexed(false, 15), background: 0, cell_px: (8, 16) }
    }

    /// The theme's text and ground, for what a program asks of them, and for the colours it sets by number.
    pub fn set_theme(&mut self, light: bool, foreground: u32, background: u32) {
        self.light = light;
        self.foreground = foreground;
        self.background = background;
    }

    /// How large a cell is in pixels, for what a program asks of the window's size.
    pub fn set_cell_pixels(&mut self, width: u16, height: u16) {
        self.cell_px = (width, height);
    }

    /// What the shell wrote. What the screen answers (a report of the cursor, a colour it was asked for) is returned, to be written back to the shell.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.advance(&mut self.term, bytes);
        self.answers()
    }

    /// When a synchronized update that a program begun is to be shown whether it has ended or not.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// The update is shown now.
    pub fn stop_sync(&mut self) -> Vec<u8> {
        self.parser.stop_sync(&mut self.term);
        self.answers()
    }

    fn answers(&mut self) -> Vec<u8> {
        let events: Vec<Event> = std::mem::take(&mut *self.events.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        let mut reply = Vec::new();
        for event in events {
            match event {
                Event::PtyWrite(text) => reply.extend_from_slice(text.as_bytes()),
                Event::ColorRequest(index, format) => {
                    let color = self.term.colors()[index].unwrap_or_else(|| rgb_of(self.default_color(index)));
                    reply.extend_from_slice(format(color).as_bytes());
                }
                Event::TextAreaSizeRequest(format) => {
                    let window = WindowSize { num_lines: self.term.screen_lines() as u16, num_cols: self.term.columns() as u16, cell_width: self.cell_px.0, cell_height: self.cell_px.1 };
                    reply.extend_from_slice(format(window).as_bytes());
                }
                // (a program's title for the window is not shown: the tab says which shell it is, or what the person called it)
                Event::Bell => self.bell = true,
                // (a program does not write to the clipboard, nor read it: that is the person's)
                _ => {}
            }
        }
        reply
    }

    /// What a colour is, where a program has not set it: 0 to 255 are the palette's, then the text, the ground, and the cursor, which is the text.
    fn default_color(&self, index: usize) -> u32 {
        match index {
            0..=255 => palette::indexed(self.light, index),
            257 => self.background,
            _ => self.foreground,
        }
    }

    /// The bell has rung since this was last asked.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.columns() as u16, self.term.screen_lines() as u16)
    }

    /// The window has this many cells for it now: the lines that were joined by the width are joined as they now fit.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size { cols: usize::from(cols.max(1)), rows: usize::from(rows.max(1)) });
    }

    pub fn modes(&self) -> Modes {
        let mode = self.term.mode();
        Modes {
            application_cursor: mode.contains(TermMode::APP_CURSOR),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            alternate_screen: mode.contains(TermMode::ALT_SCREEN),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            focus_reports: mode.contains(TermMode::FOCUS_IN_OUT),
            mouse_click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
            mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
            mouse_sgr: mode.contains(TermMode::SGR_MOUSE),
        }
    }

    // ---- Scrolling back.

    /// How many lines the view is scrolled back from the foot.
    pub fn offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// How many lines have scrolled off the top and are kept.
    #[cfg(any(test, debug_assertions))]
    pub fn history(&self) -> usize {
        self.term.history_size()
    }

    /// The view goes back this many lines (forward for less than none), up to the first line kept and the foot.
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_to_foot(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    // ---- What is shown.

    fn glyph(&self, cell: &Cell, selected: bool) -> Glyph {
        // (the text's colour, the ground and the cursor's are the theme's; dim black and so on are the plain colours, dimmed, which `dim` says)
        let paint = |color: Color| match color {
            Color::Named(named) => match named as usize {
                index @ 0..=15 => Paint::Indexed(index as u8),
                index @ 259..=266 => Paint::Indexed((index - 259) as u8),
                _ => Paint::Default,
            },
            Color::Indexed(index) => Paint::Indexed(index),
            Color::Spec(rgb) => Paint::Rgb(color_of(rgb)),
        };
        let dim_named = matches!(cell.fg, Color::Named(named) if matches!(named as usize, 259..=266 | 268));
        let flags = cell.flags;
        let underline = if flags.contains(Flags::UNDERCURL) {
            Underline::Curly
        } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
            Underline::Double
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            Underline::Dotted
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            Underline::Dashed
        } else if flags.contains(Flags::UNDERLINE) {
            Underline::Single
        } else {
            Underline::None
        };
        Glyph {
            ch: cell.c,
            marks: cell.zerowidth().filter(|marks| !marks.is_empty()).map(|marks| marks.iter().collect()),
            fg: paint(cell.fg),
            bg: paint(cell.bg),
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            dim: flags.contains(Flags::DIM) || dim_named,
            inverse: flags.contains(Flags::INVERSE),
            hidden: flags.contains(Flags::HIDDEN),
            strike: flags.contains(Flags::STRIKEOUT),
            underline,
            underline_color: cell.underline_color().map(paint),
            width: if flags.contains(Flags::WIDE_CHAR) { 2 } else if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) { 0 } else { 1 },
            selected,
        }
    }

    /// The cells in view, to be drawn: copied, so that the reader is held no longer than that takes.
    pub fn snapshot(&self) -> Snapshot {
        let (cols, rows) = (self.term.columns(), self.term.screen_lines());
        let offset = self.term.grid().display_offset();
        let selection = self.term.selection.as_ref().and_then(|selection| selection.to_range(&self.term));
        let mut cells = Vec::with_capacity(cols * rows);
        for row in 0..rows {
            let line = Line(row as i32 - offset as i32);
            for col in 0..cols {
                let cell = &self.term.grid()[line][Column(col)];
                let selected = selection.as_ref().is_some_and(|range| range.contains(Point::new(line, Column(col))));
                cells.push(self.glyph(cell, selected));
            }
        }
        let content = self.term.renderable_content();
        let style = self.term.cursor_style();
        // (the cursor is on the screen, which is not what is in view while it is scrolled back)
        let cursor = (offset == 0 && content.cursor.shape != CursorShape::Hidden).then(|| Cursor {
            col: content.cursor.point.column.0,
            row: usize::try_from(content.cursor.point.line.0).unwrap_or(0).min(rows.saturating_sub(1)),
            shape: match content.cursor.shape {
                CursorShape::Underline => Shape::Underline,
                CursorShape::Beam => Shape::Beam,
                CursorShape::HollowBlock => Shape::Hollow,
                _ => Shape::Block,
            },
            blinking: style.blinking,
        });
        let colors = self.term.colors();
        let indexed = (0..256).map(|index| colors[index].map(color_of)).collect();
        Snapshot { cols, rows, cells, cursor, offset, history: self.term.history_size(), default_fg: colors[NamedColor::Foreground].map(color_of), default_bg: colors[NamedColor::Background].map(color_of), indexed }
    }

    /// Where the cursor is on the screen, as a column and a row.
    pub fn cursor_position(&self) -> (usize, usize) {
        let point = self.term.grid().cursor.point;
        (point.column.0, usize::try_from(point.line.0).unwrap_or(0))
    }

    // ---- What can be read.

    /// One line of the grid (a negative one is in the scrollback), as text: no trailing spaces, and a wide character once.
    fn line_text(&self, line: Line) -> String {
        let cols = self.term.columns();
        let mut text = String::new();
        for col in 0..cols {
            let cell = &self.term.grid()[line][Column(col)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            text.push(cell.c);
            if let Some(marks) = cell.zerowidth() {
                text.extend(marks);
            }
        }
        text.trim_end_matches(' ').to_string()
    }

    /// The lines in view, a line to a row of the screen, as text.
    pub fn visible_lines(&self) -> Vec<String> {
        let offset = self.term.grid().display_offset() as i32;
        (0..self.term.screen_lines() as i32).map(|row| self.line_text(Line(row - offset))).collect()
    }

    /// Everything: the lines kept above the screen, and the screen, a row to a line, with the empty rows under the last that has anything left off.
    #[cfg(any(test, debug_assertions))]
    pub fn text(&self) -> String {
        let top = -(self.term.history_size() as i32);
        let mut lines: Vec<String> = (top..self.term.screen_lines() as i32).map(|line| self.line_text(Line(line))).collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Where each character that is not a space is, in the cells in view: its row and its column, the character, and how many cells it takes. For a test of what lines up.
    #[cfg(any(test, debug_assertions))]
    pub fn cells_in_view(&self) -> Vec<(usize, usize, char, u8)> {
        let snapshot = self.snapshot();
        let mut found = Vec::new();
        for row in 0..snapshot.rows {
            for col in 0..snapshot.cols {
                let glyph = &snapshot.cells[row * snapshot.cols + col];
                if glyph.width > 0 && glyph.ch != ' ' {
                    found.push((row, col, glyph.ch, glyph.width));
                }
            }
        }
        found
    }

    /// The text of the line the cursor is on, and the column it is at, for a reader of the screen.
    pub fn cursor_line(&self) -> (String, usize) {
        let point = self.term.grid().cursor.point;
        let before: usize = (0..point.column.0).filter(|col| !self.term.grid()[point.line][Column(*col)].flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)).count();
        (self.line_text(point.line), before)
    }

    // ---- The selection.

    fn point_in_view(&self, col: usize, row: usize) -> Point {
        let (cols, rows) = (self.term.columns(), self.term.screen_lines());
        let point = viewport_to_point(self.term.grid().display_offset(), Point::new(row.min(rows.saturating_sub(1)), Column(col.min(cols.saturating_sub(1)))));
        // (a wide character is selected by either of its cells)
        self.term.expand_wide(point, Direction::Left)
    }

    /// A selection begins at this cell of the view (`right`: at its right half), by characters, by words, or by lines.
    pub fn select_begin(&mut self, col: usize, row: usize, right: bool, by: SelectBy) {
        let point = self.point_in_view(col, row);
        let kind = match by {
            SelectBy::Characters => SelectionType::Simple,
            SelectBy::Words => SelectionType::Semantic,
            SelectBy::Lines => SelectionType::Lines,
            SelectBy::Block => SelectionType::Block,
        };
        self.term.selection = Some(Selection::new(kind, point, if right { Direction::Right } else { Direction::Left }));
    }

    /// The selection reaches this cell.
    pub fn select_extend(&mut self, col: usize, row: usize, right: bool) {
        let point = self.point_in_view(col, row);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, if right { Direction::Right } else { Direction::Left });
        }
    }

    pub fn select_clear(&mut self) {
        self.term.selection = None;
    }

    /// All of it: the lines kept and the screen.
    pub fn select_all(&mut self) {
        let top = Point::new(self.term.topmost_line(), Column(0));
        let foot = Point::new(self.term.bottommost_line(), Column(self.term.columns().saturating_sub(1)));
        let mut selection = Selection::new(SelectionType::Simple, top, Direction::Left);
        selection.update(foot, Direction::Right);
        self.term.selection = Some(selection);
    }

    /// What is selected, as text: lines that were wrapped are joined, and a line's trailing spaces are not copied. None where nothing is selected.
    pub fn selected_text(&self) -> Option<String> {
        let whole_lines = self.term.selection.as_ref().is_some_and(|selection| selection.ty == SelectionType::Lines);
        // (a line chosen by a triple click is copied without the line feed that ends it)
        self.term.selection_to_string().map(|text| if whole_lines { text.strip_suffix('\n').unwrap_or(&text).to_string() } else { text }).filter(|text| !text.is_empty())
    }

    pub fn has_selection(&self) -> bool {
        self.selected_text().is_some()
    }

}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectBy {
    Characters,
    Words,
    Lines,
    Block,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(cols: u16, rows: u16) -> Screen {
        Screen::new(cols, rows, 1000)
    }

    #[test]
    fn what_a_shell_writes_is_on_the_screen_a_line_to_a_row() {
        let mut screen = screen(20, 5);
        screen.feed(b"hello\r\nworld  ");
        assert_eq!(screen.visible_lines(), ["hello", "world", "", "", ""]);
        assert_eq!(screen.text(), "hello\nworld");
        assert_eq!(screen.cursor_line(), ("world".to_string(), 7));
    }

    #[test]
    fn japanese_takes_two_cells_each_and_lines_up() {
        let mut screen = screen(20, 5);
        screen.feed("あいう|\r\nabcdef|\r\n日本語1|".as_bytes());
        let cells = screen.cells_in_view();
        let at = |row: usize, ch: char| cells.iter().find(|cell| cell.0 == row && cell.2 == ch).map(|cell| (cell.1, cell.3));
        assert_eq!(at(0, 'あ'), Some((0, 2)));
        assert_eq!(at(0, 'い'), Some((2, 2)));
        assert_eq!(at(0, 'う'), Some((4, 2)));
        // (the bars of the three lines are in one column)
        assert_eq!(at(0, '|'), Some((6, 1)));
        assert_eq!(at(1, '|'), Some((6, 1)));
        assert_eq!(at(2, '|'), Some((7, 1)));
        assert_eq!(at(2, '1'), Some((6, 1)));
        // (and read back, each once)
        assert_eq!(screen.visible_lines()[0], "あいう|");
        assert_eq!(screen.visible_lines()[2], "日本語1|");
    }

    #[test]
    fn a_wide_character_that_does_not_fit_the_end_of_a_line_goes_to_the_next() {
        let mut screen = screen(5, 3);
        screen.feed("abcdあ".as_bytes());
        assert_eq!(screen.visible_lines(), ["abcd", "あ", ""]);
    }

    #[test]
    fn colours_bold_underline_and_inverse_are_in_the_cells() {
        let mut screen = screen(40, 3);
        screen.feed(b"\x1b[31mred\x1b[0m \x1b[38;5;200mx\x1b[38;2;1;2;3my\x1b[0m\x1b[1mB\x1b[4mU\x1b[0m\x1b[7mI\x1b[0m\x1b[42mg\x1b[0m\x1b[91mh");
        let snapshot = screen.snapshot();
        let row = &snapshot.cells[..40];
        assert_eq!((row[0].ch, row[0].fg), ('r', Paint::Indexed(1)));
        assert_eq!(row[3].fg, Paint::Default, "reset");
        assert_eq!((row[4].ch, row[4].fg), ('x', Paint::Indexed(200)));
        assert_eq!((row[5].ch, row[5].fg), ('y', Paint::Rgb(1 | (2 << 8) | (3 << 16))));
        assert!(row[6].bold && !row[6].underline_is_set());
        assert!(row[7].bold && row[7].underline == Underline::Single);
        assert!(row[8].inverse && !row[8].bold);
        assert_eq!(row[9].bg, Paint::Indexed(2));
        assert_eq!((row[10].ch, row[10].fg), ('h', Paint::Indexed(9)));
    }

    #[test]
    fn what_scrolled_off_the_top_is_kept_and_the_view_goes_back_to_it() {
        let mut screen = Screen::new(10, 4, 100);
        for line in 1..=30 {
            screen.feed(format!("line {line}\r\n").as_bytes());
        }
        assert_eq!(screen.history(), 27);
        assert_eq!(screen.visible_lines()[0], "line 28");
        assert!(screen.text().starts_with("line 1\nline 2\n") && screen.text().ends_with("line 30"));
        screen.scroll(5);
        assert_eq!((screen.offset(), screen.visible_lines()[0].as_str()), (5, "line 23"));
        assert!(screen.snapshot().cursor.is_none(), "the cursor is not in what is scrolled back to");
        screen.scroll_to_foot();
        assert_eq!(screen.offset(), 0);
        assert!(screen.snapshot().cursor.is_some());
        // (the scrollback is bounded)
        let mut small = Screen::new(10, 4, 10);
        for line in 1..=100 {
            small.feed(format!("l{line}\r\n").as_bytes());
        }
        assert_eq!(small.history(), 10);
    }

    #[test]
    fn a_resize_reflows_what_was_wrapped_and_joins_it_again() {
        let mut screen = screen(20, 6);
        screen.feed(b"0123456789abcdefghij");
        assert_eq!(screen.visible_lines()[0], "0123456789abcdefghij");
        screen.resize(10, 6);
        // (two lines of ten, wherever they stand: the first may have gone up into what is kept)
        assert_eq!(screen.text(), "0123456789\nabcdefghij");
        assert!(screen.visible_lines().iter().all(|line| line.chars().count() <= 10));
        assert_eq!(screen.size(), (10, 6));
        screen.resize(20, 6);
        assert_eq!(screen.text(), "0123456789abcdefghij", "and one line again");
    }

    #[test]
    fn the_modes_a_program_sets_are_seen() {
        let mut screen = screen(20, 5);
        assert_eq!(screen.modes(), Modes { alternate_scroll: true, ..Modes::default() });
        screen.feed(b"\x1b[?2004h\x1b[?1h\x1b[?1004h\x1b[?1000h\x1b[?1006h");
        let modes = screen.modes();
        assert!(modes.bracketed_paste && modes.application_cursor && modes.focus_reports && modes.mouse_click && modes.mouse_sgr);
        screen.feed(b"\x1b[?2004l\x1b[?1l");
        assert!(!screen.modes().bracketed_paste && !screen.modes().application_cursor);
        screen.feed(b"main\x1b[?1049h\x1b[Halt");
        assert!(screen.modes().alternate_screen);
        assert_eq!(screen.visible_lines()[0], "alt");
        screen.feed(b"\x1b[?1049l");
        assert!(!screen.modes().alternate_screen);
        assert_eq!(screen.visible_lines()[0], "main");
    }

    #[test]
    fn a_selection_is_text_with_its_lines_joined_where_they_wrapped() {
        let mut screen = screen(10, 4);
        screen.feed(b"one two\r\n0123456789abcde\r\nlast");
        screen.select_begin(4, 0, false, SelectBy::Characters);
        screen.select_extend(6, 0, true);
        assert_eq!(screen.selected_text().as_deref(), Some("two"));
        assert!(screen.snapshot().cells[4].selected && !screen.snapshot().cells[3].selected);
        // (a wrapped line is one line)
        screen.select_begin(0, 1, false, SelectBy::Characters);
        screen.select_extend(4, 2, true);
        assert_eq!(screen.selected_text().as_deref(), Some("0123456789abcde"));
        // (a double click takes a word, a triple click a line)
        screen.select_begin(5, 0, false, SelectBy::Words);
        assert_eq!(screen.selected_text().as_deref(), Some("two"));
        screen.select_begin(1, 3, false, SelectBy::Lines);
        assert_eq!(screen.selected_text().as_deref(), Some("last"));
        screen.select_clear();
        assert_eq!((screen.selected_text(), screen.has_selection()), (None, false));
        screen.select_all();
        assert!(screen.selected_text().unwrap().starts_with("one two"));
    }

    #[test]
    fn a_japanese_selection_takes_a_wide_character_by_either_cell() {
        let mut screen = screen(20, 3);
        screen.feed("あいうえお".as_bytes());
        screen.select_begin(3, 0, false, SelectBy::Characters);
        screen.select_extend(4, 0, true);
        assert_eq!(screen.selected_text().as_deref(), Some("いう"));
    }

    #[test]
    fn what_the_screen_is_asked_it_answers_to_the_shell() {
        let mut screen = screen(20, 5);
        // A report of where the cursor is: after "hi" and then "ab" on the second line, row 2 and column 3.
        assert_eq!(screen.feed(b"hi\r\nab\x1b[6n"), b"\x1b[2;3R");
        // The colour of the ground and of the text, by OSC 11 and 10: the theme's, until a program sets them.
        screen.set_theme(false, 0x0030_2010, 0x0003_0201);
        assert_eq!(String::from_utf8(screen.feed(b"\x1b]11;?\x1b\\")).unwrap(), "\x1b]11;rgb:0101/0202/0303\x1b\\");
        assert_eq!(String::from_utf8(screen.feed(b"\x1b]10;?\x07")).unwrap(), "\x1b]10;rgb:1010/2020/3030\x07");
        screen.feed(b"\x1b]11;rgb:ff/00/00\x1b\\");
        assert_eq!(String::from_utf8(screen.feed(b"\x1b]11;?\x1b\\")).unwrap(), "\x1b]11;rgb:ffff/0000/0000\x1b\\");
        // The window's size, in cells and in pixels.
        screen.set_cell_pixels(9, 18);
        assert_eq!(screen.feed(b"\x1b[18t"), b"\x1b[8;5;20t");
        assert_eq!(screen.feed(b"\x1b[14t"), b"\x1b[4;90;180t");
        // (and what is plain output answers nothing)
        assert!(screen.feed(b"plain").is_empty());
    }

    #[test]
    fn a_program_cannot_write_to_the_clipboard_or_read_it() {
        let mut screen = screen(20, 5);
        // OSC 52, to set it and to ask for it: nothing is answered, and the screen goes on.
        assert!(screen.feed(b"\x1b]52;c;aGVsbG8=\x1b\\\x1b]52;c;?\x1b\\ok").is_empty());
        assert_eq!(screen.visible_lines()[0], "ok");
    }

    /// A program that asks for its next screen to be shown whole (CSI ? 2026) is held until it says so; one that never does is shown when the deadline comes, which the terminal waits for.
    #[test]
    fn a_synchronized_update_is_held_until_it_ends_or_its_time_is_up() {
        let mut screen = screen(20, 5);
        screen.feed(b"before\x1b[?2026hheld");
        assert_eq!(screen.visible_lines()[0], "before", "held");
        let deadline = screen.sync_deadline().expect("a time for it to be shown");
        assert!(deadline > Instant::now() && deadline < Instant::now() + std::time::Duration::from_secs(1));
        screen.feed(b"\x1b[?2026l");
        assert_eq!(screen.visible_lines()[0], "beforeheld", "shown at its end");
        assert!(screen.sync_deadline().is_none());
        // (and one that does not end)
        screen.feed(b"\r\n\x1b[?2026hnever ended");
        assert_eq!(screen.visible_lines()[1], "");
        screen.stop_sync();
        assert_eq!(screen.visible_lines()[1], "never ended");
    }

    #[test]
    fn the_bell_is_told_once() {
        let mut screen = screen(20, 5);
        screen.feed(b"a\x07");
        assert!(screen.take_bell() && !screen.take_bell());
    }

    #[test]
    fn the_cursor_is_where_the_shell_left_it_in_the_shape_it_asked_for() {
        let mut screen = screen(20, 5);
        screen.feed(b"abc");
        let cursor = screen.snapshot().cursor.unwrap();
        assert_eq!((cursor.col, cursor.row, cursor.shape), (3, 0, Shape::Block));
        screen.feed(b"\x1b[6 q\x1b[2;1H");
        let cursor = screen.snapshot().cursor.unwrap();
        assert_eq!((cursor.col, cursor.row, cursor.shape), (0, 1, Shape::Beam));
        screen.feed(b"\x1b[?25l");
        assert!(screen.snapshot().cursor.is_none());
    }
}

#[cfg(test)]
impl Glyph {
    fn underline_is_set(&self) -> bool {
        self.underline != Underline::None
    }
}
