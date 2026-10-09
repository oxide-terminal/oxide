//! Image protocols, on the PTY thread: what a kitty, iTerm2 or sixel sequence
//! asks for, which image and placement ids exist, and how many cells a
//! picture takes. An image is put on screen by writing placeholder cells (see
//! `placeholder.rs`) into the `Term`, right where the parser would have put
//! text; the pixels go to the main thread, which decodes and draws them
//! (`images.rs`).
//!
//! Nothing is decoded here beyond an image's header (sixel aside, which the
//! scanner decodes as it streams in). The size has to be known the moment
//! the image is placed, because it decides how many cells are written and
//! where the cursor ends up.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{self, Handler, NamedMode};
use base64::Engine;

use super::event_loop::Outputs;
use super::images;
use super::placeholder::{self, DIACRITICS, PLACEHOLDER};
use super::scan::{GraphicsCommand, MAX_ENCODED, Reply};
use super::sixel::Sixel;

/// What the main thread's `ImageStore` is told.
#[derive(Debug)]
pub enum GraphicsEvent {
    /// An image arrived. It replaces whatever had this id, placements
    /// included. `size` is what it decodes to, in pixels.
    Image {
        id: u32,
        size: (u32, u32),
        data: ImageData,
        /// Held until the image is decoded (or dropped undecoded).
        reservation: Reservation,
    },
    /// How one placement of an image is drawn in its box of cells.
    Place {
        image: u32,
        placement: u32,
        spec: Placement,
    },
    /// A placement (`None`: every placement of the image) stops drawing.
    /// Its cells stay in the grid, since they're text now, and read as blank.
    Delete { image: u32, placement: Option<u32> },
    /// The image's pixels can go.
    Free { image: u32 },
}

/// A share of the decode budget, given back when it is dropped. Images are
/// small on the wire and large decoded (a few bytes of sixel can declare a
/// 64-megapixel canvas), and the PTY thread can send them far faster than
/// they decode; without a budget a hostile stream could queue gigabytes.
#[derive(Debug)]
pub struct Reservation(Arc<AtomicUsize>, usize);

impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.fetch_sub(self.1, Ordering::Relaxed);
    }
}

/// Bytes of images sent for decoding and not yet done: room for the largest
/// image there can be, twice. Past it, new images are refused.
const MAX_PENDING: usize = 512 << 20;

#[derive(Debug)]
pub enum ImageData {
    /// A file in any format the `image` crate reads.
    Encoded { bytes: Vec<u8>, zlib: bool },
    /// Raw pixels, 3 bytes each or 4 with `alpha`: kitty's, or a decoded
    /// sixel.
    Raw {
        bytes: Vec<u8>,
        alpha: bool,
        zlib: bool,
    },
}

