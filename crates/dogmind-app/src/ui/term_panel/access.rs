//! The terminal in UI Automation, which a screen reader reads through: a document named "Terminal" whose text is what is in view, with the text pattern (a range of it as text, by character,
//! word and line; where the cursor is as the selection; where each part is on the screen), and the events that say the text or the cursor has moved. The text is the transcript's way:
//! a document, and not a list of lines to be told one by one.
//!
//! What it does not do: the text is what is in view (the lines scrolled off are read by scrolling back, as a person does); a range cannot be selected (a reader moves its own); and no
//! attribute of the text (colours, bold) is told, as the screen's colours mean what the program means by them and not the words.

// (Windows's names for UI Automation's constants, which are matched on here)
#![allow(non_upper_case_globals)]

use std::cell::Cell;

use windows::core::{implement, AsImpl, Error, Interface, Result, BOOL, BSTR};
use windows_core::IUnknownImpl;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, HWND};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement};
use windows::Win32::System::Variant::{VARIANT, VT_R8, VT_UNKNOWN};
use windows::Win32::UI::Accessibility::*;

/// What a reader is told of the terminal, as of the moment it asks: the text in view, a line to a line, and where the cursor is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    /// The text, as UTF-16 (what a range is counted in), lines joined by a line feed.
    pub text: Vec<u16>,
    /// Where the cursor is in it.
    pub caret: usize,
    /// Where each line's cells begin on the screen: the left and top, and the size of a cell, in pixels of the screen.
    pub origin: (i32, i32),
    pub cell: (i32, i32),
}

/// Whether a character takes two cells.
fn wide(unit: char) -> bool {
    crate::paint::is_japanese(unit) || matches!(u32::from(unit), 0x1100..=0x115F | 0xAC00..=0xD7A3 | 0x1F300..=0x1FAFF)
}

