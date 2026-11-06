//! FEMME-skinned building blocks: window, title bar, card, buttons, icons, chips, dots, menus and the
//! pink dot-grid desk. These are plain `div`s: the look is a handful of style calls, and restyling
//! gpui-component's `Button`/`TabBar` to the 1.2 px ink rule, VT323 labels and hard shadows would take
//! more code than it saves. Every colour comes from [`crate::theme`]; nothing here hard-codes one.
//!
//! [`gallery`] lays every helper out in every state: it is the style sheet of the skin.
//!
//! Interaction states, one rule for all controls: *hover* fills the control with the FEMME pink and
//! leaves its label colour alone (gpui resolves a label's hover colour from the last mouse move but the
//! fill from the live hit test, so a label that changed colour on hover could end up unreadable);
//! *pressed* turns it neon and, for a button, drops the hard shadow and moves the box by the same 2 px;
//! *disabled* greys label and border and removes the shadow, the hover and the pointer.

use std::sync::Arc;

use gpui::{
    div, fill, point, px, relative, size, App, Bounds, Corners, Div, Element, ElementId,
    FontWeight, GlobalElementId, Image, ImageFormat, InspectorElementId, InteractiveElement as _,
    IntoElement, LayoutId, ParentElement as _, Pixels, RenderImage, Rgba, SharedString, Stateful,
    StatefulInteractiveElement as _, Style, Styled, Window,
};

use crate::theme::{
    self, accent, accent_soft, accent_strong, bar, bar_text, desk, ink, muted, on_accent_strong,
    panel, paper, Palette, BODY_SIZE, BTN_SIZE, FONT_MONO, FONT_PIXEL, FONT_UI, RULE, STRIP_W,
    TITLE_SIZE,
};

pub mod clip;
pub mod ctx;
mod gallery;
pub use gallery::gallery;

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
    el.border_color(ink())
}

/// The 1.2px ink border on all four sides.
pub fn ruled<E: Styled>(el: E) -> E {
    ruled_sides(el, RULE, RULE, RULE, RULE)
}

// ---------------------------------------------------------------------------------------------
// Surfaces
// ---------------------------------------------------------------------------------------------

/// `.win`: a panel on the desk. Paper with the 1.2 px ink rule, children stacked top to bottom: put a
/// [`title_bar`] first and the body (give it `flex_1().min_h_0()`) after it. The pink desk shows only
/// in the [`theme::GUTTER`] around it.
pub fn win() -> Div {
    ruled(div().flex().flex_col().bg(paper()))
}

/// `.tb`: the dark title bar of a window. Lowercase VT323 22 px lettering in pink
/// ([`theme::bar_text`]); the label takes the free width, so any further `.child(..)` (an
/// [`icon_button`], a [`chip`], a status) sits at the right edge. Set
/// `.text_color(theme::on_bar())` on it for the plain lettering of a dialog.
///
/// Use it as the first child of a [`win`] or a [`dialog`].
pub fn title_bar(text: impl Into<SharedString>) -> Div {
    let text: SharedString = text.into();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .px(px(8.))
        .py(px(3.))
        .bg(bar())
        .text_color(bar_text())
        .font_family(FONT_PIXEL)
        .text_size(TITLE_SIZE)
        .line_height(relative(1.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(SharedString::from(text.to_lowercase())),
        )
}

/// `.tb`: the same as [`title_bar`] (the name the first screens used).
pub fn tb(label: impl Into<SharedString>) -> Div {
    title_bar(label)
}

/// `.card`: a paper box with the ink rule and the small hard shadow ([`theme::shadow`]). A group of
/// facts on a panel: an account, a session row, a tool call. Leave 3 px of room right and below it, or
/// an `overflow_hidden` parent cuts the shadow.
pub fn card() -> Div {
    ruled(div().bg(paper()).px(px(12.)).py(px(10.))).shadow(theme::shadow())
}

/// `.dialog`: a panel-grey card with the larger hard shadow ([`theme::hard_shadow`]). A modal over the
/// centre (accounts): a [`title_bar`], then paper [`card`]s on the grey.
pub fn dialog() -> Div {
    ruled(div().bg(panel())).shadow(theme::hard_shadow())
}

/// A dropdown or popover surface: paper, ink rule, the large hard shadow, 4 px of room around its
/// [`menu_item`]s. gpui-component's own popups keep their hairline edge (the theme cannot change it);
/// build the `femme` dropdown and context menus from this instead.
pub fn menu() -> Div {
    ruled(div().flex().flex_col().gap(px(2.)).p(px(4.)).bg(paper())).shadow(theme::hard_shadow())
}

/// A row of a [`menu`]: 28 px tall, body text, pink on hover, neon while pressed. Add the label (and
/// anything after it) with `.child(..)`; wrap a key hint in [`meta`] and push it right with a
/// `div().flex_1()` before it.
pub fn menu_item(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(28.))
        .px(px(8.))
        .font_family(FONT_UI)
        .text_size(BODY_SIZE)
        .text_color(ink())
        .cursor_pointer()
        .hover(|s| s.bg(accent()))
        .active(|s| s.bg(accent_strong()).text_color(on_accent_strong()))
}