/// A box of cells and how an image sits in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub cols: u32,
    pub rows: u32,
    pub fit: Fit,
    /// The part of the image shown, `[x, y, w, h]` in image pixels; all of
    /// it when `None`.
    pub crop: Option<[u32; 4]>,
    /// Where the image starts inside the first cell, in pixels.
    pub offset: (u32, u32),
    /// The cell size the pixel figures here were worked out against.
    /// Drawing rescales by however the font has changed since, so a zoomed
    /// picture still fills its cells.
    pub cell: (f32, f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fit {
    /// Drawn this many pixels wide and high, from the box's top-left corner.
    Exact(f32, f32),
    /// Scaled to fit inside the box, centred.
    Contain,
}

/// Padding or not, and stray trailing bits: programs disagree on both.
const BASE64: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new()
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// How much of a compressed file is inflated to find its header.
const SNIFF_PREFIX: usize = 64 << 10;

struct Stored {
    size: (u32, u32),
    /// Its placements: the id the client gave (0 for none) and the id the
    /// cells carry.
    placements: Vec<(u32, u32)>,
}

pub struct GraphicsState {
    enabled: bool,
    /// One cell in device pixels.
    cell: (f32, f32),
    images: HashMap<u32, Stored>,
    /// kitty image numbers (`I=`) to the id last allocated for each.
    numbers: HashMap<u32, u32>,
    next_image: u32,
    next_placement: u32,
    /// Bytes reserved by images the main thread hasn't decoded yet.
    pending: Arc<AtomicUsize>,
    /// A kitty transmission still arriving in chunks: its first chunk's
    /// keys, and the bytes so far.
    loading: Option<(Keys, Vec<u8>)>,
    /// An iTerm2 multipart file still arriving: its arguments and base64.
    multipart: Option<(Vec<u8>, Vec<u8>)>,
}

impl GraphicsState {
    pub fn new(enabled: bool, cell: (f32, f32)) -> Self {
        Self {
            enabled,
            cell,
            images: HashMap::new(),
            numbers: HashMap::new(),
            next_image: u32::MAX,
            next_placement: 0xFF_FFFF,
            pending: Arc::default(),
            loading: None,
            multipart: None,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_cell(&mut self, cell: (f32, f32)) {
        self.cell = cell;
    }

    /// The main thread evicted these; a client that places one again is
    /// told it's gone and sends it afresh. ponytail: by id alone, so an id
    /// re-sent while this message was on its way is forgotten too and has
    /// to be sent once more; carry a transmission serial if that bites.
    pub fn forget(&mut self, ids: &[u32]) {
        for id in ids {
            self.images.remove(id);
        }
        self.numbers.retain(|_, id| !ids.contains(id));
    }

    pub fn handle<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        parser: &mut ansi::Processor,
        command: GraphicsCommand,
        out: &mut Outputs,
    ) {
        // Off: the scanner has still kept the body away from the parser,
        // but nothing is drawn and no query is answered, so programs fall
        // back to text.
        if !self.enabled {
            return;
        }
        // Inside a synchronised update the bytes before this command are
        // still buffered in the parser, and the cursor is where it was when
        // the update began. Ending the update early costs at most one torn
        // frame; not ending it puts the picture in the wrong place.
        if parser.sync_bytes_count() > 0 {
            parser.stop_sync(term);
        }
        match command {
            GraphicsCommand::Kitty(body) => self.kitty(term, &body, out),
            GraphicsCommand::ItermFile(body) => {
                if let Some(colon) = body.iter().position(|&b| b == b':') {
                    self.iterm(term, &body[..colon], &body[colon + 1..], out);
                }
            }
            GraphicsCommand::ItermMultipart(args) => self.multipart = Some((args, Vec::new())),
            GraphicsCommand::ItermPart(part) => {
                if let Some((_, data)) = &mut self.multipart {
                    if data.len() + part.len() > MAX_ENCODED {
                        self.multipart = None;
                    } else {
                        data.extend_from_slice(&part);
                    }
                }
            }
            GraphicsCommand::ItermEnd => {
                if let Some((args, data)) = self.multipart.take() {
                    self.iterm(term, &args, &data, out);
                }
            }
            GraphicsCommand::Sixel(image) => self.sixel(term, image, out),
        }
    }

    /// Answer a query the scanner found. These go straight to the PTY from
    /// this thread, ahead of anything alacritty answers through the main one.
    pub fn reply<L>(&self, term: &Term<L>, reply: Reply, out: &mut Vec<u8>) {
        let (width, height) = self.cell;
        let pane_width = (term.columns() as f32 * width).round() as u32;
        let pane_height = (term.screen_lines() as f32 * height).round() as u32;
        let text = match reply {
            // Sixel is only owned up to while images are on.
            Reply::SixelColors | Reply::SixelGeometry if !self.enabled => return,
            Reply::SixelColors => "\x1b[?1;0;256S".to_string(),
            Reply::SixelGeometry => format!("\x1b[?2;0;{pane_width};{pane_height}S"),
            Reply::CellSize => {
                format!("\x1b[6;{};{}t", height.round() as u32, width.round() as u32)
            }
            Reply::TextAreaSize => format!("\x1b[4;{pane_height};{pane_width}t"),
            Reply::Version => format!("\x1bP>|OmniPTY {}\x1b\\", env!("CARGO_PKG_VERSION")),
        };
        out.extend_from_slice(text.as_bytes());
    }

    /// An id no client will pick: downward from the top, where the id's
    /// high byte is set (clients count up from 1).
    fn alloc_image(&mut self) -> u32 {
        loop {
            let id = self.next_image;
            self.next_image = if id <= 0x8000_0000 { u32::MAX } else { id - 1 };
            if !self.images.contains_key(&id) {
                return id;
            }
        }
    }

    /// The id a placement's cells carry, 24 bits like the colour that holds
    /// it. ponytail: wraps after eight million placements without checking
    /// for one still on screen; track live ids if that ever shows.
    fn alloc_placement(&mut self) -> u32 {
        let id = self.next_placement;
        self.next_placement = if id <= 0x80_0000 { 0xFF_FFFF } else { id - 1 };
        id
    }

    /// Hand an image to the main thread. `false` if the decode budget is
    /// spent, in which case the image is dropped and must not be placed.
    fn store(&mut self, id: u32, size: (u32, u32), data: ImageData, out: &mut Outputs) -> bool {
        let (ImageData::Encoded { bytes, .. } | ImageData::Raw { bytes, .. }) = &data;
        let cost = bytes.len() + size.0 as usize * size.1 as usize * 4;
        // This thread is the only one that adds.
        if self.pending.load(Ordering::Relaxed) + cost > MAX_PENDING {
            return false;
        }
        self.pending.fetch_add(cost, Ordering::Relaxed);
        let reservation = Reservation(self.pending.clone(), cost);
        self.images.insert(
            id,
            Stored {
                size,
                placements: Vec::new(),
            },
        );
        out.graphics(GraphicsEvent::Image {
            id,
            size,
            data,
            reservation,
        });
        true
    }

    /// Put `image` at the cursor: record the placement, tell the main
    /// thread how it's drawn, and write its cells. A client placement id
    /// that is already on screen is moved, which here means the old cells
    /// go blank.
    fn show<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        image: u32,
        client: u32,
        spec: Placement,
        move_cursor: bool,
        out: &mut Outputs,
    ) {
        let placement = self.alloc_placement();
        if let Some(stored) = self.images.get_mut(&image) {
            if client != 0
                && let Some(ix) = stored.placements.iter().position(|&(c, _)| c == client)
            {
                let (_, old) = stored.placements.swap_remove(ix);
                out.graphics(GraphicsEvent::Delete {
                    image,
                    placement: Some(old),
                });
            }
            stored.placements.push((client, placement));
        }
        out.graphics(GraphicsEvent::Place {
            image,
            placement,
            spec,
        });
        write_cells(term, image, placement, spec.cols, spec.rows, move_cursor);
    }

    // --- sixel: DCS q ---

    fn sixel<L: EventListener>(&mut self, term: &mut Term<L>, image: Sixel, out: &mut Outputs) {
        let size = (image.width, image.height);
        let (width, height) = (size.0 as f32, size.1 as f32);
        let spec = Placement {
            cols: cells(width, self.cell.0),
            rows: cells(height, self.cell.1),
            fit: Fit::Exact(width, height),
            crop: None,
            offset: (0, 0),
            cell: self.cell,
        };
        let data = ImageData::Raw {
            bytes: image.pixels,
            alpha: true,
            zlib: false,
        };
        let id = self.alloc_image();
        if !self.store(id, size, data, out) {
            return;
        }
        self.show(term, id, 0, spec, true, out);
        // xterm's rule with sixel scrolling on, its default: the cursor
        // goes to the start of the line below the picture.
        term.linefeed();
        term.carriage_return();
    }

    // --- iTerm2: OSC 1337 ---

    fn iterm<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        args: &[u8],
        encoded: &[u8],
        out: &mut Outputs,
    ) {
        let args = ItermArgs::parse(args);
        // `inline=0` is a download. An escape sequence doesn't get to write
        // files to disk here.
        if !args.inline {
            return;
        }
        let Ok(bytes) = BASE64.decode(encoded) else {
            return;
        };
        let Some(size) = images::sniff(&bytes) else {
            return;
        };
        let (cell_w, cell_h) = self.cell;
        let (image_w, image_h) = (size.0 as f32, size.1 as f32);
        let pane_w = term.columns() as f32 * cell_w;
        let pane_h = term.screen_lines() as f32 * cell_h;
        // What's left of the line; all of the next one when this one is
        // full and the image is about to wrap onto it.
        let cursor = &term.grid().cursor;
        let room = match cursor.input_needs_wrap && !args.keep_cursor {
            true => pane_w,
            false => (term.columns() - cursor.point.column.0) as f32 * cell_w,
        };
        let width = args.width.pixels(cell_w, pane_w);
        let height = args.height.pixels(cell_h, pane_h);
        let (box_w, box_h, fit) = match (width, height) {
            (Some(w), Some(h)) if args.preserve_aspect => (w, h, Fit::Contain),
            (Some(w), Some(h)) => (w, h, Fit::Exact(w, h)),
            (Some(w), None) => (
                w,
                w * image_h / image_w,
                Fit::Exact(w, w * image_h / image_w),
            ),
            (None, Some(h)) => (
                h * image_w / image_h,
                h,
                Fit::Exact(h * image_w / image_h, h),
            ),
            // Its own size, or as wide as there is room for: a photo
            // straight off a camera should fit the pane, not be cut by it.
            (None, None) => {
                let scale = (room / image_w).min(1.0);
                let (w, h) = (image_w * scale, image_h * scale);
                (w, h, Fit::Exact(w, h))
            }
        };
        let spec = Placement {
            cols: cells(box_w, cell_w),
            rows: cells(box_h, cell_h),
            fit,
            crop: None,
            offset: (0, 0),
            cell: self.cell,
        };
        let id = self.alloc_image();
        if self.store(id, size, ImageData::Encoded { bytes, zlib: false }, out) {
            self.show(term, id, 0, spec, !args.keep_cursor, out);
        }
    }

    // --- kitty: APC G ---

    fn kitty<L: EventListener>(&mut self, term: &mut Term<L>, body: &[u8], out: &mut Outputs) {
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(ix) => (&body[..ix], &body[ix + 1..]),
            None => (body, &[][..]),
        };
        let keys = Keys::parse(control);
        match keys.action {
            b't' | b'T' | b'q' => {
                // While a transmission is arriving, whatever comes next is
                // its next chunk: only the first carries the keys.
                let more = keys.more;
                let (first, mut data) = self.loading.take().unwrap_or((keys, Vec::new()));
                // Chunks are decoded one by one, as kitty does: each is
                // padded on its own.
                match BASE64.decode(payload) {
                    Ok(bytes) if data.len() + bytes.len() <= MAX_ENCODED => {
                        data.extend_from_slice(&bytes)
                    }
                    Ok(_) => return first.reply(first.id, "EFBIG:image too large", out),
                    Err(_) => return first.reply(first.id, "EINVAL:bad base64", out),
                }
                if more {
                    self.loading = Some((first, data));
                } else {
                    self.kitty_transmit(term, first, data, out);
                }
            }
            b'p' => {
                let id = match keys.number {
                    0 => keys.id,
                    number => self.numbers.get(&number).copied().unwrap_or(0),
                };
                self.kitty_put(term, &keys, id, out);
            }
            b'd' => self.kitty_delete(term, &keys, out),
            // Animation (f, a, c) isn't here yet.
            _ => keys.reply(keys.id, "EINVAL:unsupported action", out),
        }
    }

    fn kitty_transmit<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        keys: Keys,
        bytes: Vec<u8>,
        out: &mut Outputs,
    ) {
        if keys.id != 0 && keys.number != 0 {
            return keys.reply(keys.id, "EINVAL:both i and I given", out);
        }
        // Files and shared memory: a program asking for either is told no
        // and falls back to sending the pixels down the pipe.
        if keys.medium != b'd' {
            return keys.reply(keys.id, "EINVAL:only direct transmission is supported", out);
        }
        let zlib = keys.zlib;
        let (size, data) = match keys.format {
            100 => {
                let size = if zlib {
                    images::sniff(&images::inflate(&bytes, SNIFF_PREFIX))
                } else {
                    images::sniff(&bytes)
                };
                let Some(size) = size else {
                    return keys.reply(keys.id, "EBADPNG:not an image this terminal reads", out);
                };
                (size, ImageData::Encoded { bytes, zlib })
            }
            24 | 32 => {
                let size = (keys.width, keys.height);
                let alpha = keys.format == 32;
                if !images::size_allowed(size) {
                    return keys.reply(keys.id, "EINVAL:bad image size", out);
                }
                let needed = size.0 as usize * size.1 as usize * if alpha { 4 } else { 3 };
                if !zlib && bytes.len() < needed {
                    return keys.reply(keys.id, "ENODATA:not enough pixel data", out);
                }
                (size, ImageData::Raw { bytes, alpha, zlib })
            }
            _ => return keys.reply(keys.id, "EINVAL:unknown format", out),
        };
        if keys.action == b'q' {
            return keys.reply(keys.id, "OK", out);
        }
        let id = match keys.id {
            0 => {
                let id = self.alloc_image();
                if keys.number != 0 {
                    self.numbers.insert(keys.number, id);
                }
                id
            }
            id => id,
        };
        if !self.store(id, size, data, out) {
            return keys.reply(id, "ENOMEM:too many images waiting to be decoded", out);
        }
        if keys.action == b'T' {
            self.kitty_put(term, &keys, id, out);
        } else {
            keys.reply(id, "OK", out);
        }
    }

    fn kitty_put<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        keys: &Keys,
        id: u32,
        out: &mut Outputs,
    ) {
        let Some(stored) = self.images.get_mut(&id) else {
            return keys.reply(id, "ENOENT:no such image", out);
        };
        let (image_w, image_h) = stored.size;
        let x = keys.x.min(image_w);
        let y = keys.y.min(image_h);
        let w = match keys.w {
            0 => image_w - x,
            w => w.min(image_w - x),
        };
        let h = match keys.h {
            0 => image_h - y,
            h => h.min(image_h - y),
        };
        if w == 0 || h == 0 {
            return keys.reply(id, "EINVAL:empty source rectangle", out);
        }
        let crop = ((x, y, w, h) != (0, 0, image_w, image_h)).then_some([x, y, w, h]);
        let (cell_w, cell_h) = self.cell;
        let (w, h) = (w as f32, h as f32);
        let mut spec = Placement {
            cols: keys.cols,
            rows: keys.rows,
            fit: Fit::Contain,
            crop,
            offset: (keys.x_offset, keys.y_offset),
            cell: self.cell,
        };
        if keys.virtual_placement {
            // The client writes the placeholder cells itself, carrying its
            // own placement id; all there is to do is remember the box.
            if spec.cols == 0 {
                spec.cols = cells(w, cell_w);
            }
            if spec.rows == 0 {
                spec.rows = cells(h, cell_h);
            }
            // It replaces a placement of the same id: an earlier virtual
            // one silently, one with cells of its own by blanking them.
            let client = keys.placement;
            stored.placements.retain(|&(c, carried)| {
                let replaced = c == client && (client != 0 || carried == 0);
                if replaced && carried != client {
                    out.graphics(GraphicsEvent::Delete {
                        image: id,
                        placement: Some(carried),
                    });
                }
                !replaced
            });
            stored.placements.push((client, client));
            out.graphics(GraphicsEvent::Place {
                image: id,
                placement: keys.placement,
                spec,
            });
        } else {
            let (offset_x, offset_y) = (keys.x_offset as f32, keys.y_offset as f32);
            // One of `c`/`r` alone keeps the aspect ratio; both stretch.
            (spec.cols, spec.rows, spec.fit) = match (keys.cols, keys.rows) {
                (0, 0) => (
                    cells(w + offset_x, cell_w),
                    cells(h + offset_y, cell_h),
                    Fit::Exact(w, h),
                ),
                (c, 0) => {
                    let box_w = c as f32 * cell_w;
                    let box_h = box_w * h / w;
                    (c, cells(box_h, cell_h), Fit::Exact(box_w, box_h))
                }
                (0, r) => {
                    let box_h = r as f32 * cell_h;
                    let box_w = box_h * w / h;
                    (cells(box_w, cell_w), r, Fit::Exact(box_w, box_h))
                }
                (c, r) => (c, r, Fit::Exact(c as f32 * cell_w, r as f32 * cell_h)),
            };
            self.show(term, id, keys.placement, spec, !keys.keep_cursor, out);
        }
        keys.reply(id, "OK", out);
    }

    /// `a=d`. Lowercase takes placements away; uppercase also frees an
    /// image left with none. The positional forms (`x`, `y`, `z`, `r`…)
    /// aren't here yet and do nothing.
    fn kitty_delete<L>(&mut self, term: &Term<L>, keys: &Keys, out: &mut Outputs) {
        let free = keys.delete.is_ascii_uppercase();
        let targets: Vec<(u32, Option<u32>)> = match keys.delete.to_ascii_lowercase() {
            // Everything on screen. Not everything there is: a program
            // clearing its own pictures must not blank the scrollback's,
            // nor the other screen's.
            b'a' => (0..term.screen_lines())
                .flat_map(|line| line_placements(term, Line(line as i32)))
                .flatten()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect(),
            b'c' => {
                let cursor = term.grid().cursor.point;
                line_placements(term, cursor.line)[cursor.column.0]
                    .into_iter()
                    .collect()
            }
            which @ (b'i' | b'n') => {
                let image = match which {
                    b'i' => keys.id,
                    _ => self.numbers.get(&keys.number).copied().unwrap_or(0),
                };
                let Some(stored) = self.images.get(&image) else {
                    return;
                };
                let placement = (keys.placement != 0).then(|| {
                    stored
                        .placements
                        .iter()
                        .find(|&&(client, _)| client == keys.placement)
                        .map_or(keys.placement, |&(_, carried)| carried)
                });
                vec![(image, placement)]
            }
            _ => return,
        };
        for (image, placement) in targets {
            out.graphics(GraphicsEvent::Delete { image, placement });
            let Some(stored) = self.images.get_mut(&image) else {
                continue;
            };
            match placement {
                Some(placement) => stored
                    .placements
                    .retain(|&(_, carried)| carried != placement),
                None => stored.placements.clear(),
            }
            if free && stored.placements.is_empty() {
                self.forget(&[image]);
                out.graphics(GraphicsEvent::Free { image });
            }
        }
    }
}

