//! The one set of measures the window is drawn with: spacing, type, corners and the size of a control. All in unscaled pixels, as `paint::scale` takes them.
//! The colours are in `paint::theme`, which every screen already asks. What is a matter of look is decided here, and a screen only says which of these it wants.

/// The steps of the space between things. Nothing else: two screens that differ by a pixel look like a mistake.
pub mod space {
    /// What keeps a row's shade from its neighbour's.
    pub const PIXEL: i32 = 1;
    /// Between rows that belong together.
    pub const HAIR: i32 = 2;
    pub const XS: i32 = 4;
    pub const S: i32 = 8;
    pub const M: i32 = 12;
    pub const L: i32 = 16;
    pub const XL: i32 = 24;
    pub const XXL: i32 = 32;
}

/// Corners, by how large the thing is.
pub mod radius {
    /// A row in a list, a picture, a small mark.
    pub const SMALL: i32 = 6;
    /// A button, a field, a select.
    pub const CONTROL: i32 = 8;
    /// A card: the composer, a list's frame.
    pub const CARD: i32 = 12;
    /// A card in the conversation, whose corners are drawn over the control's square ones: what the person wrote, a block of code or output.
    pub const CARD_CORNER: i32 = 10;
    pub const DIALOG: i32 = 14;
}

/// The heights of what can be used, and the toggle's shape.
pub mod size {
    pub const CONTROL: i32 = 32;
    pub const NAV_ROW: i32 = 36;
    pub const ICON: i32 = 16;
    pub const ICON_SMALL: i32 = 14;
    /// What says a chip is a menu, without taking the eye from its words.
    pub const ICON_TINY: i32 = 10;
    /// The session's name over the conversation.
    pub const TITLE_STRIP: i32 = 40;
    pub const ICON_LARGE: i32 = 18;
    /// The thumb of a scrollbar, and a thin bar of progress.
    pub const THUMB: i32 = 6;
    /// What a row's small action at its end takes, and a button in a head.
    pub const ROW_ACTION: i32 = 30;
    /// A head over what follows, and the room of a row of buttons.
    pub const HEAD: i32 = 44;
    /// What a button or chip has around its words.
    pub const BUTTON_PADDING: i32 = 44;
    /// A line of small words.
    pub const TEXT_LINE: i32 = 22;
    /// A message waiting above the composer, and the row after it.
    pub const QUEUE_STEP: i32 = 30;
    /// The panel on the right: its margin and the gap between its groups, and what a mark takes before its words.
    pub const RIGHT_GAP: i32 = 18;
    pub const RIGHT_INDENT: i32 = 22;
    /// What the chips are set in from the row of the send button.
    pub const CHIP_INSET: i32 = 3;
    /// An icon and the gap after it in the status line.
    pub const STATUS_ICON: i32 = 18;
    /// A switch's knob from its edge.
    pub const KNOB_INSET: i32 = 3;
    /// A button with no frame, from its edge to its words.
    pub const GHOST_INSET: i32 = 10;
    /// Where a chip's chevron and a select's stand from their right edge.
    pub const CHIP_CHEVRON: i32 = 26;
    pub const SELECT_CHEVRON: i32 = 30;
    /// The line under the composer.
    pub const STATUS: i32 = 28;
    /// What the composer keeps between its edge and its text.
    pub const COMPOSER_INSET: i32 = 14;
    /// The round button that sends.
    pub const SEND: i32 = 34;
    /// A row of small words, in the panel on the right or a list of what waits.
    pub const TEXT_ROW: i32 = 26;
    /// A row of a list with two lines of words.
    pub const LIST_ROW: i32 = 46;
    /// A row of one line in a list of what can be chosen.
    pub const MENU_ROW: i32 = 36;
    pub const ICON_BUTTON: i32 = 28;
    /// A link: small words, with room to be hit.
    pub const LINK: i32 = 24;
    pub const SIDEBAR: i32 = 272;
    pub const SIDEBAR_SMALL: i32 = 224;
    pub const RIGHT_PANEL: i32 = 280;
    pub const DIALOG_NAV: i32 = 224;
    pub const DIALOG_NAV_SMALL: i32 = 208;
    /// The column the conversation keeps to.
    pub const COLUMN: i32 = 860;
    pub const COLUMN_FIRST_RUN: i32 = 520;
    pub const WINDOW_LEAST: (i32, i32) = (780, 520);
    /// A field's words in their frame: how far from its top, and how high.
    pub const FIELD_INSET: i32 = 6;
    pub const FIELD_TEXT: i32 = 20;
    /// A line of a diff.
    pub const DIFF_ROW: i32 = 20;
    /// What stands over a diff.
    pub const DIFF_HEAD: i32 = 44;
    /// A question or a name.
    pub const SHORT_DIALOG: i32 = 400;
    /// A list to choose from.
    pub const LIST_DIALOG: i32 = 600;
    /// A row of the sidebar.
    pub const SIDE_ROW: i32 = 32;
    /// A session found by a search: its name and what was found.
    pub const SIDE_ROW_TALL: i32 = 46;
    pub const SIDE_GROUP: i32 = 30;
    pub const TOGGLE_WIDTH: i32 = 38;
    pub const TOGGLE_HEIGHT: i32 = 22;
    /// A select or a field does not need the whole of a wide pane.
    pub const VALUE_WIDTH: i32 = 340;
    pub const LABEL_WIDTH: i32 = 200;
    /// A pane narrower than this has its labels over their controls.
    pub const STACK_BELOW: i32 = 420;
}

/// Type, in points.
pub mod points {
    pub const SMALL: i32 = 9;
    pub const BODY: i32 = 10;
    pub const CODE: i32 = 9;
    pub const HEADING: i32 = 13;
    pub const GREETING: i32 = 17;
    pub const TITLE: i32 = 20;
}

/// What is behind a dialog, darkened by this much of 255.
pub const DIM_ALPHA: u8 = 130;

/// The space kept around a dialog in a small window.
pub const DIALOG_MARGIN: i32 = 24;

/// A dialog in a client of this size, as (width, height): the share of the window it wants, at least its minimum, and never out of the window.
/// In pixels already scaled, with `margin` scaled too.
pub fn dialog_size(client: (i32, i32), share_percent: i32, least: (i32, i32), most: (i32, i32), margin: i32) -> (i32, i32) {
    let fit = |window: i32, least: i32, most: i32| (window * share_percent / 100).clamp(least, most.max(least)).min((window - 2 * margin).max(0));
    (fit(client.0, least.0, most.0), fit(client.1, least.1, most.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dialog_takes_its_share_of_the_window_and_stays_inside_it() {
        // About four fifths of a large window, up to what reads well.
        assert_eq!(dialog_size((1200, 800), 80, (720, 460), (1040, 760), 24), (960, 640));
        assert_eq!(dialog_size((2400, 1600), 80, (720, 460), (1040, 760), 24), (1040, 760));
        // In the smallest window, its minimum as far as there is room.
        assert_eq!(dialog_size((780, 520), 80, (720, 460), (1040, 760), 24), (720, 460));
        assert_eq!(dialog_size((600, 400), 80, (720, 460), (1040, 760), 24), (552, 352));
        assert_eq!(dialog_size((10, 10), 80, (720, 460), (1040, 760), 24), (0, 0), "nothing to draw in, and no panic");
    }
}