/// The start and end of the line the offset is in: the end is before its line feed.
fn line_around(text: &[u16], at: usize) -> (usize, usize) {
    let at = at.min(text.len());
    let start = text[..at].iter().rposition(|unit| *unit == 10).map_or(0, |found| found + 1);
    let end = text[at..].iter().position(|unit| *unit == 10).map_or(text.len(), |found| at + found);
    (start, end)
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Kind {
    Space,
    Word,
    Other,
}

fn kind_of(unit: u16) -> Kind {
    match char::from_u32(u32::from(unit)) {
        Some(c) if c == '\n' => Kind::Other,
        Some(c) if c.is_whitespace() => Kind::Space,
        Some(c) if c.is_alphanumeric() || c == '_' => Kind::Word,
        _ => Kind::Other,
    }
}

/// The unit of text that holds the offset, as UI Automation names units: a character, a word (with the spaces after it), a line, or all of it.
fn unit_around(text: &[u16], at: usize, unit: TextUnit) -> (usize, usize) {
    let at = at.min(text.len());
    match unit {
        TextUnit_Character => (at.min(text.len().saturating_sub(1)), (at + 1).min(text.len())),
        TextUnit_Word => {
            let (line_start, line_end) = line_around(text, at);
            if at >= line_end {
                return (line_start.max(at.min(line_end)), line_end);
            }
            let kind = kind_of(text[at]);
            let mut start = at;
            while start > line_start && kind_of(text[start - 1]) == kind {
                start -= 1;
            }
            let mut end = at;
            while end < line_end && kind_of(text[end]) == kind {
                end += 1;
            }
            // (a word has the spaces after it, as a reader says it)
            while kind != Kind::Space && end < line_end && kind_of(text[end]) == Kind::Space {
                end += 1;
            }
            (start, end)
        }
        TextUnit_Line | TextUnit_Paragraph => line_around(text, at),
        _ => (0, text.len()),
    }
}

/// The offset `count` units on from `at` (back for less than none), and how many were taken.
fn step(text: &[u16], at: usize, unit: TextUnit, count: i32) -> (usize, i32) {
    if matches!(unit, TextUnit_Document | TextUnit_Page | TextUnit_Format) {
        return (if count < 0 { 0 } else { text.len() }, 0);
    }
    let mut here = at.min(text.len());
    let mut taken = 0;
    while taken < count.abs() {
        let (start, end) = unit_around(text, here, unit);
        let next = if count > 0 {
            if end >= text.len() {
                break;
            }
            // (a line's end is before its line feed: the next line begins after it)
            if unit == TextUnit_Line || unit == TextUnit_Paragraph { end + 1 } else { end }
        } else {
            if start == 0 {
                break;
            }
            let before = if unit == TextUnit_Line || unit == TextUnit_Paragraph { start - 1 } else { start - 1 };
            unit_around(text, before, unit).0
        };
        here = next;
        taken += 1;
    }
    (here, if count < 0 { -taken } else { taken })
}

fn empty_array(kind: windows::Win32::System::Variant::VARENUM) -> Result<*mut SAFEARRAY> {
    // SAFETY: an array of no elements.
    let array = unsafe { SafeArrayCreateVector(kind, 0, 0) };
    if array.is_null() { Err(Error::from_hresult(windows::Win32::Foundation::E_OUTOFMEMORY)) } else { Ok(array) }
}

fn ranges_array(ranges: &[ITextRangeProvider]) -> Result<*mut SAFEARRAY> {
    // SAFETY: an array of this many elements, each put as the pointer to its interface (which the array holds a reference to).
    unsafe {
        let array = SafeArrayCreateVector(VT_UNKNOWN, 0, ranges.len() as u32);
        if array.is_null() {
            return Err(Error::from_hresult(windows::Win32::Foundation::E_OUTOFMEMORY));
        }
        for (at, range) in ranges.iter().enumerate() {
            SafeArrayPutElement(array, &(at as i32), range.as_raw())?;
        }
        Ok(array)
    }
}

/// The window's element: the document.
#[implement(IRawElementProviderSimple, ITextProvider)]
pub struct Element {
    pub window: HWND,
}

/// A part of the text, from `start` to `end` (a cursor is one that is as long as nothing).
#[implement(ITextRangeProvider)]
struct Range {
    window: HWND,
    start: Cell<usize>,
    end: Cell<usize>,
}

fn read() -> Document {
    // SAFETY: this runs on the window's thread, as UI Automation calls a provider that does not say it keeps COM's rules.
    unsafe { super::document() }
}

impl Range {
    fn made(window: HWND, start: usize, end: usize) -> ITextRangeProvider {
        Range { window, start: Cell::new(start), end: Cell::new(end) }.into()
    }

    fn clamp(&self, text_length: usize) {
        let (start, end) = (self.start.get().min(text_length), self.end.get().min(text_length));
        self.start.set(start);
        self.end.set(end.max(start));
    }

    fn of_other(range: &windows_core::Ref<'_, ITextRangeProvider>) -> Result<(usize, usize)> {
        let range: &ITextRangeProvider = range.as_ref().ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
        // SAFETY: the only ranges this window's reader is given are this file's.
        let other: &Range = unsafe { range.as_impl() };
        Ok((other.start.get(), other.end.get()))
    }

    fn endpoint(&self, which: TextPatternRangeEndpoint) -> usize {
        if which == TextPatternRangeEndpoint_Start { self.start.get() } else { self.end.get() }
    }

    fn set_endpoint(&self, which: TextPatternRangeEndpoint, to: usize) {
        if which == TextPatternRangeEndpoint_Start {
            self.start.set(to);
            if self.end.get() < to {
                self.end.set(to);
            }
        } else {
            self.end.set(to);
            if self.start.get() > to {
                self.start.set(to);
            }
        }
    }
}

impl IRawElementProviderSimple_Impl for Element_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, pattern: UIA_PATTERN_ID) -> Result<windows::core::IUnknown> {
        if pattern == UIA_TextPatternId { Ok(self.to_interface::<ITextProvider>().into()) } else { Err(Error::empty()) }
    }

    fn GetPropertyValue(&self, property: UIA_PROPERTY_ID) -> Result<VARIANT> {
        Ok(match property {
            UIA_NamePropertyId => VARIANT::from(crate::lang::t("Terminal")),
            UIA_ControlTypePropertyId => VARIANT::from(UIA_DocumentControlTypeId.0),
            UIA_IsControlElementPropertyId | UIA_IsContentElementPropertyId | UIA_IsKeyboardFocusablePropertyId | UIA_IsTextPatternAvailablePropertyId => VARIANT::from(true),
            // (a terminal is not a form to be filled in: its text is its screen's)
            UIA_IsPasswordPropertyId => VARIANT::from(false),
            _ => VARIANT::default(),
        })
    }

    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        // SAFETY: the window is this element's.
        unsafe { UiaHostProviderFromHwnd(self.window) }
    }
}

