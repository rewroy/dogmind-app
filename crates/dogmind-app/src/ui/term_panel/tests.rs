//! What the terminal's window draws, without a window: a screen fed what a shell would write, and the scene made of it.

use super::*;
use crate::term::screen::Screen;

fn font() -> TermFont {
    TermFont { family: "Consolas".into(), size: 12.0, cell_width: 7, cell_height: 15, baseline: 12.0 }
}

fn scene(screen: &Screen, focused: bool) -> TermScene {
    theme::set_light(false);
    scene_of(&screen.snapshot(), focused, false, 6, (400, 300), 96)
}

fn screen_with(text: &str) -> Screen {
    let mut screen = Screen::new(40, 6, 1000);
    screen.feed(text.as_bytes());
    screen
}

#[test]
fn plain_characters_of_one_style_are_one_run_and_a_wide_one_is_a_run_of_its_own() {
    let scene = scene(&screen_with("hello world\r\nあいう abc"), true);
    let row = |row: usize| scene.runs.iter().filter(move |run| run.row == row).map(|run| (run.col, run.cells, run.text.as_str())).collect::<Vec<_>>();
    assert_eq!(row(0), [(0, 11, "hello world")]);
    // (each wide character in its two cells, and the plain ones after them in a run)
    assert_eq!(row(1), [(0, 2, "あ"), (2, 2, "い"), (4, 2, "う"), (7, 3, "abc")]);
}

#[test]
fn a_colour_is_the_themes_and_bold_in_a_plain_colour_is_the_bright_one() {
    let scene = scene(&screen_with("\x1b[31mred\x1b[0m \x1b[1;31mBOLD\x1b[0m \x1b[38;2;10;200;30mtrue"), true);
    let at = |text: &str| scene.runs.iter().find(|run| run.text == text).unwrap_or_else(|| panic!("no run {text}"));
    let ground = theme::sidebar();
    assert_eq!(at("red").color, palette::with_contrast(palette::indexed(false, 1), ground, palette::LEAST_CONTRAST));
    assert_eq!(at("BOLD").color, palette::with_contrast(palette::indexed(false, 9), ground, palette::LEAST_CONTRAST));
    assert!(at("BOLD").bold && !at("red").bold);
    assert_eq!(at("true").color, rgb(10, 200, 30));
}

#[test]
fn text_is_held_apart_from_its_ground_in_both_themes() {
    let screen = screen_with("\x1b[30mblack\x1b[0m \x1b[90mgrey\x1b[0m \x1b[37mwhite");
    let dark = scene(&screen, true);
    for run in &dark.runs {
        assert!(palette::contrast(run.color, theme::sidebar()) >= palette::LEAST_CONTRAST, "{}: {}", run.text, palette::contrast(run.color, theme::sidebar()));
    }
    let light = scene_of(&screen.snapshot(), true, true, 6, (400, 300), 96);
    for run in &light.runs {
        assert!(palette::contrast(run.color, light.ground) >= palette::LEAST_CONTRAST, "{}", run.text);
    }
    theme::set_light(false);
}

#[test]
fn inverse_swaps_the_two_a_ground_is_a_run_and_the_selection_is_the_accent_over_the_ground() {
    let mut screen = screen_with("\x1b[7minv\x1b[0m \x1b[44mblue\x1b[0m plain");
    let drawn = scene(&screen, true);
    // (inverse: the text colour is the ground, over the old text colour)
    assert!(drawn.grounds.iter().any(|ground| ground.col == 0 && ground.cells == 3 && ground.color == theme::text()), "{:?}", drawn.grounds.iter().map(|g| (g.col, g.cells)).collect::<Vec<_>>());
    assert!(drawn.grounds.iter().any(|ground| ground.col == 4 && ground.cells == 4 && ground.color == palette::indexed(false, 4)));
    assert!(drawn.grounds.iter().all(|ground| ground.color != drawn.ground), "the terminal's own ground is not painted over itself");
    screen.select_begin(9, 0, false, SelectBy::Characters);
    screen.select_extend(13, 0, true);
    let drawn = self::scene(&screen, true);
    let selection = mix(theme::sidebar(), theme::accent(), 0.42);
    assert!(drawn.grounds.iter().any(|ground| ground.col == 9 && ground.cells == 5 && ground.color == selection), "{:?}", drawn.grounds.iter().map(|g| (g.col, g.cells, g.color)).collect::<Vec<_>>());
}

