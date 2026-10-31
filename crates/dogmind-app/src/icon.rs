//! Lucide's icons (ISC, `assets/lucide`), drawn from their SVGs as they stand: a stroke on a 24-unit grid, through GDI+.
//! A new icon is a file there and a line in `ICONS`.

use std::collections::HashMap;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::HDC;
use windows_sys::Win32::Graphics::GdiPlus::*;

const ICONS: [(&str, &str); 18] = [
    ("arrow-up", include_str!("../../../assets/lucide/arrow-up.svg")),
    ("check", include_str!("../../../assets/lucide/check.svg")),
    ("chevron-down", include_str!("../../../assets/lucide/chevron-down.svg")),
    ("chevron-right", include_str!("../../../assets/lucide/chevron-right.svg")),
    ("chevron-up", include_str!("../../../assets/lucide/chevron-up.svg")),
    ("circle-question-mark", include_str!("../../../assets/lucide/circle-question-mark.svg")),
    ("copy", include_str!("../../../assets/lucide/copy.svg")),
    ("external-link", include_str!("../../../assets/lucide/external-link.svg")),
    ("folder", include_str!("../../../assets/lucide/folder.svg")),
    ("git-branch", include_str!("../../../assets/lucide/git-branch.svg")),
    ("info", include_str!("../../../assets/lucide/info.svg")),
    ("panel-right", include_str!("../../../assets/lucide/panel-right.svg")),
    ("paw-print", include_str!("../../../assets/lucide/paw-print.svg")),
    ("plus", include_str!("../../../assets/lucide/plus.svg")),
    ("search", include_str!("../../../assets/lucide/search.svg")),
    ("settings", include_str!("../../../assets/lucide/settings.svg")),
    ("square", include_str!("../../../assets/lucide/square.svg")),
    ("x", include_str!("../../../assets/lucide/x.svg")),
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    Start(f32, f32),
    Line(f32, f32),
    Curve(f32, f32, f32, f32, f32, f32),
    Close,
    Circle(f32, f32, f32),
}

/// The numbers of a path's data, however they are run together: `.5.5`, `1-2`, and an arc's flags, `0 01.5`.
struct Numbers<'a> {
    text: &'a [u8],
    at: usize,
}

impl Numbers<'_> {
    fn skip_separators(&mut self) {
        while self.at < self.text.len() && matches!(self.text[self.at], b' ' | b',' | b'\n' | b'\r' | b'\t') {
            self.at += 1;
        }
    }

    fn more(&mut self) -> bool {
        self.skip_separators();
        self.at < self.text.len() && !self.text[self.at].is_ascii_alphabetic()
    }

    fn number(&mut self) -> f32 {
        self.skip_separators();
        let start = self.at;
        let mut seen_dot = false;
        while self.at < self.text.len() {
            let c = self.text[self.at];
            let sign = (c == b'-' || c == b'+') && self.at == start;
            let dot = c == b'.' && !seen_dot;
            if !(c.is_ascii_digit() || sign || dot) {
                break;
            }
            seen_dot |= dot;
            self.at += 1;
        }
        std::str::from_utf8(&self.text[start..self.at]).ok().and_then(|text| text.parse().ok()).unwrap_or(0.0)
    }

    fn flag(&mut self) -> bool {
        self.skip_separators();
        let flag = self.text.get(self.at) == Some(&b'1');
        self.at += 1;
        flag
    }
}

