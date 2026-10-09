//! A pane's decoded images, on the main thread: the pixels behind the
//! placeholder cells `graphics.rs` wrote, and how each placement of an image
//! sits in its box of cells. Decoding runs on the background executor; until
//! it lands (or if it fails) the cells draw as blank.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Cursor, Read, Seek};
use std::path::Path;
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::{Term, TermMode};
use gpui::RenderImage;
use image::metadata::Orientation;
use image::{DynamicImage, Frame, ImageDecoder, ImageReader, RgbImage, RgbaImage, imageops};

use super::graphics::{Fit, ImageData, Placement};
use super::placeholder::{self, ImageCell};
use super::scan::MAX_ENCODED;

/// Images arrive from whatever is printing, a remote host included. Larger
/// than this is refused before a pixel is decoded.
pub const MAX_DIMENSION: u32 = 10_000;
pub const MAX_PIXELS: u64 = 64_000_000;
/// The long edge of what is kept. Nothing in a terminal needs more, and it
/// stays well inside the GPU atlas's texture limit.
const MAX_STORED_EDGE: u32 = 4096;

pub fn size_allowed((width, height): (u32, u32)) -> bool {
    (1..=MAX_DIMENSION).contains(&width)
        && (1..=MAX_DIMENSION).contains(&height)
        && width as u64 * height as u64 <= MAX_PIXELS
}

/// Read an encoded image's header: a decoder ready to produce the pixels,
/// and how they must be turned to stand upright (EXIF).
fn open<'a>(source: impl BufRead + Seek + 'a) -> Option<(impl ImageDecoder + 'a, Orientation)> {
    let mut reader = ImageReader::new(source).with_guessed_format().ok()?;
    // What terminal programs send. The image crate reads a dozen more
    // formats; each is a decoder a hostile stream could otherwise reach.
    use image::ImageFormat::{Bmp, Gif, Jpeg, Png, WebP};
    if !matches!(reader.format()?, Png | Jpeg | Gif | WebP | Bmp) {
        return None;
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    Some((decoder, orientation))
}

/// The size an encoded image decodes to, from its header alone. `None` for
/// anything unreadable or too large to accept.
pub fn sniff(bytes: &[u8]) -> Option<(u32, u32)> {
    sniff_from(Cursor::new(bytes))
}

/// The size of an image file OmniPTY can draw, by its extension and then its
/// header; `None` for any other file.
pub fn file_size(path: &Path) -> Option<(u32, u32)> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if !["png", "jpg", "jpeg", "gif", "webp", "bmp"].contains(&extension.as_str()) {
        return None;
    }
    sniff_from(BufReader::new(std::fs::File::open(path).ok()?))
}

fn sniff_from(source: impl BufRead + Seek) -> Option<(u32, u32)> {
    let (decoder, orientation) = open(source)?;
    let (width, height) = decoder.dimensions();
    let size = match orientation {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => (height, width),
        _ => (width, height),
    };
    size_allowed(size).then_some(size)
}

/// Inflate at most `limit` bytes. A truncated or corrupt stream yields what
/// came before the damage.
pub fn inflate(bytes: &[u8], limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = flate2::read::ZlibDecoder::new(bytes)
        .take(limit as u64)
        .read_to_end(&mut out);
    out
}

