use std::hash::{Hash, Hasher};

use alacritty_terminal::index::Point as GridPoint;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape};
use gpui::{
    App, BorderStyle, Bounds, DispatchPhase, Element, ElementId, Entity, Font, FontFallbacks,
    FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine, SharedString,
    StrikethroughStyle, Style, TextRun, UnderlineStyle, Window, fill, point, px, quad, relative,
    size,
};

use super::TerminalPane;
use super::colors::{blend, resolve};
use super::session::TermSize;
use crate::config::Theme;
use crate::config::schema::{FontWeightName, UnfocusedCursor};

/// One cell copied out of the grid while the term lock is held.
struct CellSnap {
    c: char,
    zerowidth: Option<Vec<char>>,
    fg: AnsiColor,
    bg: AnsiColor,
    flags: Flags,
}

struct CursorLayout {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    color: Hsla,
    /// Bar width / underline height in pixels, from `cursor.thickness`.
    thickness: Pixels,
    /// For block cursors: the glyph underneath, re-shaped in the background color.
    glyph: Option<ShapedLine>,
}

pub struct GridLayout {
    origin: Point<Pixels>,
    cell_height: f32,
    bg_quads: Vec<PaintQuad>,
    selection_quads: Vec<PaintQuad>,
    lines: Vec<(usize, ShapedLine)>,
    cursor: Option<CursorLayout>,
}

pub struct TerminalElement {
    pane: Entity<TerminalPane>,
    focused: bool,
}

impl TerminalElement {
    pub fn new(pane: Entity<TerminalPane>, focused: bool) -> Self {
        Self { pane, focused }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = GridLayout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> GridLayout {
        let focused = self.focused;
        self.pane
            .update(cx, |pane, _cx| layout_grid(pane, bounds, focused, window))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        layout: &mut GridLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        // The pane's own mouse handlers stop at its edges. A drag doesn't,
        // whether it's our selection or a program's: outside them it
        // scrolls the view, and ends on release.
        let pane = self.pane.read(cx);
        if pane.selecting || pane.reporting.is_some() {
            let pane = self.pane.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture
                    && event.pressed_button == Some(MouseButton::Left)
                    && !bounds.contains(&event.position)
                {
                    pane.update(cx, |pane, cx| pane.on_mouse_move(event, window, cx));
                }
            });
            let pane = self.pane.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture
                    && event.button == MouseButton::Left
                    && !bounds.contains(&event.position)
                {
                    pane.update(cx, |pane, cx| pane.on_mouse_up(event, window, cx));
                }
            });
        }
        for q in layout.bg_quads.drain(..) {
            window.paint_quad(q);
        }
        for q in layout.selection_quads.drain(..) {
            window.paint_quad(q);
        }
        let line_height = px(layout.cell_height);
        for (row, line) in &layout.lines {
            let origin = point(
                layout.origin.x,
                layout.origin.y + px(*row as f32 * layout.cell_height),
            );
            line.paint(origin, line_height, window, cx).ok();
        }
        if let Some(cursor) = &layout.cursor {
            match cursor.shape {
                CursorShape::Block => {
                    window.paint_quad(fill(cursor.bounds, cursor.color));
                    if let Some(glyph) = &cursor.glyph {
                        glyph
                            .paint(cursor.bounds.origin, cursor.bounds.size.height, window, cx)
                            .ok();
                    }
                }
                CursorShape::HollowBlock => {
                    window.paint_quad(quad(
                        cursor.bounds,
                        px(0.0),
                        gpui::transparent_black(),
                        px(1.0),
                        cursor.color,
                        BorderStyle::Solid,
                    ));
                }
                CursorShape::Beam => {
                    let mut b = cursor.bounds;
                    b.size.width = cursor.thickness;
                    window.paint_quad(fill(b, cursor.color));
                }
                CursorShape::Underline => {
                    let mut b = cursor.bounds;
                    b.origin.y = b.origin.y + b.size.height - cursor.thickness;
                    b.size.height = cursor.thickness;
                    window.paint_quad(fill(b, cursor.color));
                }
                CursorShape::Hidden => {}
            }
        }
    }
}

