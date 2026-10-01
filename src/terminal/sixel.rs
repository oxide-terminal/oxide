//! Sixel (`DCS … q … ST`) to pixels. A streaming decoder: the scanner feeds
//! it the body as it arrives, so megabytes of sixel text are never buffered.
//!
//! Each data character `?`..`~` is a column of six pixels in the current
//! colour; `$` returns to the left edge, `-` moves down six rows, `!n`
//! repeats the next character, `#` selects or defines a colour register and
//! `"` declares the picture's size. The canvas grows as bands arrive, up to
//! the same limits every other image is held to. Pixel aspect ratios other
//! than 1:1 are ignored, as most terminals ignore them.

use super::images::{MAX_DIMENSION, MAX_PIXELS};

/// A decoded sixel picture: RGBA, `width * height * 4` bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Sixel {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// The VT340's sixteen default colours, each channel a percentage.
const VT340: [[u8; 3]; 16] = [
    [0, 0, 0],
    [20, 20, 80],
    [80, 13, 13],
    [20, 80, 20],
    [80, 20, 80],
    [20, 80, 80],
    [80, 80, 20],
    [53, 53, 53],
    [26, 26, 26],
    [33, 33, 60],
    [60, 26, 26],
    [33, 60, 33],
    [60, 33, 60],
    [33, 60, 60],
    [60, 60, 33],
    [80, 80, 80],
];

const REGISTERS: usize = 256;

pub struct Decoder {
    /// RGBA rows, `stride` pixels wide. Alpha 0 is a pixel never drawn.
    pixels: Vec<u8>,
    stride: usize,
    /// The picture's extent so far: what was drawn, and for the width what
    /// `"` declared.
    width: usize,
    height: usize,
    /// The height `"` declared.
    declared_height: usize,
    /// Colour registers, private to this image.
    palette: [[u8; 3]; REGISTERS],
    color: usize,
    /// The pen: `y` is the top of the current band of six rows.
    x: usize,
    y: usize,
    /// The command being read (`"`, `#` or `!`; 0 for none) and its
    /// numeric parameters so far.
    command: u8,
    params: [u32; 5],
    count: usize,
    /// `P2=1`: pixels never drawn stay transparent. Otherwise they take
    /// colour register 0.
    transparent: bool,
}

impl Decoder {
    /// `params` is what came between `ESC P` and the `q`.
    pub fn new(params: &[u8]) -> Self {
        let mut palette = [[0; 3]; REGISTERS];
        for (register, color) in palette.iter_mut().zip(VT340) {
            *register = color.map(percent);
        }
        Self {
            pixels: Vec::new(),
            stride: 0,
            width: 0,
            height: 0,
            declared_height: 0,
            palette,
            color: 0,
            x: 0,
            y: 0,
            command: 0,
            params: [0; 5],
            count: 0,
            transparent: params.split(|&b| b == b';').nth(1) == Some(&b"1"[..]),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match byte {
                b'0'..=b'9' if self.command != 0 => {
                    self.count = self.count.max(1);
                    if let Some(param) = self.params.get_mut(self.count - 1) {
                        *param = param
                            .saturating_mul(10)
                            .saturating_add((byte - b'0') as u32);
                    }
                }
                b';' if self.command != 0 => self.count = self.count.max(1) + 1,
                // A repeat count is finished by the character it repeats.
                0x3f..=0x7e if self.command == b'!' => {
                    self.command = 0;
                    self.draw(byte - 0x3f, self.params[0].max(1) as usize);
                }
                _ => {
                    self.finish_command();
                    match byte {
                        b'"' | b'#' | b'!' => {
                            self.command = byte;
                            self.params = [0; 5];
                            self.count = 0;
                        }
                        b'$' => self.x = 0,
                        b'-' => {
                            self.x = 0;
                            self.y += 6;
                        }
                        0x3f..=0x7e => self.draw(byte - 0x3f, 1),
                        _ => {}
                    }
                }
            }
        }
    }