/// An SVG arc as cubic curves.
#[allow(clippy::too_many_arguments)]
fn arc(out: &mut Vec<Segment>, x0: f32, y0: f32, rx: f32, ry: f32, rotation: f32, large: bool, sweep: bool, x1: f32, y1: f32) {
    if rx == 0.0 || ry == 0.0 || (x0 == x1 && y0 == y1) {
        out.push(Segment::Line(x1, y1));
        return;
    }
    let (sin, cos) = rotation.to_radians().sin_cos();
    let (dx, dy) = ((x0 - x1) / 2.0, (y0 - y1) / 2.0);
    let (px, py) = (cos * dx + sin * dy, -sin * dx + cos * dy);
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    let lambda = px * px / (rx * rx) + py * py / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let top = (rx * rx * ry * ry - rx * rx * py * py - ry * ry * px * px).max(0.0);
    let bottom = rx * rx * py * py + ry * ry * px * px;
    let factor = (top / bottom).sqrt() * if large == sweep { -1.0 } else { 1.0 };
    let (cxp, cyp) = (factor * rx * py / ry, -factor * ry * px / rx);
    let (cx, cy) = (cos * cxp - sin * cyp + (x0 + x1) / 2.0, sin * cxp + cos * cyp + (y0 + y1) / 2.0);
    let angle = |ux: f32, uy: f32| uy.atan2(ux);
    let theta = angle((px - cxp) / rx, (py - cyp) / ry);
    let mut delta = angle((-px - cxp) / rx, (-py - cyp) / ry) - theta;
    let tau = std::f32::consts::TAU;
    if sweep && delta < 0.0 {
        delta += tau;
    } else if !sweep && delta > 0.0 {
        delta -= tau;
    }
    let pieces = (delta.abs() / (tau / 4.0)).ceil().max(1.0) as usize;
    let step = delta / pieces as f32;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    for piece in 0..pieces {
        let a = theta + step * piece as f32;
        let b = a + step;
        let point = |t: f32, ox: f32, oy: f32| {
            let (ux, uy) = (rx * t.cos() + ox, ry * t.sin() + oy);
            (cos * ux - sin * uy + cx, sin * ux + cos * uy + cy)
        };
        let (c1x, c1y) = point(a, -k * rx * a.sin() - rx * a.cos() + rx * a.cos(), k * ry * a.cos());
        let (c2x, c2y) = point(b, k * rx * b.sin(), -k * ry * b.cos());
        let (ex, ey) = point(b, 0.0, 0.0);
        out.push(Segment::Curve(c1x, c1y, c2x, c2y, ex, ey));
    }
}

/// An SVG path's data, as lines and curves in absolute coordinates.
pub fn parse_path(data: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let bytes = data.as_bytes();
    let mut numbers = Numbers { text: bytes, at: 0 };
    let (mut x, mut y, mut start_x, mut start_y) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let (mut control_x, mut control_y) = (0.0f32, 0.0f32);
    let mut last = b' ';
    loop {
        numbers.skip_separators();
        if numbers.at >= bytes.len() {
            return out;
        }
        let command = if bytes[numbers.at].is_ascii_alphabetic() {
            numbers.at += 1;
            bytes[numbers.at - 1]
        } else {
            last
        };
        let relative = command.is_ascii_lowercase();
        let (ox, oy) = if relative { (x, y) } else { (0.0, 0.0) };
        match command.to_ascii_uppercase() {
            b'M' => {
                x = ox + numbers.number();
                y = oy + numbers.number();
                (start_x, start_y) = (x, y);
                out.push(Segment::Start(x, y));
                last = if relative { b'l' } else { b'L' };
                continue;
            }
            b'L' => {
                x = ox + numbers.number();
                y = oy + numbers.number();
                out.push(Segment::Line(x, y));
            }
            b'H' => {
                x = ox + numbers.number();
                out.push(Segment::Line(x, y));
            }
            b'V' => {
                y = oy + numbers.number();
                out.push(Segment::Line(x, y));
            }
            b'C' | b'S' => {
                let (c1x, c1y) = if command.eq_ignore_ascii_case(&b'C') {
                    (ox + numbers.number(), oy + numbers.number())
                } else if matches!(last.to_ascii_uppercase(), b'C' | b'S') {
                    (2.0 * x - control_x, 2.0 * y - control_y)
                } else {
                    (x, y)
                };
                let (c2x, c2y) = (ox + numbers.number(), oy + numbers.number());
                let (ex, ey) = (ox + numbers.number(), oy + numbers.number());
                out.push(Segment::Curve(c1x, c1y, c2x, c2y, ex, ey));
                (control_x, control_y, x, y) = (c2x, c2y, ex, ey);
            }
            b'Q' | b'T' => {
                let (qx, qy) = if command.eq_ignore_ascii_case(&b'Q') {
                    (ox + numbers.number(), oy + numbers.number())
                } else if matches!(last.to_ascii_uppercase(), b'Q' | b'T') {
                    (2.0 * x - control_x, 2.0 * y - control_y)
                } else {
                    (x, y)
                };
                let (ex, ey) = (ox + numbers.number(), oy + numbers.number());
                let third = 2.0 / 3.0;
                out.push(Segment::Curve(x + third * (qx - x), y + third * (qy - y), ex + third * (qx - ex), ey + third * (qy - ey), ex, ey));
                (control_x, control_y, x, y) = (qx, qy, ex, ey);
            }
            b'A' => {
                let (rx, ry, rotation) = (numbers.number(), numbers.number(), numbers.number());
                let (large, sweep) = (numbers.flag(), numbers.flag());
                let (ex, ey) = (ox + numbers.number(), oy + numbers.number());
                arc(&mut out, x, y, rx, ry, rotation, large, sweep, ex, ey);
                (x, y) = (ex, ey);
            }
            b'Z' => {
                out.push(Segment::Close);
                (x, y) = (start_x, start_y);
            }
            _ => return out, // nothing Lucide writes
        }
        last = command;
        // The same command may repeat with only its numbers.
        if !numbers.more() && command.to_ascii_uppercase() != b'Z' {
            continue;
        }
    }
}