/// The glyph to shape for a cell. Alacritty stores the literal control
/// character in the cell where e.g. a TAB began and pads the span with spaces;
/// shaping a real '\t' would use the font's tab advance instead of one cell
/// and skew everything after it (visible as ragged `ls` columns).
fn display_char(c: char) -> char {
    if (c as u32) < 0x20 || c == '\u{7f}' {
        ' '
    } else {
        c
    }
}

fn base_font(pane: &TerminalPane, bold: bool, italic: bool) -> Font {
    let weight = match (pane.config().font.weight, bold) {
        (_, true) => FontWeight::BOLD,
        (FontWeightName::Normal, false) => FontWeight::NORMAL,
        (FontWeightName::Medium, false) => FontWeight::MEDIUM,
        (FontWeightName::Bold, false) => FontWeight::BOLD,
    };
    let family = &pane.config().font.family;
    Font {
        family: SharedString::from(family.primary().to_string()),
        features: if pane.config().font.ligatures {
            Default::default()
        } else {
            gpui::FontFeatures::disable_ligatures()
        },
        fallbacks: (!family.fallbacks().is_empty())
            .then(|| FontFallbacks::from_fonts(family.fallbacks().to_vec())),
        weight,
        style: if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
    }
}

fn color_key(c: Hsla) -> u64 {
    let rgba: gpui::Rgba = c.into();
    let r = (rgba.r * 255.0) as u64;
    let g = (rgba.g * 255.0) as u64;
    let b = (rgba.b * 255.0) as u64;
    let a = (rgba.a * 255.0) as u64;
    (r << 24) | (g << 16) | (b << 8) | a
}