/// A row of a list (sessions, files, accounts): at least 30 px, paper, a soft pink wash on hover and
/// neon while pressed. `selected` makes it the pink of the current item with a 4 px ink bar on its
/// left edge. Add the content with `.child(..)`.
pub fn row(id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    let r = div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .gap(px(8.))
        .min_h(px(30.))
        .px(px(10.))
        .py(px(4.))
        .text_color(ink())
        .cursor_pointer()
        .bg(if selected { accent() } else { paper() })
        .hover(move |s| s.bg(if selected { accent() } else { accent_soft() }))
        .active(|s| s.bg(accent_strong()).text_color(on_accent_strong()));
    if selected {
        r.child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(4.))
                .bg(ink()),
        )
    } else {
        r
    }
}

// ---------------------------------------------------------------------------------------------
// Controls: one look table drives the real (interactive) controls and the gallery's samples, so the
// style sheet can never drift from what the app shows.
// ---------------------------------------------------------------------------------------------

/// What a control looks like in one state.
#[derive(Clone, Copy)]
struct Look {
    bg: Rgba,
    fg: Rgba,
    edge: Rgba,
    /// Has the small hard shadow.
    raised: bool,
    /// Moved by the shadow's offset, covering where the shadow was.
    pushed: bool,
}

/// The flavours of control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Paper fill, ink label: the ordinary button.
    Light,
    /// Ink fill, paper label: the main action, a selected tab.
    Dark,
    /// A square glyph button with no fill or edge until the pointer comes.
    Ghost,
    /// The selected [`Kind::Ghost`]: ink fill, paper glyph.
    GhostOn,
    /// A square glyph button on an ink bar (a title bar): paper glyph, deep pink under the pointer.
    OnInk,
}

/// The states a control can be shown in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Idle,
    Hover,
    Pressed,
    Disabled,
}

/// What a control of `kind` looks like in `state`, in the live palette.
fn look(kind: Kind, state: State) -> Look {
    look_in(theme::current(), kind, state)
}

/// [`look`] in the palette `p` (the tests check both).
fn look_in(p: &Palette, kind: Kind, state: State) -> Look {
    use Kind::*;
    use State::*;
    let clear = theme::rgba8(0xFEFEFE, 0.);
    let flat = |bg, fg, edge| Look {
        bg,
        fg,
        edge,
        raised: false,
        pushed: false,
    };
    let raised = |bg, fg| Look {
        bg,
        fg,
        edge: p.ink,
        raised: true,
        pushed: false,
    };
    // Pressed: neon, shadow dropped and the box moved onto it. The label is the dark colour (ink on
    // the light palette, paper on the dark one).
    let pushed = Look {
        bg: p.accent_strong,
        fg: p.on_accent_strong,
        edge: p.ink,
        raised: false,
        pushed: true,
    };
    match (kind, state) {
        (Light, Idle) => raised(p.paper, p.ink),
        (Light, Hover) => raised(p.accent, p.on_accent),
        (Light, Pressed) | (Dark, Pressed) => pushed,
        (Light, Disabled) => flat(p.paper, p.muted_text, p.muted_text),
        (Dark, Idle) => raised(p.ink, p.paper),
        // Deep pink: the label stays paper, and paper on it is 4.7:1 or better.
        (Dark, Hover) => raised(p.accent_deep, p.paper),
        (Dark, Disabled) => flat(p.panel, p.muted_text, p.muted_text),
        (Ghost, Idle) => flat(clear, p.ink, clear),
        (Ghost, Hover) => flat(p.accent, p.on_accent, p.ink),
        // On a bar the box has no visible edge, like the bar.
        (OnInk, Pressed) => Look {
            pushed: false,
            edge: p.bar,
            ..pushed
        },
        (Ghost | GhostOn, Pressed) => Look {
            pushed: false,
            ..pushed
        },
        (Ghost | GhostOn | OnInk, Disabled) => flat(clear, p.muted_text, clear),
        (GhostOn, Idle) => flat(p.ink, p.paper, p.ink),
        (GhostOn, Hover) => flat(p.accent_deep, p.paper, p.ink),
        (OnInk, Idle) => flat(clear, p.on_bar, clear),
        (OnInk, Hover) => flat(p.bar_hover, p.on_bar, p.on_bar),
    }
}

