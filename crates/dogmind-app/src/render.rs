//! What the window draws itself, through Direct2D and DirectWrite: text with colour emoji, and antialiased shapes. All of it on the CPU (a
//! software render target), so that drawing takes no VRAM from the model; each call is bound to the GDI device it is given, over just the
//! area it draws, so that it goes between what GDI and the controls draw as before. Where Direct2D cannot start, each call says so and the
//! caller draws as it did with GDI.
//!
//! The faces are Windows's own and the Japanese one shipped beside the app, which GDI holds privately and DirectWrite does not see: so
//! DirectWrite is given a collection of both, and a fallback that sends what is Japanese to that face, as `paint`'s runs did.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;

use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows_numerics::Vector2;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{GetObjectW, DT_BOTTOM, DT_CENTER, DT_END_ELLIPSIS, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, HDC, HFONT, LOGFONTW};

/// A face as DirectWrite names it: its family, weight and size in pixels (the target is at 96 dots an inch, so a DIP is a pixel).
#[derive(Clone, Debug, PartialEq)]
pub struct Face {
    pub family: String,
    pub weight: u32,
    pub size: f32,
}

/// The faces of the GDI fonts `paint` makes, by their handles: what is drawn in one is drawn in its face here.
static FACES: Mutex<Vec<(isize, Face)>> = Mutex::new(Vec::new());

pub fn register(font: HFONT, face: Face) {
    let mut faces = FACES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    faces.retain(|(handle, _)| *handle != font as isize);
    faces.push((font as isize, face));
}

/// A font's face: as registered, or as GDI describes it.
unsafe fn face_of(font: HFONT) -> Face {
    if let Some((_, face)) = FACES.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().find(|(handle, _)| *handle == font as isize) {
        return face.clone();
    }
    let mut described: LOGFONTW = std::mem::zeroed();
    GetObjectW(font, std::mem::size_of::<LOGFONTW>() as i32, std::ptr::addr_of_mut!(described).cast());
    let name = String::from_utf16_lossy(&described.lfFaceName[..described.lfFaceName.iter().position(|unit| *unit == 0).unwrap_or(32)]);
    Face { family: if name.is_empty() { "Segoe UI".into() } else { name }, weight: if described.lfWeight > 0 { described.lfWeight as u32 } else { 400 }, size: described.lfHeight.unsigned_abs().max(1) as f32 }
}

/// What is Japanese, in the ranges `paint::is_japanese` names.
const JAPANESE: [(u32, u32); 10] = [(0x2E80, 0x2FDF), (0x3000, 0x30FF), (0x31C0, 0x31FF), (0x3200, 0x33FF), (0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xF900, 0xFAFF), (0xFE30, 0xFE4F), (0xFF00, 0xFFEF), (0x20000, 0x3FFFF)];

struct Engine {
    d2d: ID2D1Factory1,
    write: IDWriteFactory,
    collection: Option<IDWriteFontCollection>,
    fallback: Option<IDWriteFontFallback>,
    target: Option<ID2D1DCRenderTarget>,
    formats: HashMap<(String, u32, u32), IDWriteTextFormat>,
    round: Option<ID2D1StrokeStyle>,
}

thread_local! {
    /// Made on first use, on the window's thread; None for good once it could not be.
    static ENGINE: RefCell<Option<Option<Engine>>> = const { RefCell::new(None) };
}

/// The Japanese face's files, when they are beside the app (or fetched, for a build that was not packaged), and its family's name.
fn bundled_japanese() -> Option<(Vec<std::path::PathBuf>, &'static str)> {
    let folder = crate::paint::font_folders().into_iter().find(|folder| crate::paint::BUNDLED_FILES.iter().all(|file| folder.join(file).is_file()))?;
    Some((crate::paint::BUNDLED_FILES.iter().map(|file| folder.join(file)).collect(), crate::paint::BUNDLED_FACE))
}

impl Engine {
    unsafe fn new() -> windows::core::Result<Engine> {
        let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let (collection, fallback) = Engine::faces(&write).map_or((None, None), |(collection, fallback)| (collection, Some(fallback)));
        Ok(Engine { d2d, write, collection, fallback, target: None, formats: HashMap::new(), round: None })
    }

