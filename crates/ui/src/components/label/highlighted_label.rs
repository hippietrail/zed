use std::ops::Range;

use gpui::{FontWeight, HighlightStyle, StyledText};

use crate::{LabelCommon, LabelLike, LabelSize, LineHeightStyle, prelude::*};

#[derive(IntoElement)]
pub struct HighlightedLabel {
    base: LabelLike,
    label: SharedString,
    highlight_indices: Vec<usize>,
}

impl HighlightedLabel {
    /// Constructs a label with the given characters highlighted.
    /// Characters are identified by UTF-8 byte position.
    pub fn new(label: impl Into<SharedString>, highlight_indices: Vec<usize>) -> Self {
        Self {
            base: LabelLike::new(),
            label: label.into(),
            highlight_indices,
        }
    }
}

impl LabelCommon for HighlightedLabel {
    fn size(mut self, size: LabelSize) -> Self {
        self.base = self.base.size(size);
        self
    }

    fn weight(mut self, weight: FontWeight) -> Self {
        self.base = self.base.weight(weight);
        self
    }

    fn line_height_style(mut self, line_height_style: LineHeightStyle) -> Self {
        self.base = self.base.line_height_style(line_height_style);
        self
    }

    fn color(mut self, color: Color) -> Self {
        self.base = self.base.color(color);
        self
    }

    fn strikethrough(mut self) -> Self {
        self.base = self.base.strikethrough();
        self
    }

    fn italic(mut self) -> Self {
        self.base = self.base.italic();
        self
    }

    fn alpha(mut self, alpha: f32) -> Self {
        self.base = self.base.alpha(alpha);
        self
    }

    fn underline(mut self) -> Self {
        self.base = self.base.underline();
        self
    }

    fn truncate(mut self) -> Self {
        self.base = self.base.truncate();
        self
    }

    fn single_line(mut self) -> Self {
        self.base = self.base.single_line();
        self
    }

    fn buffer_font(mut self, cx: &App) -> Self {
        self.base = self.base.buffer_font(cx);
        self
    }
}

pub fn highlight_ranges(
    text: &str,
    indices: &[usize],
    style: HighlightStyle,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut highlight_indices = indices.iter().copied().peekable();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();

    while let Some(start_ix) = highlight_indices.next() {
        let mut end_ix = start_ix;

        loop {
            end_ix = end_ix + text[end_ix..].chars().next().unwrap().len_utf8();
            if let Some(&next_ix) = highlight_indices.peek() {
                if next_ix == end_ix {
                    end_ix = next_ix;
                    highlight_indices.next();
                    continue;
                }
            }
            break;
        }

        highlights.push((start_ix..end_ix, style));
    }

    highlights
}

impl RenderOnce for HighlightedLabel {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let highlight_color = cx.theme().colors().text_accent;

        let highlights = highlight_ranges(
            &self.label,
            &self.highlight_indices,
            HighlightStyle {
                color: Some(highlight_color),
                ..Default::default()
            },
        );

        let mut text_style = window.text_style();
        text_style.color = self.base.color.color(cx);

        self.base
            .child(StyledText::new(self.label).with_default_highlights(&text_style, highlights))
    }
}