/// The picture each cell of a screen line belongs to: its image, and its
/// placement unless the cell names none (a client's own placeholder with no
/// underline colour, which stands for any placement of that image).
fn line_placements<L>(term: &Term<L>, line: Line) -> Vec<Option<(u32, Option<u32>)>> {
    let row = &term.grid()[line];
    let mut left = None;
    (0..term.columns())
        .map(|col| {
            let cell = &row[Column(col)];
            left = placeholder::decode(
                cell.c,
                cell.zerowidth(),
                cell.fg,
                cell.underline_color(),
                left,
            );
            left.map(|cell| (cell.image, (cell.placement != 0).then_some(cell.placement)))
        })
        .collect()
}

/// How many cells `pixels` needs, at least one. The small slack keeps an
/// exact multiple from rounding up to one more.
fn cells(pixels: f32, cell: f32) -> u32 {
    ((pixels / cell - 1e-3).ceil() as u32).max(1)
}

/// Write an image's placeholder cells at the cursor, through the same
/// `Handler` the parser drives, so the grid treats them as it treats text:
/// they scroll, wrap on resize, and are erased by whatever is printed over
/// them. The box is clipped at the right edge, never wrapped.
///
/// With `move_cursor` the cursor ends right of the image on its last row,
/// and an image that runs past the bottom scrolls the screen as text would.
/// Without it the cursor stays put and the image is cut at the bottom.
fn write_cells<L: EventListener>(
    term: &mut Term<L>,
    image: u32,
    placement: u32,
    cols: u32,
    rows: u32,
    move_cursor: bool,
) {
    // In insert mode `input` would shove the rest of the line right.
    let insert = term.mode().contains(TermMode::INSERT);
    if insert {
        term.unset_mode(NamedMode::Insert.into());
    }
    let mut cursor = term.grid().cursor.point;
    let origin = (cursor, term.grid().cursor.input_needs_wrap);
    // A cursor waiting to wrap is past the end of its line.
    if move_cursor && origin.1 {
        term.linefeed();
        cursor = term.grid().cursor.point;
        cursor.column = Column(0);
    }
    let first_column = cursor.column;
    let cols = (cols as usize)
        .min(term.columns() - first_column.0)
        .min(DIACRITICS.len());
    let rows = (rows as usize).min(DIACRITICS.len());
    let bottom = term.screen_lines() as i32 - 1;

    // The program's pen is put back afterwards: red, an image, then `x`
    // must give a red `x`. Ours carries nothing over (no link, no flags).
    let mut pen = Cell {
        fg: placeholder::id_color(image),
        ..Cell::default()
    };
    pen.set_underline_color(Some(placeholder::id_color(placement)));
    let saved_pen = std::mem::replace(&mut term.grid_mut().cursor.template, pen);

    for (row, &row_mark) in DIACRITICS.iter().enumerate().take(rows) {
        if row > 0 {
            if move_cursor {
                // Not our own row arithmetic: at the bottom margin this
                // scrolls, pushing the top of the image into history.
                term.linefeed();
            } else if cursor.line.0 + row as i32 > bottom {
                break;
            } else {
                term.grid_mut().cursor.point.line = cursor.line + row as i32;
            }
        }
        let grid_cursor = &mut term.grid_mut().cursor;
        grid_cursor.point.column = first_column;
        grid_cursor.input_needs_wrap = false;
        for &col_mark in &DIACRITICS[..cols] {
            term.input(PLACEHOLDER);
            term.input(row_mark);
            term.input(col_mark);
            if image > 0xFF_FFFF {
                term.input(DIACRITICS[(image >> 24) as usize]);
            }
        }
    }

    let grid_cursor = &mut term.grid_mut().cursor;
    grid_cursor.template = saved_pen;
    if !move_cursor {
        (grid_cursor.point, grid_cursor.input_needs_wrap) = origin;
    }
    if insert {
        term.set_mode(NamedMode::Insert.into());
    }
}