    /// The picture, or `None` if nothing was drawn or declared.
    pub fn finish(mut self) -> Option<Sixel> {
        self.finish_command();
        // A declared height counts as far as the bands that were sent, so
        // that a few bytes of raster attributes can't claim a canvas the
        // stream never fills. (xterm, too, goes by what was drawn.)
        let reached = self.y.saturating_add(6);
        self.height = self.height.max(self.declared_height.min(reached));
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let (width, height) = self.reserve(self.width, self.height);
        // Drop the canvas's spare columns and rows.
        let mut pixels = if self.stride == width {
            self.pixels
        } else {
            self.pixels
                .chunks_exact(self.stride * 4)
                .flat_map(|row| &row[..width * 4])
                .copied()
                .collect()
        };
        pixels.truncate(width * height * 4);
        if !self.transparent {
            let background = opaque(self.palette[0]);
            for pixel in pixels.as_chunks_mut::<4>().0 {
                if pixel[3] == 0 {
                    *pixel = background;
                }
            }
        }
        Some(Sixel {
            width: width as u32,
            height: height as u32,
            pixels,
        })
    }

    fn finish_command(&mut self) {
        let [a, b, c, d, e] = self.params;
        match std::mem::take(&mut self.command) {
            // "Pan;Pad;Ph;Pv: the size, when the encoder says. Taking the
            // width now saves laying the canvas out again as it's drawn.
            b'"' if self.count >= 4 => {
                let (width, _) = self.reserve(c as usize, 0);
                self.width = self.width.max(width);
                self.declared_height = d as usize;
            }
            b'#' => {
                self.color = a as usize % REGISTERS;
                // #Pc;Pu;Px;Py;Pz defines it first: 1 is HLS, 2 is RGB.
                if self.count >= 5 {
                    let (d, e) = (d.min(100), e.min(100));
                    match b {
                        1 => self.palette[self.color] = hls(c.min(360), d, e),
                        2 => {
                            self.palette[self.color] = [c.min(100), d, e].map(|v| percent(v as u8))
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// Make the canvas reach `right` × `bottom` pixels, as far as the
    /// limits allow, and say how far that was. Rows are appended; a wider
    /// row means laying the canvas out again, so the width at least doubles.
    fn reserve(&mut self, right: usize, bottom: usize) -> (usize, usize) {
        let rows = self.pixels.len().checked_div(self.stride * 4).unwrap_or(0);
        let widest = (MAX_DIMENSION as usize).min(MAX_PIXELS as usize / rows.max(1));
        let right = right.min(widest);
        if right > self.stride {
            let stride = right.max(self.stride * 2).min(widest);
            let mut wider = vec![0; stride * rows * 4];
            if self.stride > 0 {
                for (old, new) in self
                    .pixels
                    .chunks_exact(self.stride * 4)
                    .zip(wider.chunks_exact_mut(stride * 4))
                {
                    new[..old.len()].copy_from_slice(old);
                }
            }
            self.pixels = wider;
            self.stride = stride;
        }
        let bottom = bottom
            .min(MAX_DIMENSION as usize)
            .min(MAX_PIXELS as usize / self.stride.max(1));
        if bottom > rows {
            self.pixels.resize(self.stride * bottom * 4, 0);
        }
        (right, bottom)
    }

    /// One sixel, `repeat` times: bit 0 is the top row of the band.
    fn draw(&mut self, bits: u8, repeat: usize) {
        let left = self.x;
        self.x = self.x.saturating_add(repeat);
        if bits == 0 {
            return;
        }
        let (right, bottom) = self.reserve(self.x, self.y.saturating_add(6));
        if left >= right {
            return;
        }
        let color = opaque(self.palette[self.color]);
        let top = self.y;
        for row in (top..bottom).filter(|row| bits >> (row - top) & 1 == 1) {
            let start = (row * self.stride + left) * 4;
            let run = &mut self.pixels[start..start + (right - left) * 4];
            run.as_chunks_mut::<4>().0.fill(color);
            self.height = self.height.max(row + 1);
            self.width = self.width.max(right);
        }
    }
}

fn percent(value: u8) -> u8 {
    (value.min(100) as u32 * 255 / 100) as u8
}

fn opaque([r, g, b]: [u8; 3]) -> [u8; 4] {
    [r, g, b, 255]
}

/// DEC's HLS: hue in degrees with blue at 0, red at 120 and green at 240;
/// lightness and saturation in percent.
fn hls(hue: u32, lightness: u32, saturation: u32) -> [u8; 3] {
    let (l, s) = (lightness as f32 / 100.0, saturation as f32 / 100.0);
    let chroma = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let channel = |offset: f32| {
        // The usual HSL wheel has red at 0; DEC's is turned by 120 degrees.
        let h = (hue as f32 + 240.0 + offset).rem_euclid(360.0) / 60.0;
        let x = chroma * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
        let value = match h as u32 {
            0 | 5 => chroma,
            1 | 4 => x,
            _ => 0.0,
        };
        ((value + l - chroma / 2.0) * 255.0).round() as u8
    };
    [channel(0.0), channel(240.0), channel(120.0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(params: &str, body: &str) -> Option<Sixel> {
        // Fed a byte at a time: commands and numbers span chunks.
        let mut decoder = Decoder::new(params.as_bytes());
        for byte in body.bytes() {
            decoder.feed(&[byte]);
        }
        decoder.finish()
    }

    fn pixel(image: &Sixel, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * image.width + x) * 4) as usize;
        image.pixels[at..at + 4].try_into().unwrap()
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];

    #[test]
    fn data_repeats_and_bands_draw_where_they_should() {
        // Red in register 1, blue in register 2. `~` is all six rows, `@`
        // only the top one. Band one: three red columns. Band two: two blue
        // columns, top row only.
        let image = decode("", "#1;2;100;0;0#2;2;0;0;100#1!3~-#2@@").unwrap();
        assert_eq!((image.width, image.height), (3, 7));
        assert_eq!(pixel(&image, 0, 0), RED);
        assert_eq!(pixel(&image, 2, 5), RED);
        assert_eq!(pixel(&image, 1, 6), BLUE);
        // Never drawn, and not asked to be transparent: register 0, which
        // is the VT340's black until the image says otherwise.
        assert_eq!(pixel(&image, 2, 6), [0, 0, 0, 255]);

        // `$` goes back to the left edge to overprint in another colour:
        // red on the top row (`@`), blue on the second (`A`).
        let image = decode("", "#1;2;100;0;0#2;2;0;0;100#1@@$#2AA").unwrap();
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!((pixel(&image, 1, 0), pixel(&image, 1, 1)), (RED, BLUE));
    }

    #[test]
    fn raster_attributes_size_the_picture_and_p2_decides_its_background() {
        // 4 × 8 declared, one pixel drawn, two bands sent.
        let body = "\"1;1;4;8#1;2;100;0;0#1@-";
        let image = decode("0;1;0", body).unwrap();
        assert_eq!((image.width, image.height), (4, 8));
        assert_eq!(pixel(&image, 0, 0), RED);
        assert_eq!(pixel(&image, 3, 7), [0, 0, 0, 0], "P2=1 is transparent");
        // Without it the rest takes register 0, as the image defines it.
        let image = decode("", &format!("#0;2;0;0;100{body}")).unwrap();
        assert_eq!(pixel(&image, 3, 7), BLUE);
        // Declared and never drawn is still a picture; nothing at all isn't.
        assert_eq!(decode("", "\"1;1;2;2").unwrap().pixels.len(), 16);
        // A height the stream never reaches is cut to the bands it sent.
        let image = decode("", "\"1;1;4;10000#1@").unwrap();
        assert_eq!((image.width, image.height), (4, 6));
        assert_eq!(decode("", ""), None);
        assert_eq!(decode("", "#1;2;100;0;0$-"), None);
    }

    #[test]
    fn hls_colours_follow_decs_wheel() {
        assert_eq!(hls(0, 50, 100), [0, 0, 255]);
        assert_eq!(hls(120, 50, 100), [255, 0, 0]);
        assert_eq!(hls(240, 50, 100), [0, 255, 0]);
        assert_eq!(hls(0, 100, 0), [255, 255, 255]);
        assert_eq!(hls(77, 0, 100), [0, 0, 0]);
        let image = decode("", "#5;1;120;50;100#5@").unwrap();
        assert_eq!(pixel(&image, 0, 0), RED);
    }

    #[test]
    fn the_canvas_is_bounded_whatever_the_stream_says() {
        // A repeat count and a raster width far past the limits, and enough
        // bands to run off the bottom: clamped, not allocated.
        let image = decode("", "\"1;1;4000000000;2!4000000000~").unwrap();
        assert_eq!((image.width, image.height), (MAX_DIMENSION, 6));
        let tall = "~-".repeat(3000);
        let image = decode("", &tall).unwrap();
        assert_eq!((image.width, image.height), (1, MAX_DIMENSION));
        // Garbage parameters and stray bytes are skipped.
        assert!(decode("", "#;;;;;;;;99999999999999999999!!\"#\u{7f}\n ~").is_some());
    }
}
