//! Raster images and SVG: decoded off the UI thread, shown fitted on the panel colour, one click for
//! 1:1. Also the one place a rendered buffer becomes a gpui image (`bgra_image`), which the PDF pages
//! use too.

use std::fmt::Write as _;
use std::io::Cursor;
use std::sync::Arc;

use gpui::{
    div, img, point, px, size, App, ClickEvent, Context, DevicePixels, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement as _, Render, RenderImage, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, SvgRenderer,
    SvgSize, Task, Window,
};
use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, Frame, ImageBuffer, ImageDecoder, ImageReader};

use super::chrome;
use super::guarded;
use super::hex::human_size;
use super::kind::{Kind, Raster};
use crate::theme::PANEL;

/// The longest side a decoded raster keeps; bigger pictures are scaled down while decoding.
pub(crate) const MAX_SIDE: u32 = 8192;

/// An animation keeps at most this many frames...
const MAX_FRAMES: usize = 400;
/// ...and at most this many bytes of decoded frames.
const MAX_ANIMATION_BYTES: usize = 256 << 20;

/// An SVG is drawn with its longest side at about this many pixels, so that a 24px icon still has a
/// sharp edge when it is fitted into a big panel...
const SVG_TARGET_SIDE: f32 = 2048.;
/// ...unless gpui's own pass (twice the document size) is already this big.
const SVG_ENOUGH_SIDE: u32 = 1536;

/// A decoded picture, ready to paint.
pub(crate) struct Decoded {
    /// What gpui paints: BGRA frames.
    pub(crate) image: Arc<RenderImage>,
    /// The picture's own size in pixels (for an SVG, the document's size).
    pub(crate) source: (u32, u32),
    /// The size of the bitmap that is painted.
    pub(crate) shown: (u32, u32),
    /// Animation frames; 1 for a still.
    pub(crate) frames: usize,
    /// A vector picture: its natural size is the document's, not the bitmap's.
    pub(crate) vector: bool,
}

