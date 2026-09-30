//! A one-line text field with a cursor, for the small inputs that live in
//! footers and overlays. Owns the editing keys so each input doesn't have to.

use std::ops::Range;

use gpui::prelude::FluentBuilder;
use gpui::{
    Bounds, Context, Div, HighlightStyle, InteractiveElement, Keystroke, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Point, SharedString, Styled,
    StyledText, TextLayout, canvas, div, fill, px, size,
};

use crate::config::Theme;
use crate::terminal::colors::blend;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LineEdit {
    pub text: String,
    /// In chars, not bytes.
    cursor: usize,
    /// The other end of the selection, in chars. Nothing is selected when
    /// it's `None` or where the cursor is.
    anchor: Option<usize>,
}

impl LineEdit {
    /// Start with `text` and the cursor at its end.
    pub fn new(text: String) -> Self {
        let cursor = text.chars().count();
        Self {
            text,
            cursor,
            anchor: None,
        }
    }

    /// Start with `text` and the cursor at its start — for prefilled paths
    /// that most often get a directory typed in front.
    pub fn at_start(text: String) -> Self {
        Self {
            text,
            cursor: 0,
            anchor: None,
        }
    }

    fn byte(&self, at: usize) -> usize {
        self.text
            .char_indices()
            .nth(at)
            .map_or(self.text.len(), |(b, _)| b)
    }

    /// The far edge of the word before or after the cursor: across any
    /// separators first, then across the word. Punctuation separates, so a
    /// path moves a component at a time.
    fn word_edge(&self, forward: bool) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut at = self.cursor;
        for in_word in [false, true] {
            loop {
                let next = if forward { Some(at) } else { at.checked_sub(1) };
                match next.and_then(|i| chars.get(i)) {
                    Some(c) if c.is_alphanumeric() == in_word => {
                        at = if forward { at + 1 } else { at - 1 }
                    }
                    _ => break,
                }
            }
        }
        at
    }

    /// Remove the chars in `from..to` and leave the cursor where they were.
    fn delete(&mut self, from: usize, to: usize) {
        let (b, e) = (self.byte(from), self.byte(to));
        self.text.replace_range(b..e, "");
        self.cursor = from;
        self.anchor = None;
    }

    /// The selection in chars, low end first, if there is one.
    fn selected(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor.filter(|a| *a != self.cursor)?;
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// The selection in bytes, if there is one.
    pub fn selection(&self) -> Option<Range<usize>> {
        let (from, to) = self.selected()?;
        Some(self.byte(from)..self.byte(to))
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|range| &self.text[range])
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.chars().count();
    }

    /// Type or paste `text` at the cursor, over the selection if there is
    /// one. The field is one line, so line breaks become spaces.
    pub fn insert(&mut self, text: &str) {
        if let Some((from, to)) = self.selected() {
            self.delete(from, to);
        }
        self.anchor = None;
        let text = text.replace(['\r', '\n'], " ");
        self.text.insert_str(self.byte(self.cursor), &text);
        self.cursor += text.chars().count();
    }

    /// Put the cursor at byte offset `byte`, as a click there does; `extend`
    /// selects up to it from where the cursor was, as a drag or shift-click
    /// does.
    pub fn click(&mut self, byte: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = self
            .text
            .char_indices()
            .take_while(|(b, _)| *b < byte)
            .count();
    }

    /// Apply an editing key. Returns whether it was one; anything else
    /// (enter, escape) is the caller's.
    pub fn handle(&mut self, ks: &Keystroke) -> bool {
        let m = ks.modifiers;
        let len = self.text.chars().count();
        // Word-wise: option on macOS, control everywhere else.
        let word = m.alt || (m.control && !cfg!(target_os = "macos"));
        let sel = self.selected();
        // Shift selects as it moves; without it the selection is dropped.
        let to = match ks.key.as_str() {
            "left" if m.platform => 0,
            "right" if m.platform => len,
            "left" if word => self.word_edge(false),
            "right" if word => self.word_edge(true),
            // A plain arrow leaves a selection by the end it points at.
            "left" => match sel {
                Some((from, _)) if !m.shift => from,
                _ => self.cursor.saturating_sub(1),
            },
            "right" => match sel {
                Some((_, to)) if !m.shift => to,
                _ => (self.cursor + 1).min(len),
            },
            "home" => 0,
            "end" => len,
            "a" if m.control => 0,
            "e" if m.control => len,
            key => {
                match (key, sel) {
                    ("backspace" | "delete", Some((from, to))) => self.delete(from, to),
                    ("backspace", _) if m.platform => self.delete(0, self.cursor),
                    ("backspace", _) if word => self.delete(self.word_edge(false), self.cursor),
                    ("backspace", _) => self.delete(self.cursor.saturating_sub(1), self.cursor),
                    ("delete", _) if word => self.delete(self.cursor, self.word_edge(true)),
                    ("delete", _) => self.delete(self.cursor, (self.cursor + 1).min(len)),
                    _ => {
                        let plain = !m.platform && !m.control && !m.function;
                        let Some(c) = ks.key_char.as_deref().filter(|_| plain) else {
                            return false;
                        };
                        self.insert(c);
                    }
                }
                return true;
            }
        };
        if m.shift {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = to;
        true
    }

    /// The text either side of the cursor.
    #[cfg(test)]
    fn split(&self) -> (&str, &str) {
        self.text.split_at(self.byte(self.cursor))
    }
}