    /// Windows's faces and the shipped Japanese one in one collection, and the fallback that sends Japanese to it. Without a newer
    /// DirectWrite or the files, Windows's own: its collection, and Yu Gothic UI for Japanese.
    unsafe fn faces(write: &IDWriteFactory) -> Option<(Option<IDWriteFontCollection>, IDWriteFontFallback)> {
        let second: IDWriteFactory2 = write.cast().ok()?;
        let mut collection: Option<IDWriteFontCollection> = None;
        let mut family = crate::paint::FALLBACK_FACE;
        if let (Some((files, name)), Ok(newer)) = (bundled_japanese(), write.cast::<IDWriteFactory6>()) {
            let made = (|| -> windows::core::Result<IDWriteFontCollection> {
                let builder: IDWriteFontSetBuilder2 = newer.CreateFontSetBuilder()?;
                for file in &files {
                    builder.AddFontFile(&HSTRING::from(file.as_os_str()))?;
                }
                builder.AddFontSet(&newer.GetSystemFontSet(false)?)?;
                let set = builder.CreateFontSet()?;
                newer.CreateFontCollectionFromFontSet(&set, DWRITE_FONT_FAMILY_MODEL_WEIGHT_STRETCH_STYLE)?.cast()
            })();
            if let Ok(made) = made {
                collection = Some(made);
                family = name;
            }
        }
        let builder = second.CreateFontFallbackBuilder().ok()?;
        let ranges: Vec<DWRITE_UNICODE_RANGE> = JAPANESE.iter().map(|(first, last)| DWRITE_UNICODE_RANGE { first: *first, last: *last }).collect();
        let name = crate::paint::wide(family);
        builder.AddMapping(&ranges, &[name.as_ptr()], collection.as_ref(), PCWSTR::null(), PCWSTR::null(), 1.0).ok()?;
        // (emoji, and every script else, as Windows has them)
        builder.AddMappings(&second.GetSystemFontFallback().ok()?).ok()?;
        Some((collection, builder.CreateFontFallback().ok()?))
    }

    unsafe fn format(&mut self, face: &Face) -> windows::core::Result<IDWriteTextFormat> {
        let key = (face.family.clone(), face.weight, (face.size * 100.0) as u32);
        if let Some(format) = self.formats.get(&key) {
            return Ok(format.clone());
        }
        let locale = if crate::lang::is_japanese() { "ja-jp" } else { "en-us" };
        let format = self.write.CreateTextFormat(&HSTRING::from(face.family.as_str()), self.collection.as_ref(), DWRITE_FONT_WEIGHT(face.weight as i32), DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, face.size, &HSTRING::from(locale))?;
        if let (Some(fallback), Ok(newer)) = (&self.fallback, format.cast::<IDWriteTextFormat1>()) {
            newer.SetFontFallback(fallback)?;
        }
        self.formats.insert(key, format.clone());
        Ok(format)
    }

    unsafe fn target(&mut self) -> windows::core::Result<ID2D1DCRenderTarget> {
        if let Some(target) = &self.target {
            return Ok(target.clone());
        }
        // On the CPU, and opaque: what is on the device is read in, and text over it can be ClearType.
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let target = self.d2d.CreateDCRenderTarget(&properties)?;
        target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
        self.target = Some(target.clone());
        Ok(target)
    }

    unsafe fn round_stroke(&mut self) -> windows::core::Result<ID2D1StrokeStyle> {
        if let Some(style) = &self.round {
            return Ok(style.clone());
        }
        let properties = D2D1_STROKE_STYLE_PROPERTIES1 {
            startCap: D2D1_CAP_STYLE_ROUND,
            endCap: D2D1_CAP_STYLE_ROUND,
            dashCap: D2D1_CAP_STYLE_ROUND,
            lineJoin: D2D1_LINE_JOIN_ROUND,
            miterLimit: 10.0,
            dashStyle: D2D1_DASH_STYLE_SOLID,
            dashOffset: 0.0,
            transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
        };
        let style: ID2D1StrokeStyle = self.d2d.CreateStrokeStyle(&properties, None)?.cast()?;
        self.round = Some(style.clone());
        Ok(style)
    }

    /// `paint` on the device over `area`, at that area's origin. A target that has gone (a device that was lost) is made again next time.
    unsafe fn draw(&mut self, device: HDC, area: RECT, paint: impl FnOnce(&ID2D1DCRenderTarget, &mut Engine) -> windows::core::Result<()>) -> windows::core::Result<()> {
        if area.right <= area.left || area.bottom <= area.top {
            return Ok(());
        }
        let target = self.target()?;
        let bounds = windows::Win32::Foundation::RECT { left: area.left, top: area.top, right: area.right, bottom: area.bottom };
        target.BindDC(windows::Win32::Graphics::Gdi::HDC(device), &bounds)?;
        target.BeginDraw();
        let painted = paint(&target, self);
        let ended = target.EndDraw(None, None);
        if ended.is_err() {
            self.target = None;
        }
        painted.and(ended)
    }
}