fn attribute(element: &str, name: &str) -> f32 {
    attribute_text(element, name).and_then(|text| text.parse().ok()).unwrap_or(0.0)
}

fn attribute_text<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = element.find(&key)? + key.len();
    element[start..].split('"').next()
}

/// A whole icon's strokes, from its SVG: the paths, circles and rounded rectangles Lucide draws with.
pub fn parse_svg(svg: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    for piece in svg.split('<').skip(1) {
        let element = format!(" {}", piece.split('>').next().unwrap_or(""));
        let element = element.as_str();
        let kind = element.split_whitespace().next().unwrap_or("");
        match kind {
            "path" => out.extend(parse_path(attribute_text(element, "d").unwrap_or(""))),
            "circle" => out.push(Segment::Circle(attribute(element, "cx"), attribute(element, "cy"), attribute(element, "r"))),
            "line" => {
                out.push(Segment::Start(attribute(element, "x1"), attribute(element, "y1")));
                out.push(Segment::Line(attribute(element, "x2"), attribute(element, "y2")));
            }
            "rect" => {
                let (x, y, w, h) = (attribute(element, "x"), attribute(element, "y"), attribute(element, "width"), attribute(element, "height"));
                let r = attribute(element, "rx");
                let data = format!("M{} {}h{}a{r} {r} 0 0 1 {r} {r}v{}a{r} {r} 0 0 1 -{r} {r}h-{}a{r} {r} 0 0 1 -{r} -{r}v-{}a{r} {r} 0 0 1 {r} -{r}z", x + r, y, w - 2.0 * r, h - 2.0 * r, w - 2.0 * r, h - 2.0 * r);
                out.extend(parse_path(&data));
            }
            _ => {}
        }
    }
    out
}

fn table() -> &'static HashMap<&'static str, Vec<Segment>> {
    static TABLE: OnceLock<HashMap<&'static str, Vec<Segment>>> = OnceLock::new();
    TABLE.get_or_init(|| ICONS.iter().map(|(name, svg)| (*name, parse_svg(svg))).collect())
}

pub fn exists(name: &str) -> bool {
    table().contains_key(name)
}

fn argb(color: u32) -> u32 {
    0xFF00_0000 | ((color & 0xFF) << 16) | (color & 0xFF00) | ((color >> 16) & 0xFF)
}