/// The byte offset in a laid-out field's text nearest to `position`.
fn index_at(layout: &TextLayout, position: Point<Pixels>) -> usize {
    let Some(line) = layout.line_layout_for_index(0) else {
        return 0;
    };
    let position = position - layout.bounds().origin;
    match line.closest_index_for_position(position, layout.line_height()) {
        Ok(ix) | Err(ix) => ix,
    }
}

/// Draw `edit` — its text, wrapped when it's longer than the field, its
/// selection, and a thin caret (a glyph would take a whole monospace cell) —
/// or a dimmed placeholder. Clicking moves the cursor and dragging selects,
/// in whichever field `get` finds on the view.
pub fn render<V: 'static>(
    edit: &LineEdit,
    placeholder: &'static str,
    theme: &Theme,
    cx: &Context<V>,
    get: fn(&mut V) -> Option<&mut LineEdit>,
) -> Div {
    let empty = edit.text.is_empty();
    let text: SharedString = match (empty, placeholder) {
        (false, _) => edit.text.clone().into(),
        // Still a line to lay out, so the caret has somewhere to be.
        (true, "") => " ".into(),
        (true, _) => placeholder.into(),
    };
    let mut styled = StyledText::new(text);
    if let Some(range) = edit.selection() {
        let highlight = HighlightStyle {
            background_color: Some(theme.selection_bg),
            color: theme.selection_fg,
            ..Default::default()
        };
        styled = styled.with_highlights([(range, highlight)]);
    }
    let layout = styled.layout().clone();
    let caret_at = edit.byte(edit.cursor);
    // Painted over the text rather than laid out in it, so the letters
    // don't shift as the caret passes.
    let caret = canvas(|_, _, _| (), {
        let layout = layout.clone();
        move |_, _, window, _| {
            if let Some(origin) = layout.position_for_index(caret_at) {
                let bounds = Bounds::new(origin, size(px(1.5), layout.line_height()));
                window.paint_quad(fill(bounds, window.text_style().color));
            }
        }
    })
    .absolute()
    .size_0();
    let dim = blend(theme.foreground, theme.background, 0.45);
    div()
        .flex_1()
        .overflow_hidden()
        .whitespace_normal()
        .cursor_text()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener({
                let layout = layout.clone();
                move |view, event: &MouseDownEvent, _w, cx| {
                    if let Some(edit) = get(view) {
                        edit.click(index_at(&layout, event.position), event.modifiers.shift);
                        cx.notify();
                    }
                }
            }),
        )
        // ponytail: a drag selects only while the pointer is over the field;
        // following it outside needs a window-level mouse listener.
        .on_mouse_move(cx.listener(move |view, event: &MouseMoveEvent, _w, cx| {
            if event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            if let Some(edit) = get(view) {
                edit.click(index_at(&layout, event.position), true);
                cx.notify();
            }
        }))
        .child(div().when(empty, |d| d.text_color(dim)).child(styled))
        .child(caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Keystroke {
        let mut ks = Keystroke::parse(s).unwrap();
        if ks.key.chars().count() == 1 && !ks.modifiers.control && !ks.modifiers.platform {
            ks.key_char = Some(ks.key.clone());
        }
        ks
    }

    #[test]
    fn cursor_moves_and_edits_in_the_middle() {
        let mut e = LineEdit::new("tést".into());
        assert_eq!(e.split(), ("tést", ""));
        for _ in 0..4 {
            e.handle(&key("left"));
        }
        assert!(e.handle(&key("x")));
        assert_eq!(e.split(), ("x", "tést"));
        e.handle(&key("cmd-right"));
        e.handle(&key("backspace"));
        e.handle(&key("ctrl-a"));
        e.handle(&key("delete"));
        assert_eq!(e.text, "tés");
        assert!(!e.handle(&key("enter")));
    }

    #[test]
    fn option_moves_and_deletes_by_word() {
        let mut e = LineEdit::new("src/té st.rs".into());
        e.handle(&key("alt-left"));
        assert_eq!(e.split(), ("src/té st.", "rs"));
        e.handle(&key("alt-backspace"));
        assert_eq!(e.split(), ("src/té ", "rs"), "separators go with the word");
        e.handle(&key("alt-left"));
        e.handle(&key("alt-left"));
        assert_eq!(e.split(), ("", "src/té rs"));
        e.handle(&key("alt-right"));
        e.handle(&key("alt-delete"));
        assert_eq!(e.split(), ("src", " rs"));
        e.handle(&key("cmd-backspace"));
        assert_eq!(e.text, " rs");
        // Nothing to cross at either end.
        e.handle(&key("alt-backspace"));
        e.handle(&key("cmd-right"));
        e.handle(&key("alt-delete"));
        assert_eq!(e.text, " rs");
    }

    #[test]
    fn shift_and_the_mouse_select_and_edits_replace_the_selection() {
        let mut e = LineEdit::new("tést one".into());
        e.handle(&key("shift-alt-left"));
        assert_eq!(e.selected_text(), Some("one"));
        e.handle(&key("x"));
        assert_eq!((e.text.as_str(), e.selection()), ("tést x", None));
        // Click before the "s" (byte 3: é is two), drag back to the start.
        e.click(3, false);
        assert_eq!(e.split(), ("té", "st x"));
        e.click(0, true);
        assert_eq!(e.selected_text(), Some("té"));
        // A plain arrow drops the selection at that end of it.
        e.handle(&key("right"));
        assert_eq!((e.split(), e.selection()), (("té", "st x"), None));
        e.handle(&key("shift-left"));
        e.handle(&key("backspace"));
        assert_eq!(e.text, "tst x");
        e.select_all();
        e.insert("a\nb");
        assert_eq!(e.split(), ("a b", ""));
    }
}