/// With the engine, made if it is not: None where Direct2D or DirectWrite could not start, or `act` failed (the caller then draws with GDI).
fn with_engine<R>(act: impl FnOnce(&mut Engine) -> windows::core::Result<R>) -> Option<R> {
    ENGINE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            // SAFETY: COM objects made and used on this one thread.
            *slot = Some(unsafe { Engine::new() }.ok());
        }
        slot.as_mut().and_then(Option::as_mut).and_then(|engine| act(engine).ok())
    })
}

pub fn color(color: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: (color & 0xFF) as f32 / 255.0, g: ((color >> 8) & 0xFF) as f32 / 255.0, b: ((color >> 16) & 0xFF) as f32 / 255.0, a: 1.0 }
}

/// A layout of `text` in `font`, `width` by `height`, as `flags` (GDI's `DT_` ones) ask: where it goes across and down, whether it wraps, and an
/// ellipsis where it is cut.
unsafe fn layout(engine: &mut Engine, text: &str, font: HFONT, width: f32, height: f32, flags: u32) -> windows::core::Result<IDWriteTextLayout> {
    let format = engine.format(&face_of(font))?;
    let single = flags & DT_SINGLELINE != 0;
    let shown: Vec<u16> = if single { text.replace(['\r', '\n'], " ") } else { text.replace("\r\n", "\n") }.encode_utf16().collect();
    let layout = engine.write.CreateTextLayout(&shown, &format, width.max(1.0), height.max(1.0))?;
    layout.SetTextAlignment(if flags & DT_CENTER != 0 { DWRITE_TEXT_ALIGNMENT_CENTER } else if flags & DT_RIGHT != 0 { DWRITE_TEXT_ALIGNMENT_TRAILING } else { DWRITE_TEXT_ALIGNMENT_LEADING })?;
    layout.SetParagraphAlignment(if flags & DT_VCENTER != 0 { DWRITE_PARAGRAPH_ALIGNMENT_CENTER } else if flags & DT_BOTTOM != 0 { DWRITE_PARAGRAPH_ALIGNMENT_FAR } else { DWRITE_PARAGRAPH_ALIGNMENT_NEAR })?;
    layout.SetWordWrapping(if flags & DT_WORDBREAK != 0 && !single { DWRITE_WORD_WRAPPING_WRAP } else { DWRITE_WORD_WRAPPING_NO_WRAP })?;
    if flags & DT_END_ELLIPSIS != 0 {
        let sign = engine.write.CreateEllipsisTrimmingSign(&layout)?;
        let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
        layout.SetTrimming(&trimming, &sign)?;
    }
    Ok(layout)
}

/// `text` in `area`, as GDI's `DrawTextW` would put it with these flags, in colour where the font has colour (emoji). False: not drawn.
pub unsafe fn text(device: HDC, text: &str, area: RECT, font: HFONT, color: u32, flags: u32) -> bool {
    with_engine(|engine| {
        let (width, height) = ((area.right - area.left) as f32, (area.bottom - area.top) as f32);
        let laid = layout(engine, text, font, width, height, flags)?;
        engine.draw(device, area, |target, _| {
            let brush = target.CreateSolidColorBrush(&self::color(color), None)?;
            target.DrawTextLayout(Vector2 { X: 0.0, Y: 0.0 }, &laid, &brush, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT | D2D1_DRAW_TEXT_OPTIONS_CLIP);
            Ok(())
        })
    })
    .is_some()
}

/// How wide `text` is on one line, and how tall wrapped to `width` (None: on one line). None where DirectWrite is not there.
pub unsafe fn measure(font: HFONT, text: &str, width: Option<i32>) -> Option<(i32, i32)> {
    with_engine(|engine| {
        let flags = if width.is_some() { DT_WORDBREAK } else { DT_SINGLELINE };
        let laid = layout(engine, text, font, width.map_or(1.0e6, |width| width as f32), 1.0e6, flags)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        laid.GetMetrics(&mut metrics)?;
        Ok((metrics.widthIncludingTrailingWhitespace.ceil() as i32, metrics.height.ceil() as i32))
    })
}