fn layout_grid(
    pane: &mut TerminalPane,
    bounds: Bounds<Pixels>,
    focused: bool,
    window: &mut Window,
) -> GridLayout {
    let theme: Theme = (**pane.theme()).clone();
    let pad_x = pane.config().window.padding.x;
    let pad_y = pane.config().window.padding.y;
    let font_size = px((pane.config().font.size + pane.font_delta).max(6.0));
    let line_height_mult = pane.config().font.line_height.max(1.0);

    // Measure the cell. cell_width stays the exact glyph advance so painted
    // runs, background quads, and the cursor all use the same column math;
    // cell_height is rounded to whole pixels so rows don't accumulate error.
    let font = base_font(pane, false, false);
    let text_system = window.text_system().clone();
    let font_id = text_system.resolve_font(&font);
    let cell_width = text_system
        .advance(font_id, font_size, 'm')
        .map(|s| f32::from(s.width))
        .unwrap_or(8.0);
    let cell_height = (f32::from(font_size) * line_height_mult).round();

    let origin = point(bounds.origin.x + px(pad_x), bounds.origin.y + px(pad_y));
    let avail_w = f32::from(bounds.size.width) - pad_x * 2.0;
    let avail_h = f32::from(bounds.size.height) - pad_y * 2.0;
    let columns = ((avail_w / cell_width).floor() as usize).max(2);
    let screen_lines = ((avail_h / cell_height).floor() as usize).max(1);

    let new_size = TermSize {
        columns,
        screen_lines,
        cell_width,
        cell_height,
    };
    let grid_changed = columns != pane.size.columns || screen_lines != pane.size.screen_lines;
    pane.size = new_size;
    if grid_changed {
        // Resize only when the *cell* dimensions changed — this is the
        // debounce that prevents SIGWINCH storms during window drags.
        if let Some(session) = &pane.session {
            session.resize(new_size);
        }
    }

    let mut layout = GridLayout {
        origin,
        cell_height,
        bg_quads: Vec::new(),
        selection_quads: Vec::new(),
        lines: Vec::with_capacity(screen_lines),
        cursor: None,
    };
    let Some(session) = &pane.session else {
        pane.last_layout = Some(super::LastLayout {
            bounds,
            cell_width,
            cell_height,
            display_offset: 0,
        });
        return layout;
    };

    // --- Lock the term, copy out, release. Never hold this into shaping. ---
    let mut rows: Vec<Vec<CellSnap>> = (0..screen_lines).map(|_| Vec::new()).collect();
    let (cursor, display_offset, selection, mode, cursor_style);
    {
        let term = session.term.lock();
        let content = term.renderable_content();
        display_offset = content.display_offset;
        selection = content.selection;
        cursor = content.cursor;
        mode = content.mode;
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + display_offset as i32;
            if row < 0 || row as usize >= screen_lines {
                continue;
            }
            let cell = &indexed.cell;
            rows[row as usize].push(CellSnap {
                c: cell.c,
                zerowidth: cell.zerowidth().map(|z| z.to_vec()),
                fg: cell.fg,
                bg: cell.bg,
                flags: cell.flags,
            });
        }
        cursor_style = term.cursor_style();
        drop(term);
    }

    pane.last_layout = Some(super::LastLayout {
        bounds,
        cell_width,
        cell_height,
        display_offset,
    });

    // --- Shape rows (with a per-frame cache) and build quads. ---
    pane.prev_shape_cache = std::mem::take(&mut pane.shape_cache);

    let default_bg = theme.background;
    for (row_idx, row) in rows.iter().enumerate() {
        let row_y = origin.y + px(row_idx as f32 * cell_height);

        // Background + selection quads, coalescing adjacent same-color cells.
        let mut col = 0usize;
        let mut open_bg: Option<(usize, usize, Hsla)> = None; // (start, end_exclusive, color)
        let mut open_sel: Option<(usize, usize)> = None;
        for cell in row {
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = resolve(cell.fg, &theme);
            let mut bg = resolve(cell.bg, &theme);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if color_key(bg) != color_key(default_bg) {
                open_bg = match open_bg {
                    Some((start, end, color))
                        if end == col && color_key(color) == color_key(bg) =>
                    {
                        Some((start, col + width, color))
                    }
                    Some((start, end, color)) => {
                        layout.bg_quads.push(cell_run_quad(
                            origin,
                            row_y,
                            start,
                            end,
                            cell_width,
                            cell_height,
                            color,
                        ));
                        Some((col, col + width, bg))
                    }
                    None => Some((col, col + width, bg)),
                };
            } else if let Some((start, end, color)) = open_bg.take() {
                layout.bg_quads.push(cell_run_quad(
                    origin,
                    row_y,
                    start,
                    end,
                    cell_width,
                    cell_height,
                    color,
                ));
            }

            let grid_point = GridPoint::new(
                alacritty_terminal::index::Line(row_idx as i32 - display_offset as i32),
                alacritty_terminal::index::Column(col),
            );
            let selected = selection.is_some_and(|r| r.contains(grid_point));
            if selected {
                open_sel = match open_sel {
                    Some((start, end)) if end == col => Some((start, col + width)),
                    Some((start, end)) => {
                        layout.selection_quads.push(cell_run_quad(
                            origin,
                            row_y,
                            start,
                            end,
                            cell_width,
                            cell_height,
                            theme.selection_bg,
                        ));
                        Some((col, col + width))
                    }
                    None => Some((col, col + width)),
                };
            } else if let Some((start, end)) = open_sel.take() {
                layout.selection_quads.push(cell_run_quad(
                    origin,
                    row_y,
                    start,
                    end,
                    cell_width,
                    cell_height,
                    theme.selection_bg,
                ));
            }
            col += width;
        }
        if let Some((start, end, color)) = open_bg {
            layout.bg_quads.push(cell_run_quad(
                origin,
                row_y,
                start,
                end,
                cell_width,
                cell_height,
                color,
            ));
        }
        if let Some((start, end)) = open_sel {
            layout.selection_quads.push(cell_run_quad(
                origin,
                row_y,
                start,
                end,
                cell_width,
                cell_height,
                theme.selection_bg,
            ));
        }

        // Text runs: coalesce consecutive cells sharing style.
        let selected_line = alacritty_terminal::index::Line(row_idx as i32 - display_offset as i32);
        let shaped = shape_row(
            pane,
            row,
            &theme,
            font_size,
            &text_system,
            selection.as_ref(),
            selected_line,
        );
        if let Some(shaped) = shaped {
            layout.lines.push((row_idx, shaped));
        }
    }

    // --- cmd-hover underline. ---
    if let Some(span) = pane.hover
        && span.row < screen_lines
        && span.end > span.start
    {
        let y = origin.y + px((span.row + 1) as f32 * cell_height - 1.5);
        layout.selection_quads.push(fill(
            Bounds {
                origin: point(origin.x + px(span.start as f32 * cell_width), y),
                size: size(px((span.end - span.start) as f32 * cell_width), px(1.0)),
            },
            theme.foreground,
        ));
    }

    // --- Cursor. ---
    let cursor_row = cursor.point.line.0 + display_offset as i32;
    let cursor_on_screen = cursor_row >= 0 && (cursor_row as usize) < screen_lines;
    let vi_mode = mode.contains(TermMode::VI);
    let cursor_config = &pane.config().cursor;
    let unfocused_hidden = !focused && cursor_config.unfocused == UnfocusedCursor::Hidden;
    if (mode.contains(TermMode::SHOW_CURSOR) || vi_mode)
        && cursor_on_screen
        && pane.child_exited.is_none()
        && !unfocused_hidden
    {
        let row_idx = cursor_row as usize;
        let col = cursor.point.column.0;
        let cell = rows.get(row_idx).and_then(|r| {
            let mut c = 0usize;
            for snap in r {
                if snap.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let width = if snap.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                if col >= c && col < c + width {
                    return Some((snap, c));
                }
                c += width;
            }
            None
        });
        let wide = cell.is_some_and(|(snap, _)| snap.flags.contains(Flags::WIDE_CHAR));
        let width_cells = if wide { 2.0 } else { 1.0 };
        // The vi cursor is always a block, never blinks, and stays visible
        // unfocused, so the copy-mode position is never in doubt.
        let shape = if vi_mode {
            CursorShape::Block
        } else if focused || cursor_config.unfocused == UnfocusedCursor::Solid {
            cursor.shape
        } else {
            CursorShape::HollowBlock
        };
        let shape =
            match cursor_style.blinking && focused && !vi_mode && shape != CursorShape::Hidden {
                true if !pane.blink_show => CursorShape::Hidden,
                _ => shape,
            };
        let thickness = px((cursor_config.thickness * cell_width).max(1.0).round());
        let cursor_color = if vi_mode { theme.ansi[3] } else { theme.cursor };
        let cursor_bounds = Bounds {
            origin: point(
                origin.x + px(col as f32 * cell_width),
                origin.y + px(row_idx as f32 * cell_height),
            ),
            size: size(px(cell_width * width_cells), px(cell_height)),
        };
        let glyph = cell.and_then(|(snap, _)| {
            if shape != CursorShape::Block || snap.c == ' ' {
                return None;
            }
            let mut text = String::new();
            text.push(display_char(snap.c));
            if let Some(zw) = &snap.zerowidth {
                text.extend(zw.iter());
            }
            let run = TextRun {
                len: text.len(),
                font: base_font(
                    pane,
                    snap.flags.contains(Flags::BOLD),
                    snap.flags.contains(Flags::ITALIC),
                ),
                color: theme.background,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            Some(text_system.shape_line(SharedString::from(text), font_size, &[run], None))
        });
        layout.cursor = Some(CursorLayout {
            bounds: cursor_bounds,
            shape,
            color: cursor_color,
            thickness,
            glyph,
        });
    }

    layout
}