/// Wraps an RGBA buffer as the BGRA image gpui paints (it takes straight, not premultiplied, alpha).
pub(crate) fn bgra_image(width: u32, height: u32, mut rgba: Vec<u8>) -> Option<Arc<RenderImage>> {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let buffer = ImageBuffer::from_raw(width, height, rgba)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

/// Decodes a raster file. Animated GIF and WebP keep their frames; anything with a side over
/// [`MAX_SIDE`] is scaled down to it; the EXIF orientation is applied.
pub(crate) fn decode_raster(data: &[u8], raster: Raster) -> Result<Decoded, String> {
    if let Some(mut frames) = animation(data, raster) {
        let (w, h) = frames[0].buffer().dimensions();
        for frame in &mut frames {
            for px in frame.buffer_mut().as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
        }
        let count = frames.len();
        return Ok(Decoded {
            image: Arc::new(RenderImage::new(frames)),
            source: (w, h),
            shown: (w, h),
            frames: count,
            vector: false,
        });
    }
    let fail = |e: image::ImageError| format!("could not decode the {}: {e}", raster.name());
    let mut decoder = ImageReader::with_format(Cursor::new(data), raster.format())
        .into_decoder()
        .map_err(fail)?;
    let orientation = decoder.orientation().map_err(fail)?;
    let mut picture = DynamicImage::from_decoder(decoder).map_err(fail)?;
    picture.apply_orientation(orientation);
    let source = (picture.width(), picture.height());
    if source.0.max(source.1) > MAX_SIDE {
        picture = picture.resize(MAX_SIDE, MAX_SIDE, FilterType::Triangle);
    }
    let shown = (picture.width(), picture.height());
    let image = bgra_image(shown.0, shown.1, picture.into_rgba8().into_raw())
        .ok_or_else(|| "decoded buffer does not match its size".to_string())?;
    Ok(Decoded {
        image,
        source,
        shown,
        frames: 1,
        vector: false,
    })
}

/// The frames of an animated GIF or WebP; `None` for a still (or an animation too big to keep, which
/// is then shown as its first frame).
fn animation(data: &[u8], raster: Raster) -> Option<Vec<Frame>> {
    let frames = match raster {
        Raster::Gif => collect_frames(GifDecoder::new(Cursor::new(data)).ok()?)?,
        Raster::Webp => {
            let decoder = WebPDecoder::new(Cursor::new(data)).ok()?;
            if !decoder.has_animation() {
                return None;
            }
            collect_frames(decoder)?
        }
        _ => return None,
    };
    (frames.len() > 1).then_some(frames)
}

/// Frames up to the caps; `None` if the canvas itself is over [`MAX_SIDE`].
fn collect_frames<'a, D: ImageDecoder + AnimationDecoder<'a>>(decoder: D) -> Option<Vec<Frame>> {
    let (w, h) = decoder.dimensions();
    if w.max(h) > MAX_SIDE {
        return None;
    }
    let mut frames = Vec::new();
    let mut bytes = 0usize;
    for frame in decoder.into_frames() {
        let Ok(frame) = frame else { break };
        bytes += frame.buffer().as_raw().len();
        frames.push(frame);
        if frames.len() >= MAX_FRAMES || bytes >= MAX_ANIMATION_BYTES {
            break;
        }
    }
    Some(frames)
}

/// Draws an SVG with gpui's own renderer (resvg, with the system fonts).
pub(crate) fn decode_svg(data: &[u8], renderer: &SvgRenderer) -> Result<Decoded, String> {
    let svg = renderer
        .parse_svg(data)
        .map_err(|e| format!("could not read the SVG: {e}"))?;
    let draw = |size: SvgSize| {
        renderer
            .render_parsed(&svg, size)
            .map_err(|e| format!("could not draw the SVG: {e}"))
    };
    // gpui's pass at scale 1 is drawn at twice the document size: that tells how big the document is.
    let probe = draw(SvgSize::ScaleFactor(1.))?;
    let probe_size = probe.size(0);
    let (pw, ph) = (probe_size.width.0 as u32, probe_size.height.0 as u32);
    let source = (pw.div_ceil(2), ph.div_ceil(2));
    if pw.max(ph) >= SVG_ENOUGH_SIDE {
        return Ok(Decoded {
            image: probe,
            source,
            shown: (pw, ph),
            frames: 1,
            vector: true,
        });
    }
    let k = SVG_TARGET_SIDE / pw.max(ph).max(1) as f32;
    let shown = (
        ((pw as f32 * k).round() as u32).max(1),
        ((ph as f32 * k).round() as u32).max(1),
    );
    let image = draw(SvgSize::ExactSize(size(
        DevicePixels(shown.0 as i32),
        DevicePixels(shown.1 as i32),
    )))?;
    Ok(Decoded {
        image,
        source,
        shown,
        frames: 1,
        vector: true,
    })
}

/// Logical size of the picture at 1:1: one image pixel per device pixel for a raster, the
/// document's own size for an SVG.
fn natural_size(d: &Decoded, scale_factor: f32) -> (f32, f32) {
    if d.vector {
        (d.source.0 as f32, d.source.1 as f32)
    } else {
        let s = scale_factor.max(0.5);
        (d.shown.0 as f32 / s, d.shown.1 as f32 / s)
    }
}

/// `1920×1080`, `12000×9000 (shown at 8192×6144)`, `64×64 · 12 frames`.
fn dimensions_text(d: &Decoded) -> String {
    let mut text = format!("{}×{}", d.source.0, d.source.1);
    if !d.vector && d.shown != d.source {
        let _ = write!(text, " (shown at {}×{})", d.shown.0, d.shown.1);
    }
    if d.frames > 1 {
        let _ = write!(text, " · {} frames", d.frames);
    }
    text
}

/// Where the decode stands.
enum State {
    Loading,
    Failed(String),
    Ready(Decoded),
}

/// A raster or SVG file: spinner while it decodes, then the picture fitted into the view; a click
/// switches between fitted and 1:1 (scrolling inside when it is bigger than the view).
pub(crate) struct ImageView {
    bytes: usize,
    state: State,
    one_to_one: bool,
    scroll: ScrollHandle,
    _load: Task<()>,
    _release: Subscription,
}

impl ImageView {
    /// Starts decoding `data` (a raster of the given kind, or an SVG) in the background.
    pub(crate) fn new(kind: Kind, data: Arc<Vec<u8>>, cx: &mut Context<Self>) -> Self {
        let renderer = cx.svg_renderer();
        let bytes = data.len();
        let work = cx.background_executor().spawn(async move {
            guarded("the image decoder", || match kind {
                Kind::Raster(raster) => decode_raster(&data, raster),
                Kind::Svg => decode_svg(&data, &renderer),
                _ => Err("not a picture".to_string()),
            })
        });
        let load = cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(decoded) => State::Ready(decoded),
                    Err(why) => State::Failed(why),
                };
                cx.notify();
            })
            .ok();
        });
        let release = cx.on_release(|this, cx: &mut App| {
            // The window's texture atlas keeps a decoded picture until told to let go.
            if let State::Ready(d) = &this.state {
                cx.drop_image(d.image.clone(), None);
            }
        });
        ImageView {
            bytes,
            state: State::Loading,
            one_to_one: false,
            scroll: ScrollHandle::new(),
            _load: load,
            _release: release,
        }
    }

    /// The picture, fitted or at 1:1, as a click target.
    fn picture(&self, d: &Decoded, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let toggle = cx.listener(|this, _: &ClickEvent, _, cx| {
            this.one_to_one = !this.one_to_one;
            this.scroll.set_offset(point(px(0.), px(0.)));
            cx.notify();
        });
        if self.one_to_one {
            let (w, h) = natural_size(d, window.scale_factor());
            div()
                .id("picture-1to1")
                .size_full()
                .overflow_scroll()
                .track_scroll(&self.scroll)
                .cursor_pointer()
                .on_click(toggle)
                .child(
                    div()
                        .flex()
                        .min_w_full()
                        .min_h_full()
                        .items_center()
                        .justify_center()
                        .child(img(d.image.clone()).id("pic").flex_none().w(px(w)).h(px(h))),
                )
                .into_any_element()
        } else {
            div()
                .id("picture-fit")
                .size_full()
                .p(px(8.))
                .cursor_pointer()
                .on_click(toggle)
                .child(
                    img(d.image.clone())
                        .id("pic")
                        .object_fit(ObjectFit::Contain)
                        .size_full(),
                )
                .into_any_element()
        }
    }
}