/// A rounded rectangle filling `area` (its right and bottom edges are its last pixels'), and a line of one pixel around it.
pub unsafe fn rounded(device: HDC, area: RECT, radius: i32, fill: Option<u32>, border: Option<u32>) -> bool {
    with_engine(|engine| {
        engine.draw(device, area, |target, _| {
            let (width, height) = ((area.right - area.left) as f32, (area.bottom - area.top) as f32);
            let radius = radius as f32;
            if let Some(fill) = fill {
                let brush = target.CreateSolidColorBrush(&color(fill), None)?;
                let shape = D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left: 0.0, top: 0.0, right: width, bottom: height }, radiusX: radius, radiusY: radius };
                target.FillRoundedRectangle(&shape, &brush);
            }
            if let Some(border) = border {
                let brush = target.CreateSolidColorBrush(&color(border), None)?;
                let shape = D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left: 0.5, top: 0.5, right: width - 0.5, bottom: height - 0.5 }, radiusX: (radius - 0.5).max(0.0), radiusY: (radius - 0.5).max(0.0) };
                target.DrawRoundedRectangle(&shape, &brush, 1.0, None);
            }
            Ok(())
        })
    })
    .is_some()
}

/// `area` darkened by `alpha` (of 255) of black, over what is there.
pub unsafe fn dim(device: HDC, area: RECT, alpha: u8) -> bool {
    with_engine(|engine| {
        engine.draw(device, area, |target, _| {
            let brush = target.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: f32::from(alpha) / 255.0 }, None)?;
            target.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: (area.right - area.left) as f32, bottom: (area.bottom - area.top) as f32 }, &brush);
            Ok(())
        })
    })
    .is_some()
}

/// A figure's pieces, in the area's own pixels: what `icon` and `paint` say their shapes in.
pub enum Piece {
    Start(f32, f32),
    Line(f32, f32),
    Curve(f32, f32, f32, f32, f32, f32),
    /// An arc of the ellipse in a box (left, top, width, height), from a start and through a sweep, in degrees, clockwise from three o'clock
    /// (GDI+'s `AddPathArc`): a line to its start from where the figure is, if that is elsewhere.
    Arc(f32, f32, f32, f32, f32, f32),
    Close,
    Circle(f32, f32, f32),
}

/// Where an arc of that box is at an angle.
fn on_arc(left: f32, top: f32, width: f32, height: f32, degrees: f32) -> (f32, f32) {
    let radians = degrees.to_radians();
    (left + width / 2.0 + width / 2.0 * radians.cos(), top + height / 2.0 + height / 2.0 * radians.sin())
}

/// The pieces as a geometry: figures open unless closed, and filled where `filled`. Circles are their own figures.
unsafe fn geometry(engine: &Engine, pieces: &[Piece], filled: bool) -> windows::core::Result<ID2D1PathGeometry> {
    let path = engine.d2d.CreatePathGeometry()?;
    let sink = path.Open()?;
    sink.SetFillMode(D2D1_FILL_MODE_WINDING);
    let begin = if filled { D2D1_FIGURE_BEGIN_FILLED } else { D2D1_FIGURE_BEGIN_HOLLOW };
    let mut open = false;
    // (where the figure is, and where it began: a close goes back there, and what follows a close without a start begins there, as SVG has it)
    let (mut at, mut began) = ((0.0f32, 0.0f32), (0.0f32, 0.0f32));
    let point = |(x, y): (f32, f32)| Vector2 { X: x, Y: y };
    for piece in pieces {
        if matches!(piece, Piece::Line(..) | Piece::Curve(..)) && !open {
            sink.BeginFigure(point(at), begin);
            open = true;
            began = at;
        }
        match *piece {
            Piece::Start(x, y) => {
                if open {
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                }
                sink.BeginFigure(point((x, y)), begin);
                open = true;
                at = (x, y);
                began = at;
            }
            Piece::Line(x, y) => {
                sink.AddLine(point((x, y)));
                at = (x, y);
            }
            Piece::Curve(c1x, c1y, c2x, c2y, x, y) => {
                sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: point((c1x, c1y)), point2: point((c2x, c2y)), point3: point((x, y)) });
                at = (x, y);
            }
            Piece::Arc(left, top, width, height, start, sweep) => {
                let from = on_arc(left, top, width, height, start);
                if !open {
                    sink.BeginFigure(point(from), begin);
                    open = true;
                    began = from;
                } else if (from.0 - at.0).abs() > 0.01 || (from.1 - at.1).abs() > 0.01 {
                    sink.AddLine(point(from));
                }
                let to = on_arc(left, top, width, height, start + sweep);
                sink.AddArc(&D2D1_ARC_SEGMENT {
                    point: point(to),
                    size: D2D_SIZE_F { width: width / 2.0, height: height / 2.0 },
                    rotationAngle: 0.0,
                    sweepDirection: if sweep >= 0.0 { D2D1_SWEEP_DIRECTION_CLOCKWISE } else { D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE },
                    arcSize: if sweep.abs() > 180.0 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
                });
                at = to;
            }
            Piece::Close => {
                if open {
                    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                    open = false;
                    at = began;
                }
            }
            Piece::Circle(..) => {}
        }
    }
    if open {
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
    }
    sink.Close()?;
    path.cast()
}