#[test]
fn underline_and_strike_are_lines_over_their_cells() {
    let drawn = scene(&screen_with("\x1b[4munder\x1b[0m \x1b[9mstruck\x1b[0m \x1b[4:3mwave"), true);
    let kinds: Vec<(usize, usize, TermLineKind)> = drawn.lines.iter().map(|line| (line.col, line.cells, line.kind)).collect();
    assert!(kinds.contains(&(0, 5, TermLineKind::Underline)), "{kinds:?}");
    assert!(kinds.contains(&(6, 6, TermLineKind::Strike)), "{kinds:?}");
    assert!(kinds.contains(&(13, 4, TermLineKind::Curly)), "{kinds:?}");
}

#[test]
fn the_cursor_is_a_block_over_its_character_with_the_keys_and_a_frame_without() {
    let screen = screen_with("abc\x1b[1D");
    let (cursor, hidden) = scene(&screen, true).cursor.unwrap();
    assert_eq!((cursor.col, cursor.row, cursor.cells, cursor.shape), (2, 0, 1, TermCursorShape::Block));
    assert_eq!(hidden.map(|run| (run.text, run.color)), Some(("c".to_string(), theme::on_accent())));
    let (cursor, _) = scene(&screen, false).cursor.unwrap();
    assert_eq!(cursor.shape, TermCursorShape::Hollow);
    // (over a wide character it covers its two cells)
    let wide = screen_with("あ\x1b[1D");
    assert_eq!(scene(&wide, true).cursor.unwrap().0.cells, 2);
    // (and a program that hid it has none)
    assert!(scene(&screen_with("x\x1b[?25l"), true).cursor.is_none());
}

#[test]
fn the_scrollbars_thumb_is_there_while_lines_are_kept_and_goes_up_as_the_view_goes_back() {
    let mut screen = Screen::new(40, 6, 1000);
    assert!(scene(&screen, true).thumb.is_none(), "nothing is kept");
    for line in 0..100 {
        screen.feed(format!("line {line}\r\n").as_bytes());
    }
    let ((_, at_foot, _, foot_bottom), _) = scene(&screen, true).thumb.unwrap();
    screen.scroll(60);
    let ((_, back, _, back_bottom), color) = scene(&screen, true).thumb.unwrap();
    assert!(back < at_foot && back_bottom < foot_bottom, "{at_foot} {back}");
    assert_eq!(color, theme::muted(), "lit while it is back");
    screen.scroll(1000);
    let ((_, top, _, _), _) = scene(&screen, true).thumb.unwrap();
    assert_eq!(top, 6, "at the first line kept, it is at the top");
}

#[test]
fn ctrl_and_the_backquote_open_a_terminal_and_nothing_else_does() {
    let (none, ctrl, shift, alt) = (Mods::default(), Mods { ctrl: true, ..Mods::default() }, Mods { shift: true, ctrl: true, alt: false }, Mods { alt: true, ctrl: true, shift: false });
    assert!(opens_a_terminal(VK_OEM_3, ctrl));
    assert!(opens_a_terminal(VK_OEM_3, shift), "where the backquote is Shift and another key");
    assert!(!opens_a_terminal(VK_OEM_3, none), "a backquote is text");
    assert!(!opens_a_terminal(VK_OEM_3, alt), "Ctrl and Alt are AltGr");
    assert!(!opens_a_terminal(0x42, ctrl) && !opens_a_terminal(VK_OEM_2, ctrl), "Ctrl+B and Ctrl+/ are the window's and the shell's");
}

#[test]
fn a_shell_that_ended_says_so_with_its_code_and_a_status_in_hex() {
    assert_eq!(exit_words(0), "Exited with code 0");
    assert_eq!(exit_words(3), "Exited with code 3");
    assert_eq!(exit_words(0xC000_013A), "Exited with code 0xC000013A");
    lang::set("ja");
    assert_eq!(exit_words(3), "終了コード 3 で終了しました");
    lang::set("en");
}

#[test]
fn a_window_has_as_many_cells_as_its_face_fits_inside_its_padding() {
    let font = font();
    assert_eq!(cells_in(400, 300, &font, 96), (55, 19));
    assert_eq!(cells_in(10, 10, &font, 96), (2, 1), "never none");
    assert_eq!(cells_in(10_000, 10_000, &font, 96), (400, 200), "and not more than a terminal is made for");
}
