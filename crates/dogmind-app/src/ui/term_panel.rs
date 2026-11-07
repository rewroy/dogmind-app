//! The Terminal view of the panel on the right: the person's own shells, as tabs, each a window drawn in the app's own paint. This file is the view's page and what it asks of the rest of
//! the app; the shells themselves are `term`.

use super::*;

impl App {
    /// The Terminal view, from `top` to the foot of the window: for now its empty page, which says what the view is and what it is not.
    pub(super) unsafe fn layout_terminal_view(&mut self, shapes: &mut Vec<Shape>, x: i32, width: i32, top: i32, bottom: i32) {
        let s = |value: i32| scale(self.dpi, value);
        let mut y = top + s(ds::space::XL);
        shapes.push(Shape::Icon { name: "terminal", area: rect(x, y, s(ds::size::ICON_LARGE), s(ds::size::ICON_LARGE)), size: s(ds::size::ICON_LARGE), color: theme::muted() });
        y += s(ds::size::ICON_LARGE) + s(ds::space::S);
        shapes.push(Shape::Text { area: rect(x, y, width, s(ds::space::XL)), text: t("Terminal").to_string(), font: FontKind::Strong, color: theme::text(), flags: ONE_LINE });
        y += s(ds::size::QUEUE_STEP);
        let about = t("Your own shell, in this session's folder.");
        let tall = text_height(self.fonts.small, about, width);
        shapes.push(Shape::Text { area: rect(x, y, width, tall), text: about.to_string(), font: FontKind::Small, color: theme::muted(), flags: DT_WORDBREAK });
        // At the foot of the page, quietly: what these commands are, and that the model does not see them.
        let rights = t("Commands here run with your own rights, apart from the agent's permissions. Nothing shown here is sent to the model.");
        let tall = text_height(self.fonts.small, rights, width);
        let at = (bottom - s(ds::space::L) - tall).max(y);
        shapes.push(Shape::Text { area: rect(x, at, width, tall), text: rights.to_string(), font: FontKind::Small, color: theme::faint(), flags: DT_WORDBREAK });
    }
}