/// The pieces in `area` (whose origin they are given from): filled in `fill`, and stroked in `line` this thick with round ends and joins.
pub unsafe fn figure(device: HDC, area: RECT, pieces: &[Piece], fill: Option<u32>, line: Option<(u32, f32)>) -> bool {
    with_engine(|engine| {
        let shape = geometry(engine, pieces, fill.is_some())?;
        let round = engine.round_stroke()?;
        engine.draw(device, area, |target, _| {
            if let Some(fill) = fill {
                let brush = target.CreateSolidColorBrush(&color(fill), None)?;
                target.FillGeometry(&shape, &brush, None);
            }
            if let Some((line, thickness)) = line {
                let brush = target.CreateSolidColorBrush(&color(line), None)?;
                target.DrawGeometry(&shape, &brush, thickness, &round);
                for piece in pieces {
                    if let Piece::Circle(x, y, r) = *piece {
                        target.DrawEllipse(&D2D1_ELLIPSE { point: Vector2 { X: x, Y: y }, radiusX: r, radiusY: r }, &brush, thickness, &round);
                    }
                }
            }
            Ok(())
        })
    })
    .is_some()
}

/// A picture of `width` by `height` pixels whose every pixel is `ink` at its alpha (`alphas`, one a pixel), drawn into `shown` smoothly.
pub unsafe fn ink(device: HDC, alphas: &[u8], width: u32, height: u32, ink: u32, shown: RECT) -> bool {
    with_engine(|engine| {
        let (r, g, b) = (ink & 0xFF, (ink >> 8) & 0xFF, (ink >> 16) & 0xFF);
        // (premultiplied, in Direct2D's order: blue, green, red, alpha)
        let pixels: Vec<u8> = alphas.iter().flat_map(|&alpha| {
            let a = u32::from(alpha);
            [(b * a / 255) as u8, (g * a / 255) as u8, (r * a / 255) as u8, alpha]
        }).collect();
        engine.draw(device, shown, |target, _| {
            let properties = D2D1_BITMAP_PROPERTIES { pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED }, dpiX: 96.0, dpiY: 96.0 };
            let bitmap = target.CreateBitmap(D2D_SIZE_U { width, height }, Some(pixels.as_ptr().cast()), width * 4, &properties)?;
            let place = D2D_RECT_F { left: 0.0, top: 0.0, right: (shown.right - shown.left) as f32, bottom: (shown.bottom - shown.top) as f32 };
            match target.cast::<ID2D1DeviceContext>() {
                Ok(context) => context.DrawBitmap(&bitmap, Some(&place), 1.0, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None),
                Err(_) => target.DrawBitmap(&bitmap, Some(&place), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None),
            }
            Ok(())
        })
    })
    .is_some()
}

// ---- A terminal's cells.

/// The face a terminal is drawn in, and how large its cell is: the size is tuned so that a character is a whole number of pixels wide, and so a row of them, drawn as one run, ends
/// exactly where its cells do (and the line characters of a box meet).
#[derive(Clone, Debug, PartialEq)]
pub struct TermFont {
    pub family: String,
    /// In pixels, as tuned.
    pub size: f32,
    pub cell_width: i32,
    pub cell_height: i32,
    /// From the top of a cell to the baseline.
    pub baseline: f32,
}

/// A row of cells, drawn: `text` starts at the cell `col` of row `row` and takes `cells` cells. A wide character is a run of its own, which is centred in its two cells.
pub struct TermRun {
    pub row: usize,
    pub col: usize,
    pub cells: usize,
    pub text: String,
    pub color: u32,
    pub bold: bool,
    pub italic: bool,
}