fn cell_run_quad(
    origin: Point<Pixels>,
    row_y: Pixels,
    start: usize,
    end: usize,
    cell_width: f32,
    cell_height: f32,
    color: Hsla,
) -> PaintQuad {
    fill(
        Bounds {
            origin: point(origin.x + px(start as f32 * cell_width), row_y),
            size: size(px((end - start) as f32 * cell_width), px(cell_height)),
        },
        color,
    )
}

fn shape_row(
    pane: &mut TerminalPane,
    row: &[CellSnap],
    theme: &Theme,
    font_size: Pixels,
    text_system: &std::sync::Arc<gpui::WindowTextSystem>,
    selection: Option<&SelectionRange>,
    line: alacritty_terminal::index::Line,
) -> Option<ShapedLine> {
    // Trim trailing default-styled blanks so we don't shape padding.
    let last = row.iter().rposition(|cell| {
        !(display_char(cell.c) == ' '
            && cell.zerowidth.is_none()
            && !cell.flags.intersects(
                Flags::INVERSE
                    | Flags::UNDERLINE
                    | Flags::DOUBLE_UNDERLINE
                    | Flags::UNDERCURL
                    | Flags::STRIKEOUT,
            ))
    })?;

    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    f32::from(font_size).to_bits().hash(&mut hasher);

    // Only a configured selection_fg recolours selected text; otherwise the
    // selection is just the background quad and the cache key is untouched.
    let selection_fg = theme.selection_fg.zip(selection);
    let mut col = 0usize;
    for cell in &row[..=last] {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let cell_col = col;
        col += width;
        let mut fg = resolve(cell.fg, theme);
        let mut bg = resolve(cell.bg, theme);
        // Brighten bold indexed colors 0-7 to 8-15.
        if cell.flags.contains(Flags::BOLD) {
            if let AnsiColor::Indexed(i @ 0..=7) = cell.fg {
                fg = theme.ansi[i as usize + 8];
            } else if let AnsiColor::Named(named) = cell.fg {
                let idx = named as usize;
                if idx < 8 {
                    fg = theme.ansi[idx + 8];
                }
            }
        }
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if cell.flags.contains(Flags::DIM) {
            fg = blend(fg, bg, 0.4);
        }
        if let Some((sel_fg, range)) = selection_fg
            && range.contains(GridPoint::new(
                line,
                alacritty_terminal::index::Column(cell_col),
            ))
        {
            fg = sel_fg;
        }

        let bold = cell.flags.contains(Flags::BOLD);
        let italic = cell.flags.contains(Flags::ITALIC);
        let underline = if cell
            .flags
            .intersects(Flags::UNDERLINE | Flags::DOUBLE_UNDERLINE | Flags::UNDERCURL)
        {
            Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(fg),
                wavy: cell.flags.contains(Flags::UNDERCURL),
            })
        } else {
            None
        };
        let strikethrough = if cell.flags.contains(Flags::STRIKEOUT) {
            Some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(fg),
            })
        } else {
            None
        };

        let start_len = text.len();
        text.push(display_char(cell.c));
        if let Some(zw) = &cell.zerowidth {
            text.extend(zw.iter());
        }
        let added = text.len() - start_len;

        let style_key = (
            color_key(fg),
            bold,
            italic,
            underline.is_some(),
            cell.flags.contains(Flags::UNDERCURL),
            strikethrough.is_some(),
        );
        match runs.last_mut() {
            Some(run)
                if color_key(run.color) == style_key.0
                    && run.font.weight
                        == if bold {
                            FontWeight::BOLD
                        } else {
                            base_font(pane, false, italic).weight
                        }
                    && (run.font.style == FontStyle::Italic) == italic
                    && run.underline.is_some() == underline.is_some()
                    && run.strikethrough.is_some() == strikethrough.is_some() =>
            {
                run.len += added;
            }
            _ => {
                runs.push(TextRun {
                    len: added,
                    font: base_font(pane, bold, italic),
                    color: fg,
                    background_color: None,
                    underline,
                    strikethrough,
                });
            }
        }
        style_key.hash(&mut hasher);
    }

    if text.trim_end().is_empty()
        && runs
            .iter()
            .all(|r| r.underline.is_none() && r.strikethrough.is_none())
    {
        // A row of plain spaces with non-default colors still got bg quads;
        // nothing to shape.
        if row[..=last].iter().all(|c| display_char(c.c) == ' ') {
            return None;
        }
    }

    text.hash(&mut hasher);
    let key = hasher.finish();
    if let Some(line) = pane.prev_shape_cache.remove(&key) {
        pane.shape_cache.insert(key, line.clone());
        return Some(line);
    }
    if let Some(line) = pane.shape_cache.get(&key) {
        return Some(line.clone());
    }
    let line = text_system.shape_line(SharedString::from(text), font_size, &runs, None);
    pane.shape_cache.insert(key, line.clone());
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::display_char;

    #[test]
    fn control_chars_render_as_single_cell_blanks() {
        // macOS `ls` pads columns with TABs; shaping a real tab glyph would
        // advance by the font's tab width instead of one cell.
        assert_eq!(display_char('\t'), ' ');
        assert_eq!(display_char('\r'), ' ');
        assert_eq!(display_char('\u{0}'), ' ');
        assert_eq!(display_char('\u{7f}'), ' ');
        // Printable characters, including wide and combining ones, pass through.
        assert_eq!(display_char('a'), 'a');
        assert_eq!(display_char('漢'), '漢');
        assert_eq!(display_char('\u{e0b0}'), '\u{e0b0}');
    }
}
