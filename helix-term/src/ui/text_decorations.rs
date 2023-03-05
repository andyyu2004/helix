use std::cmp::Ordering;

use helix_core::{
    coords_at_pos,
    doc_formatter::{DocumentFormatter, FormattedGrapheme, TextFormat},
    text_annotations::TextAnnotations,
    RopeSlice,
};
use helix_view::{editor::CursorCache, theme::Style};

use crate::ui::document::{LinePos, TextRenderer};

pub use diagnostics::InlineDiagnostics;

mod diagnostics;

/// Decorations are the primary mechanisim for extending the text rendering.
///
/// Any on-screen element which is anchored to the rendered text in some form should
/// be implemented using this trait. Translating char positions to
/// on-screen positions can be expensive and should not be done during rendering.
/// Instead such translations are performed on the fly while the text is being rendered.
/// The results are provided to this trait
///
/// To reserve space for virtual text lines (which is then filled by this trait) emit appropriate
/// [`LineAnnotation`](helix_core::text_annotations::LineAnnotation) in [`helix_view::View::text_annotations`]
pub trait Decoration {
    /// Called **before** a **visual** line is rendered. A visual line does not
    /// necessairly correspond to a single line in a document as soft wrapping can
    /// spread a single document line across multiple visual lines.
    ///
    /// This function is called before text is rendered as any decorations should
    /// never overlap the document text. That means that setting the forground color
    /// here is (essentially) useless as the text color is overwritten by the
    /// rendered text. This -ofcourse- doesn't apply when rendering inside virtual lines
    /// below the line reserved by `LineAnnotation`s. e as no text will be rendered here.
    fn decorate_line(&mut self, _renderer: &mut TextRenderer, _pos: LinePos) {}

    /// Called **after** a **visual** line is rendered. A visual line does not
    /// necessairly correspond to a single line in a document as soft wrapping can
    /// spread a single document line across multiple visual lines.
    ///
    /// This function is called after text is rendered so that decorations can collect
    /// horizontal positions on the line (see [`Decoration::decorate_grapheme`]) first and
    /// use those positions` while rendering
    /// virtual text.
    /// That means that setting the forground color
    /// here is (essentially) useless as the text color is overwritten by the
    /// rendered text. This -ofcourse- doesn't apply when rendering inside virtual lines
    /// below the line reserved by `LineAnnotation`s. e as no text will be rendered here.
    /// **Note**: To avoid overlapping decorations in the virtual lines, each decoration
    /// must return the number of virtual text lines it has taken up. Each `Decoration` recieves
    /// an offset `virt_off` based on these return values where it can render virtual text:
    ///
    /// That means that a `render_line` implementation that returns `X` can render virtual text
    /// in the following area:
    /// ``` no-compile
    /// let start = inner.y + pos.virtual_line + virt_off;
    /// start .. start + X
    /// ````
    fn render_virt_lines(
        &mut self,
        _renderer: &mut TextRenderer,
        _pos: LinePos,
        _virt_off: u16,
    ) -> u16 {
        0
    }

    fn reset_pos(&mut self, _pos: usize) -> usize {
        usize::MAX
    }

    fn skip_concealed_anchor(&mut self, conceal_end_char_idx: usize) -> usize {
        self.reset_pos(conceal_end_char_idx)
    }

    /// This function is called **before** the grapheme at `char_idx` is rendered.
    ///
    /// # Returns
    /// The next
    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        _grapheme: &FormattedGrapheme,
    ) -> usize {
        usize::MAX
    }
}

impl<F: FnMut(&mut TextRenderer, LinePos)> Decoration for F {
    fn decorate_line(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        self(renderer, pos);
    }
}

#[derive(Default)]
pub struct DecorationManager<'a> {
    decorations: Vec<(Box<dyn Decoration + 'a>, usize)>,
}

impl<'a> DecorationManager<'a> {
    pub fn add_decoration(&mut self, decoration: impl Decoration + 'a) {
        self.decorations.push((Box::new(decoration), 0));
    }

    pub fn prepare_for_rendering(&mut self, first_visible_char: usize) {
        for (decoration, next_position) in &mut self.decorations {
            *next_position = decoration.reset_pos(first_visible_char)
        }
    }