/// A part of a row painted in a colour: a ground that is not the terminal's own, or a selection.
pub struct TermGround {
    pub row: usize,
    pub col: usize,
    pub cells: usize,
    pub color: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TermLineKind {
    Underline,
    Double,
    Curly,
    Dotted,
    Dashed,
    Strike,
}

pub struct TermLine {
    pub row: usize,
    pub col: usize,
    pub cells: usize,
    pub color: u32,
    pub kind: TermLineKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TermCursorShape {
    Block,
    Underline,
    Beam,
    Hollow,
}

pub struct TermCursor {
    pub row: usize,
    pub col: usize,
    pub cells: usize,
    pub shape: TermCursorShape,
    pub color: u32,
}

/// What is drawn of a terminal: the ground, the parts of it in other colours, the text, its lines, and the cursor (drawn last, over a character that it hides; that character is in
/// `runs` as well, and the cursor's block draws it again in the colour it is given).
pub struct TermScene {
    pub ground: u32,
    /// Where the cells begin, from the area's own corner.
    pub origin: (i32, i32),
    pub grounds: Vec<TermGround>,
    pub runs: Vec<TermRun>,
    pub lines: Vec<TermLine>,
    pub cursor: Option<(TermCursor, Option<TermRun>)>,
    /// Where the scrollbar's thumb is, as left, top, right and bottom in the area's own pixels, and its colour: there is one while lines are kept above the screen.
    pub thumb: Option<((i32, i32, i32, i32), u32)>,
}

impl Engine {
    /// The format of a terminal's text: its family at the size tuned, in the weight and style asked for.
    unsafe fn terminal_format(&mut self, font: &TermFont, bold: bool, italic: bool) -> windows::core::Result<IDWriteTextFormat> {
        let key = (format!("{}\u{1}{}", font.family, italic), if bold { 700 } else { 400 }, (font.size * 100.0) as u32);
        if let Some(format) = self.formats.get(&key) {
            return Ok(format.clone());
        }
        let locale = if crate::lang::is_japanese() { "ja-jp" } else { "en-us" };
        let style = if italic { DWRITE_FONT_STYLE_ITALIC } else { DWRITE_FONT_STYLE_NORMAL };
        let format = self.write.CreateTextFormat(&HSTRING::from(font.family.as_str()), self.collection.as_ref(), DWRITE_FONT_WEIGHT(key.1 as i32), style, DWRITE_FONT_STRETCH_NORMAL, font.size, &HSTRING::from(locale))?;
        if let (Some(fallback), Ok(newer)) = (&self.fallback, format.cast::<IDWriteTextFormat1>()) {
            newer.SetFontFallback(fallback)?;
        }
        self.formats.insert(key, format.clone());
        Ok(format)
    }