impl Render for ImageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let size_text = human_size(self.bytes);
        match &self.state {
            State::Loading => chrome::framed(chrome::spinner("decoding"), size_text, PANEL),
            State::Failed(why) => chrome::framed(
                chrome::notice(
                    ("viewer-notice", cx.entity_id()),
                    "cannot show this picture",
                    vec![why.clone()],
                ),
                size_text,
                PANEL,
            ),
            State::Ready(d) => {
                let mode = if self.one_to_one {
                    "1:1 · click to fit"
                } else {
                    "fit · click for 1:1"
                };
                let caption: SharedString =
                    format!("{} · {size_text} · {mode}", dimensions_text(d)).into();
                chrome::framed(self.picture(d, window, cx), caption, PANEL)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, ImageFormat, Rgba, RgbaImage};

    /// A PNG of `w`x`h` pixels, every pixel the same colour.
    fn png(w: u32, h: u32, pixel: [u8; 4]) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, Rgba(pixel));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn a_two_by_two_png_decodes_to_bgra() {
        let mut img = RgbaImage::new(2, 2);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, Rgba([0, 255, 0, 255]));
        img.put_pixel(0, 1, Rgba([0, 0, 255, 255]));
        img.put_pixel(1, 1, Rgba([10, 20, 30, 128]));
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();

        let d = decode_raster(&bytes, Raster::Png).unwrap();
        assert_eq!((d.source, d.shown, d.frames), ((2, 2), (2, 2), 1));
        assert!(!d.vector);
        let px = d.image.as_bytes(0).unwrap();
        assert_eq!(px.len(), 16);
        assert_eq!(&px[0..4], &[0, 0, 255, 255], "red as BGRA");
        assert_eq!(&px[4..8], &[0, 255, 0, 255], "green as BGRA");
        assert_eq!(&px[8..12], &[255, 0, 0, 255], "blue as BGRA");
        assert_eq!(&px[12..16], &[30, 20, 10, 128], "straight alpha is kept");
    }

    #[test]
    fn pictures_over_the_cap_are_scaled_down_and_keep_their_real_size() {
        let d = decode_raster(&png(9000, 10, [1, 2, 3, 255]), Raster::Png).unwrap();
        assert_eq!(d.source, (9000, 10));
        assert_eq!(d.shown.0, MAX_SIDE);
        assert!(d.shown.1 >= 8 && d.shown.1 <= 10, "{:?}", d.shown);
        assert!(dimensions_text(&d).starts_with("9000×10 (shown at 8192×"));
    }

    #[test]
    fn an_animated_gif_keeps_its_frames() {
        let mut bytes = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut bytes);
            let frame = |c: u8| {
                Frame::from_parts(
                    RgbaImage::from_pixel(3, 2, Rgba([c, 0, 0, 255])),
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                )
            };
            enc.encode_frames([frame(10), frame(200)]).unwrap();
        }
        let d = decode_raster(&bytes, Raster::Gif).unwrap();
        assert_eq!((d.shown, d.frames), ((3, 2), 2));
        assert_eq!(dimensions_text(&d), "3×2 · 2 frames");
    }

    #[test]
    fn garbage_fails_with_a_reason() {
        let err = decode_raster(b"\x89PNG\r\n\x1a\nnot really", Raster::Png)
            .err()
            .unwrap();
        assert!(err.starts_with("could not decode the PNG"), "{err}");
    }

    #[test]
    fn one_to_one_uses_device_pixels_for_rasters_and_the_document_size_for_svg() {
        let image = bgra_image(1, 1, vec![0; 4]).unwrap();
        let raster = Decoded {
            image: image.clone(),
            source: (400, 200),
            shown: (400, 200),
            frames: 1,
            vector: false,
        };
        assert_eq!(natural_size(&raster, 2.), (200., 100.));
        assert_eq!(natural_size(&raster, 1.), (400., 200.));
        let vector = Decoded {
            image,
            source: (24, 12),
            shown: (2048, 1024),
            frames: 1,
            vector: true,
        };
        assert_eq!(natural_size(&vector, 2.), (24., 12.));
    }

    #[test]
    fn a_small_svg_is_drawn_big_and_reports_its_document_size() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="12">
            <rect width="24" height="12" fill="#f00"/></svg>"##;
        let renderer = SvgRenderer::new(Arc::new(()));
        let d = decode_svg(svg, &renderer).unwrap();
        assert_eq!(d.source, (24, 12));
        assert_eq!(d.shown, (2048, 1024));
        assert!(d.vector);
        let px = d.image.as_bytes(0).unwrap();
        assert_eq!(&px[0..4], &[0, 0, 255, 255], "red, as BGRA");
    }

    #[test]
    fn a_broken_svg_fails_with_a_reason() {
        let renderer = SvgRenderer::new(Arc::new(()));
        let err = decode_svg(b"<svg", &renderer).err().unwrap();
        assert!(err.starts_with("could not read the SVG"), "{err}");
    }
}
