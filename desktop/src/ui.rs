//! Small FEMME-skinned building blocks (title bar, window, card, button, pill). These are plain
//! `div`s: the look is a handful of style calls, and restyling gpui-component's `Button`/`TabBar` to
//! the 1.2px ink rule, VT323 labels and hard shadows would take more code than it saves.

use std::sync::Arc;

use gpui::{
    div, fill, point, px, relative, size, App, Bounds, Corners, Div, Element, ElementId,
    GlobalElementId, Image, ImageFormat, InspectorElementId, InteractiveElement as _, IntoElement,
    LayoutId, ParentElement as _, Pixels, RenderImage, SharedString, Stateful, Style, Styled,
    Window,
};

use crate::theme::{
    self, BTN_SIZE, DESK, FONT_MONO, FONT_PIXEL, HOVER, INK, MUTED, PANEL, PAPER, RULE,
};

/// Sets the four border widths of `el` (top, right, bottom, left) and the ink border colour.
pub fn ruled_sides<E: Styled>(
    mut el: E,
    top: Pixels,
    right: Pixels,
    bottom: Pixels,
    left: Pixels,
) -> E {
    let b = &mut el.style().border_widths;
    b.top = Some(top.into());
    b.right = Some(right.into());
    b.bottom = Some(bottom.into());
    b.left = Some(left.into());
    el.border_color(INK)
}

/// The 1.2px ink border on all four sides.
pub fn ruled<E: Styled>(el: E) -> E {
    ruled_sides(el, RULE, RULE, RULE, RULE)
}

/// `.win`: ink border over the desk colour; its children sit in a 3px frame.
pub fn win() -> Div {
    ruled(div().flex().flex_col().bg(DESK).p(px(3.)).gap(px(3.)))
}

/// `.tb`: black bar, desk-coloured lowercase VT323 22px label, optional right-aligned tail.
pub fn tb(label: impl Into<SharedString>) -> Div {
    let label: SharedString = label.into();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .px(px(8.))
        .py(px(3.))
        .bg(INK)
        .text_color(DESK)
        .font_family(FONT_PIXEL)
        .text_size(theme::TB_SIZE)
        .line_height(relative(1.))
        .child(SharedString::from(label.to_lowercase()))
}

/// `.card`: paper box with the ink rule.
pub fn card() -> Div {
    ruled(div().bg(PAPER).px(px(12.)).py(px(10.)))
}

/// `.dialog`: panel-grey card with the hard 3px shadow.
pub fn dialog() -> Div {
    let mut d = ruled(div().bg(PANEL));
    d.style().box_shadow = Some(theme::hard_shadow());
    d
}

/// `button`: VT323 20px label in a paper box, pink on hover.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    ruled(
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(34.))
            .px(px(10.))
            .bg(PAPER)
            .text_color(INK)
            .font_family(FONT_PIXEL)
            .text_size(BTN_SIZE)
            .line_height(relative(1.))
            .cursor_pointer(),
    )
    .hover(|s| s.bg(HOVER))
    .child(label.into())
}

/// `button.dark`: inverse of [`button`], also used for the selected tab.
pub fn button_dark(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    button(id, label)
        .bg(INK)
        .text_color(PAPER)
        .hover(|s| s.bg(HOVER).text_color(INK))
}

/// `.st`: status pill, VT323 18px with a currentColor border.
pub fn st(label: impl Into<SharedString>, color: gpui::Rgba) -> Div {
    let mut d = div()
        .px(px(6.))
        .py(px(1.))
        .text_color(color)
        .font_family(FONT_PIXEL)
        .text_size(px(18.))
        .line_height(relative(1.))
        .child(label.into());
    let b = &mut d.style().border_widths;
    b.top = Some(RULE.into());
    b.right = Some(RULE.into());
    b.bottom = Some(RULE.into());
    b.left = Some(RULE.into());
    d.border_color(color)
}

/// `.dot`: 10px square status marker.
pub fn dot(color: gpui::Rgba) -> Div {
    ruled(div().flex_none().size(px(10.)).bg(color))
}

/// `.meta`/`.mini`: small muted mono text.
pub fn meta(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(FONT_MONO)
        .text_size(theme::META_SIZE)
        .text_color(MUTED)
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .child(text.into())
}

/// Logical size of one dot-grid tile: a multiple of the board's 6px period.
const DOT_TILE: f32 = 192.;

/// The desk: pink with a 1px ink dot every 6px (the board's `body` background). gpui has no tiled
/// backgrounds, so a pre-rendered 192px tile is painted as a grid of sprites (about forty at 1440x900).
// ponytail: the tile density (1x or 2x) is picked once from the window's scale factor at creation;
// a window dragged between displays of different density just shows a slightly soft or fat dot.
#[derive(Clone)]
pub struct DotGrid {
    tile: Arc<RenderImage>,
}

impl DotGrid {
    /// Decodes the embedded tile that matches `scale` (the window's scale factor).
    pub fn new(scale: f32, cx: &App) -> Self {
        let bytes: &[u8] = if scale > 1.5 {
            include_bytes!("../assets/dots-2x.png")
        } else {
            include_bytes!("../assets/dots-1x.png")
        };
        let tile = Image::from_bytes(ImageFormat::Png, bytes.to_vec())
            .to_image_data(cx.svg_renderer())
            .expect("embedded dot tile is a valid PNG");
        Self { tile }
    }
}

impl IntoElement for DotGrid {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for DotGrid {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        window.paint_quad(fill(bounds, DESK));
        let t = px(DOT_TILE);
        let mut y = bounds.origin.y;
        while y < bounds.bottom() {
            let mut x = bounds.origin.x;
            while x < bounds.right() {
                let tile = Bounds::new(point(x, y), size(t, t));
                let _ =
                    window.paint_image(tile, tile, Corners::default(), self.tile.clone(), 0, false);
                x += t;
            }
            y += t;
        }
    }
}