    pub fn decorate_grapheme(&mut self, renderer: &mut TextRenderer, grapheme: &FormattedGrapheme) {
        for (decoration, hook_char_idx) in &mut self.decorations {
            loop {
                match (*hook_char_idx).cmp(&grapheme.char_idx) {
                    // this grapheme has been concealed or we are at the first grapheme
                    Ordering::Less => {
                        *hook_char_idx = decoration.skip_concealed_anchor(grapheme.char_idx)
                    }
                    Ordering::Equal => {
                        *hook_char_idx = decoration.decorate_grapheme(renderer, grapheme)
                    }
                    Ordering::Greater => break,
                }
            }
        }
    }

    pub fn decorate_line(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        for (decoration, _) in &mut self.decorations {
            decoration.decorate_line(renderer, pos);
        }
    }

    pub fn render_virtual_lines(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        let mut virt_off = 1; // start at 1 so the line is never overwritten
        for (decoration, _) in &mut self.decorations {
            if pos.visual_line + virt_off >= renderer.viewport.height {
                break;
            }
            virt_off += decoration.render_virt_lines(renderer, pos, virt_off);
        }
    }
}

/// Cursor rendering is done externally so all the cursor decoration
/// does is save the position of primary cursor
pub struct Cursor<'a> {
    pub cache: &'a CursorCache,
    pub primary_cursor: usize,
}
impl Decoration for Cursor<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        if pos <= self.primary_cursor {
            self.primary_cursor
        } else {
            usize::MAX
        }
    }

    fn decorate_grapheme(
        &mut self,
        renderer: &mut TextRenderer,
        grapheme: &FormattedGrapheme,
    ) -> usize {
        if renderer.column_in_bounds(grapheme.visual_pos.col) {
            let mut position = grapheme.visual_pos;
            position.col -= renderer.col_offset;
            self.cache.set(Some(position));
        }
        usize::MAX
    }
}

pub struct CopilotDecoration {
    style: Style,
    text: String,
    row: usize,
    col: usize,
    view_width: u16,
}

impl CopilotDecoration {
    pub fn new(
        style: Style,
        doc_text: RopeSlice,
        completion_text: String,
        completion_pos: usize,
        view_width: u16,
    ) -> CopilotDecoration {
        let coords = coords_at_pos(doc_text, completion_pos);
        CopilotDecoration {
            style,
            text: completion_text,
            row: coords.row,
            col: coords.col,
            view_width,
        }
    }
}

impl Decoration for CopilotDecoration {
    fn render_virt_lines(
        &mut self,
        _renderer: &mut TextRenderer,
        _pos: LinePos,
        _virt_off: u16,
    ) -> u16 {
        if _pos.doc_line != self.row {
            return 0;
        }

        let mut lines = self.text.split('\n').enumerate();
        lines.next();
        let n_lines = lines.clone().count();

        let mut text_fmt = TextFormat::default();
        text_fmt.viewport_width = self.view_width;
        let annotations = TextAnnotations::default();

        while let Some((idx, line)) = lines.next() {
            let formatter =
                DocumentFormatter::new_at_prev_checkpoint(line.into(), &text_fmt, &annotations, 0);

            for grapheme in formatter {
                _renderer.draw_decoration_grapheme(
                    grapheme.raw,
                    self.style,
                    _pos.visual_line + grapheme.visual_pos.row as u16 + idx as u16,
                    grapheme.visual_pos.col as u16,
                );
            }
        }

        n_lines as u16
    }

    fn decorate_line(&mut self, _renderer: &mut TextRenderer, _pos: LinePos) {
        if self.row != _pos.doc_line {
            return;
        }

        let first_line = if let Some(split) = self.text.split_once('\n') {
            split.0
        } else {
            &self.text
        };

        let mut text_fmt = TextFormat::default();
        text_fmt.viewport_width = self.view_width;
        let annotations = TextAnnotations::default();
        let formatter = DocumentFormatter::new_at_prev_checkpoint(
            first_line.into(),
            &text_fmt,
            &annotations,
            0,
        );

        for grapheme in formatter {
            if grapheme.char_idx < self.col {
                continue;
            }
            _renderer.draw_decoration_grapheme(
                grapheme.raw,
                self.style,
                _pos.visual_line + grapheme.visual_pos.row as u16,
                grapheme.visual_pos.col as u16,
            );
        }
    }
}