/// Decode to what the renderer draws: BGRA, no larger than
/// `MAX_STORED_EDGE`. `size` is what `sniff` (or the client, for raw pixels)
/// said the image is. Hostile or broken input gives `None`, never a panic.
pub fn decode(data: ImageData, size: (u32, u32)) -> Option<Arc<RenderImage>> {
    if !size_allowed(size) {
        return None;
    }
    let mut pixels: RgbaImage = match data {
        ImageData::Encoded { bytes, zlib } => {
            let bytes = if zlib {
                inflate(&bytes, MAX_ENCODED)
            } else {
                bytes
            };
            let (decoder, orientation) = open(Cursor::new(&bytes))?;
            if !size_allowed(decoder.dimensions()) {
                return None;
            }
            let mut image = DynamicImage::from_decoder(decoder).ok()?;
            image.apply_orientation(orientation);
            image.into_rgba8()
        }
        ImageData::Raw { bytes, alpha, zlib } => {
            let (width, height) = size;
            let len = width as usize * height as usize * if alpha { 4 } else { 3 };
            let mut bytes = if zlib { inflate(&bytes, len) } else { bytes };
            if bytes.len() < len {
                return None;
            }
            bytes.truncate(len);
            if alpha {
                RgbaImage::from_raw(width, height, bytes)?
            } else {
                DynamicImage::ImageRgb8(RgbImage::from_raw(width, height, bytes)?).into_rgba8()
            }
        }
    };
    let long_edge = pixels.width().max(pixels.height());
    if long_edge > MAX_STORED_EDGE {
        let scale = MAX_STORED_EDGE as f32 / long_edge as f32;
        let scaled = |edge: u32| ((edge as f32 * scale).round() as u32).max(1);
        pixels = imageops::resize(
            &pixels,
            scaled(pixels.width()),
            scaled(pixels.height()),
            imageops::FilterType::Triangle,
        );
    }
    // GPUI's sprite atlas takes BGRA.
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![Frame::new(pixels)])))
}

/// Where a placement's image goes, relative to the top-left corner of its
/// box and in the units of `cell` (one cell now, in device pixels): first
/// the rectangle the *whole* image is scaled into, then the part of the box
/// that may show it. With a source crop the first is larger than the second,
/// and clipping to the second does the cropping.
pub fn layout(spec: &Placement, image: (u32, u32), cell: (f32, f32)) -> ([f32; 4], [f32; 4]) {
    let [crop_x, crop_y, crop_w, crop_h] = spec
        .crop
        .unwrap_or([0, 0, image.0, image.1])
        .map(|v| v as f32);
    let (x, y, w, h) = match spec.fit {
        Fit::Exact(w, h) => {
            // Pixel figures were for the cell size at placement.
            let (zoom_x, zoom_y) = (cell.0 / spec.cell.0, cell.1 / spec.cell.1);
            (
                spec.offset.0 as f32 * zoom_x,
                spec.offset.1 as f32 * zoom_y,
                w * zoom_x,
                h * zoom_y,
            )
        }
        Fit::Contain => {
            let (box_w, box_h) = (spec.cols as f32 * cell.0, spec.rows as f32 * cell.1);
            let scale = (box_w / crop_w).min(box_h / crop_h);
            let (w, h) = (crop_w * scale, crop_h * scale);
            ((box_w - w) / 2.0, (box_h - h) / 2.0, w, h)
        }
    };
    let (scale_x, scale_y) = (w / crop_w, h / crop_h);
    let whole = [
        x - crop_x * scale_x,
        y - crop_y * scale_y,
        image.0 as f32 * scale_x,
        image.1 as f32 * scale_y,
    ];
    (whole, [x, y, w, h])
}

/// The low 24 bits of every image id with a cell still in the grid, history
/// included. `None` on the alternate screen: the shell's grid is then
/// alacritty's private inactive one, and reading only this one would write
/// off every picture in the scrollback the moment vim opens.
pub fn live_ids<L>(term: &Term<L>) -> Option<HashSet<u32>> {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return None;
    }
    let grid = term.grid();
    let mut live = HashSet::new();
    for line in term.topmost_line().0..=term.bottommost_line().0 {
        for cell in &grid[Line(line)][..] {
            if let Some(cell) = placeholder::decode(cell.c, None, cell.fg, None, None) {
                live.insert(cell.image);
            }
        }
    }
    Some(live)
}

enum State {
    Pending,
    Ready(Arc<RenderImage>),
    Failed,
}

struct Stored {
    state: State,
    /// The size the image was placed at; the stored pixels may be fewer.
    size: (u32, u32),
    /// Decoded bytes held.
    bytes: usize,
    /// The frame it was last drawn in, for eviction.
    drawn: u64,
    /// Which transmission of this id it is: a decode that finishes after
    /// the id was reused must not overwrite the newer image.
    generation: u64,
}

