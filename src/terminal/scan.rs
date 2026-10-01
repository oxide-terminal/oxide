//! Byte-level scanner over the raw PTY stream, ahead of the VT parser. It
//! finds what Oxide handles itself: the OSCs `osc.rs` decodes (133, 7, 9,
//! 777), image transfers (kitty's `APC G`, iTerm2's OSC 1337, sixel's
//! `DCS q`), and a few queries alacritty's parser has no answer for.
//!
//! It is resumable across `read` chunks and mirrors vte's rules: BEL or
//! `ESC \` terminates a string, CAN/SUB abort it, other C0 controls inside
//! one are dropped. One rule keeps it and vte in step around images:
//!
//! > Introducers and terminators go to the parser. Bodies never do. An `ESC`
//! > inside a body always goes to the parser.
//!
//! By the time `ESC _ G`, `ESC P q` or `ESC ] 1337;File=` has been
//! recognised, vte has already seen those bytes and is inside a string,
//! waiting for its end. The body is then withheld (megabytes of base64 vte
//! would only buffer or discard), and whatever ends it is handed over, so
//! vte leaves the string exactly when the scanner does.

use std::ops::Range;

use super::osc::{MarkerKind, parse_payload};
use super::sixel::{self, Sixel};

/// What to do with a chunk, in stream order.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// Feed these bytes of the chunk to the VT parser.
    Parse(Range<usize>),
    /// An OSC Oxide consumes finished here.
    Marker(MarkerKind),
    /// A graphics command finished here.
    Graphics(GraphicsCommand),
    /// A query to answer from the PTY thread.
    Reply(Reply),
}

/// An image sequence, undecoded: `graphics.rs` makes sense of it.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphicsCommand {
    /// `ESC _ G … ESC \`: kitty's control keys, `;`, and base64 payload.
    Kitty(Vec<u8>),
    /// OSC 1337 `File=`: the arguments, `:`, and the base64 file.
    ItermFile(Vec<u8>),
    /// OSC 1337 `MultipartFile=`: the arguments of a file sent in parts.
    ItermMultipart(Vec<u8>),
    /// OSC 1337 `FilePart=`: the next piece of its base64.
    ItermPart(Vec<u8>),
    /// OSC 1337 `FileEnd`.
    ItermEnd,
    /// `ESC P … q … ESC \`, already decoded: sixel is streamed through its
    /// decoder as it arrives rather than buffered as text.
    Sixel(Sixel),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reply {
    /// `CSI 16 t`: one cell in pixels.
    CellSize,
    /// `CSI 14 t`: the text area in pixels.
    TextAreaSize,
    /// `CSI > q` (XTVERSION): the terminal's name and version.
    Version,
    /// `CSI ? 1 ; 1 S` (XTSMGRAPHICS): how many sixel colour registers.
    SixelColors,
    /// `CSI ? 2 ; 1 S` (XTSMGRAPHICS): the largest sixel image, in pixels.
    SixelGeometry,
}

/// Longest OSC payload we'll accumulate. OSC 7 carries a path and OSC 777 a
/// notification body; anything longer is not one of ours.
const MAX_PAYLOAD: usize = 4096;
/// A kitty chunk is 4096 bytes of base64 by spec; this is the hard stop.
const MAX_KITTY: usize = 1 << 20;
/// Encoded bytes of one image. Past it the rest is swallowed, not kept.
pub const MAX_ENCODED: usize = 64 << 20;
/// The parameters of the CSIs answered here, and of a sixel's DCS, are a
/// few bytes; a longer one is someone else's.
const MAX_CSI: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Body {
    Kitty,
    ItermFile,
    ItermPart,
    Sixel,
}