impl ITextProvider_Impl for Element_Impl {
    fn GetSelection(&self) -> Result<*mut SAFEARRAY> {
        let document = read();
        ranges_array(&[Range::made(self.window, document.caret, document.caret)])
    }

    fn GetVisibleRanges(&self) -> Result<*mut SAFEARRAY> {
        let document = read();
        ranges_array(&[Range::made(self.window, 0, document.text.len())])
    }

    fn RangeFromChild(&self, _child: windows_core::Ref<'_, IRawElementProviderSimple>) -> Result<ITextRangeProvider> {
        Err(Error::from_hresult(E_INVALIDARG))
    }

    fn RangeFromPoint(&self, point: &UiaPoint) -> Result<ITextRangeProvider> {
        let document = read();
        let (cell_width, cell_height) = (document.cell.0.max(1), document.cell.1.max(1));
        let row = (((point.y as i32) - document.origin.1).max(0) / cell_height) as usize;
        let column = (((point.x as i32) - document.origin.0).max(0) / cell_width) as usize;
        // (the cells of a line, as characters: a wide one is two cells)
        let mut at = 0;
        for _ in 0..row {
            at = line_around(&document.text, at).1 + 1;
            if at >= document.text.len() {
                break;
            }
        }
        let (start, end) = line_around(&document.text, at.min(document.text.len()));
        let mut cells = 0;
        let mut offset = start;
        while offset < end && cells < column {
            let c = char::decode_utf16(document.text[offset..].iter().copied()).next().and_then(|c| c.ok()).unwrap_or(' ');
            cells += if wide(c) { 2 } else { 1 };
            offset += c.len_utf16();
        }
        Ok(Range::made(self.window, offset, offset))
    }

    fn DocumentRange(&self) -> Result<ITextRangeProvider> {
        Ok(Range::made(self.window, 0, read().text.len()))
    }

    fn SupportedTextSelection(&self) -> Result<SupportedTextSelection> {
        Ok(SupportedTextSelection_Single)
    }
}

impl ITextRangeProvider_Impl for Range_Impl {
    fn Clone(&self) -> Result<ITextRangeProvider> {
        Ok(Range::made(self.window, self.start.get(), self.end.get()))
    }

    fn Compare(&self, range: windows_core::Ref<'_, ITextRangeProvider>) -> Result<BOOL> {
        let (start, end) = Range::of_other(&range)?;
        Ok((start == self.start.get() && end == self.end.get()).into())
    }

