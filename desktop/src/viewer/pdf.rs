//! PDF files. hayro (pure Rust: no pdfium, no system libraries) draws the pages, lazily: only the
//! pages in view and one beyond are rendered, at the view's width times the screen's scale, and the
//! bitmaps of pages that have scrolled away are dropped again.

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    div, img, px, AnyElement, Context, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, Render, RenderImage, ScrollHandle, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Subscription, Window,
};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::{LoadPdfError, Pdf};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};

use super::bitmap::{bgra_image, MAX_SIDE};
use super::chrome;
use super::guarded;
use super::hex::human_size;
use crate::theme::{PANEL, PAPER};
use crate::ui;

/// Space above the first page, between pages and below the last (logical pixels).
const GAP: f32 = 12.;
/// The narrowest a page is drawn, however small the view.
const MIN_PAGE_WIDTH: f32 = 96.;
/// The page width assumed before the view has been laid out once.
const DEFAULT_WIDTH: f32 = 640.;
/// How many pages are drawn before the view's size is known.
const FIRST_PAGES: usize = 3;
/// Pages beyond the visible ones that are drawn ahead of the scroll.
const AHEAD: usize = 1;
/// Pages beyond the visible ones whose bitmaps are kept; further ones are dropped.
const KEEP: usize = 2;
/// Pages drawn at the same time.
const MAX_INFLIGHT: usize = 2;
/// Bitmap widths are rounded up to a multiple of this, so a slow resize does not redraw every frame.
const WIDTH_STEP: u32 = 64;
/// No page bitmap has more pixels than this.
const MAX_PAGE_PIXELS: f32 = 40_000_000.;

/// Where each page's slot begins in the scrolled column: the page `width` wide, then [`GAP`] blank.
/// `starts[i]` is the top of slot `i` and the last entry is the height of the whole column.
fn slot_starts(pages: &[(f32, f32)], width: f32) -> Vec<f32> {
    let mut starts = Vec::with_capacity(pages.len() + 1);
    let mut y = GAP;
    starts.push(y);
    for &page in pages {
        y += page_height(width, page) + GAP;
        starts.push(y);
    }
    starts
}

/// The height of a page of `size` points drawn `width` wide.
fn page_height(width: f32, size: (f32, f32)) -> f32 {
    width * size.1 / size.0.max(1.)
}

/// The pages whose slots meet the window `[top, top + height]` of the column: at least one page.
fn pages_in_window(starts: &[f32], top: f32, height: f32) -> Range<usize> {
    let n = starts.len().saturating_sub(1);
    if n == 0 {
        return 0..0;
    }
    let first = starts[1..].partition_point(|&end| end <= top).min(n - 1);
    let last = starts[..n]
        .partition_point(|&start| start < top + height)
        .clamp(first + 1, n);
    first..last
}

/// `range` made `by` pages wider on both sides, within `n` pages.
fn widen(range: Range<usize>, by: usize, n: usize) -> Range<usize> {
    range.start.saturating_sub(by)..(range.end + by).min(n)
}

/// The page whose slot holds height `y` of the column.
fn page_at(starts: &[f32], y: f32) -> usize {
    let n = starts.len().saturating_sub(1);
    starts
        .get(1..)
        .map_or(0, |ends| ends.partition_point(|&end| end <= y))
        .min(n.saturating_sub(1))
}

/// The bitmap width to draw a page `display` logical pixels wide on a screen of `scale`.
fn pixel_width(display: f32, scale: f32) -> u32 {
    let px = (display * scale.max(1.)).ceil().max(1.) as u32;
    (px.div_ceil(WIDTH_STEP) * WIDTH_STEP).min(MAX_SIDE)
}

/// Whether a bitmap drawn for `have` pixels of width is too soft or too big for `want`.
fn needs_redraw(have: u32, want: u32) -> bool {
    have * 20 < want * 19 || have * 2 > want * 3
}

/// The scale at which a page of `page` points comes out `want_width` pixels wide, held back by the
/// side and pixel caps.
fn page_scale(page: (f32, f32), want_width: u32) -> f32 {
    let (w, h) = (page.0.max(1.), page.1.max(1.));
    let side = MAX_SIDE as f32;
    let scale = (want_width as f32 / w).min(side / w).min(side / h);
    let pixels = w * h * scale * scale;
    if pixels > MAX_PAGE_PIXELS {
        scale * (MAX_PAGE_PIXELS / pixels).sqrt()
    } else {
        scale
    }
}