/// The box every control shares, before any state is applied.
fn frame(kind: Kind) -> Div {
    let base = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .font_family(FONT_PIXEL)
        .text_size(BTN_SIZE)
        .line_height(relative(1.))
        .whitespace_nowrap();
    ruled(match kind {
        Kind::Ghost | Kind::GhostOn | Kind::OnInk => base.size(px(22.)),
        Kind::Light | Kind::Dark => base.h(px(30.)).px(px(10.)).gap(px(6.)),
    })
}

/// Pressed: neon, no shadow, moved 2 px down and right (taffy's relative offset, so siblings do not
/// move).
fn press<E: Styled>(el: E, l: Look) -> E {
    el.bg(l.bg)
        .text_color(l.fg)
        .shadow(Vec::new())
        .top(px(2.))
        .left(px(2.))
}

/// Paints `l` onto `el` as its resting look (for the real controls) or as a frozen sample (gallery).
fn apply<E: Styled>(el: E, l: Look) -> E {
    if l.pushed {
        return press(el.border_color(l.edge), l);
    }
    let el = el.bg(l.bg).text_color(l.fg).border_color(l.edge);
    if l.raised {
        el.shadow(theme::shadow())
    } else {
        el.shadow(Vec::new())
    }
}

/// An interactive control: resting look, hover fill, pressed look, pointer cursor.
fn control(kind: Kind, id: impl Into<ElementId>) -> Stateful<Div> {
    let (hover, pressed) = (look(kind, State::Hover), look(kind, State::Pressed));
    apply(frame(kind), look(kind, State::Idle))
        .id(id)
        .cursor_pointer()
        // Hover recolours the fill (and a ghost's edge) only: no label colour, see the module docs.
        .hover(move |s| s.bg(hover.bg).border_color(hover.edge))
        .active(move |s| press(s, pressed))
}

/// `button`: a paper box with the ink rule and the small hard shadow, a VT323 20 px label, 30 px
/// tall. Hover fills it with the FEMME pink, pressed turns it neon, drops the shadow and moves it 2 px
/// down and right. Attach `.on_click(..)`; override size with `.h(..)`, `.flex_1()`. The default
/// action of a row of buttons; [`button_dark`] is the main action or the selected tab.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    control(Kind::Light, id).child(label.into())
}

/// An unlabelled [`button`]: the same box with the same shadow, hover and pressed looks, for content a
/// plain label cannot hold: the `femme` wordmark with a [`Glyph::Down`] chevron in the top bar, a
/// name next to an icon. Add the content with `.child(..)`; children sit in a row, centred, 6 px
/// apart (give a child its own `.font_family(..)`). For an "open" look override the fill:
/// `.bg(theme::accent_strong())`.
pub fn button_box(id: impl Into<ElementId>) -> Stateful<Div> {
    control(Kind::Light, id)
}

/// `button.dark`: [`button`] inverted: ink fill, paper label. The main action ("send") and the
/// selected tab. Hover fills it deep pink (the label stays paper), pressed turns it neon.
pub fn button_dark(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    control(Kind::Dark, id).child(label.into())
}