    /// Whether DirectWrite has a family by this name.
    unsafe fn has_family(&self, name: &str) -> bool {
        let find = |collection: &IDWriteFontCollection| {
            let (mut index, mut exists) = (0u32, windows::core::BOOL(0));
            collection.FindFamilyName(&HSTRING::from(name), &mut index, &mut exists).is_ok() && exists.as_bool()
        };
        match &self.collection {
            Some(collection) => find(collection),
            None => {
                let mut system: Option<IDWriteFontCollection> = None;
                self.write.GetSystemFontCollection(&mut system, false).is_ok() && system.as_ref().is_some_and(find)
            }
        }
    }
}

/// The advance of a character of the face at a size, and the height of its line and where the baseline is in it.
unsafe fn measure_cell(engine: &mut Engine, family: &str, size: f32) -> windows::core::Result<(f32, f32, f32)> {
    let probe = TermFont { family: family.to_string(), size, cell_width: 0, cell_height: 0, baseline: 0.0 };
    let format = engine.terminal_format(&probe, false, false)?;
    let units: Vec<u16> = "0000000000".encode_utf16().collect();
    let layout = engine.write.CreateTextLayout(&units, &format, 10_000.0, 10_000.0)?;
    let mut metrics = DWRITE_TEXT_METRICS::default();
    layout.GetMetrics(&mut metrics)?;
    let mut lines = [DWRITE_LINE_METRICS::default(); 1];
    let mut count = 0u32;
    layout.GetLineMetrics(Some(&mut lines), &mut count)?;
    Ok((metrics.widthIncludingTrailingWhitespace / 10.0, lines[0].height, lines[0].baseline))
}

/// The face for a terminal, the first of `families` this PC has, at `points` for a screen of `dpi`; with a size that makes a character a whole number of pixels wide. None where
/// DirectWrite is not there.
pub unsafe fn terminal_font(families: &[&str], points: i32, dpi: u32) -> Option<TermFont> {
    with_engine(|engine| {
        let family = families.iter().find(|name| engine.has_family(name)).or(families.last()).copied().unwrap_or("Consolas");
        let wanted = points as f32 * dpi as f32 / 72.0;
        let (advance, ..) = measure_cell(engine, family, wanted)?;
        let tuned = wanted * (advance.round().max(1.0) / advance.max(1.0));
        let (advance, height, baseline) = measure_cell(engine, family, tuned)?;
        Ok(TermFont { family: family.to_string(), size: tuned, cell_width: advance.round().max(1.0) as i32, cell_height: height.ceil().max(1.0) as i32, baseline })
    })
}

/// A terminal's scene in `area` of the device: all of it, from the ground up. False where DirectWrite could not draw it.
pub unsafe fn terminal(device: HDC, area: RECT, font: &TermFont, scene: &TermScene) -> bool {
    with_engine(|engine| {
        // (the formats are made first: a format is made with the engine, which the drawing below holds)
        let mut formats = Vec::new();
        for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
            formats.push(engine.terminal_format(font, bold, italic)?);
        }
        let format_of = |bold: bool, italic: bool| &formats[usize::from(bold) + 2 * usize::from(italic)];
        let write = engine.write.clone();
        engine.draw(device, area, |target, _| {
            let (cell_w, cell_h) = (font.cell_width as f32, font.cell_height as f32);
            let (left, top) = (scene.origin.0 as f32, scene.origin.1 as f32);
            let brush = target.CreateSolidColorBrush(&color(scene.ground), None)?;
            let rectangle = |row: usize, col: usize, cells: usize| D2D_RECT_F { left: left + col as f32 * cell_w, top: top + row as f32 * cell_h, right: left + (col + cells) as f32 * cell_w, bottom: top + (row + 1) as f32 * cell_h };
            target.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: (area.right - area.left) as f32, bottom: (area.bottom - area.top) as f32 }, &brush);
            for ground in &scene.grounds {
                brush.SetColor(&color(ground.color));
                target.FillRectangle(&rectangle(ground.row, ground.col, ground.cells), &brush);
            }
            let draw_run = |run: &TermRun, ink: u32, brush: &ID2D1SolidColorBrush| -> windows::core::Result<()> {
                let units: Vec<u16> = run.text.encode_utf16().collect();
                let place = rectangle(run.row, run.col, run.cells);
                let layout = write.CreateTextLayout(&units, format_of(run.bold, run.italic), (place.right - place.left).max(1.0), cell_h)?;
                layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                // (a wide character is centred in its cells, whichever face drew it)
                layout.SetTextAlignment(if run.cells > 1 && run.text.chars().count() == 1 { DWRITE_TEXT_ALIGNMENT_CENTER } else { DWRITE_TEXT_ALIGNMENT_LEADING })?;
                // (the baseline is where the measured cell has it, whichever face a character falls back to)
                layout.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, cell_h, font.baseline)?;
                brush.SetColor(&color(ink));
                target.DrawTextLayout(Vector2 { X: place.left, Y: place.top }, &layout, brush, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT | D2D1_DRAW_TEXT_OPTIONS_CLIP);
                Ok(())
            };
            for run in &scene.runs {
                draw_run(run, run.color, &brush)?;
            }
            let thick = (cell_h / 16.0).round().max(1.0);
            for line in &scene.lines {
                brush.SetColor(&color(line.color));
                let cell = rectangle(line.row, line.col, line.cells);
                let under = cell.top + font.baseline + thick.max(1.0);
                let (left, right) = (cell.left, cell.right);
                let bar = |from: f32, to: f32, at: f32, height: f32| D2D_RECT_F { left: from, top: at, right: to, bottom: at + height };
                match line.kind {
                    TermLineKind::Underline => target.FillRectangle(&bar(left, right, under.min(cell.bottom - thick), thick), &brush),
                    TermLineKind::Double => {
                        target.FillRectangle(&bar(left, right, (under - thick - 1.0).max(cell.top), thick), &brush);
                        target.FillRectangle(&bar(left, right, under.min(cell.bottom - thick), thick), &brush);
                    }
                    TermLineKind::Strike => target.FillRectangle(&bar(left, right, cell.top + font.baseline * 0.62, thick), &brush),
                    TermLineKind::Dotted | TermLineKind::Dashed | TermLineKind::Curly => {
                        // (dots, dashes, or a wave of steps up and down)
                        let step = if line.kind == TermLineKind::Dashed { 4.0 * thick } else { 2.0 * thick };
                        let mut x = left;
                        while x < right {
                            let to = (x + if line.kind == TermLineKind::Dashed { 3.0 * thick } else { thick }).min(right);
                            let rise = if line.kind == TermLineKind::Curly && ((x - left) / step) as i32 % 2 == 1 { -thick } else { 0.0 };
                            target.FillRectangle(&bar(x, to, under.min(cell.bottom - thick) + rise, thick), &brush);
                            x += step;
                        }
                    }
                }
            }
            if let Some((cursor, hidden)) = &scene.cursor {
                brush.SetColor(&color(cursor.color));
                let cell = rectangle(cursor.row, cursor.col, cursor.cells);
                match cursor.shape {
                    TermCursorShape::Block => {
                        target.FillRectangle(&cell, &brush);
                        if let Some(run) = hidden {
                            draw_run(run, run.color, &brush)?;
                        }
                    }
                    TermCursorShape::Underline => target.FillRectangle(&D2D_RECT_F { top: cell.bottom - thick.max(2.0), ..cell }, &brush),
                    TermCursorShape::Beam => target.FillRectangle(&D2D_RECT_F { right: cell.left + thick.max(2.0), ..cell }, &brush),
                    TermCursorShape::Hollow => target.DrawRectangle(&D2D_RECT_F { left: cell.left + 0.5, top: cell.top + 0.5, right: cell.right - 0.5, bottom: cell.bottom - 0.5 }, &brush, 1.0, None),
                }
            }
            if let Some(((left, top, right, bottom), thumb)) = scene.thumb {
                brush.SetColor(&color(thumb));
                let radius = (right - left) as f32 / 2.0;
                target.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left: left as f32, top: top as f32, right: right as f32, bottom: bottom as f32 }, radiusX: radius, radiusY: radius }, &brush);
            }
            Ok(())
        })
    })
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal's face makes a character a whole number of pixels wide, at each scale a screen has, and a line of them as wide as its cells.
    #[test]
    fn a_terminals_face_is_tuned_to_whole_pixels_and_a_run_ends_where_its_cells_do() {
        // SAFETY: DirectWrite, on this thread, which makes its own engine.
        unsafe {
            for dpi in [96, 120, 144, 168, 192] {
                let font = terminal_font(&["Cascadia Mono", "Consolas"], 9, dpi).expect("DirectWrite");
                assert!(font.cell_width >= 5 && font.cell_height > font.cell_width, "{font:?}");
                // (ten characters, laid out as a run is: exactly ten cells, and so for sixty)
                let widths = with_engine(|engine| {
                    let format = engine.terminal_format(&font, false, false)?;
                    let mut found = Vec::new();
                    for count in [10usize, 60] {
                        let units: Vec<u16> = "Wi0.".repeat(count / 4 + 1).encode_utf16().take(count).collect();
                        let layout = engine.write.CreateTextLayout(&units, &format, 10_000.0, 100.0)?;
                        let mut metrics = DWRITE_TEXT_METRICS::default();
                        layout.GetMetrics(&mut metrics)?;
                        found.push(metrics.widthIncludingTrailingWhitespace);
                    }
                    Ok(found)
                })
                .unwrap();
                assert!((widths[0] - 10.0 * font.cell_width as f32).abs() < 0.5, "dpi {dpi}: {widths:?} for cells of {}", font.cell_width);
                assert!((widths[1] - 60.0 * font.cell_width as f32).abs() < 1.0, "dpi {dpi}: {widths:?} for cells of {}", font.cell_width);
            }
        }
    }

    #[test]
    fn an_arc_starts_and_ends_where_gdi_plus_puts_them() {
        let (x, y) = on_arc(0.0, 0.0, 20.0, 20.0, 180.0);
        assert!((x - 0.0).abs() < 1e-4 && (y - 10.0).abs() < 1e-4, "{x} {y}");
        let (x, y) = on_arc(0.0, 0.0, 20.0, 20.0, 270.0);
        assert!((x - 10.0).abs() < 1e-4 && (y - 0.0).abs() < 1e-4, "{x} {y}");
    }
}