/// An opened document: the reader and the size of every page.
struct Opened {
    pdf: Arc<Pdf>,
    pages: Vec<(f32, f32)>,
}

/// Reads the page tree. Encrypted files are refused, not asked for a password.
fn open(data: Arc<Vec<u8>>) -> Result<Opened, String> {
    let pdf = Pdf::new(data).map_err(|e| match e {
        LoadPdfError::Decryption(_) => {
            "this PDF is encrypted; passwords are not supported".to_string()
        }
        LoadPdfError::Invalid => "this file is not a PDF the reader can open".to_string(),
    })?;
    let pages: Vec<(f32, f32)> = pdf.pages().iter().map(|p| p.render_dimensions()).collect();
    if pages.is_empty() {
        return Err("this PDF has no pages".to_string());
    }
    Ok(Opened {
        pdf: Arc::new(pdf),
        pages,
    })
}

/// One drawn page: straight RGBA, opaque (the page is drawn over white).
struct Drawn {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Draws page `index` about `want_width` pixels wide.
fn draw_page(pdf: &Pdf, index: usize, want_width: u32) -> Result<Drawn, String> {
    let page = pdf
        .pages()
        .get(index)
        .ok_or_else(|| format!("page {} is missing", index + 1))?;
    let scale = page_scale(page.render_dimensions(), want_width);
    let cache = RenderCache::new();
    let pixmap = hayro::render(
        page,
        &cache,
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        },
    );
    Ok(Drawn {
        width: pixmap.width() as u32,
        height: pixmap.height() as u32,
        rgba: pixmap.data_as_u8_slice().to_vec(),
    })
}

/// A page's bitmap and the width it was drawn for.
struct Bitmap {
    image: Arc<RenderImage>,
    wanted: u32,
}

/// One page of the document.
struct Slot {
    bitmap: Option<Bitmap>,
    /// A render of this page is running.
    busy: bool,
    /// Why the page could not be drawn.
    failed: Option<String>,
}

/// The pages of an opened document.
struct Doc {
    pdf: Arc<Pdf>,
    /// Page sizes in points, after rotation.
    sizes: Vec<(f32, f32)>,
    slots: Vec<Slot>,
    inflight: usize,
}

/// Where the opening stands.
enum State {
    Opening,
    Failed(String),
    Ready(Doc),
}

/// A PDF file: its pages in a vertical scroll, with `page N / M` in the caption.
pub(crate) struct PdfView {
    bytes: usize,
    state: State,
    scroll: ScrollHandle,
    _release: Subscription,
}