/// A [`button`] that cannot be used: grey label and edge, no shadow, no hover, a not-allowed cursor.
/// Swap it in for [`button`] while the action is unavailable; it takes no `.on_click(..)`.
pub fn button_disabled(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    apply(frame(Kind::Light), look(Kind::Light, State::Disabled))
        .id(id)
        .cursor_not_allowed()
        .child(label.into())
}

/// The glyphs [`icon`] draws. Each is one or two characters of text (no icon font): VT323 where it has
/// the character, JetBrains Mono for the arrows and shapes VT323 lacks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Glyph {
    /// `▸` a collapsed tree node, go in.
    Right,
    /// `▾` an expanded tree node, a dropdown.
    Down,
    /// `‹` back, close an overlay.
    Back,
    /// `›` forward, next.
    Forward,
    /// `×` close, remove an attachment.
    Close,
    /// `+` new, add.
    Plus,
    /// `↓` jump to the newest, download.
    Download,
    /// `⌂` the session's home directory.
    Home,
    /// `>_` the terminal tab of the right strip.
    Terminal,
    /// `≡` the files tab of the right strip.
    Files,
    /// `◈` the drawing board button of the right strip: a diamond around a dot, the flow chart's node.
    /// The bundled fonts have no pencil, and a plain `◇` is too thin next to `>_` and `≡`.
    Board,
}

impl Glyph {
    /// Every glyph, in the order the style sheet shows them.
    pub const ALL: [Glyph; 11] = [
        Glyph::Right,
        Glyph::Down,
        Glyph::Back,
        Glyph::Forward,
        Glyph::Close,
        Glyph::Plus,
        Glyph::Download,
        Glyph::Home,
        Glyph::Terminal,
        Glyph::Files,
        Glyph::Board,
    ];

    /// The glyph's name, for tooltips and the style sheet.
    pub fn name(self) -> &'static str {
        match self {
            Glyph::Right => "right",
            Glyph::Down => "down",
            Glyph::Back => "back",
            Glyph::Forward => "forward",
            Glyph::Close => "close",
            Glyph::Plus => "plus",
            Glyph::Download => "download",
            Glyph::Home => "home",
            Glyph::Terminal => "terminal",
            Glyph::Files => "files",
            Glyph::Board => "board",
        }
    }

    /// How the glyph is drawn: see [`GlyphSpec`]. Sizes were picked so the ink of every glyph is 9 to 14
    /// px tall; JetBrains Mono's hairline outlines are set bold to match VT323's heavy pixels.
    fn spec(self) -> GlyphSpec {
        let (text, font, bold, size, dy) = match self {
            Glyph::Right => ("▸", FONT_MONO, true, 28., 0.),
            Glyph::Down => ("▾", FONT_MONO, true, 28., -0.5),
            Glyph::Back => ("‹", FONT_PIXEL, false, 32., -3.),
            Glyph::Forward => ("›", FONT_PIXEL, false, 32., -3.),
            Glyph::Close => ("×", FONT_PIXEL, false, 26., 1.),
            Glyph::Plus => ("+", FONT_PIXEL, false, 28., -1.),
            Glyph::Download => ("↓", FONT_MONO, true, 17., -0.5),
            Glyph::Home => ("⌂", FONT_MONO, true, 20., -1.5),
            Glyph::Terminal => (">_", FONT_PIXEL, false, 24., -0.5),
            Glyph::Files => ("≡", FONT_MONO, true, 20., -0.5),
            Glyph::Board => ("◈", FONT_MONO, true, 20., 0.),
        };
        GlyphSpec {
            text,
            font,
            bold,
            size,
            dy,
        }
    }
}

/// How one [`Glyph`] is drawn.
struct GlyphSpec {
    /// The characters.
    text: &'static str,
    /// The font that has them.
    font: &'static str,
    /// Bold strokes.
    bold: bool,
    /// Font size in px.
    size: f32,
    /// Vertical nudge in px, positive down, that centres the ink in the cell: a font's line box is not
    /// the centre of its ink (measured per glyph from the font metrics, checked on the gallery).
    dy: f32,
}

/// A [`Glyph`] in a 20 px cell. It takes the text colour of whatever it sits in, so it follows a
/// button's hover/selected label colour and a title bar's lettering.
pub fn icon(glyph: Glyph) -> Div {
    let g = glyph.spec();
    let cell = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(20.))
        .font_family(g.font)
        .text_size(px(g.size))
        .line_height(relative(1.));
    let cell = if g.bold {
        cell.font_weight(FontWeight::BOLD)
    } else {
        cell
    };
    cell.child(div().flex_none().relative().top(px(g.dy)).child(g.text))
}