#[derive(Default)]
pub struct ImageStore {
    images: HashMap<u32, Stored>,
    placements: HashMap<(u32, u32), Placement>,
    bytes: usize,
    frame: u64,
    generation: u64,
}

impl ImageStore {
    /// An image is on its way: forget whatever had the id. Returns the
    /// generation to hand back to `finish`, and the old pixels, whose atlas
    /// tile the caller releases.
    pub fn begin(&mut self, id: u32, size: (u32, u32)) -> (u64, Option<Arc<RenderImage>>) {
        let old = self.free(id);
        self.generation += 1;
        self.images.insert(
            id,
            Stored {
                state: State::Pending,
                size,
                bytes: 0,
                drawn: self.frame,
                generation: self.generation,
            },
        );
        (self.generation, old)
    }

    /// A decode finished. Ignored if the image was replaced or freed in the
    /// meantime.
    pub fn finish(&mut self, id: u32, generation: u64, image: Option<Arc<RenderImage>>) {
        let Some(stored) = self.images.get_mut(&id) else {
            return;
        };
        if stored.generation != generation {
            return;
        }
        match image {
            Some(image) => {
                stored.bytes = image.as_bytes(0).map_or(0, <[u8]>::len);
                self.bytes += stored.bytes;
                stored.state = State::Ready(image);
            }
            None => stored.state = State::Failed,
        }
    }

    pub fn place(&mut self, image: u32, placement: u32, spec: Placement) {
        self.placements.insert((image, placement), spec);
    }

    /// Stop drawing a placement, or with `None` every placement of `image`.
    pub fn delete(&mut self, image: u32, placement: Option<u32>) {
        match placement {
            Some(placement) => {
                self.placements.remove(&(image, placement));
            }
            None => self.placements.retain(|&(id, _), _| id != image),
        }
    }

    /// Drop an image and its placements. Returns its pixels, if it had any,
    /// for the caller to release from the atlas.
    pub fn free(&mut self, image: u32) -> Option<Arc<RenderImage>> {
        self.delete(image, None);
        let stored = self.images.remove(&image)?;
        self.bytes -= stored.bytes;
        match stored.state {
            State::Ready(image) => Some(image),
            _ => None,
        }
    }