    fn CompareEndpoints(&self, endpoint: TextPatternRangeEndpoint, target: windows_core::Ref<'_, ITextRangeProvider>, target_endpoint: TextPatternRangeEndpoint) -> Result<i32> {
        let (start, end) = Range::of_other(&target)?;
        let there = if target_endpoint == TextPatternRangeEndpoint_Start { start } else { end };
        Ok(match self.endpoint(endpoint).cmp(&there) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })
    }

    fn ExpandToEnclosingUnit(&self, unit: TextUnit) -> Result<()> {
        let document = read();
        self.clamp(document.text.len());
        let (start, end) = unit_around(&document.text, self.start.get(), unit);
        self.start.set(start);
        self.end.set(end);
        Ok(())
    }

    fn FindAttribute(&self, _attribute: UIA_TEXTATTRIBUTE_ID, _value: &VARIANT, _backward: BOOL) -> Result<ITextRangeProvider> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn FindText(&self, text: &BSTR, backward: BOOL, ignore_case: BOOL) -> Result<ITextRangeProvider> {
        let document = read();
        self.clamp(document.text.len());
        let looked: Vec<u16> = text.to_string().encode_utf16().collect();
        let (from, to) = (self.start.get(), self.end.get());
        if looked.is_empty() || to - from < looked.len() {
            return Err(Error::empty());
        }
        let same = |a: u16, b: u16| if ignore_case.as_bool() { char::from_u32(u32::from(a)).map(|c| c.to_lowercase().next()) == char::from_u32(u32::from(b)).map(|c| c.to_lowercase().next()) } else { a == b };
        let found = |at: usize| looked.iter().enumerate().all(|(offset, unit)| same(document.text[at + offset], *unit));
        let at = if backward.as_bool() { (from..=to - looked.len()).rev().find(|at| found(*at)) } else { (from..=to - looked.len()).find(|at| found(*at)) };
        at.map(|at| Range::made(self.window, at, at + looked.len())).ok_or_else(Error::empty)
    }

    fn GetAttributeValue(&self, _attribute: UIA_TEXTATTRIBUTE_ID) -> Result<VARIANT> {
        // (not told: what a colour means is the program's)
        // SAFETY: the reserved value is UI Automation's own.
        Ok(VARIANT::from(unsafe { UiaGetReservedNotSupportedValue() }?))
    }

    fn GetBoundingRectangles(&self) -> Result<*mut SAFEARRAY> {
        let document = read();
        self.clamp(document.text.len());
        // One rectangle for each line the range has a part of: left, top, width and height, in pixels of the screen.
        let (start, end) = (self.start.get(), self.end.get());
        let mut rectangles: Vec<f64> = Vec::new();
        let mut line_start = 0;
        let mut row = 0;
        loop {
            let (_, line_end) = line_around(&document.text, line_start);
            if line_end >= start && line_start <= end.max(start) {
                let cells = |from: usize, to: usize| -> i32 {
                    char::decode_utf16(document.text[from..to].iter().copied()).map(|c| if c.map_or(false, wide) { 2 } else { 1 }).sum()
                };
                let (from, to) = (start.max(line_start), end.min(line_end).max(start.max(line_start)));
                let (left, width) = (cells(line_start, from.min(line_end)), cells(from.min(line_end), to).max(if start == end { 1 } else { 0 }));
                rectangles.extend([f64::from(document.origin.0 + left * document.cell.0), f64::from(document.origin.1 + row * document.cell.1), f64::from(width * document.cell.0), f64::from(document.cell.1)]);
            }
            if line_end >= document.text.len() || line_start > end {
                break;
            }
            line_start = line_end + 1;
            row += 1;
        }
        // SAFETY: an array of this many numbers, each put as the number itself.
        unsafe {
            let array = SafeArrayCreateVector(VT_R8, 0, rectangles.len() as u32);
            if array.is_null() {
                return Err(Error::from_hresult(E_FAIL));
            }
            for (at, number) in rectangles.iter().enumerate() {
                SafeArrayPutElement(array, &(at as i32), (number as *const f64).cast())?;
            }
            Ok(array)
        }
    }

    fn GetEnclosingElement(&self) -> Result<IRawElementProviderSimple> {
        Ok(Element { window: self.window }.into())
    }

    fn GetText(&self, max_length: i32) -> Result<BSTR> {
        let document = read();
        self.clamp(document.text.len());
        let mut end = self.end.get();
        if max_length >= 0 {
            end = end.min(self.start.get() + max_length as usize);
        }
        Ok(BSTR::from_wide(&document.text[self.start.get()..end]))
    }

    fn Move(&self, unit: TextUnit, count: i32) -> Result<i32> {
        let document = read();
        self.clamp(document.text.len());
        let (start, _) = unit_around(&document.text, self.start.get(), unit);
        let (here, taken) = step(&document.text, start, unit, count);
        let (from, to) = unit_around(&document.text, here, unit);
        self.start.set(from);
        self.end.set(to);
        Ok(taken)
    }

    fn MoveEndpointByUnit(&self, endpoint: TextPatternRangeEndpoint, unit: TextUnit, count: i32) -> Result<i32> {
        let document = read();
        self.clamp(document.text.len());
        let (here, taken) = step(&document.text, self.endpoint(endpoint), unit, count);
        self.set_endpoint(endpoint, here);
        Ok(taken)
    }

    fn MoveEndpointByRange(&self, endpoint: TextPatternRangeEndpoint, target: windows_core::Ref<'_, ITextRangeProvider>, target_endpoint: TextPatternRangeEndpoint) -> Result<()> {
        let (start, end) = Range::of_other(&target)?;
        self.set_endpoint(endpoint, if target_endpoint == TextPatternRangeEndpoint_Start { start } else { end });
        Ok(())
    }

    fn Select(&self) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn AddToSelection(&self) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn RemoveFromSelection(&self) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn ScrollIntoView(&self, _align_to_top: BOOL) -> Result<()> {
        Ok(())
    }

    fn GetChildren(&self) -> Result<*mut SAFEARRAY> {
        empty_array(VT_UNKNOWN)
    }
}