/// A 22 px square glyph button: no fill or edge at rest, pink under the pointer, neon pressed. For
/// strips, title bars and row actions. [`icon_button_on`] is the selected one.
pub fn icon_button(id: impl Into<ElementId>, glyph: Glyph) -> Stateful<Div> {
    control(Kind::Ghost, id).child(icon(glyph))
}

/// The selected [`icon_button`] (the tab whose panel is open): ink fill, paper glyph, deep pink on hover.
pub fn icon_button_on(id: impl Into<ElementId>, glyph: Glyph) -> Stateful<Div> {
    control(Kind::GhostOn, id).child(icon(glyph))
}

/// A [`icon_button`] for a [`title_bar`]'s right-hand slot (close, back): a paper glyph on the ink bar,
/// deep pink under the pointer, neon pressed.
pub fn title_button(id: impl Into<ElementId>, glyph: Glyph) -> Stateful<Div> {
    control(Kind::OnInk, id).child(icon(glyph))
}

/// The 28 px vertical hover strip for the right edge: panel grey with the ink rule, [`icon_button`]s
/// stacked top to bottom (add them with `.child(..)`). It fills the height of its row, washes soft pink
/// while the pointer is on it (so it reads as live) and each icon goes full pink under the pointer;
/// attach `.on_hover(..)` to open the panel it fronts.
pub fn strip(id: impl Into<ElementId>) -> Stateful<Div> {
    ruled(
        div()
            .id(id)
            .flex()
            .flex_col()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .w(STRIP_W)
            .py(px(4.))
            .bg(panel())
            .hover(|s| s.bg(accent_soft())),
    )
}

// ---------------------------------------------------------------------------------------------
// Marks
// ---------------------------------------------------------------------------------------------

/// A small bordered tag: 22 px tall, panel grey, ink rule, body text 13 px. A file name, a model, an
/// attachment (add an [`icon_button`] with `.child(..)` for a removable one). Square like the rest of
/// the skin; cap its width with `.max_w(..)`, the label truncates.
pub fn chip(text: impl Into<SharedString>) -> Div {
    ruled(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .h(px(22.))
            .px(px(8.))
            .overflow_hidden()
            .bg(panel())
            .text_color(ink())
            .font_family(FONT_UI)
            .text_size(px(13.))
            .line_height(relative(1.))
            .child(div().min_w_0().truncate().child(text.into())),
    )
}

/// `.st`: a status pill: a solid status-colour fill, ink rule, VT323 18 px label in whichever of ink and
/// paper reads better on that fill. `work 4`, `blocked`, `failed`.
pub fn st(label: impl Into<SharedString>, color: Rgba) -> Div {
    ruled(
        div()
            .px(px(6.))
            .py(px(1.))
            .bg(color)
            .text_color(theme::text_on(color))
            .font_family(FONT_PIXEL)
            .text_size(px(18.))
            .line_height(relative(1.))
            .child(label.into()),
    )
}

/// `.dot`: an 8 px square status marker with the ink rule, filled with the status colour (a session
/// row, a connection). Working is [`theme::neon`], blocked [`theme::warn`], failed [`theme::danger`], idle
/// [`theme::queue`].
pub fn dot(color: Rgba) -> Div {
    ruled(div().flex_none().size(px(8.)).bg(color))
}

/// `.meta`/`.mini`: small muted mono text, one line, ellipsis.
pub fn meta(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(FONT_MONO)
        .text_size(theme::META_SIZE)
        .text_color(muted())
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .child(text.into())
}

// ---------------------------------------------------------------------------------------------
// The desk
// ---------------------------------------------------------------------------------------------

/// Logical size of one dot-grid tile: a multiple of the board's 6px period.
const DOT_TILE: f32 = 192.;