impl PdfView {
    /// Starts reading `data` in the background.
    pub(crate) fn new(data: Arc<Vec<u8>>, cx: &mut Context<Self>) -> Self {
        let bytes = data.len();
        let work = cx
            .background_executor()
            .spawn(async move { guarded("the PDF reader", || open(data)) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(opened) => State::Ready(Doc {
                        pdf: opened.pdf,
                        slots: opened
                            .pages
                            .iter()
                            .map(|_| Slot {
                                bitmap: None,
                                busy: false,
                                failed: None,
                            })
                            .collect(),
                        sizes: opened.pages,
                        inflight: 0,
                    }),
                    Err(why) => State::Failed(why),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        let release = cx.on_release(|this, cx| {
            // The window's texture atlas keeps a page bitmap until told to let go.
            if let State::Ready(doc) = &this.state {
                for bitmap in doc.slots.iter().filter_map(|s| s.bitmap.as_ref()) {
                    cx.drop_image(bitmap.image.clone(), None);
                }
            }
        });
        PdfView {
            bytes,
            state: State::Opening,
            scroll: ScrollHandle::new(),
            _release: release,
        }
    }

    /// Draws page `index` in the background and stores the bitmap when it is done.
    fn start_page(&mut self, pdf: Arc<Pdf>, index: usize, want: u32, cx: &mut Context<Self>) {
        let work = cx
            .background_executor()
            .spawn(async move { guarded("the PDF renderer", || draw_page(&pdf, index, want)) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| this.page_done(index, want, result, cx))
                .ok();
        })
        .detach();
    }

    /// Stores a drawn page and asks for a frame, which starts the next missing page.
    fn page_done(
        &mut self,
        index: usize,
        want: u32,
        result: Result<Drawn, String>,
        cx: &mut Context<Self>,
    ) {
        let State::Ready(doc) = &mut self.state else {
            return;
        };
        doc.inflight = doc.inflight.saturating_sub(1);
        let slot = &mut doc.slots[index];
        slot.busy = false;
        let image = result.and_then(|d| {
            bgra_image(d.width, d.height, d.rgba).ok_or_else(|| "bad page bitmap".to_string())
        });
        match image {
            Ok(image) => {
                if let Some(old) = slot.bitmap.replace(Bitmap {
                    image,
                    wanted: want,
                }) {
                    cx.drop_image(old.image, None);
                }
            }
            Err(why) => slot.failed = Some(why),
        }
        cx.notify();
    }

    /// The scrolled column of pages. Works out which pages are in view from the scroll position of
    /// the previous frame, starts the missing ones and drops the ones far away.
    fn pages(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let viewport = self.scroll.bounds().size;
        let top = -f32::from(self.scroll.offset().y);
        let known = viewport.height > px(1.);
        let width = if viewport.width > px(1.) {
            (f32::from(viewport.width) - 2. * GAP).max(MIN_PAGE_WIDTH)
        } else {
            DEFAULT_WIDTH
        };
        let want = pixel_width(width, window.scale_factor());
        let size_text = human_size(self.bytes);
        let State::Ready(doc) = &mut self.state else {
            return div().into_any_element();
        };

        let n = doc.slots.len();
        let starts = slot_starts(&doc.sizes, width);
        let (shown, current) = if known {
            let height = f32::from(viewport.height);
            (
                pages_in_window(&starts, top, height),
                page_at(&starts, top + height / 2.),
            )
        } else {
            (0..FIRST_PAGES.min(n), 0)
        };

        let kept = widen(shown.clone(), KEEP, n);
        for (i, slot) in doc.slots.iter_mut().enumerate() {
            if !kept.contains(&i) {
                if let Some(bitmap) = slot.bitmap.take() {
                    let _ = window.drop_image(bitmap.image);
                }
            }
        }

        let ahead = widen(shown.clone(), AHEAD, n);
        let mut jobs = Vec::new();
        for i in shown.clone().chain(ahead.filter(|i| !shown.contains(i))) {
            if doc.inflight + jobs.len() >= MAX_INFLIGHT {
                break;
            }
            let slot = &mut doc.slots[i];
            let current_bitmap = slot
                .bitmap
                .as_ref()
                .is_some_and(|b| !needs_redraw(b.wanted, want));
            if !slot.busy && slot.failed.is_none() && !current_bitmap {
                slot.busy = true;
                jobs.push(i);
            }
        }
        doc.inflight += jobs.len();

        let mut column = div().flex().flex_col().w_full();
        column = column.child(div().flex_none().h(px(starts[shown.start])));
        for i in shown.clone() {
            let height = page_height(width, doc.sizes[i]);
            let slot = &doc.slots[i];
            let body = match (&slot.bitmap, &slot.failed) {
                (Some(b), _) => img(b.image.clone())
                    .size_full()
                    .object_fit(ObjectFit::Fill)
                    .into_any_element(),
                (None, Some(why)) => div()
                    .p(px(8.))
                    .child(ui::meta(format!("page {} failed: {why}", i + 1)))
                    .into_any_element(),
                (None, None) => chrome::spinner(format!("page {}", i + 1)).into_any_element(),
            };
            column = column.child(
                div()
                    .flex()
                    .flex_none()
                    .justify_center()
                    .w_full()
                    .h(px(height + GAP))
                    .child(
                        ui::ruled(
                            div()
                                .flex_none()
                                .w(px(width))
                                .h(px(height))
                                .bg(PAPER)
                                .overflow_hidden(),
                        )
                        .child(body),
                    ),
            );
        }
        column = column.child(div().flex_none().h(px(starts[n] - starts[shown.end])));

        let pdf = doc.pdf.clone();
        for i in jobs {
            self.start_page(pdf.clone(), i, want, cx);
        }
        chrome::framed(
            div()
                .id("pdf-pages")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(column),
            format!("page {} / {n} · {size_text}", current + 1),
            PANEL,
        )
        .into_any_element()
    }
}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.state {
            State::Opening => {
                chrome::framed(chrome::spinner("opening"), human_size(self.bytes), PANEL)
                    .into_any_element()
            }
            State::Failed(why) => chrome::framed(
                chrome::notice(
                    ("viewer-notice", cx.entity_id()),
                    "cannot show this PDF",
                    vec![why.clone()],
                ),
                human_size(self.bytes),
                PANEL,
            )
            .into_any_element(),
            State::Ready(_) => self.pages(window, cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-page PDF, 200 x 100 points, with "Hello" in Helvetica; the offsets of its cross-reference
    /// table are computed, not typed.
    fn hello_pdf() -> Vec<u8> {
        let content = "BT /F1 36 Tf 20 40 Td (Hello) Tj ET";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_string(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n{body}\nendobj\n", i + 1).bytes());
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for off in offsets {
            pdf.extend(format!("{off:010} 00000 n \n").bytes());
        }
        pdf.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        pdf
    }

    #[test]
    fn a_hello_pdf_renders_to_a_pixmap_of_the_expected_size() {
        let opened = open(Arc::new(hello_pdf())).unwrap();
        assert_eq!(opened.pages, vec![(200., 100.)]);

        let page = draw_page(&opened.pdf, 0, 400).unwrap();
        assert_eq!((page.width, page.height), (400, 200));
        assert_eq!(page.rgba.len(), 400 * 200 * 4);
        let pixel = |x: usize, y: usize| &page.rgba[(y * 400 + x) * 4..][..4];
        assert_eq!(pixel(2, 2), &[255, 255, 255, 255], "paper corner");
        assert!(
            page.rgba.as_chunks::<4>().0.iter().any(|p| p[0] < 128),
            "the text drew dark pixels"
        );
    }

    #[test]
    fn broken_and_missing_pdfs_fail_with_a_reason() {
        let err = |data: &[u8]| open(Arc::new(data.to_vec())).err().unwrap();
        assert!(err(b"%PDF-1.4\nnothing else").contains("not a PDF"));
        assert!(err(b"").contains("not a PDF"));
        let opened = open(Arc::new(hello_pdf())).unwrap();
        assert!(draw_page(&opened.pdf, 7, 100).is_err());
    }

    #[test]
    fn page_scale_respects_the_caps() {
        assert_eq!(page_scale((200., 100.), 400), 2.);
        // A very tall page is held to MAX_SIDE high, not wide.
        let tall = page_scale((100., 10_000.), 800);
        assert!((tall * 10_000. - MAX_SIDE as f32).abs() < 1.);
        // A big page is held to the pixel budget.
        let big = page_scale((8_000., 8_000.), 8_000);
        assert!(8_000. * big * 8_000. * big <= MAX_PAGE_PIXELS * 1.001);
    }

    #[test]
    fn bitmap_widths_are_stepped_and_redrawn_only_when_off() {
        assert_eq!(pixel_width(600., 1.), 640);
        assert_eq!(pixel_width(600., 2.), 1216);
        assert_eq!(pixel_width(100_000., 2.), MAX_SIDE);
        assert!(!needs_redraw(640, 640));
        assert!(!needs_redraw(640, 620), "a bit big is fine");
        assert!(needs_redraw(640, 1280), "too soft");
        assert!(needs_redraw(1280, 640), "too big");
    }

    /// Three pages 100 x 200 points drawn 100 wide: 200 high each, with 12 gaps.
    fn three() -> Vec<f32> {
        slot_starts(&[(100., 200.); 3], 100.)
    }

    #[test]
    fn slots_stack_with_gaps() {
        assert_eq!(three(), vec![12., 224., 436., 648.]);
        assert_eq!(page_height(100., (100., 200.)), 200.);
        assert_eq!(slot_starts(&[], 100.), vec![GAP]);
    }

    #[test]
    fn the_window_picks_the_pages_it_meets() {
        let s = three();
        assert_eq!(pages_in_window(&s, 0., 100.), 0..1);
        assert_eq!(pages_in_window(&s, 0., 300.), 0..2);
        assert_eq!(
            pages_in_window(&s, 200., 100.),
            0..2,
            "straddles the first gap"
        );
        assert_eq!(pages_in_window(&s, 224., 100.), 1..2);
        assert_eq!(pages_in_window(&s, 300., 2000.), 1..3);
        assert_eq!(
            pages_in_window(&s, 5000., 100.),
            2..3,
            "past the end shows the last page"
        );
        assert_eq!(pages_in_window(&[GAP], 0., 100.), 0..0);
    }

    #[test]
    fn widening_stays_inside_the_document() {
        assert_eq!(widen(0..1, 1, 10), 0..2);
        assert_eq!(widen(4..6, 2, 10), 2..8);
        assert_eq!(widen(8..10, 3, 10), 5..10);
    }

    #[test]
    fn the_current_page_is_the_one_under_a_height() {
        let s = three();
        assert_eq!(page_at(&s, 0.), 0);
        assert_eq!(page_at(&s, 223.), 0);
        assert_eq!(page_at(&s, 224.), 1);
        assert_eq!(page_at(&s, 5000.), 2);
        assert_eq!(page_at(&[GAP], 10.), 0);
    }
}
