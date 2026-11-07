//! The colours of a terminal's cells. The sixteen of the theme (a warm set that is the app's own greens and reds, one for the dark theme and one for the light), the 256 of xterm, and how
//! far apart text is held from its ground: a program writes "white" or "bright black" for a ground of its own, and on this one that text has to be read. All as `COLORREF`s, as the rest of
//! what the window draws.

use crate::paint::rgb;

/// The sixteen of the dark theme, on the panel's own dark ground.
const DARK: [u32; 16] = [
    rgb(60, 58, 54),
    rgb(232, 120, 112),
    rgb(140, 190, 130),
    rgb(226, 190, 120),
    rgb(112, 160, 224),
    rgb(200, 140, 200),
    rgb(110, 196, 196),
    rgb(205, 201, 194),
    rgb(134, 130, 123),
    rgb(240, 138, 130),
    rgb(160, 210, 150),
    rgb(240, 210, 140),
    rgb(140, 184, 240),
    rgb(220, 164, 220),
    rgb(140, 220, 220),
    rgb(236, 233, 227),
];

/// And of the light one, on its paper. Its "white" is not light: what a program writes in white is meant to stand out from its ground, and here that is dark.
const LIGHT: [u32; 16] = [
    rgb(36, 34, 31),
    rgb(186, 58, 48),
    rgb(56, 128, 66),
    rgb(150, 102, 16),
    rgb(40, 88, 170),
    rgb(140, 50, 140),
    rgb(16, 110, 120),
    rgb(104, 100, 93),
    rgb(118, 114, 106),
    rgb(200, 60, 50),
    rgb(40, 150, 70),
    rgb(170, 120, 10),
    rgb(60, 110, 200),
    rgb(160, 70, 160),
    rgb(20, 130, 140),
    rgb(20, 18, 16),
];

/// One of the 256 of xterm: the sixteen of the theme, then the cube of 6 by 6 by 6, then the grey ramp of 24.
pub fn indexed(light: bool, index: usize) -> u32 {
    match index {
        0..=15 => if light { LIGHT[index] } else { DARK[index] },
        16..=231 => {
            let step = |part: usize| if part == 0 { 0 } else { 55 + 40 * part as u32 };
            let cube = index - 16;
            rgb(step(cube / 36), step(cube / 6 % 6), step(cube % 6))
        }
        _ => {
            let level = 8 + 10 * (index.min(255) as u32 - 232);
            rgb(level, level, level)
        }
    }
}

fn channels(color: u32) -> (f64, f64, f64) {
    (f64::from(color & 0xFF), f64::from((color >> 8) & 0xFF), f64::from((color >> 16) & 0xFF))
}

/// How light a colour is to the eye (WCAG's relative luminance).
fn luminance(color: u32) -> f64 {
    let (r, g, b) = channels(color);
    let linear = |value: f64| {
        let value = value / 255.0;
        if value <= 0.03928 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// How far apart two colours are as text and ground: 1 is none, 21 is black on white.
pub fn contrast(a: u32, b: u32) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn mix(from: u32, to: u32, part: f64) -> u32 {
    let ((fr, fg, fb), (tr, tg, tb)) = (channels(from), channels(to));
    let one = |from: f64, to: f64| (from + (to - from) * part).round().clamp(0.0, 255.0) as u32;
    rgb(one(fr, tr), one(fg, tg), one(fb, tb))
}

/// `text` on `ground`, as it stands if it is at least `least` apart from it, and otherwise moved toward white or black, whichever is the way away from the ground, only as far as it takes.
pub fn with_contrast(text: u32, ground: u32, least: f64) -> u32 {
    if contrast(text, ground) >= least {
        return text;
    }
    let (white, black) = (rgb(255, 255, 255), rgb(0, 0, 0));
    let target = if contrast(white, ground) >= contrast(black, ground) { white } else { black };
    // (in steps of a twentieth, which keeps the hue about where it was)
    for step in 1..=20 {
        let moved = mix(text, target, f64::from(step) / 20.0);
        if contrast(moved, ground) >= least {
            return moved;
        }
    }
    target
}

/// What a cell's text is held to, against its ground.
pub const LEAST_CONTRAST: f64 = 4.5;

#[cfg(test)]
mod tests {
    use super::*;

    const DARK_GROUND: u32 = rgb(25, 24, 23);
    const LIGHT_GROUND: u32 = rgb(241, 239, 234);

    #[test]
    fn the_cube_and_the_ramp_are_xterms() {
        assert_eq!(indexed(false, 16), rgb(0, 0, 0));
        assert_eq!(indexed(false, 21), rgb(0, 0, 255));
        assert_eq!(indexed(false, 196), rgb(255, 0, 0));
        assert_eq!(indexed(false, 231), rgb(255, 255, 255));
        assert_eq!(indexed(true, 232), rgb(8, 8, 8));
        assert_eq!(indexed(true, 255), rgb(238, 238, 238));
        // (the first sixteen are the theme's, and not the same in the two)
        assert_ne!(indexed(false, 7), indexed(true, 7));
        assert_eq!(indexed(true, 1), LIGHT[1]);
    }

    #[test]
    fn every_colour_of_the_theme_is_readable_on_its_ground_once_held_to_it() {
        for (light, ground) in [(false, DARK_GROUND), (true, LIGHT_GROUND)] {
            for index in 0..16 {
                let held = with_contrast(indexed(light, index), ground, LEAST_CONTRAST);
                assert!(contrast(held, ground) >= LEAST_CONTRAST, "colour {index}, light {light}: {}", contrast(held, ground));
            }
        }
    }

    #[test]
    fn text_that_reads_well_already_is_left_as_it_is_and_what_does_not_is_moved_away_from_its_ground() {
        assert_eq!(with_contrast(rgb(236, 233, 227), DARK_GROUND, 4.5), rgb(236, 233, 227));
        // (dark grey on dark: lifted toward white; light grey on light: lowered toward black)
        let lifted = with_contrast(rgb(60, 58, 54), DARK_GROUND, 4.5);
        assert!(luminance(lifted) > luminance(rgb(60, 58, 54)) && contrast(lifted, DARK_GROUND) >= 4.5);
        let lowered = with_contrast(rgb(225, 222, 215), LIGHT_GROUND, 4.5);
        assert!(luminance(lowered) < luminance(rgb(225, 222, 215)) && contrast(lowered, LIGHT_GROUND) >= 4.5);
        // (and the same colour on a ground that is its own is as readable as it can be made)
        assert!(contrast(with_contrast(rgb(10, 10, 10), rgb(10, 10, 10), 4.5), rgb(10, 10, 10)) >= 4.5);
    }

    #[test]
    fn contrast_is_one_for_the_same_and_twenty_one_for_black_on_white() {
        assert!((contrast(rgb(9, 9, 9), rgb(9, 9, 9)) - 1.0).abs() < 1e-9);
        assert!((contrast(rgb(0, 0, 0), rgb(255, 255, 255)) - 21.0).abs() < 1e-6);
    }
}