/// The desk: pink with a 1px ink dot every 6px (the board's `body` background), or near-black with
/// a dim pink dot in the dark palette. gpui has no tiled backgrounds, so a pre-rendered 192px tile is
/// painted as a grid of sprites (about forty at 1440x900). It fills the window behind the shell;
/// panels cover it, so it shows only in the [`theme::GUTTER`]s.
// ponytail: the tile density (1x or 2x) is picked once from the window's scale factor at creation;
// a window dragged between displays of different density just shows a slightly soft or fat dot.
#[derive(Clone)]
pub struct DotGrid {
    /// The embedded tile (pink with ink dots).
    light: Arc<RenderImage>,
    /// The same dots recoloured for the dark desk, made once alongside it.
    dark: Arc<RenderImage>,
}

impl DotGrid {
    /// Decodes the embedded tile that matches `scale` (the window's scale factor), and derives the
    /// dark one from it.
    pub fn new(scale: f32, cx: &App) -> Self {
        let bytes: &[u8] = if scale > 1.5 {
            include_bytes!("../assets/dots-2x.png")
        } else {
            include_bytes!("../assets/dots-1x.png")
        };
        let light = Image::from_bytes(ImageFormat::Png, bytes.to_vec())
            .to_image_data(cx.svg_renderer())
            .expect("embedded dot tile is a valid PNG");
        let dark = dark_tile(&light);
        Self { light, dark }
    }
}

/// The dark desk's tile: the light tile's dots in [`theme::DARK`]'s dot colour on its desk colour.
fn dark_tile(light: &RenderImage) -> Arc<RenderImage> {
    let size = light.size(0);
    let mut bgra = light.as_bytes(0).expect("the tile has a frame").to_vec();
    recolour(&mut bgra, &theme::DARK);
    let buffer = image::ImageBuffer::from_raw(size.width.0 as u32, size.height.0 as u32, bgra)
        .expect("the buffer keeps the tile's size");
    Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
}