    /// Everything, for a pane that is closing or restarting its shell.
    pub fn clear(&mut self) -> Vec<Arc<RenderImage>> {
        let ids: Vec<u32> = self.images.keys().copied().collect();
        ids.into_iter().filter_map(|id| self.free(id)).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// A new frame is being laid out; images drawn in it count as recent.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// What a placeholder cell draws: the pixels, the image's nominal size,
    /// and its placement. `None` while the image is still decoding, if it
    /// failed, or once it or the placement is gone.
    pub fn lookup(
        &mut self,
        cell: &ImageCell,
    ) -> Option<(Arc<RenderImage>, (u32, u32), Placement)> {
        let stored = self.images.get_mut(&cell.image)?;
        let State::Ready(image) = &stored.state else {
            return None;
        };
        let spec = match self.placements.get(&(cell.image, cell.placement)) {
            Some(spec) => *spec,
            // A cell that names no placement stands for any one of the
            // image's.
            None if cell.placement == 0 => {
                *self
                    .placements
                    .iter()
                    .find(|((image, _), _)| *image == cell.image)?
                    .1
            }
            None => return None,
        };
        stored.drawn = self.frame;
        Some((image.clone(), stored.size, spec))
    }

    /// Bring the store back under `limit` bytes. First anything whose cells
    /// are gone from the grid (`live` holds the low 24 bits of every id
    /// still there; `None` when the grid couldn't be read), then the least
    /// recently drawn. Returns the ids dropped and the pixels to release.
    pub fn evict(
        &mut self,
        limit: usize,
        live: Option<&HashSet<u32>>,
    ) -> (Vec<u32>, Vec<Arc<RenderImage>>) {
        let mut ids: Vec<u32> = Vec::new();
        if let Some(live) = live {
            // Only images that were placed: one a program has sent and not
            // yet shown has no cells to find, and isn't dead for it.
            let placed: HashSet<u32> = self.placements.keys().map(|&(id, _)| id).collect();
            ids.extend(
                placed
                    .into_iter()
                    .filter(|id| !live.contains(&(id & 0xFF_FFFF))),
            );
        }
        let mut pixels: Vec<_> = ids.iter().filter_map(|&id| self.free(id)).collect();
        while self.bytes > limit {
            let Some(oldest) = self
                .images
                .iter()
                .filter(|(_, stored)| stored.bytes > 0)
                .min_by_key(|(_, stored)| stored.drawn)
                .map(|(&id, _)| id)
            else {
                break;
            };
            pixels.extend(self.free(oldest));
            ids.push(oldest);
        }
        (ids, pixels)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            width,
            height,
            image::Rgba([255, 0, 0, 255]),
        ))
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
        bytes
    }

    fn spec(cols: u32, rows: u32, fit: Fit) -> Placement {
        Placement {
            cols,
            rows,
            fit,
            crop: None,
            offset: (0, 0),
            cell: (10.0, 20.0),
        }
    }

    #[test]
    fn decode_gives_bgra_and_refuses_what_it_should() {
        let image = decode(
            ImageData::Encoded {
                bytes: png(3, 2),
                zlib: false,
            },
            (3, 2),
        )
        .unwrap();
        // Red, with the channels swapped for the atlas.
        assert_eq!(&image.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
        assert_eq!(image.as_bytes(0).unwrap().len(), 3 * 2 * 4);

        // Raw RGB, deflated: one green pixel.
        let mut deflated = Vec::new();
        flate2::read::ZlibEncoder::new(&[0u8, 255, 0][..], flate2::Compression::fast())
            .read_to_end(&mut deflated)
            .unwrap();
        let raw = ImageData::Raw {
            bytes: deflated,
            alpha: false,
            zlib: true,
        };
        let image = decode(raw, (1, 1)).unwrap();
        assert_eq!(image.as_bytes(0).unwrap(), &[0, 255, 0, 255]);

        // Too few bytes, garbage, and a size past the limits: `None`.
        let short = ImageData::Raw {
            bytes: vec![0; 5],
            alpha: true,
            zlib: false,
        };
        assert!(decode(short, (2, 2)).is_none());
        let junk = ImageData::Encoded {
            bytes: b"not an image".to_vec(),
            zlib: false,
        };
        assert!(decode(junk, (1, 1)).is_none());
        let mut truncated = png(40, 40);
        truncated.truncate(60);
        assert_eq!(sniff(&truncated), Some((40, 40)), "the header is intact");
        let truncated = ImageData::Encoded {
            bytes: truncated,
            zlib: false,
        };
        assert!(decode(truncated, (40, 40)).is_none());
        assert!(!size_allowed((MAX_DIMENSION + 1, 1)));
        assert!(!size_allowed((9000, 9000)));
        assert!(!size_allowed((0, 10)));
    }

    #[test]
    fn oversized_images_are_stored_downscaled() {
        let wide = ImageData::Raw {
            bytes: vec![0; 8192 * 4 * 3],
            alpha: false,
            zlib: false,
        };
        let image = decode(wide, (8192, 4)).unwrap();
        let size = image.size(0);
        assert_eq!((size.width.0, size.height.0), (4096, 2));
    }

    #[test]
    fn layout_places_the_image_in_its_box() {
        let cell = (10.0, 20.0);
        // Natural size sits at the top-left corner.
        let natural = spec(5, 2, Fit::Exact(45.0, 30.0));
        assert_eq!(
            layout(&natural, (45, 30), cell),
            ([0.0, 0.0, 45.0, 30.0], [0.0, 0.0, 45.0, 30.0])
        );
        // A wide image contained in a square box is centred vertically.
        let contain = spec(10, 5, Fit::Contain);
        assert_eq!(
            layout(&contain, (200, 100), cell).0,
            [0.0, 25.0, 100.0, 50.0]
        );
        // A crop: the right half of the image fills what the left half
        // would have, so the whole image starts one width to the left.
        let mut cropped = spec(5, 2, Fit::Exact(50.0, 40.0));
        cropped.crop = Some([100, 0, 100, 80]);
        assert_eq!(
            layout(&cropped, (200, 80), cell),
            ([-50.0, 0.0, 100.0, 40.0], [0.0, 0.0, 50.0, 40.0])
        );
        // The font doubled since the image was placed: so does the image.
        assert_eq!(
            layout(&natural, (45, 30), (20.0, 40.0)).0,
            [0.0, 0.0, 90.0, 60.0]
        );
    }

    fn ready(store: &mut ImageStore, id: u32, side: u32) {
        let (generation, _) = store.begin(id, (side, side));
        let pixels = ImageData::Raw {
            bytes: vec![0; (side * side * 4) as usize],
            alpha: true,
            zlib: false,
        };
        store.finish(id, generation, decode(pixels, (side, side)));
    }

    fn cell(image: u32, placement: u32) -> ImageCell {
        ImageCell {
            image,
            placement,
            row: 0,
            col: 0,
        }
    }

    #[test]
    fn cells_find_their_placement_until_it_is_deleted() {
        let mut store = ImageStore::default();
        let natural = spec(1, 1, Fit::Contain);
        let (generation, _) = store.begin(1, (4, 4));
        store.place(1, 9, natural);
        assert!(store.lookup(&cell(1, 9)).is_none(), "still decoding");
        store.finish(1, generation, None);
        assert!(store.lookup(&cell(1, 9)).is_none(), "failed to decode");

        ready(&mut store, 1, 4);
        assert!(
            store.lookup(&cell(1, 9)).is_none(),
            "re-sent: placements go"
        );
        store.place(1, 9, natural);
        assert_eq!(store.lookup(&cell(1, 9)).unwrap().1, (4, 4));
        // A cell with no placement id takes any placement; a wrong id none.
        assert!(store.lookup(&cell(1, 0)).is_some());
        assert!(store.lookup(&cell(1, 8)).is_none());
        store.delete(1, Some(9));
        assert!(store.lookup(&cell(1, 9)).is_none());
        assert!(store.lookup(&cell(1, 0)).is_none());
    }

    #[test]
    fn a_stale_decode_does_not_overwrite_a_newer_image() {
        let mut store = ImageStore::default();
        let (old, _) = store.begin(1, (2, 2));
        ready(&mut store, 1, 4);
        let late = ImageData::Raw {
            bytes: vec![0; 16],
            alpha: true,
            zlib: false,
        };
        store.finish(1, old, decode(late, (2, 2)));
        assert_eq!(store.bytes(), 4 * 4 * 4);
    }

    #[test]
    fn eviction_takes_dead_images_first_then_the_least_recently_drawn() {
        let mut store = ImageStore::default();
        for id in 1..=4 {
            ready(&mut store, id, 10);
            store.place(id, 1, spec(1, 1, Fit::Contain));
        }
        assert_eq!(store.bytes(), 4 * 400);
        // 2 was drawn most recently, 3 before it; 1 and 4 never.
        store.begin_frame();
        store.lookup(&cell(3, 1));
        store.begin_frame();
        store.lookup(&cell(2, 1));

        // Under the limit, only what has left the grid goes: 4, but not 5,
        // which was sent and is yet to be placed.
        ready(&mut store, 5, 1);
        let live = HashSet::from([1, 2, 3]);
        let (ids, pixels) = store.evict(usize::MAX, Some(&live));
        assert_eq!((ids, pixels.len()), (vec![4], 1));
        store.free(5);
        // Over it, the least recently drawn follow until it fits.
        let (ids, _) = store.evict(450, None);
        assert_eq!(ids, vec![1, 3]);
        assert_eq!(store.bytes(), 400);
        assert!(store.lookup(&cell(2, 1)).is_some());
        assert_eq!(store.clear().len(), 1);
        assert!(store.is_empty());
        assert_eq!(store.bytes(), 0);
    }
}