// --- iTerm2 arguments ---

#[derive(Debug, Clone, Copy, PartialEq)]
enum Dim {
    Auto,
    Cells(f32),
    Pixels(f32),
    Percent(f32),
}

impl Dim {
    fn parse(text: &str) -> Dim {
        let number = |s: &str| s.parse::<f32>().ok().filter(|n| n.is_finite() && *n > 0.0);
        if let Some(n) = text.strip_suffix("px").and_then(number) {
            Dim::Pixels(n)
        } else if let Some(n) = text.strip_suffix('%').and_then(number) {
            Dim::Percent(n)
        } else {
            number(text).map_or(Dim::Auto, Dim::Cells)
        }
    }

    /// Capped far beyond any screen, so that an absurd request stays a
    /// finite number all the way to the renderer.
    fn pixels(self, cell: f32, pane: f32) -> Option<f32> {
        let pixels = match self {
            Dim::Auto => return None,
            Dim::Cells(n) => n * cell,
            Dim::Pixels(n) => n,
            Dim::Percent(n) => pane * n / 100.0,
        };
        Some(pixels.min(100_000.0))
    }
}

/// `File=` arguments. `name` and `size` are of no use here.
#[derive(Debug, PartialEq)]
struct ItermArgs {
    inline: bool,
    width: Dim,
    height: Dim,
    preserve_aspect: bool,
    keep_cursor: bool,
}

impl ItermArgs {
    fn parse(args: &[u8]) -> Self {
        let mut parsed = Self {
            inline: false,
            width: Dim::Auto,
            height: Dim::Auto,
            preserve_aspect: true,
            keep_cursor: false,
        };
        for pair in String::from_utf8_lossy(args).split(';') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "inline" => parsed.inline = value == "1",
                "width" => parsed.width = Dim::parse(value),
                "height" => parsed.height = Dim::parse(value),
                "preserveAspectRatio" => parsed.preserve_aspect = value != "0",
                "doNotMoveCursor" => parsed.keep_cursor = value == "1",
                _ => {}
            }
        }
        parsed
    }
}

// --- kitty control keys ---

/// The keys of one `APC G` command that are acted on. `z` is read and
/// ignored (an image is its cells; there are no layers), and the relative
/// placement keys (`P`, `Q`, `H`, `V`) aren't supported.
#[derive(Debug, Clone, PartialEq)]
struct Keys {
    action: u8,
    /// `q`: 1 keeps `OK` quiet, 2 errors as well.
    quiet: u32,
    format: u32,
    medium: u8,
    zlib: bool,
    more: bool,
    /// `s`, `v`: the size of raw pixel data.
    width: u32,
    height: u32,
    id: u32,
    number: u32,
    placement: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    x_offset: u32,
    y_offset: u32,
    cols: u32,
    rows: u32,
    keep_cursor: bool,
    virtual_placement: bool,
    delete: u8,
}

impl Keys {
    fn parse(control: &[u8]) -> Self {
        let mut keys = Self {
            action: b't',
            quiet: 0,
            format: 32,
            medium: b'd',
            zlib: false,
            more: false,
            width: 0,
            height: 0,
            id: 0,
            number: 0,
            placement: 0,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            x_offset: 0,
            y_offset: 0,
            cols: 0,
            rows: 0,
            keep_cursor: false,
            virtual_placement: false,
            delete: b'a',
        };
        for pair in control.split(|&b| b == b',') {
            let [key, b'=', value @ ..] = pair else {
                continue;
            };
            let letter = value.first().copied().unwrap_or(0);
            let number = std::str::from_utf8(value)
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            match key {
                b'a' => keys.action = letter,
                b'q' => keys.quiet = number,
                b'f' => keys.format = number,
                b't' => keys.medium = letter,
                b'o' => keys.zlib = letter == b'z',
                b'm' => keys.more = number == 1,
                b's' => keys.width = number,
                b'v' => keys.height = number,
                b'i' => keys.id = number,
                b'I' => keys.number = number,
                b'p' => keys.placement = number,
                b'x' => keys.x = number,
                b'y' => keys.y = number,
                b'w' => keys.w = number,
                b'h' => keys.h = number,
                b'X' => keys.x_offset = number,
                b'Y' => keys.y_offset = number,
                b'c' => keys.cols = number,
                b'r' => keys.rows = number,
                b'C' => keys.keep_cursor = number == 1,
                b'U' => keys.virtual_placement = number == 1,
                b'd' => keys.delete = letter,
                _ => {}
            }
        }
        keys
    }