/// Recolours a BGRA tile of two flat colours, ink dots (dark pixels) on a pink desk (light ones), as
/// the dots and the desk of `p`.
fn recolour(bgra: &mut [u8], p: &Palette) {
    let bgra_of = |c: Rgba| {
        let byte = |v: f32| (v * 255.).round() as u8;
        [byte(c.b), byte(c.g), byte(c.r), 0xFF]
    };
    let (desk, dot) = (bgra_of(p.desk), bgra_of(p.desk_dot));
    for px in bgra.as_chunks_mut::<4>().0 {
        *px = if px[2] < 0x80 { dot } else { desk };
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
        window.paint_quad(fill(bounds, desk()));
        let tile_image = if theme::is_dark() {
            &self.dark
        } else {
            &self.light
        };
        let t = px(DOT_TILE);
        let mut y = bounds.origin.y;
        while y < bounds.bottom() {
            let mut x = bounds.origin.x;
            while x < bounds.right() {
                let tile = Bounds::new(point(x, y), size(t, t));
                let _ = window.paint_image(
                    tile,
                    tile,
                    Corners::default(),
                    tile_image.clone(),
                    0,
                    false,
                );
                x += t;
            }
            y += t;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{contrast, DARK, LIGHT};

    const KINDS: [Kind; 5] = [
        Kind::Light,
        Kind::Dark,
        Kind::Ghost,
        Kind::GhostOn,
        Kind::OnInk,
    ];
    const STATES: [State; 4] = [State::Idle, State::Hover, State::Pressed, State::Disabled];
    /// Both palettes with their names, for the failure messages. The tests read the palettes
    /// directly and never flip the process-wide theme (they run in parallel).
    const BOTH: [(&str, &Palette); 2] = [("light", &LIGHT), ("dark", &DARK)];

    /// The colour a control's fill shows when the fill is transparent: the window it sits on.
    fn solid(p: &Palette, kind: Kind, bg: Rgba) -> Rgba {
        match (bg.a < 1., kind) {
            (false, _) => bg,
            (true, Kind::OnInk) => p.bar,
            (true, _) => p.paper,
        }
    }

    /// Every label a user is meant to read (every state but disabled) is AA (4.5:1) on its fill:
    /// this is the check that a pink hover never leaves white text at 3.5:1, and that the pressed
    /// neon keeps a dark label in the dark palette.
    #[test]
    fn every_label_is_readable_on_its_fill() {
        for (mode, p) in BOTH {
            for kind in KINDS {
                for state in STATES {
                    if state == State::Disabled {
                        continue;
                    }
                    let l = look_in(p, kind, state);
                    let c = contrast(l.fg, solid(p, kind, l.bg));
                    assert!(
                        c >= 4.5,
                        "{mode} {kind:?} {state:?}: {c:.2}:1 is below 4.5:1"
                    );
                }
            }
        }
    }

    /// Hover recolours the fill only (see the module docs): the table never changes the label.
    #[test]
    fn hover_keeps_the_label_colour() {
        for (mode, p) in BOTH {
            for kind in KINDS {
                assert_eq!(
                    look_in(p, kind, State::Hover).fg,
                    look_in(p, kind, State::Idle).fg,
                    "{mode} {kind:?}"
                );
            }
        }
    }

    /// Pressed is neon with the dark label for every kind; only a button box moves.
    #[test]
    fn pressed_is_neon_for_every_kind() {
        for (mode, p) in BOTH {
            for kind in KINDS {
                let l = look_in(p, kind, State::Pressed);
                assert_eq!(l.bg, p.accent_strong, "{mode} {kind:?}");
                assert_eq!(l.fg, p.on_accent_strong, "{mode} {kind:?}");
                assert!(!l.raised, "{mode} {kind:?} drops its shadow");
                assert_eq!(
                    l.pushed,
                    matches!(kind, Kind::Light | Kind::Dark),
                    "{mode} {kind:?}"
                );
            }
        }
    }

    /// A disabled control has no shadow and does not move.
    #[test]
    fn disabled_is_flat() {
        for (mode, p) in BOTH {
            for kind in KINDS {
                let l = look_in(p, kind, State::Disabled);
                assert!(!l.raised && !l.pushed, "{mode} {kind:?}");
                assert_eq!(l.fg, p.muted_text, "{mode} {kind:?}");
            }
        }
    }

    /// The live look is the live palette's (the light one: no test flips the theme).
    #[test]
    fn the_live_look_follows_the_live_palette() {
        for kind in KINDS {
            for state in STATES {
                let (live, light) = (look(kind, state), look_in(&LIGHT, kind, state));
                assert_eq!(
                    (live.bg, live.fg, live.edge),
                    (light.bg, light.fg, light.edge)
                );
            }
        }
    }

    /// `recolour` assumes the embedded tiles are two flat colours: pink desk, ink dots.
    #[test]
    fn the_embedded_tiles_are_a_desk_and_its_dots() {
        let want =
            std::collections::BTreeSet::from([[0xF3, 0x86, 0xA1, 0xFF], [0x1E, 0x1E, 0x1E, 0xFF]]);
        for bytes in [
            include_bytes!("../assets/dots-1x.png").as_slice(),
            include_bytes!("../assets/dots-2x.png").as_slice(),
        ] {
            let rgba = image::load_from_memory(bytes).expect("a PNG").into_rgba8();
            let seen: std::collections::BTreeSet<[u8; 4]> = rgba.pixels().map(|p| p.0).collect();
            assert_eq!(seen, want);
        }
    }

    /// The dark tile keeps every dot where it was and has only the dark desk's two colours.
    #[test]
    fn recolour_keeps_the_dots_where_they_were() {
        // BGRA: a dot pixel, a desk pixel, a dot pixel.
        let mut bgra = vec![
            0x1E, 0x1E, 0x1E, 0xFF, 0xA1, 0x86, 0xF3, 0xFF, 0x1E, 0x1E, 0x1E, 0xFF,
        ];
        recolour(&mut bgra, &DARK);
        let bgra_of = |c: Rgba| {
            let byte = |v: f32| (v * 255.).round() as u8;
            [byte(c.b), byte(c.g), byte(c.r), 0xFF]
        };
        assert_eq!(bgra[0..4], bgra_of(DARK.desk_dot));
        assert_eq!(bgra[4..8], bgra_of(DARK.desk));
        assert_eq!(bgra[8..12], bgra_of(DARK.desk_dot));
    }

    #[test]
    fn glyphs_have_distinct_names_and_text() {
        let mut names: Vec<_> = Glyph::ALL.iter().map(|g| g.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Glyph::ALL.len());
        for g in Glyph::ALL {
            let s = g.spec();
            assert!(!s.text.is_empty() && s.size > 0., "{}", g.name());
        }
    }
}