/// The icon `name`, centred in `area`, `size` pixels across, its stroke scaled with it.
pub unsafe fn draw(device: HDC, name: &str, area: RECT, size: i32, color: u32) {
    let Some(segments) = table().get(name) else { return };
    let mut graphics = std::ptr::null_mut();
    if GdipCreateFromHDC(device, &mut graphics) != 0 {
        return;
    }
    GdipSetSmoothingMode(graphics, 4); // SmoothingModeAntiAlias
    let unit = size as f32 / 24.0;
    let left = (area.left + area.right - size) as f32 / 2.0;
    let top = (area.top + area.bottom - size) as f32 / 2.0;
    let (px, py) = (|x: f32| left + x * unit, |y: f32| top + y * unit);
    let mut path = std::ptr::null_mut();
    GdipCreatePath(0, &mut path);
    let (mut x, mut y) = (0.0f32, 0.0f32);
    for segment in segments {
        match *segment {
            Segment::Start(to_x, to_y) => {
                GdipStartPathFigure(path);
                (x, y) = (to_x, to_y);
            }
            Segment::Line(to_x, to_y) => {
                GdipAddPathLine(path, px(x), py(y), px(to_x), py(to_y));
                (x, y) = (to_x, to_y);
            }
            Segment::Curve(c1x, c1y, c2x, c2y, to_x, to_y) => {
                GdipAddPathBezier(path, px(x), py(y), px(c1x), py(c1y), px(c2x), py(c2y), px(to_x), py(to_y));
                (x, y) = (to_x, to_y);
            }
            Segment::Close => {
                GdipClosePathFigure(path);
            }
            Segment::Circle(cx, cy, r) => {
                GdipStartPathFigure(path);
                GdipAddPathEllipse(path, px(cx - r), py(cy - r), 2.0 * r * unit, 2.0 * r * unit);
                GdipClosePathFigure(path);
            }
        }
    }
    let mut pen = std::ptr::null_mut();
    GdipCreatePen1(argb(color), (2.0 * unit).max(1.2), 2, &mut pen); // UnitPixel
    GdipSetPenLineCap197819(pen, 2, 2, 0); // LineCapRound
    GdipSetPenLineJoin(pen, 2); // LineJoinRound
    GdipDrawPath(graphics, pen, path);
    GdipDeletePen(pen);
    GdipDeletePath(path);
    GdipDeleteGraphics(graphics);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_parses_to_something() {
        for (name, svg) in ICONS {
            assert!(parse_svg(svg).len() >= 1, "{name}");
        }
    }

    #[test]
    fn paths_read_relative_commands_and_run_together_numbers() {
        assert_eq!(parse_path("M4 16h2v-1z"), [Segment::Start(4.0, 16.0), Segment::Line(6.0, 16.0), Segment::Line(6.0, 15.0), Segment::Close]);
        assert_eq!(parse_path("M1 1l1-1 .5.5"), [Segment::Start(1.0, 1.0), Segment::Line(2.0, 0.0), Segment::Line(2.5, 0.5)]);
        let curve = parse_path("M0 0c1 1 2 2 3 3");
        assert_eq!(curve[1], Segment::Curve(1.0, 1.0, 2.0, 2.0, 3.0, 3.0));
    }

    #[test]
    fn an_arc_ends_where_it_says() {
        let segments = parse_path("M9 10a5 5 0 0 1 5 5");
        let Some(Segment::Curve(_, _, _, _, x, y)) = segments.last().copied() else { panic!("{segments:?}") };
        assert!((x - 14.0).abs() < 0.01 && (y - 15.0).abs() < 0.01, "{x} {y}");
        // A quarter circle around (9, 15): its first control point lies straight to the right of the start.
        let Some(Segment::Curve(c1x, c1y, ..)) = segments.get(1).copied() else { panic!() };
        assert!(c1x > 9.0 && (c1y - 10.0).abs() < 0.01, "{c1x} {c1y}");
    }
}