    /// `ESC _ G i=…;OK ESC \`, or an error. Only a command that named an
    /// image gets an answer, and `q` can silence it. `message` is always
    /// one of our own fixed strings: nothing a client sent is echoed back
    /// into its input.
    fn reply(&self, id: u32, message: &str, out: &mut Outputs) {
        let ok = message == "OK";
        if (self.id == 0 && self.number == 0) || self.quiet >= if ok { 1 } else { 2 } {
            return;
        }
        let mut text = format!("\x1b_Gi={id}");
        if self.number != 0 {
            write!(text, ",I={}", self.number).ok();
        }
        if self.placement != 0 {
            write!(text, ",p={}", self.placement).ok();
        }
        write!(text, ";{message}\x1b\\").ok();
        out.replies.extend_from_slice(text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::super::event_loop::{SessionEvent, feed};
    use super::super::images::tests::png;
    use super::super::placeholder::ImageCell;
    use super::super::scan::Scanner;
    use super::super::session::TermSize;
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::Point;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};
    use std::io::Read;

    /// A real `Term` behind the same chunk function the PTY thread runs:
    /// 80 × 24 cells of 10 × 20 pixels.
    struct Harness {
        term: Term<VoidListener>,
        parser: ansi::Processor,
        scanner: Scanner,
        graphics: GraphicsState,
    }

    impl Harness {
        fn new() -> Self {
            let size = TermSize {
                columns: 80,
                screen_lines: 24,
                cell_width: 10.0,
                cell_height: 20.0,
                scale: 1.0,
            };
            Self {
                term: Term::new(Config::default(), &size, VoidListener),
                parser: ansi::Processor::new(),
                scanner: Scanner::new(),
                graphics: GraphicsState::new(true, size.cell_pixels()),
            }
        }

        fn feed(&mut self, bytes: impl AsRef<[u8]>) -> Outputs {
            feed(
                &mut self.term,
                &mut self.parser,
                &mut self.scanner,
                &mut self.graphics,
                bytes.as_ref(),
            )
        }

        /// Feed, and return the graphics events and the reply text.
        fn run(&mut self, bytes: impl AsRef<[u8]>) -> (Vec<GraphicsEvent>, String) {
            let out = self.feed(bytes);
            let events = out
                .events
                .into_iter()
                .filter_map(|event| match event {
                    SessionEvent::Graphics(event) => Some(event),
                    _ => None,
                })
                .collect();
            (events, String::from_utf8(out.replies).unwrap())
        }

        fn image_at(&self, line: i32, col: usize) -> Option<ImageCell> {
            let row = &self.term.grid()[Line(line)];
            let mut left = None;
            for col in 0..=col {
                let cell = &row[Column(col)];
                left = placeholder::decode(
                    cell.c,
                    cell.zerowidth(),
                    cell.fg,
                    cell.underline_color(),
                    left,
                );
            }
            left
        }

        fn char_at(&self, line: i32, col: usize) -> char {
            self.term.grid()[Line(line)][Column(col)].c
        }

        fn cursor(&self) -> (i32, usize) {
            let point = self.term.grid().cursor.point;
            (point.line.0, point.column.0)
        }
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn iterm(args: &str, file: &[u8]) -> String {
        format!("\x1b]1337;File={args}:{}\x07", b64(file))
    }

    fn kitty(control: &str, payload: &[u8]) -> String {
        format!("\x1b_G{control};{}\x1b\\", b64(payload))
    }

    /// The `Place` among some events.
    fn placed(events: &[GraphicsEvent]) -> (u32, u32, Placement) {
        events
            .iter()
            .find_map(|event| match event {
                GraphicsEvent::Place {
                    image,
                    placement,
                    spec,
                } => Some((*image, *placement, *spec)),
                _ => None,
            })
            .expect("a placement")
    }

    #[test]
    fn an_image_becomes_cells_at_the_cursor_and_the_cursor_ends_beside_it() {
        let mut h = Harness::new();
        // 25 × 30 pixels on 10 × 20 cells: three columns, two rows.
        let (events, replies) = h.run(format!("ab{}x", iterm("inline=1", &png(25, 30))));
        assert_eq!(replies, "");
        let GraphicsEvent::Image { id, size, .. } = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!(*size, (25, 30));
        assert!(*id > 0xFF_FFFF, "our ids stay clear of a client's");
        let (image, placement, spec) = placed(&events);
        assert_eq!(image, *id);
        assert_eq!(
            (spec.cols, spec.rows, spec.fit, spec.cell),
            (3, 2, Fit::Exact(25.0, 30.0), (10.0, 20.0))
        );
        for row in 0..2u16 {
            for col in 0..3u16 {
                assert_eq!(
                    h.image_at(row as i32, 2 + col as usize),
                    Some(ImageCell {
                        image,
                        placement,
                        row,
                        col
                    })
                );
            }
        }
        // The text around it is where text would be around three wide,
        // two tall anything: `ab` before, `x` right of the last row.
        assert_eq!((h.char_at(0, 0), h.char_at(0, 1)), ('a', 'b'));
        assert_eq!(h.image_at(0, 5), None);
        assert_eq!(h.char_at(1, 5), 'x');
        assert_eq!(h.cursor(), (1, 6));
    }

    #[test]
    fn the_programs_pen_survives_an_image() {
        let mut h = Harness::new();
        h.feed(format!("\x1b[31m{}x", iterm("inline=1", &png(10, 20))));
        let x = &h.term.grid()[Line(0)][Column(1)];
        assert_eq!((x.c, x.fg), ('x', Color::Named(NamedColor::Red)));
        assert_eq!(x.underline_color(), None);
    }

    #[test]
    fn an_image_taller_than_the_screen_scrolls_its_top_into_history() {
        let mut h = Harness::new();
        // 30 rows on a 24-line screen.
        h.feed(iterm("inline=1", &png(10, 600)));
        assert_eq!(h.term.grid().history_size(), 6);
        assert_eq!(h.image_at(-6, 0).unwrap().row, 0);
        assert_eq!(h.image_at(23, 0).unwrap().row, 29);
        assert_eq!(h.cursor(), (23, 1));
    }

    #[test]
    fn the_right_edge_clips_rather_than_wraps() {
        let mut h = Harness::new();
        // Five columns wide with two left. (kitty: iTerm2's images shrink
        // to the room there is instead.)
        h.feed(format!(
            "\x1b[1;79H{}",
            kitty("a=T,f=24,s=50,v=40", &[0; 6000])
        ));
        for line in 0..2 {
            assert_eq!(h.image_at(line, 78).unwrap().col, 0);
            assert_eq!(h.image_at(line, 79).unwrap().col, 1);
            assert_eq!(h.image_at(line + 1, 0), None, "nothing wrapped");
        }
        // Past the edge like text that filled the line: the next character
        // wraps.
        h.feed("x");
        assert_eq!(h.char_at(2, 0), 'x');
    }

    #[test]
    fn insert_mode_does_not_shove_the_line_along() {
        let mut h = Harness::new();
        h.feed(format!(
            "\x1b[4habcdef\r{}Z",
            iterm("inline=1", &png(30, 20))
        ));
        assert!(h.image_at(0, 2).is_some());
        // The image replaced `abc`; the mode is still on for the `Z`.
        assert_eq!(
            (h.char_at(0, 3), h.char_at(0, 4), h.char_at(0, 6)),
            ('Z', 'd', 'f')
        );
    }

    #[test]
    fn an_image_inside_a_synchronised_update_lands_after_the_text_before_it() {
        let mut h = Harness::new();
        h.feed(format!("\x1b[?2026hab{}", iterm("inline=1", &png(10, 20))));
        assert_eq!((h.char_at(0, 0), h.char_at(0, 1)), ('a', 'b'));
        assert!(h.image_at(0, 2).is_some());
        assert_eq!(h.image_at(0, 0), None);
    }

    #[test]
    fn narrowing_the_window_wraps_an_image_and_widening_restores_it() {
        let mut h = Harness::new();
        // Five columns by two rows, then a window only three columns wide.
        h.feed(kitty("a=T,f=24,s=50,v=40", &[0; 6000]));
        let resize = |h: &mut Harness, columns| {
            h.term.resize(TermSize {
                columns,
                screen_lines: 24,
                cell_width: 10.0,
                cell_height: 20.0,
                scale: 1.0,
            })
        };
        resize(&mut h, 3);
        // Each image row is now two lines, and every cell still knows its
        // slice. (Lines count from the top of the buffer: the reflow pushes
        // what no longer fits above the cursor into history.)
        let slice = |h: &Harness, line: i32, col| {
            let top = -(h.term.grid().history_size() as i32);
            h.image_at(top + line, col).map(|c| (c.row, c.col))
        };
        assert_eq!(slice(&h, 0, 2), Some((0, 2)));
        assert_eq!(slice(&h, 1, 0), Some((0, 3)));
        assert_eq!(slice(&h, 1, 1), Some((0, 4)));
        assert_eq!(slice(&h, 2, 0), Some((1, 0)));
        resize(&mut h, 80);
        assert_eq!(slice(&h, 0, 4), Some((0, 4)));
        assert_eq!(slice(&h, 1, 4), Some((1, 4)));
        assert_eq!(slice(&h, 2, 0), None);
    }

    #[test]
    fn alternate_screen_images_leave_with_it() {
        let mut h = Harness::new();
        h.feed(format!("\x1b[?1049h{}", iterm("inline=1", &png(10, 20))));
        assert!(h.image_at(0, 0).is_some());
        h.feed("\x1b[?1049l");
        assert_eq!(h.image_at(0, 0), None);
    }

    #[test]
    fn a_fixed_cursor_stays_put_and_the_image_is_cut_at_the_bottom() {
        let mut h = Harness::new();
        h.feed(format!(
            "\x1b[24;3H{}",
            iterm("inline=1;doNotMoveCursor=1", &png(10, 60))
        ));
        assert_eq!(h.image_at(23, 2).unwrap().row, 0);
        assert_eq!(h.term.grid().history_size(), 0, "no scrolling");
        assert_eq!(h.cursor(), (23, 2));
        // kitty's C=1 is the same rule.
        h.feed(format!(
            "\x1b[1;1H{}",
            kitty("a=T,f=24,s=1,v=1,C=1", &[0; 3])
        ));
        assert!(h.image_at(0, 0).is_some());
        assert_eq!(h.cursor(), (0, 0));
    }

    #[test]
    fn copied_text_has_no_placeholders() {
        use alacritty_terminal::index::Side;
        use alacritty_terminal::selection::{Selection, SelectionType};
        let mut h = Harness::new();
        h.feed(format!("ab{}cd", iterm("inline=1", &png(30, 20))));
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Side::Left,
        );
        selection.update(Point::new(Line(0), Column(79)), Side::Right);
        h.term.selection = Some(selection);
        let text = h.term.selection_to_string().unwrap();
        // What alacritty hands back is why the strip exists.
        assert!(text.contains(PLACEHOLDER));
        assert_eq!(placeholder::strip_placeholders(&text).trim(), "abcd");
    }

    #[test]
    fn iterm_arguments_size_the_box() {
        assert_eq!(Dim::parse("12"), Dim::Cells(12.0));
        assert_eq!(Dim::parse("120px"), Dim::Pixels(120.0));
        assert_eq!(Dim::parse("50%"), Dim::Percent(50.0));
        for auto in ["auto", "", "-3", "0", "wide"] {
            assert_eq!(Dim::parse(auto), Dim::Auto, "{auto:?}");
        }
        assert_eq!(
            ItermArgs::parse(b"name=YS5wbmc=;size=70;inline=1;width=3;preserveAspectRatio=0"),
            ItermArgs {
                inline: true,
                width: Dim::Cells(3.0),
                height: Dim::Auto,
                preserve_aspect: false,
                keep_cursor: false,
            }
        );

        // Against a 200 × 100 image on an 80 × 24 screen of 10 × 20 cells.
        let boxes = |args: &str| {
            let mut h = Harness::new();
            let (events, _) = h.run(iterm(args, &png(200, 100)));
            let (_, _, spec) = placed(&events);
            (spec.cols, spec.rows, spec.fit)
        };
        assert_eq!(boxes("inline=1"), (20, 5, Fit::Exact(200.0, 100.0)));
        // One side given: the other follows the aspect ratio.
        assert_eq!(boxes("inline=1;width=10"), (10, 3, Fit::Exact(100.0, 50.0)));
        assert_eq!(
            boxes("inline=1;height=40px;width=auto"),
            (8, 2, Fit::Exact(80.0, 40.0))
        );
        assert_eq!(
            boxes("inline=1;width=50%"),
            (40, 10, Fit::Exact(400.0, 200.0))
        );
        // Both: fitted inside the box, unless told to stretch.
        assert_eq!(boxes("inline=1;width=10;height=10"), (10, 10, Fit::Contain));
        assert_eq!(
            boxes("inline=1;width=10;height=10;preserveAspectRatio=0"),
            (10, 10, Fit::Exact(100.0, 200.0))
        );

        // Wider than the room left on the line: scaled down to fit it.
        let mut h = Harness::new();
        let (events, _) = h.run(format!("\x1b[1;41H{}", iterm("inline=1", &png(800, 100))));
        let (_, _, spec) = placed(&events);
        assert_eq!(
            (spec.cols, spec.rows, spec.fit),
            (40, 3, Fit::Exact(400.0, 50.0))
        );
        // After text that filled its line, the room is the next line's.
        let mut h = Harness::new();
        let (events, _) = h.run(format!(
            "{}{}",
            "x".repeat(80),
            iterm("inline=1", &png(100, 20))
        ));
        let (_, _, spec) = placed(&events);
        assert_eq!((spec.cols, h.image_at(1, 0).map(|c| c.col)), (10, Some(0)));
    }

    #[test]
    fn iterm_downloads_and_garbage_draw_nothing() {
        let mut h = Harness::new();
        let (events, _) = h.run(iterm("inline=0", &png(10, 20)));
        assert!(events.is_empty(), "inline=0 is a download");
        let (events, _) = h.run(iterm("name=eA==", &png(10, 20)));
        assert!(events.is_empty(), "so is no inline at all");
        let (events, _) = h.run(iterm("inline=1", b"not an image"));
        assert!(events.is_empty());
        let (events, _) = h.run("\x1b]1337;File=inline=1:@@@@\x07");
        assert!(events.is_empty());
        // And the text after each is still text.
        h.feed("ok");
        assert_eq!((h.char_at(0, 0), h.char_at(0, 1)), ('o', 'k'));
    }

    #[test]
    fn iterm_multipart_files_are_assembled() {
        let mut h = Harness::new();
        let file = png(20, 20);
        let encoded = b64(&file);
        let (head, tail) = encoded.split_at(encoded.len() / 2);
        let (events, _) = h.run(format!(
            "\x1b]1337;MultipartFile=inline=1;size={}\x07\x1b]1337;FilePart={head}\x07\x1b]1337;FilePart={tail}\x1b\\",
            file.len()
        ));
        assert!(events.is_empty(), "nothing until the end arrives");
        let (events, _) = h.run("\x1b]1337;FileEnd\x07");
        let GraphicsEvent::Image { data, .. } = &events[0] else {
            panic!("{events:?}");
        };
        assert!(matches!(data, ImageData::Encoded { bytes, .. } if *bytes == file));
        assert_eq!(h.image_at(0, 1).unwrap().col, 1);
        // A stray end with nothing pending is nothing.
        assert!(h.run("\x1b]1337;FileEnd\x07").0.is_empty());
    }

    #[test]
    fn kitty_keys_are_read_with_their_defaults() {
        let keys = Keys::parse(
            b"a=T,f=24,s=10,v=20,i=7,p=3,c=4,r=2,x=1,y=2,w=3,h=4,X=5,Y=6,C=1,U=1,q=2,o=z,m=1,z=-1",
        );
        assert_eq!(
            (keys.action, keys.format, keys.width, keys.height),
            (b'T', 24, 10, 20)
        );
        assert_eq!(
            (keys.id, keys.placement, keys.cols, keys.rows),
            (7, 3, 4, 2)
        );
        assert_eq!((keys.x, keys.y, keys.w, keys.h), (1, 2, 3, 4));
        assert_eq!((keys.x_offset, keys.y_offset), (5, 6));
        assert!(keys.keep_cursor && keys.virtual_placement && keys.zlib && keys.more);
        assert_eq!(keys.quiet, 2);
        let bare = Keys::parse(b"");
        assert_eq!(
            (bare.action, bare.format, bare.medium, bare.delete),
            (b't', 32, b'd', b'a')
        );
        // Malformed pairs are skipped, not fatal.
        assert_eq!(Keys::parse(b"i=abc,,=,i,p=2").placement, 2);
    }

    #[test]
    fn kitty_replies_only_when_asked_and_never_when_hushed() {
        let mut h = Harness::new();
        let pixel = [0u8; 3];
        // The support probe: a query, answered without storing anything.
        let (events, reply) = h.run(kitty("a=q,i=31,s=1,v=1,f=24", &pixel));
        assert!(events.is_empty());
        assert_eq!(reply, "\x1b_Gi=31;OK\x1b\\");
        // No id, no answer; q=1 hushes OK; errors still come until q=2.
        assert_eq!(h.run(kitty("a=T,s=1,v=1,f=24", &pixel)).1, "");
        assert_eq!(h.run(kitty("a=t,i=1,q=1,s=1,v=1,f=24", &pixel)).1, "");
        assert_eq!(
            h.run("\x1b_Ga=p,i=99,q=1\x1b\\").1,
            "\x1b_Gi=99;ENOENT:no such image\x1b\\"
        );
        assert_eq!(h.run("\x1b_Ga=p,i=99,q=2\x1b\\").1, "");
        // Errors a client has to hear to fall back: too little data, a
        // medium that isn't the pipe, a file that isn't an image.
        assert!(
            h.run(kitty("a=t,i=2,s=2,v=2,f=24", &pixel))
                .1
                .contains(";ENODATA:")
        );
        assert!(
            h.run(kitty("a=q,i=2,t=f", b"/etc/passwd"))
                .1
                .contains(";EINVAL:")
        );
        assert!(h.run(kitty("a=q,i=2,t=s", b"shm")).1.contains(";EINVAL:"));
        assert!(
            h.run(kitty("a=t,i=2,f=100", b"junk"))
                .1
                .contains(";EBADPNG:")
        );
        assert!(
            h.run(kitty("a=t,i=2,f=24,s=99999,v=1", &pixel))
                .1
                .contains(";EINVAL:")
        );
        // The placement id is echoed with the image id.
        assert_eq!(h.run("\x1b_Ga=p,i=1,p=5\x1b\\").1, "\x1b_Gi=1,p=5;OK\x1b\\");
    }

    #[test]
    fn kitty_chunks_are_assembled_and_answered_once() {
        let mut h = Harness::new();
        let file = png(20, 40);
        let (a, rest) = file.split_at(file.len() / 3);
        let (b, c) = rest.split_at(rest.len() / 2);
        let (events, reply) = h.run(kitty("a=T,f=100,i=5,m=1", a));
        assert!(events.is_empty() && reply.is_empty());
        let (events, reply) = h.run(kitty("m=1", b));
        assert!(events.is_empty() && reply.is_empty());
        let (events, reply) = h.run(kitty("m=0", c));
        assert_eq!(reply, "\x1b_Gi=5;OK\x1b\\");
        let GraphicsEvent::Image { id, size, data, .. } = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!((*id, *size), (5, (20, 40)));
        assert!(matches!(data, ImageData::Encoded { bytes, zlib: false } if *bytes == file));
        // Displayed, by the keys of the first chunk: 2 × 2 cells.
        assert_eq!(
            h.image_at(1, 1),
            Some(ImageCell {
                image: 5,
                placement: placed(&events).1,
                row: 1,
                col: 1
            })
        );
    }

    #[test]
    fn kitty_compressed_data_is_passed_on_compressed() {
        let deflate = |bytes: &[u8]| {
            let mut out = Vec::new();
            flate2::read::ZlibEncoder::new(bytes, flate2::Compression::fast())
                .read_to_end(&mut out)
                .unwrap();
            out
        };
        let mut h = Harness::new();
        // Raw pixels: the size is the client's word.
        let (events, reply) = h.run(kitty("a=t,i=1,f=24,s=4,v=4,o=z", &deflate(&[9; 48])));
        assert_eq!(reply, "\x1b_Gi=1;OK\x1b\\");
        assert!(matches!(
            &events[0],
            GraphicsEvent::Image {
                size: (4, 4),
                data: ImageData::Raw {
                    alpha: false,
                    zlib: true,
                    ..
                },
                ..
            }
        ));
        // A PNG: the size is read from the start of the inflated file.
        let (events, _) = h.run(kitty("a=t,i=2,f=100,o=z", &deflate(&png(33, 44))));
        assert!(matches!(
            &events[0],
            GraphicsEvent::Image {
                size: (33, 44),
                data: ImageData::Encoded { zlib: true, .. },
                ..
            }
        ));
    }

    #[test]
    fn kitty_image_numbers_get_an_id_of_ours() {
        let mut h = Harness::new();
        let (events, reply) = h.run(kitty("a=t,I=3,f=24,s=1,v=1", &[0; 3]));
        let GraphicsEvent::Image { id, .. } = events[0] else {
            panic!("{events:?}");
        };
        assert!(id > 0xFF_FFFF);
        assert_eq!(reply, format!("\x1b_Gi={id},I=3;OK\x1b\\"));
        // The number then stands for that image.
        let (events, _) = h.run("\x1b_Ga=p,I=3\x1b\\");
        assert_eq!(placed(&events).0, id);
        assert_eq!(h.image_at(0, 0).unwrap().image, id);
        // An id and a number together is the client's mistake.
        assert!(
            h.run(kitty("a=t,i=1,I=3,f=24,s=1,v=1", &[0; 3]))
                .1
                .contains("EINVAL")
        );
    }

    #[test]
    fn kitty_placements_size_crop_and_replace() {
        let mut h = Harness::new();
        // 40 × 40 pixels: two columns, two rows at its own size.
        h.run(kitty("a=t,i=7,f=24,s=40,v=40", &[0; 4800]));
        let put = |h: &mut Harness, keys: &str| {
            let (events, _) = h.run(format!("\x1b[H\x1b_Ga=p,i=7,{keys}\x1b\\"));
            let (_, _, spec) = placed(&events);
            (spec.cols, spec.rows, spec.fit)
        };
        assert_eq!(put(&mut h, "q=1"), (4, 2, Fit::Exact(40.0, 40.0)));
        // Both given stretches; one keeps the aspect ratio.
        assert_eq!(put(&mut h, "c=6,r=1"), (6, 1, Fit::Exact(60.0, 20.0)));
        assert_eq!(put(&mut h, "c=6"), (6, 3, Fit::Exact(60.0, 60.0)));
        assert_eq!(put(&mut h, "r=1"), (2, 1, Fit::Exact(20.0, 20.0)));
        // A source rectangle is what gets sized, clamped to the image.
        let (events, _) = h.run("\x1b_Ga=p,i=7,x=30,y=0,w=100,h=20\x1b\\");
        let (_, _, spec) = placed(&events);
        assert_eq!(spec.crop, Some([30, 0, 10, 20]));
        assert_eq!((spec.cols, spec.rows), (1, 1));
        // An offset into the first cell can push it into one more.
        let (events, _) = h.run("\x1b_Ga=p,i=7,X=5,Y=1\x1b\\");
        let (_, _, spec) = placed(&events);
        assert_eq!((spec.cols, spec.rows, spec.offset), (5, 3, (5, 1)));

        // The same placement id again moves it: the old cells go blank.
        let (events, _) = h.run("\x1b_Ga=p,i=7,p=1\x1b\\");
        let (_, first, _) = placed(&events);
        let (events, _) = h.run("\x1b_Ga=p,i=7,p=1\x1b\\");
        assert!(matches!(
            events[0],
            GraphicsEvent::Delete { image: 7, placement: Some(old) } if old == first
        ));
        assert_ne!(placed(&events).1, first);
    }

    #[test]
    fn kitty_virtual_placements_write_no_cells() {
        let mut h = Harness::new();
        let (events, reply) = h.run(kitty("a=T,U=1,i=7,p=2,c=4,r=3,f=24,s=1,v=1", &[0; 3]));
        assert_eq!(reply, "\x1b_Gi=7,p=2;OK\x1b\\");
        // The client's placement id, as its own placeholder cells will say.
        let (image, placement, spec) = placed(&events);
        assert_eq!((image, placement), (7, 2));
        assert_eq!((spec.cols, spec.rows, spec.fit), (4, 3, Fit::Contain));
        assert_eq!(h.image_at(0, 0), None);
        assert_eq!(h.cursor(), (0, 0));

        // Those cells, as a client writes them: the id in the foreground
        // colour, the placement in the underline colour, row and column in
        // the marks of the first cell only.
        h.feed(format!(
            "\x1b[38;2;0;0;7m\x1b[58;2;0;0;2m{PLACEHOLDER}{}{}{PLACEHOLDER}{PLACEHOLDER}\x1b[m",
            DIACRITICS[1], DIACRITICS[0]
        ));
        assert_eq!(
            h.image_at(0, 2),
            Some(ImageCell {
                image: 7,
                placement: 2,
                row: 1,
                col: 2
            })
        );
    }

    #[test]
    fn kitty_deletes_take_placements_and_capitals_free_the_image() {
        let mut h = Harness::new();
        let pixel = [0u8; 3];
        h.run(kitty("a=T,i=1,f=24,s=1,v=1", &pixel));
        // A second image, placed and then scrolled into history.
        h.run(format!(
            "{}{}",
            kitty("a=T,i=2,f=24,s=1,v=1", &pixel),
            "\n".repeat(30)
        ));
        h.run(format!("\x1b[H{}", kitty("a=T,i=3,f=24,s=1,v=1", &pixel)));

        // `d=A`: what is on screen, and only that. Image 3 is; image 1
        // and 2 have scrolled away and keep their pixels.
        let (events, reply) = h.run("\x1b_Ga=d,d=A\x1b\\");
        assert_eq!(reply, "");
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(
            events[0],
            GraphicsEvent::Delete {
                image: 3,
                placement: Some(_)
            }
        ));
        assert!(matches!(events[1], GraphicsEvent::Free { image: 3 }));
        assert!(h.run("\x1b_Ga=p,i=3\x1b\\").1.contains("ENOENT"));

        // By id: lowercase keeps the image for another placement.
        let (events, _) = h.run("\x1b_Ga=d,d=i,i=1\x1b\\");
        assert!(matches!(
            events[..],
            [GraphicsEvent::Delete {
                image: 1,
                placement: None
            }]
        ));
        assert!(h.run("\x1b_Ga=p,i=1\x1b\\").1.contains("OK"));
        // ...and by client placement id, which names the cells' own id.
        let (events, _) = h.run("\x1b_Ga=p,i=1,p=9\x1b\\");
        let (_, carried, _) = placed(&events);
        let (events, _) = h.run("\x1b_Ga=d,d=i,i=1,p=9\x1b\\");
        assert!(matches!(
            events[..],
            [GraphicsEvent::Delete { image: 1, placement: Some(p) }] if p == carried
        ));
        // Uppercase with placements left elsewhere frees nothing; with
        // none left, it does.
        let (events, _) = h.run("\x1b_Ga=d,d=I,i=1,p=77\x1b\\");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GraphicsEvent::Free { .. }))
        );
        let (events, _) = h.run("\x1b_Ga=d,d=I,i=1\x1b\\");
        assert!(matches!(events[1], GraphicsEvent::Free { image: 1 }));

        // At the cursor.
        h.run(format!(
            "\x1b[5;5H{}\x1b[5;5H",
            kitty("a=T,i=4,f=24,s=1,v=1", &pixel)
        ));
        let (events, _) = h.run("\x1b_Ga=d,d=C\x1b\\");
        assert!(matches!(
            events[0],
            GraphicsEvent::Delete {
                image: 4,
                placement: Some(_)
            }
        ));
        assert!(matches!(events[1], GraphicsEvent::Free { image: 4 }));
    }

    #[test]
    fn images_awaiting_decode_are_budgeted() {
        let mut h = Harness::new();
        // A few bytes that claim 256 MB once decoded. Two fit the budget.
        let huge = |id: u32| kitty(&format!("a=t,i={id},f=32,s=8000,v=8000,o=z"), &[0; 4]);
        let (first, reply) = h.run(huge(1));
        assert!(reply.contains(";OK"));
        let (second, reply) = h.run(huge(2));
        assert!(reply.contains(";OK"));
        // The third is refused, and nothing is placed.
        let (events, reply) = h.run(huge(3));
        assert!(events.is_empty());
        assert_eq!(
            reply,
            "\x1b_Gi=3;ENOMEM:too many images waiting to be decoded\x1b\\"
        );
        assert!(h.run("\x1b_Ga=p,i=3\x1b\\").1.contains("ENOENT"));
        // 26 MB of sixel doesn't fit in what's left either; a small image
        // still does.
        let (events, _) = h.run(format!("\x1bPq{}\x1b\\", "!10000~-".repeat(110)));
        assert!(events.is_empty());
        assert_eq!(h.image_at(0, 0), None);
        let (events, _) = h.run(iterm("inline=1", &png(10, 10)));
        assert_eq!(events.len(), 2);
        // Decoded (here: dropped), their share comes back.
        drop((first, second));
        assert!(h.run(huge(3)).1.contains(";OK"));
    }

    #[test]
    fn a_virtual_placement_replaces_a_drawn_one_of_the_same_id() {
        let mut h = Harness::new();
        let (events, _) = h.run(kitty("a=T,i=1,p=5,f=24,s=1,v=1", &[0; 3]));
        let (_, drawn, _) = placed(&events);
        h.run("\x1b_Ga=p,i=1\x1b\\");
        // The cells of the drawn placement 5 go blank...
        let (events, _) = h.run("\x1b_Ga=p,i=1,p=5,U=1\x1b\\");
        assert!(matches!(
            events[0],
            GraphicsEvent::Delete { image: 1, placement: Some(p) } if p == drawn
        ));
        // ...and a virtual placement with no id leaves the anonymous drawn
        // one alone: deleting placement 5 still doesn't empty the image.
        let (events, _) = h.run("\x1b_Ga=p,i=1,U=1\x1b\\");
        assert!(matches!(
            events[..],
            [GraphicsEvent::Place { placement: 0, .. }]
        ));
        let (events, _) = h.run("\x1b_Ga=d,d=I,i=1,p=5\x1b\\");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GraphicsEvent::Free { .. }))
        );
    }

    #[test]
    fn evicted_images_are_gone_to_the_client_too() {
        let mut h = Harness::new();
        h.run(kitty("a=t,i=1,f=24,s=1,v=1", &[0; 3]));
        h.graphics.forget(&[1]);
        assert!(h.run("\x1b_Ga=p,i=1\x1b\\").1.contains("ENOENT"));
    }

    #[test]
    fn queries_are_answered_in_device_pixels() {
        let mut h = Harness::new();
        h.graphics.set_cell((16.8, 34.0));
        assert_eq!(h.run("\x1b[16t").1, "\x1b[6;34;17t");
        // 24 × 34 high, 80 × 16.8 wide: from the unrounded cell.
        assert_eq!(h.run("\x1b[14t").1, "\x1b[4;816;1344t");
        assert_eq!(
            h.run("\x1b[>q").1,
            format!("\x1bP>|OmniPTY {}\x1b\\", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn switched_off_nothing_is_drawn_or_answered_and_text_goes_on() {
        let mut h = Harness::new();
        h.graphics.set_enabled(false);
        let (events, reply) = h.run(format!(
            "a{}b{}c\x1bPq#1~\x1b\\d\x1b[?1;1S",
            iterm("inline=1", &png(10, 20)),
            kitty("a=q,i=31,s=1,v=1,f=24", &[0; 3])
        ));
        assert!(events.is_empty());
        assert_eq!(reply, "");
        assert_eq!(
            (
                h.char_at(0, 0),
                h.char_at(0, 1),
                h.char_at(0, 2),
                h.char_at(0, 3)
            ),
            ('a', 'b', 'c', 'd')
        );
    }

    #[test]
    fn a_sixel_becomes_cells_and_the_cursor_goes_below_it() {
        let mut h = Harness::new();
        // 25 pixels wide (`!25~`), four bands and so 24 high: three columns
        // and two rows, two columns in.
        let (events, _) = h.run("ab\x1bPq#1;2;100;0;0#1!25~-!25~-!25~-!25~\x1b\\x");
        let GraphicsEvent::Image { size, data, .. } = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!(*size, (25, 24));
        assert!(
            matches!(data, ImageData::Raw { bytes, alpha: true, zlib: false }
            if bytes.len() == 25 * 24 * 4 && bytes[..4] == [255, 0, 0, 255])
        );
        let (_, _, spec) = placed(&events);
        assert_eq!(
            (spec.cols, spec.rows, spec.fit),
            (3, 2, Fit::Exact(25.0, 24.0))
        );
        assert_eq!(h.image_at(0, 2).unwrap().col, 0);
        assert_eq!(h.image_at(1, 4).unwrap().row, 1);
        // xterm's rule: the start of the line under the picture.
        assert_eq!(h.char_at(2, 0), 'x');
        assert_eq!(
            h.run("\x1b[?1;1S\x1b[?2;1S").1,
            "\x1b[?1;0;256S\x1b[?2;0;800;480S"
        );
    }

    #[test]
    fn hostile_input_is_refused_without_a_panic() {
        let mut h = Harness::new();
        // Sizes that overflow, a crop outside the image, absurd boxes.
        h.run(kitty("a=T,i=1,f=32,s=4294967295,v=4294967295", &[0; 4]));
        h.run(kitty("a=t,i=1,f=24,s=2,v=2", &[0; 12]));
        h.run("\x1b_Ga=p,i=1,x=4294967295,y=4294967295,w=4294967295,h=4294967295\x1b\\");
        let (events, _) = h.run("\x1b_Ga=p,i=1,c=4294967295,r=4294967295\x1b\\");
        assert_eq!(placed(&events).2.cols, u32::MAX, "the box is as asked");
        // ...but only what fits the diacritics and the line is written.
        assert_eq!(h.term.grid().history_size(), DIACRITICS.len() - 24);
        h.run(iterm(
            "inline=1;width=99999999;height=99999999px",
            &png(1, 1),
        ));
        for absurd in ["width=3e38", "width=1e35px", "height=3e38%"] {
            let (events, _) = h.run(iterm(&format!("inline=1;{absurd}"), &png(1, 9)));
            let Fit::Exact(w, h) = placed(&events).2.fit else {
                panic!("{events:?}");
            };
            assert!(w.is_finite() && h.is_finite(), "{absurd}: {w} × {h}");
        }
        h.run("\x1b_Ga=d,d=q\x1b\\\x1b_Ga=f,i=1\x1b\\\x1b_G\x1b\\\x1b_G;;;\x1b\\");
    }
}