impl Body {
    fn limit(self) -> usize {
        match self {
            Body::Kitty => MAX_KITTY,
            Body::ItermFile | Body::ItermPart => MAX_ENCODED,
            // Never buffered; its canvas has limits of its own.
            Body::Sixel => usize::MAX,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
enum State {
    #[default]
    Ground,
    /// Saw ESC; next byte decides.
    Escape,
    /// Inside `ESC ]`, collecting the payload.
    Osc,
    /// Inside the payload and saw ESC: `\` ends it, anything else aborts.
    OscEscape,
    /// Payload overflowed: swallow until the terminator, emit nothing.
    Overflow,
    /// Inside `ESC [`, collecting parameter bytes.
    Csi,
    /// Saw `ESC _`: a `G` makes it kitty graphics.
    Apc,
    /// Inside `ESC P`, collecting parameters: a `q` makes it sixel.
    Dcs,
    /// Inside an image body, which the parser never sees.
    Body(Body),
    /// Inside a body and saw ESC: `\` completes it, anything else aborts.
    BodyEscape(Body),
}

#[derive(Default)]
pub struct Scanner {
    state: State,
    payload: Vec<u8>,
    body: Vec<u8>,
    /// The body passed its limit and was dropped; its end is still awaited.
    body_overflowed: bool,
    /// The sixel being drawn, while inside one.
    sixel: Option<sixel::Decoder>,
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan a chunk. The `Parse` ranges cover every byte the VT parser
    /// should see, in order; a marker, command or reply sits where its
    /// sequence *completed*, so nothing before it is still unparsed and the
    /// cursor sampled there is exact. A sequence split across chunks is
    /// reported in the chunk where it ends.
    pub fn scan(&mut self, chunk: &[u8]) -> Vec<Action> {
        let mut out = Vec::new();
        // Start of the bytes not yet handed to the parser; `None` while a
        // body is being withheld.
        let mut parse_from = (!matches!(self.state, State::Body(_))).then_some(0);
        // Hand over everything up to `end`, then emit `action` there.
        let mut emit = |parse_from: &mut Option<usize>, end: usize, action: Option<Action>| {
            if let Some(start) = parse_from.replace(end)
                && start < end
            {
                out.push(Action::Parse(start..end));
            }
            out.extend(action);
        };

        let mut i = 0;
        while i < chunk.len() {
            let b = chunk[i];
            match self.state {
                State::Ground => match memchr::memchr(0x1b, &chunk[i..]) {
                    Some(offset) => {
                        i += offset;
                        self.state = State::Escape;
                    }
                    None => break,
                },
                State::Escape => {
                    self.payload.clear();
                    self.state = match b {
                        b']' => State::Osc,
                        b'[' => State::Csi,
                        b'_' => State::Apc,
                        b'P' => State::Dcs,
                        0x18 | 0x1a => State::Ground,
                        // vte runs C0 controls and waits on; ESC ESC is
                        // still one escape pending.
                        0x00..=0x1f | 0x7f..=0xff => State::Escape,
                        _ => State::Ground,
                    };
                }
                State::Osc | State::Overflow => match b {
                    0x07 => {
                        if matches!(self.state, State::Osc) {
                            emit(&mut parse_from, i + 1, osc_action(&self.payload));
                        }
                        self.state = State::Ground;
                    }
                    0x1b => self.state = State::OscEscape,
                    0x18 | 0x1a => self.state = State::Ground,
                    0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f => {}
                    _ if matches!(self.state, State::Overflow) => {}
                    _ if self.payload.len() < MAX_PAYLOAD => {
                        self.payload.push(b);
                        let body = match (b, self.payload.as_slice()) {
                            (b'=', b"1337;File=") => Some(Body::ItermFile),
                            (b'=', b"1337;FilePart=") => Some(Body::ItermPart),
                            _ => None,
                        };
                        if let Some(body) = body {
                            emit(&mut parse_from, i + 1, None);
                            parse_from = None;
                            self.begin_body(body);
                        }
                    }
                    _ => {
                        self.payload.clear();
                        self.state = State::Overflow;
                    }
                },
                State::OscEscape => {
                    if b == b'\\' {
                        emit(&mut parse_from, i + 1, osc_action(&self.payload));
                        self.state = State::Ground;
                    } else {
                        // Aborted; this byte belongs to a fresh escape
                        // (`ESC ]` inside an unterminated OSC starts over).
                        self.state = State::Escape;
                        continue;
                    }
                }
                State::Csi => match b {
                    // The queries answered here begin `1`, `>` or `?`. Any
                    // other CSI (most colour changes) is left to the parser
                    // at its first byte.
                    0x20..=0x3f if self.payload.is_empty() && !matches!(b, b'1' | b'>' | b'?') => {
                        self.state = State::Ground
                    }
                    0x20..=0x3f if self.payload.len() < MAX_CSI => self.payload.push(b),
                    0x20..=0x3f => self.state = State::Ground,
                    0x40..=0x7e => {
                        if let Some(reply) = csi_reply(&self.payload, b) {
                            emit(&mut parse_from, i + 1, Some(Action::Reply(reply)));
                        }
                        self.state = State::Ground;
                    }
                    0x1b => self.state = State::Escape,
                    0x18 | 0x1a => self.state = State::Ground,
                    _ => {}
                },
                State::Apc => {
                    if b == b'G' {
                        emit(&mut parse_from, i + 1, None);
                        parse_from = None;
                        self.begin_body(Body::Kitty);
                    } else if b == 0x1b {
                        self.state = State::Escape;
                    } else {
                        // Some other APC; vte swallows it unaided.
                        self.state = State::Ground;
                    }
                }
                State::Dcs => match b {
                    b'0'..=b'9' | b';' if self.payload.len() < MAX_CSI => self.payload.push(b),
                    b'q' => {
                        emit(&mut parse_from, i + 1, None);
                        parse_from = None;
                        self.begin_body(Body::Sixel);
                    }
                    0x1b => self.state = State::Escape,
                    // vte drops C0 controls here and reads on.
                    0x00..=0x17 | 0x19 | 0x1c..=0x1f => {}
                    // An intermediate, a private marker or another final
                    // byte: some other DCS, which vte handles unaided.
                    _ => self.state = State::Ground,
                },
                State::Body(kind) => {
                    let rest = &chunk[i..];
                    let plain = rest.iter().position(|&b| b < 0x20).unwrap_or(rest.len());
                    self.push_body(kind, &rest[..plain]);
                    i += plain;
                    let Some(&control) = chunk.get(i) else { break };
                    // Whatever ends the body is the parser's to see.
                    match control {
                        0x1b => {
                            parse_from = Some(i);
                            self.state = State::BodyEscape(kind);
                        }
                        0x07 if matches!(kind, Body::ItermFile | Body::ItermPart) => {
                            parse_from = Some(i);
                            emit(&mut parse_from, i + 1, self.finish_body(kind));
                        }
                        0x18 | 0x1a => {
                            parse_from = Some(i);
                            self.finish_body(kind);
                        }
                        // Dropped, as vte drops C0 controls inside a string.
                        _ => {}
                    }
                }
                State::BodyEscape(kind) => {
                    if b == b'\\' {
                        emit(&mut parse_from, i + 1, self.finish_body(kind));
                    } else {
                        self.finish_body(kind);
                        self.state = State::Escape;
                        continue;
                    }
                }
            }
            i += 1;
        }
        if parse_from.is_some() {
            emit(&mut parse_from, chunk.len(), None);
        }
        out
    }

    fn begin_body(&mut self, kind: Body) {
        self.body.clear();
        self.body_overflowed = false;
        self.sixel = (kind == Body::Sixel).then(|| sixel::Decoder::new(&self.payload));
        self.state = State::Body(kind);
    }

    fn push_body(&mut self, kind: Body, bytes: &[u8]) {
        if let Some(decoder) = &mut self.sixel {
            return decoder.feed(bytes);
        }
        if self.body_overflowed {
            return;
        }
        if self.body.len() + bytes.len() > kind.limit() {
            self.body = Vec::new();
            self.body_overflowed = true;
        } else {
            self.body.extend_from_slice(bytes);
        }
    }

    /// Leave the body. Yields its command, unless it overflowed; a caller
    /// that is aborting just drops it. The buffer is handed over whole, so
    /// a large image leaves no capacity pinned here.
    fn finish_body(&mut self, kind: Body) -> Option<Action> {
        self.state = State::Ground;
        let body = std::mem::take(&mut self.body);
        let command = match kind {
            Body::Kitty => GraphicsCommand::Kitty(body),
            Body::ItermFile => GraphicsCommand::ItermFile(body),
            Body::ItermPart => GraphicsCommand::ItermPart(body),
            Body::Sixel => GraphicsCommand::Sixel(self.sixel.take()?.finish()?),
        };
        (!self.body_overflowed).then_some(Action::Graphics(command))
    }
}

/// What a finished OSC payload (everything between `ESC ]` and the
/// terminator) asks for, or `None` for sequences that aren't ours.
fn osc_action(payload: &[u8]) -> Option<Action> {
    if let Some(args) = payload.strip_prefix(b"1337;MultipartFile=") {
        return Some(Action::Graphics(GraphicsCommand::ItermMultipart(
            args.to_vec(),
        )));
    }
    if payload == b"1337;FileEnd" {
        return Some(Action::Graphics(GraphicsCommand::ItermEnd));
    }
    parse_payload(payload).map(Action::Marker)
}

fn csi_reply(params: &[u8], final_byte: u8) -> Option<Reply> {
    match (params, final_byte) {
        (b"16", b't') => Some(Reply::CellSize),
        // Every form alacritty's parser reads as "text area in pixels".
        (b"14", b't') => Some(Reply::TextAreaSize),
        (p, b't') if p.starts_with(b"14;") => Some(Reply::TextAreaSize),
        (b">" | b">0", b'q') => Some(Reply::Version),
        // Read (1) or read the maximum (4): the answer is the same.
        (b"?1;1" | b"?1;4", b'S') => Some(Reply::SixelColors),
        (b"?2;1" | b"?2;4", b'S') => Some(Reply::SixelGeometry),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(scanner: &mut Scanner, chunk: &[u8]) -> Vec<MarkerKind> {
        scanner
            .scan(chunk)
            .into_iter()
            .filter_map(|action| match action {
                Action::Marker(kind) => Some(kind),
                _ => None,
            })
            .collect()
    }

    /// The offset each marker was reported at: the end of the last `Parse`
    /// range before it.
    fn marker_offsets(scanner: &mut Scanner, chunk: &[u8]) -> Vec<usize> {
        let mut end = 0;
        let mut offsets = Vec::new();
        for action in scanner.scan(chunk) {
            match action {
                Action::Parse(range) => end = range.end,
                Action::Marker(_) => offsets.push(end),
                _ => {}
            }
        }
        offsets
    }

    /// Scan `stream` cut into two chunks at `split`: the bytes the parser
    /// was handed, and everything else that came out.
    fn run(stream: &[u8], split: usize) -> (Vec<u8>, Vec<Action>) {
        let mut scanner = Scanner::new();
        let mut parsed = Vec::new();
        let mut others = Vec::new();
        for chunk in [&stream[..split], &stream[split..]] {
            for action in scanner.scan(chunk) {
                match action {
                    Action::Parse(range) => parsed.extend_from_slice(&chunk[range]),
                    other => others.push(other),
                }
            }
        }
        (parsed, others)
    }

    #[test]
    fn recognises_each_marker_with_both_terminators() {
        let mut s = Scanner::new();
        assert_eq!(
            kinds(&mut s, b"\x1b]133;A\x1b\\"),
            vec![MarkerKind::PromptStart]
        );
        assert_eq!(
            kinds(&mut s, b"\x1b]133;B\x07"),
            vec![MarkerKind::InputStart]
        );
        assert_eq!(
            kinds(&mut s, b"\x1b]133;C\x1b\\"),
            vec![MarkerKind::CommandStart { cmdline: None }]
        );
        assert_eq!(
            kinds(&mut s, b"\x1b]133;D;0\x07"),
            vec![MarkerKind::CommandEnd { exit: Some(0) }]
        );
        assert_eq!(
            kinds(&mut s, b"\x1b]133;D;127\x1b\\"),
            vec![MarkerKind::CommandEnd { exit: Some(127) }]
        );
    }

    #[test]
    fn reports_the_offset_just_past_the_terminator() {
        let mut s = Scanner::new();
        let chunk = b"hello\x1b]133;A\x07world";
        let found = marker_offsets(&mut s, chunk);
        assert_eq!(found, vec![13]);
        assert_eq!(&chunk[found[0]..], b"world");
        assert_eq!(marker_offsets(&mut s, b"\x1b]133;B\x1b\\x"), vec![9]);
    }

    #[test]
    fn every_byte_without_a_body_reaches_the_parser() {
        let stream = b"hello\x1b]133;A\x07wor\x1b[16tld\x1b]0;title\x07";
        for split in 0..=stream.len() {
            let (parsed, others) = run(stream, split);
            assert_eq!(parsed, stream, "split at {split}");
            assert_eq!(
                others,
                vec![
                    Action::Marker(MarkerKind::PromptStart),
                    Action::Reply(Reply::CellSize)
                ]
            );
        }
    }

    #[test]
    fn survives_splits_at_every_byte_boundary() {
        let seq = b"prompt\x1b]133;D;1\x1b\\tail";
        for split in 0..seq.len() {
            let mut s = Scanner::new();
            let mut got = kinds(&mut s, &seq[..split]);
            got.extend(kinds(&mut s, &seq[split..]));
            assert_eq!(
                got,
                vec![MarkerKind::CommandEnd { exit: Some(1) }],
                "split at {split}"
            );
        }
    }

    #[test]
    fn missing_or_bad_exit_code_is_none_not_a_panic() {
        let mut s = Scanner::new();
        assert_eq!(
            kinds(&mut s, b"\x1b]133;D\x07"),
            vec![MarkerKind::CommandEnd { exit: None }]
        );
        assert_eq!(
            kinds(&mut s, b"\x1b]133;D;\x07"),
            vec![MarkerKind::CommandEnd { exit: None }]
        );
        assert_eq!(
            kinds(&mut s, b"\x1b]133;D;abc\x07"),
            vec![MarkerKind::CommandEnd { exit: None }]
        );
    }

    #[test]
    fn cmdline_is_extracted_including_semicolons() {
        let mut s = Scanner::new();
        assert_eq!(
            kinds(&mut s, b"\x1b]133;C;cmdline=cargo build; echo done\x07"),
            vec![MarkerKind::CommandStart {
                cmdline: Some("cargo build; echo done".into())
            }]
        );
        assert_eq!(
            kinds(&mut s, b"\x1b]133;C;cmdline=\x07"),
            vec![MarkerKind::CommandStart { cmdline: None }]
        );
    }

    #[test]
    fn ignores_unrelated_and_malformed_sequences() {
        let mut s = Scanner::new();
        assert!(kinds(&mut s, b"\x1b]0;title\x07").is_empty());
        assert!(kinds(&mut s, b"\x1b]133;Z\x07").is_empty());
        assert!(kinds(&mut s, b"\x1b[31mred\x1b[0m").is_empty());
        // CAN aborts; the next real marker still parses.
        assert_eq!(
            kinds(&mut s, b"\x1b]133;A\x18\x1b]133;B\x07"),
            vec![MarkerKind::InputStart]
        );
        // An unterminated OSC followed by a fresh `ESC ]` starts over.
        assert_eq!(
            kinds(&mut s, b"\x1b]133;A\x1b]133;B\x07"),
            vec![MarkerKind::InputStart]
        );
        // A control character inside the payload is dropped, like vte does.
        assert_eq!(
            kinds(&mut s, b"\x1b]133;C;cmdline=a\nb\x07"),
            vec![MarkerKind::CommandStart {
                cmdline: Some("ab".into())
            }]
        );
    }

    #[test]
    fn overlong_payload_is_dropped_and_scanner_recovers() {
        let mut s = Scanner::new();
        let mut junk = b"\x1b]133;C;cmdline=".to_vec();
        junk.extend(std::iter::repeat_n(b'x', MAX_PAYLOAD + 10));
        junk.extend_from_slice(b"\x07\x1b]133;A\x07");
        assert_eq!(kinds(&mut s, &junk), vec![MarkerKind::PromptStart]);
    }

    #[test]
    fn queries_are_recognised_and_nothing_else_is() {
        let replies = |bytes: &[u8]| -> Vec<Action> { run(bytes, 0).1 };
        assert_eq!(replies(b"\x1b[16t"), vec![Action::Reply(Reply::CellSize)]);
        assert_eq!(
            replies(b"\x1b[14t\x1b[14;2t"),
            vec![
                Action::Reply(Reply::TextAreaSize),
                Action::Reply(Reply::TextAreaSize)
            ]
        );
        assert_eq!(
            replies(b"\x1b[>q\x1b[>0q"),
            vec![Action::Reply(Reply::Version), Action::Reply(Reply::Version)]
        );
        assert_eq!(
            replies(b"\x1b[?1;1S\x1b[?2;1S"),
            vec![
                Action::Reply(Reply::SixelColors),
                Action::Reply(Reply::SixelGeometry)
            ]
        );
        // Cursor style, a window title push, SGR, DA1: not ours.
        assert!(replies(b"\x1b[2 q\x1b[22;0t\x1b[18t\x1b[1;31m\x1b[c\x1b[>c").is_empty());
    }

    /// Each image sequence, cut at every byte boundary: the same command
    /// comes out, and the parser gets the introducer, the terminator and
    /// the text around them, never a byte of the body.
    #[test]
    fn bodies_never_reach_the_parser_wherever_the_chunk_splits() {
        let red_pixel = Sixel {
            width: 1,
            height: 1,
            pixels: vec![255, 0, 0, 255],
        };
        let cases: [(&[u8], &[u8], GraphicsCommand); 5] = [
            (
                b"a\x1bP0;1q#1;2;100;0;0#1@\x1b\\b",
                b"a\x1bP0;1q\x1b\\b",
                GraphicsCommand::Sixel(red_pixel),
            ),
            (
                b"a\x1b_Gi=1,f=24;QUJD\x1b\\b",
                b"a\x1b_G\x1b\\b",
                GraphicsCommand::Kitty(b"i=1,f=24;QUJD".to_vec()),
            ),
            (
                b"a\x1b]1337;File=inline=1:QUJD\x07b",
                b"a\x1b]1337;File=\x07b",
                GraphicsCommand::ItermFile(b"inline=1:QUJD".to_vec()),
            ),
            (
                b"a\x1b]1337;FilePart=QUJD\x1b\\b",
                b"a\x1b]1337;FilePart=\x1b\\b",
                GraphicsCommand::ItermPart(b"QUJD".to_vec()),
            ),
            (
                b"a\x1b]1337;MultipartFile=inline=1\x07\x1b]1337;FileEnd\x07b",
                b"a\x1b]1337;MultipartFile=inline=1\x07\x1b]1337;FileEnd\x07b",
                GraphicsCommand::ItermMultipart(b"inline=1".to_vec()),
            ),
        ];
        for (stream, expect_parsed, expect_command) in cases {
            for split in 0..=stream.len() {
                let (parsed, others) = run(stream, split);
                assert_eq!(parsed, expect_parsed, "split at {split}");
                assert_eq!(
                    others.first(),
                    Some(&Action::Graphics(expect_command.clone())),
                    "split at {split}"
                );
            }
        }
    }

    #[test]
    fn an_escape_inside_a_body_aborts_it_and_goes_to_the_parser() {
        // The ESC that isn't followed by `\` starts a CSI the parser must
        // see whole; the half-sent image is dropped.
        let (parsed, others) = run(b"\x1b_Gi=1;QUJD\x1b[31mred", 0);
        assert_eq!(parsed, b"\x1b_G\x1b[31mred");
        assert!(others.is_empty());
        // ...and when it starts another image, that one is read.
        let (parsed, others) = run(b"\x1b_Gi=1;QU\x1b_Gi=2;QUJD\x1b\\", 0);
        assert_eq!(parsed, b"\x1b_G\x1b_G\x1b\\");
        assert_eq!(
            others,
            vec![Action::Graphics(GraphicsCommand::Kitty(
                b"i=2;QUJD".to_vec()
            ))]
        );
    }

    #[test]
    fn can_and_sub_abort_a_body_like_any_string() {
        for abort in [0x18u8, 0x1a] {
            let mut stream = b"\x1b]1337;File=inline=1:QUJD".to_vec();
            stream.push(abort);
            stream.extend_from_slice(b"text\x1b]133;A\x07");
            let (parsed, others) = run(&stream, 0);
            let mut expect = b"\x1b]1337;File=".to_vec();
            expect.push(abort);
            expect.extend_from_slice(b"text\x1b]133;A\x07");
            assert_eq!(parsed, expect);
            assert_eq!(others, vec![Action::Marker(MarkerKind::PromptStart)]);
        }
    }

    #[test]
    fn control_characters_inside_a_body_are_dropped() {
        // Line-wrapped base64, and a BEL that only ends OSC strings.
        let (_, others) = run(b"\x1b_Gi=1;QU\r\nJD\x07\x1b\\", 0);
        assert_eq!(
            others,
            vec![Action::Graphics(GraphicsCommand::Kitty(
                b"i=1;QUJD".to_vec()
            ))]
        );
    }

    #[test]
    fn only_a_plain_q_makes_a_dcs_sixel() {
        // DECRQSS (`$q`) and XTGETTCAP (`+q`) have intermediates, and a BEL
        // doesn't end a DCS: everything goes to the parser.
        for stream in [
            &b"\x1bP$qm\x1b\\"[..],
            b"\x1bP+q544e\x1b\\",
            b"\x1bP1$r0m\x1b\\",
            b"\x1bP>|name\x1b\\",
        ] {
            let (parsed, others) = run(stream, 0);
            assert_eq!(parsed, stream);
            assert!(others.is_empty(), "{others:?}");
        }
        let (parsed, others) = run(b"\x1bPq#1@\x07@\x1b\\", 0);
        assert_eq!(parsed, b"\x1bPq\x1b\\");
        assert!(matches!(
            &others[..],
            [Action::Graphics(GraphicsCommand::Sixel(Sixel {
                width: 2,
                ..
            }))]
        ));
        // An empty one draws nothing and reports nothing.
        assert!(run(b"\x1bPq\x1b\\", 0).1.is_empty());
    }

    #[test]
    fn an_oversized_body_is_swallowed_and_the_scanner_recovers() {
        let mut stream = b"\x1b_Gi=1;".to_vec();
        stream.extend(std::iter::repeat_n(b'A', MAX_KITTY + 1));
        stream.extend_from_slice(b"\x1b\\\x1b_Gi=2;QUJD\x1b\\ok");
        // In two pieces, so the overflow is carried across a chunk.
        let (parsed, others) = run(&stream, MAX_KITTY / 2);
        assert_eq!(parsed, b"\x1b_G\x1b\\\x1b_G\x1b\\ok");
        assert_eq!(
            others,
            vec![Action::Graphics(GraphicsCommand::Kitty(
                b"i=2;QUJD".to_vec()
            ))]
        );
    }
}
