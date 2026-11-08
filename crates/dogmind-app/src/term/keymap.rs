//! What a key sends to a shell: the sequences a terminal sends (xterm's, as Windows Terminal sends them), from the key's virtual-key code and the modifiers. Pure, so that the table is said
//! once and tested. A key that makes text is not here: the system's translation of it (a character message, or with Alt a system character message) is what is sent, so that the layout,
//! dead keys and an IME are the system's.

use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Mods {
    /// xterm's number for the modifiers held: 1 plus shift, alt as 2, ctrl as 4.
    fn parameter(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }

    fn any(self) -> bool {
        self.shift || self.ctrl || self.alt
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum Keyed {
    /// These bytes, to the shell.
    Send(Vec<u8>),
    /// A key that makes text: it is left to the system, whose translation of it comes as a character.
    Text,
    /// Nothing for the shell: a modifier by itself.
    Nothing,
}

fn send(bytes: &[u8]) -> Keyed {
    Keyed::Send(bytes.to_vec())
}

/// `CSI`, or with the modifiers held `CSI 1 ; m` before the letter or `CSI n ; m` before the tilde.
fn csi(mods: Mods, number: Option<u8>, last: char) -> Keyed {
    let mut text = String::from("\x1b[");
    match (number, mods.any()) {
        (None, false) => {}
        (None, true) => text.push_str(&format!("1;{}", mods.parameter())),
        (Some(number), false) => text.push_str(&number.to_string()),
        (Some(number), true) => text.push_str(&format!("{number};{}", mods.parameter())),
    }
    text.push(last);
    Keyed::Send(text.into_bytes())
}

/// What the key `vk` sends with these modifiers held. `application_cursor` is the shell's asking for the arrows and Home and End as `SS3` (DECCKM): what a full-screen program does.
pub fn key(vk: u16, mods: Mods, application_cursor: bool) -> Keyed {
    // The cursor keys: a letter after `CSI`, or `SS3` in the mode of a program that asked, and with a modifier held `CSI 1 ; m` either way.
    let cursor = |last: char| if application_cursor && !mods.any() { Keyed::Send(format!("\x1bO{last}").into_bytes()) } else { csi(mods, None, last) };
    match vk {
        VK_UP => cursor('A'),
        VK_DOWN => cursor('B'),
        VK_RIGHT => cursor('C'),
        VK_LEFT => cursor('D'),
        VK_HOME => cursor('H'),
        VK_END => cursor('F'),
        VK_INSERT => csi(mods, Some(2), '~'),
        VK_DELETE => csi(mods, Some(3), '~'),
        VK_PRIOR => csi(mods, Some(5), '~'),
        VK_NEXT => csi(mods, Some(6), '~'),
        VK_F1..=VK_F4 => {
            let last = ['P', 'Q', 'R', 'S'][usize::from(vk - VK_F1)];
            if mods.any() { csi(mods, None, last) } else { Keyed::Send(format!("\x1bO{last}").into_bytes()) }
        }
        VK_F5..=VK_F12 => csi(mods, Some([15, 17, 18, 19, 20, 21, 23, 24][usize::from(vk - VK_F5)]), '~'),
        VK_RETURN if mods.alt => send(b"\x1b\r"),
        // (Ctrl+Enter is a line feed, as Windows Terminal sends it)
        VK_RETURN if mods.ctrl => send(b"\n"),
        VK_RETURN => send(b"\r"),
        VK_TAB if mods.shift => send(b"\x1b[Z"),
        VK_TAB => send(b"\t"),
        VK_BACK if mods.ctrl => send(b"\x08"),
        VK_BACK if mods.alt => send(b"\x1b\x7f"),
        VK_BACK => send(b"\x7f"),
        VK_ESCAPE => send(b"\x1b"),
        VK_SHIFT | VK_CONTROL | VK_MENU | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL | VK_RCONTROL | VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN | VK_CAPITAL | VK_NUMLOCK | VK_SCROLL | VK_APPS => Keyed::Nothing,
        // Control and Alt together are AltGr to a layout that has one: what it makes is text, and what it does not make is nothing.
        _ if mods.ctrl && mods.alt => Keyed::Text,
        // Control with a letter, or with a key that is a control character in ASCII.
        0x41..=0x5A if mods.ctrl => send(&[(vk - 0x40) as u8]),
        VK_SPACE if mods.ctrl => send(&[0]),
        VK_OEM_4 if mods.ctrl => send(&[0x1b]),
        VK_OEM_5 if mods.ctrl => send(&[0x1c]),
        VK_OEM_6 if mods.ctrl => send(&[0x1d]),
        VK_OEM_MINUS if mods.ctrl => send(&[0x1f]),
        VK_OEM_2 if mods.ctrl => send(&[if mods.shift { 0x7f } else { 0x1f }]),
        _ => Keyed::Text,
    }
}

/// What a character the system made is sent as, and what an Alt before it adds.
pub fn character(unit: char, alt: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(5);
    if alt {
        bytes.push(0x1b);
    }
    let mut buffer = [0u8; 4];
    bytes.extend_from_slice(unit.encode_utf8(&mut buffer).as_bytes());
    bytes
}

/// A paste as a shell takes it: lines end with a carriage return, as Enter sends it, what would end a bracketed paste early (or begin anything else) is taken out, and in the bracketed
/// mode a shell asked for it comes between its two marks, so that it is a paste to the shell and not typing.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let body: String = text.replace("\r\n", "\r").replace('\n', "\r").chars().filter(|unit| *unit != '\x1b').collect();
    if bracketed {
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        body.into_bytes()
    }
}

/// The report of a focus change, for a program that asked to be told (`CSI ? 1004 h`).
pub fn focus(gained: bool) -> &'static [u8] {
    if gained { b"\x1b[I" } else { b"\x1b[O" }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods { shift: false, ctrl: false, alt: false };
    const SHIFT: Mods = Mods { shift: true, ctrl: false, alt: false };
    const CTRL: Mods = Mods { shift: false, ctrl: true, alt: false };
    const ALT: Mods = Mods { shift: false, ctrl: false, alt: true };

    fn sent(vk: u16, mods: Mods, application: bool) -> String {
        match key(vk, mods, application) {
            Keyed::Send(bytes) => bytes.iter().map(|byte| match byte {
                0x1b => "\\e".to_string(),
                0x0d => "\\r".to_string(),
                0x0a => "\\n".to_string(),
                0x09 => "\\t".to_string(),
                0..=0x1f | 0x7f => format!("\\x{byte:02x}"),
                _ => (*byte as char).to_string(),
            }).collect(),
            Keyed::Text => "text".into(),
            Keyed::Nothing => "nothing".into(),
        }
    }

    #[test]
    fn the_arrows_home_and_end_are_csi_and_ss3_for_a_program_that_asked() {
        for (vk, last) in [(VK_UP, 'A'), (VK_DOWN, 'B'), (VK_RIGHT, 'C'), (VK_LEFT, 'D'), (VK_HOME, 'H'), (VK_END, 'F')] {
            assert_eq!(sent(vk, NONE, false), format!("\\e[{last}"));
            assert_eq!(sent(vk, NONE, true), format!("\\eO{last}"));
            // (with a modifier held it is CSI 1 ; m either way: SS3 has no place for it)
            assert_eq!(sent(vk, CTRL, true), format!("\\e[1;5{last}"));
            assert_eq!(sent(vk, SHIFT, false), format!("\\e[1;2{last}"));
            assert_eq!(sent(vk, ALT, false), format!("\\e[1;3{last}"));
        }
        assert_eq!(sent(VK_LEFT, Mods { shift: true, ctrl: true, alt: true }, false), "\\e[1;8D");
    }

    #[test]
    fn insert_delete_and_the_pages_are_numbers_and_a_tilde() {
        assert_eq!(sent(VK_INSERT, NONE, false), "\\e[2~");
        assert_eq!(sent(VK_DELETE, NONE, false), "\\e[3~");
        assert_eq!(sent(VK_PRIOR, NONE, false), "\\e[5~");
        assert_eq!(sent(VK_NEXT, NONE, false), "\\e[6~");
        assert_eq!(sent(VK_DELETE, CTRL, false), "\\e[3;5~");
        assert_eq!(sent(VK_NEXT, SHIFT, true), "\\e[6;2~", "the mode of the cursor keys is not theirs");
    }

    #[test]
    fn the_function_keys_are_ss3_for_the_first_four_and_numbers_for_the_rest() {
        assert_eq!(sent(VK_F1, NONE, false), "\\eOP");
        assert_eq!(sent(VK_F4, NONE, false), "\\eOS");
        assert_eq!(sent(VK_F1, SHIFT, false), "\\e[1;2P");
        assert_eq!(sent(VK_F4, CTRL, false), "\\e[1;5S");
        for (vk, number) in [(VK_F5, 15), (VK_F6, 17), (VK_F7, 18), (VK_F8, 19), (VK_F9, 20), (VK_F10, 21), (VK_F11, 23), (VK_F12, 24)] {
            assert_eq!(sent(vk, NONE, false), format!("\\e[{number}~"));
            assert_eq!(sent(vk, CTRL, false), format!("\\e[{number};5~"));
        }
        assert_eq!(sent(VK_F12, SHIFT, false), "\\e[24;2~");
    }

    #[test]
    fn tab_enter_backspace_and_escape() {
        assert_eq!(sent(VK_TAB, NONE, false), "\\t", "Tab is the shell's, for its completion");
        assert_eq!(sent(VK_TAB, SHIFT, false), "\\e[Z");
        assert_eq!(sent(VK_RETURN, NONE, false), "\\r");
        assert_eq!(sent(VK_RETURN, SHIFT, false), "\\r");
        assert_eq!(sent(VK_RETURN, ALT, false), "\\e\\r");
        assert_eq!(sent(VK_RETURN, CTRL, false), "\\n");
        assert_eq!(sent(VK_BACK, NONE, false), "\\x7f");
        assert_eq!(sent(VK_BACK, CTRL, false), "\\x08");
        assert_eq!(sent(VK_BACK, ALT, false), "\\e\\x7f");
        assert_eq!(sent(VK_ESCAPE, NONE, false), "\\e");
    }

    #[test]
    fn control_with_a_letter_is_its_control_character() {
        for letter in 0x41u16..=0x5A {
            assert_eq!(key(letter, CTRL, false), Keyed::Send(vec![(letter - 0x40) as u8]), "Ctrl+{}", letter as u8 as char);
            // (with Shift too: a terminal cannot tell them apart)
            assert_eq!(key(letter, Mods { shift: true, ctrl: true, alt: false }, false), Keyed::Send(vec![(letter - 0x40) as u8]));
        }
        assert_eq!(sent(0x43, CTRL, false), "\\x03", "^C is the shell's to stop what runs");
        assert_eq!(sent(0x44, CTRL, false), "\\x04");
        assert_eq!(sent(0x4C, CTRL, false), "\\x0c");
        assert_eq!(sent(VK_SPACE, CTRL, false), "\\x00");
        assert_eq!(sent(VK_OEM_4, CTRL, false), "\\e");
        assert_eq!(sent(VK_OEM_5, CTRL, false), "\\x1c");
        assert_eq!(sent(VK_OEM_6, CTRL, false), "\\x1d");
        assert_eq!(sent(VK_OEM_MINUS, CTRL, false), "\\x1f");
        assert_eq!(sent(VK_OEM_2, CTRL, false), "\\x1f");
    }

    #[test]
    fn a_key_that_makes_text_is_left_to_the_system_and_so_is_control_with_alt_which_is_altgr() {
        for vk in [0x41u16, 0x5A, 0x30, 0x39, VK_SPACE, VK_OEM_1, VK_OEM_PLUS, VK_OEM_COMMA, VK_NUMPAD0, VK_ADD, VK_DIVIDE] {
            assert_eq!(key(vk, NONE, false), Keyed::Text, "{vk}");
            assert_eq!(key(vk, SHIFT, false), Keyed::Text, "{vk}");
            // (Alt and a letter: the system's own character message for it, which gets an escape in front)
            assert_eq!(key(vk, ALT, false), Keyed::Text, "{vk}");
        }
        assert_eq!(key(0x51, Mods { shift: false, ctrl: true, alt: true }, false), Keyed::Text, "AltGr+Q makes an @ on some layouts");
        assert_eq!(key(VK_LEFT, Mods { shift: false, ctrl: true, alt: true }, false), Keyed::Send(b"\x1b[1;7D".to_vec()), "but an arrow is an arrow");
    }

    #[test]
    fn a_modifier_by_itself_sends_nothing() {
        for vk in [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_CAPITAL, VK_NUMLOCK] {
            assert_eq!(key(vk, NONE, false), Keyed::Nothing);
        }
    }

    #[test]
    fn a_character_goes_as_utf8_and_with_alt_after_an_escape() {
        assert_eq!(character('a', false), b"a");
        assert_eq!(character('a', true), b"\x1ba");
        assert_eq!(character('あ', false), "あ".as_bytes());
        assert_eq!(character('😀', false), "😀".as_bytes());
        assert_eq!(character('あ', true), [&[0x1b][..], "あ".as_bytes()].concat());
    }

    #[test]
    fn a_paste_ends_its_lines_as_enter_does_and_is_a_paste_to_a_shell_that_asked() {
        assert_eq!(paste("one\r\ntwo\nthree", false), b"one\rtwo\rthree");
        assert_eq!(paste("a", true), b"\x1b[200~a\x1b[201~");
        assert_eq!(paste("日本語\nabc", true), [&b"\x1b[200~"[..], "日本語\rabc".as_bytes(), b"\x1b[201~"].concat());
        // (what would end the paste early, or begin a sequence of its own, is not let through)
        assert_eq!(paste("a\x1b[201~rm -rf /\x1b[200~b", true), b"\x1b[200~a[201~rm -rf /[200~b\x1b[201~");
        assert_eq!(paste("", true), b"\x1b[200~\x1b[201~");
    }

    #[test]
    fn focus_is_told_as_the_two_reports() {
        assert_eq!(focus(true), b"\x1b[I");
        assert_eq!(focus(false), b"\x1b[O");
    }
}