/// UI Automation is given the window's element for a request of it, and made once.
pub unsafe fn give(window: windows_sys::Win32::Foundation::HWND, wparam: usize, lparam: isize, element: &IRawElementProviderSimple) -> isize {
    let windows::Win32::Foundation::LRESULT(result) = UiaReturnRawElementProvider(HWND(window), windows::Win32::Foundation::WPARAM(wparam), windows::Win32::Foundation::LPARAM(lparam), element);
    result
}

pub fn element(window: windows_sys::Win32::Foundation::HWND) -> IRawElementProviderSimple {
    Element { window: HWND(window) }.into()
}

/// As the window goes: UI Automation lets go of its element.
pub unsafe fn let_go(window: windows_sys::Win32::Foundation::HWND, element: &IRawElementProviderSimple) {
    UiaReturnRawElementProvider(HWND(window), windows::Win32::Foundation::WPARAM(0), windows::Win32::Foundation::LPARAM(0), None::<&IRawElementProviderSimple>);
    let _ = UiaDisconnectProvider(element);
}

/// The text has changed, or the cursor has moved: a reader that listens is told. Only where one does listen.
pub unsafe fn changed(element: &IRawElementProviderSimple, text: bool, cursor: bool) {
    if !UiaClientsAreListening().as_bool() {
        return;
    }
    if text {
        let _ = UiaRaiseAutomationEvent(element, UIA_Text_TextChangedEventId);
    }
    if cursor {
        let _ = UiaRaiseAutomationEvent(element, UIA_Text_TextSelectionChangedEventId);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(words: &str) -> Vec<u16> {
        words.encode_utf16().collect()
    }

    fn taken(words: &str, (start, end): (usize, usize)) -> String {
        String::from_utf16_lossy(&text(words)[start..end])
    }

    #[test]
    fn a_line_is_the_text_between_its_line_feeds_and_a_word_has_the_spaces_after_it() {
        let words = "first line\nsecond one here\nthird";
        let at = |needle: &str| words.find(needle).unwrap();
        assert_eq!(taken(words, unit_around(&text(words), at("cond"), TextUnit_Line)), "second one here");
        assert_eq!(taken(words, unit_around(&text(words), 0, TextUnit_Line)), "first line");
        assert_eq!(taken(words, unit_around(&text(words), words.len(), TextUnit_Line)), "third");
        assert_eq!(taken(words, unit_around(&text(words), at("one"), TextUnit_Word)), "one ");
        assert_eq!(taken(words, unit_around(&text(words), at("second") + 2, TextUnit_Word)), "second ");
        assert_eq!(taken(words, unit_around(&text(words), 3, TextUnit_Character)), "s");
        assert_eq!(unit_around(&text(words), 5, TextUnit_Document), (0, words.len()));
        // (punctuation is a word of its own, and a line break is not in a word)
        let code = "a = f(x);";
        assert_eq!(taken(code, unit_around(&text(code), 4, TextUnit_Word)), "f");
        assert_eq!(taken(code, unit_around(&text(code), 5, TextUnit_Word)), "(");
    }

    #[test]
    fn a_move_by_lines_goes_to_the_next_and_the_one_before_and_says_how_many_it_could_take() {
        let words = "one\ntwo\nthree";
        let data = text(words);
        let (at, count) = step(&data, 0, TextUnit_Line, 1);
        assert_eq!((taken(words, unit_around(&data, at, TextUnit_Line)), count), ("two".to_string(), 1));
        let (at, count) = step(&data, 0, TextUnit_Line, 5);
        assert_eq!((taken(words, unit_around(&data, at, TextUnit_Line)), count), ("three".to_string(), 2), "only two lines are there");
        let (at, count) = step(&data, words.len(), TextUnit_Line, -1);
        assert_eq!((taken(words, unit_around(&data, at, TextUnit_Line)), count), ("two".to_string(), -1));
        let (at, count) = step(&data, 0, TextUnit_Line, -3);
        assert_eq!((at, count), (0, 0), "nothing before the first");
        let (at, count) = step(&data, 0, TextUnit_Character, 4);
        assert_eq!((at, count), (4, 4), "a line feed is a character");
    }

    #[test]
    fn japanese_is_two_cells_and_one_character() {
        assert!(wide('日') && wide('あ') && wide('ア') && !wide('a') && !wide('1'));
    }
}
