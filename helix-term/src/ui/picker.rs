use crate::{
    alt,
    compositor::{self, Component, Compositor, Context, Event, EventResult},
    ctrl,
    job::{dispatch_blocking, Callback},
    key, shift,
    ui::{
        self,
        document::{render_document, LinePos, TextRenderer},
        text_decorations::DecorationManager,
        EditorView,
    },
};
use futures_util::{future::BoxFuture, FutureExt};
use helix_event::AsyncHook;
use nucleo::pattern::CaseMatching;
use nucleo::{Config, Nucleo, Utf32String};
use tokio::time::Instant;
use tui::{
    buffer::Buffer as Surface,
    layout::Constraint,
    text::{Span, Spans},
    widgets::{Block, BorderType, Borders, Cell, Row, Table},
};

use tui::widgets::Widget;

use std::{
    borrow::Cow,
    collections::HashMap,
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{self, AtomicUsize},
        Arc,
    },
    time::Duration,
};

use crate::ui::{Prompt, PromptEvent};
use helix_core::{
    char_idx_at_visual_offset, fuzzy::MATCHER, movement::Direction,
    text_annotations::TextAnnotations, unicode::segmentation::UnicodeSegmentation, Position,
    Syntax,
};
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Margin, Modifier, Rect},
    theme::Style,
    view::ViewPosition,
    Document, DocumentId, Editor,
};

pub const ID: &str = "picker";
use super::overlay::Overlay;

pub const MIN_AREA_WIDTH_FOR_PREVIEW: u16 = 72;
/// Biggest file size to preview in bytes
pub const MAX_FILE_SIZE_FOR_PREVIEW: u64 = 10 * 1024 * 1024;

#[derive(PartialEq, Eq, Hash)]
pub enum PathOrId {
    Id(DocumentId),
    Path(PathBuf),
}

impl PathOrId {
    fn get_canonicalized(self) -> Self {
        use PathOrId::*;
        match self {
            Path(path) => Path(helix_core::path::get_canonicalized_path(&path)),
            Id(id) => Id(id),
        }
    }
}

impl From<PathBuf> for PathOrId {
    fn from(v: PathBuf) -> Self {
        Self::Path(v)
    }
}

impl From<DocumentId> for PathOrId {
    fn from(v: DocumentId) -> Self {
        Self::Id(v)
    }
}

type FileCallback<T> = Box<dyn Fn(&Editor, &T) -> Option<FileLocation>>;

/// File path and range of lines (used to align and highlight lines)
pub type FileLocation = (PathOrId, Option<(usize, usize)>);

pub enum CachedPreview {
    Document(Box<Document>),
    Binary,
    LargeFile,
    NotFound,
}

// We don't store this enum in the cache so as to avoid lifetime constraints
// from borrowing a document already opened in the editor.
pub enum Preview<'picker, 'editor> {
    Cached(&'picker CachedPreview),
    EditorDocument(&'editor Document),
}

impl Preview<'_, '_> {
    fn document(&self) -> Option<&Document> {
        match self {
            Preview::EditorDocument(doc) => Some(doc),
            Preview::Cached(CachedPreview::Document(doc)) => Some(doc),
            _ => None,
        }
    }

    /// Alternate text to show for the preview.
    fn placeholder(&self) -> &str {
        match *self {
            Self::EditorDocument(_) => "<Invalid file location>",
            Self::Cached(preview) => match preview {
                CachedPreview::Document(_) => "<Invalid file location>",
                CachedPreview::Binary => "<Binary file>",
                CachedPreview::LargeFile => "<File too large to preview>",
                CachedPreview::NotFound => "<File not found>",
            },
        }
    }
}

fn inject_nucleo_item<T, D>(
    injector: &nucleo::Injector<T>,
    columns: &[Column<T, D>],
    item: T,
    editor_data: &D,
) {
    let column_texts: Vec<Utf32String> = columns
        .iter()
        .filter(|column| column.filter)
        .map(|column| column.format_text(&item, editor_data).into())
        .collect();
    injector.push(item, |dst| {
        for (i, text) in column_texts.into_iter().enumerate() {
            dst[i] = text;
        }
    });
}

pub struct Injector<T, D> {
    dst: nucleo::Injector<T>,
    columns: Arc<Vec<Column<T, D>>>,
    editor_data: Arc<D>,
    version: usize,
    picker_version: Arc<AtomicUsize>,
}

impl<I, D> Clone for Injector<I, D> {
    fn clone(&self) -> Self {
        Injector {
            dst: self.dst.clone(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version,
            picker_version: self.picker_version.clone(),
        }
    }
}

#[derive(Debug)]
pub struct InjectorShutdown;

impl std::fmt::Display for InjectorShutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: better message
        write!(f, "picker has been closed")
    }
}

impl std::error::Error for InjectorShutdown {}

impl<T, D> Injector<T, D> {
    pub fn push(&self, item: T) -> Result<(), InjectorShutdown> {
        if self.version != self.picker_version.load(atomic::Ordering::Relaxed) {
            return Err(InjectorShutdown);
        }

        inject_nucleo_item(&self.dst, &self.columns, item, &self.editor_data);
        Ok(())
    }
}

type ColumnFormatFn<T, D> = for<'a> fn(&'a T, &'a D) -> Cell<'a>;

pub struct Column<T, D> {
    name: &'static str,
    format: ColumnFormatFn<T, D>,
    /// Whether the column should be passed to nucleo for matching and filtering.
    /// `DynamicPicker` uses this so that the dynamic column (for example regex in
    /// global search) is not used for filtering twice.
    filter: bool,
}

impl<T, D> Column<T, D> {
    pub fn new(name: &'static str, format: ColumnFormatFn<T, D>) -> Self {
        Self {
            name,
            format,
            filter: true,
        }
    }

    pub fn without_filtering(mut self) -> Self {
        self.filter = false;
        self
    }

    fn format<'a>(&self, item: &'a T, data: &'a D) -> Cell<'a> {
        (self.format)(item, data)
    }

    fn format_text<'a>(&self, item: &'a T, data: &'a D) -> Cow<'a, str> {
        let text: String = self.format(item, data).content.into();
        text.into()
    }
}

pub struct Picker<T: 'static + Send + Sync, D: 'static> {
    column_names: Vec<&'static str>,
    columns: Arc<Vec<Column<T, D>>>,
    primary_column: usize,
    editor_data: Arc<D>,
    version: Arc<AtomicUsize>,
    matcher: Nucleo<T>,

    /// Current height of the completions box
    completion_height: u16,

    cursor: u32,
    prompt: Prompt,
    query: HashMap<&'static str, String>,

    /// Whether to show the preview panel (default true)
    show_preview: bool,
    /// Constraints for tabular formatting
    widths: Vec<Constraint>,

    callback_fn: PickerCallback<T>,

    pub truncate_start: bool,
    /// Caches paths to documents
    preview_cache: HashMap<PathBuf, CachedPreview>,
    read_buffer: Vec<u8>,
    /// Given an item in the picker, return the file path and line number to display.
    file_fn: Option<FileCallback<T>>,

    pub tmp_running: bool,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> Picker<T, D> {
    pub fn stream(columns: Vec<Column<T, D>>, editor_data: D) -> (Nucleo<T>, Injector<T, D>) {
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let streamer = Injector {
            dst: matcher.injector(),
            columns: Arc::new(columns),
            editor_data: Arc::new(editor_data),
            version: 0,
            picker_version: Arc::new(AtomicUsize::new(0)),
        };
        (matcher, streamer)
    }

    pub fn new(
        columns: Vec<Column<T, D>>,
        primary_column: usize,
        options: Vec<T>,
        editor_data: D,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let injector = matcher.injector();
        for item in options {
            inject_nucleo_item(&injector, &columns, item, &editor_data);
        }
        Self::with(
            matcher,
            Arc::new(columns),
            primary_column,
            Arc::new(editor_data),
            Arc::new(AtomicUsize::new(0)),
            callback_fn,
        )
    }

    pub fn with_stream(
        matcher: Nucleo<T>,
        primary_column: usize,
        injector: Injector<T, D>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        Self::with(
            matcher,
            injector.columns,
            primary_column,
            injector.editor_data,
            injector.picker_version,
            callback_fn,
        )
    }

    fn with(
        matcher: Nucleo<T>,
        columns: Arc<Vec<Column<T, D>>>,
        default_column: usize,
        editor_data: Arc<D>,
        version: Arc<AtomicUsize>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        assert!(!columns.is_empty());

        let prompt = Prompt::new(
            "".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {},
        );

        let column_names: Vec<_> = columns.iter().map(|column| column.name).collect();
        let widths = columns
            .iter()
            .map(|column| Constraint::Length(column.name.chars().count() as u16))
            .collect();

        Self {
            column_names,
            columns,
            primary_column: default_column,
            matcher,
            editor_data,
            version,
            cursor: 0,
            prompt,
            query: HashMap::default(),
            truncate_start: true,
            show_preview: true,
            callback_fn: Box::new(callback_fn),
            completion_height: 0,
            widths,
            preview_cache: HashMap::new(),
            read_buffer: Vec::with_capacity(1024),
            file_fn: None,
            tmp_running: false,
        }
    }

    pub fn injector(&self) -> Injector<T, D> {
        Injector {
            dst: self.matcher.injector(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version.load(atomic::Ordering::Relaxed),
            picker_version: self.version.clone(),
        }
    }

    pub fn truncate_start(mut self, truncate_start: bool) -> Self {
        self.truncate_start = truncate_start;
        self
    }

    pub fn with_preview(
        mut self,
        preview_fn: impl Fn(&Editor, &T) -> Option<FileLocation> + 'static,
    ) -> Self {
        self.file_fn = Some(Box::new(preview_fn));
        // assumption: if we have a preview we are matching paths... If this is ever
        // not true this could be a separate builder function
        self.matcher.update_config(Config::DEFAULT.match_paths());
        self
    }

    pub fn with_line(mut self, line: String, editor: &Editor) -> Self {
        self.prompt.set_line(line, editor);
        self
    }

    /// Move the cursor by a number of lines, either down (`Forward`) or up (`Backward`)
    pub fn move_by(&mut self, amount: u32, direction: Direction) {
        let len = self.matcher.snapshot().matched_item_count();

        if len == 0 {
            // No results, can't move.
            return;
        }

        match direction {
            Direction::Forward => {
                self.cursor = self.cursor.saturating_add(amount) % len;
            }
            Direction::Backward => {
                self.cursor = self.cursor.saturating_add(len).saturating_sub(amount) % len;
            }
        }
    }

    /// Move the cursor down by exactly one page. After the last page comes the first page.
    pub fn page_up(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Backward);
    }

    /// Move the cursor up by exactly one page. After the first page comes the last page.
    pub fn page_down(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Forward);
    }

    /// Move the cursor to the first entry
    pub fn to_start(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the last entry
    pub fn to_end(&mut self) {
        self.cursor = self
            .matcher
            .snapshot()
            .matched_item_count()
            .saturating_sub(1);
    }

    pub fn selection(&self) -> Option<&T> {
        self.matcher
            .snapshot()
            .get_matched_item(self.cursor)
            .map(|item| item.data)
    }

    fn primary_query(&self) -> &str {
        self.query
            .get(self.column_names[self.primary_column])
            .map(AsRef::as_ref)
            .unwrap_or_default()
    }

    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
    }

    fn prompt_handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let EventResult::Consumed(_) = self.prompt.handle_event(event, cx) {
            // TODO: better track how the pattern has changed
            let line = self.prompt.line();
            let new_query = parse_query(&self.column_names, self.primary_column, line);
            if new_query != self.query {
                for (i, column) in self
                    .columns
                    .iter()
                    .filter(|column| column.filter)
                    .enumerate()
                {
                    let pattern = new_query
                        .get(column.name)
                        .map(|pattern| pattern.as_str())
                        .unwrap_or_default();
                    let append = self
                        .query
                        .get(column.name)
                        .map(|old_pattern| {
                            pattern.starts_with(old_pattern) && !old_pattern.ends_with('\\')
                        })
                        .unwrap_or(false);

                    self.matcher
                        .pattern
                        .reparse(i, pattern, CaseMatching::Smart, append);
                }
                self.query = new_query;
            }
        }
        EventResult::Consumed(None)
    }

    fn current_file(&self, editor: &Editor) -> Option<FileLocation> {
        self.selection()
            .and_then(|current| (self.file_fn.as_ref()?)(editor, current))
            .map(|(path_or_id, line)| (path_or_id.get_canonicalized(), line))
    }

    /// Get (cached) preview for a given path. If a document corresponding
    /// to the path is already open in the editor, it is used instead.
    fn get_preview<'picker, 'editor>(
        &'picker mut self,
        path_or_id: PathOrId,
        editor: &'editor Editor,
    ) -> Preview<'picker, 'editor> {
        match path_or_id {
            PathOrId::Path(path) => {
                let path = &path;
                if let Some(doc) = editor.document_by_path(path) {
                    return Preview::EditorDocument(doc);
                }

                if self.preview_cache.contains_key(path) {
                    return Preview::Cached(&self.preview_cache[path]);
                }

                let data = std::fs::File::open(path).and_then(|file| {
                    let metadata = file.metadata()?;
                    // Read up to 1kb to detect the content type
                    let n = file.take(1024).read_to_end(&mut self.read_buffer)?;
                    let content_type = content_inspector::inspect(&self.read_buffer[..n]);
                    self.read_buffer.clear();
                    Ok((metadata, content_type))
                });
                let preview = data
                    .map(
                        |(metadata, content_type)| match (metadata.len(), content_type) {
                            (_, content_inspector::ContentType::BINARY) => CachedPreview::Binary,
                            (size, _) if size > MAX_FILE_SIZE_FOR_PREVIEW => {
                                CachedPreview::LargeFile
                            }
                            _ => Document::open(path, None, None, editor.config.clone())
                                .map(|doc| CachedPreview::Document(Box::new(doc)))
                                .unwrap_or(CachedPreview::NotFound),
                        },
                    )
                    .unwrap_or(CachedPreview::NotFound);
                self.preview_cache.insert(path.to_owned(), preview);
                Preview::Cached(&self.preview_cache[path])
            }
            PathOrId::Id(id) => {
                let doc = editor.documents.get(&id).unwrap();
                Preview::EditorDocument(doc)
            }
        }
    }

    fn handle_idle_timeout(&mut self, cx: &mut Context) -> EventResult {
        let Some((current_file, _)) = self.current_file(cx.editor) else {
            return EventResult::Consumed(None);
        };

        // Try to find a document in the cache
        let doc = match &current_file {
            PathOrId::Id(doc_id) => doc_mut!(cx.editor, doc_id),
            PathOrId::Path(path) => match self.preview_cache.get_mut(path) {
                Some(CachedPreview::Document(ref mut doc)) => doc,
                _ => return EventResult::Consumed(None),
            },
        };

        let mut callback: Option<compositor::Callback> = None;

        // Then attempt to highlight it if it has no language set
        if doc.language_config().is_none() {
            if let Some(language_config) = doc.detect_language_config(&cx.editor.syn_loader) {
                doc.language = Some(language_config.clone());
                let text = doc.text().clone();
                let loader = cx.editor.syn_loader.clone();
                let job = tokio::task::spawn_blocking(move || {
                    let syntax = language_config.highlight_config(&loader.scopes()).and_then(
                        |highlight_config| Syntax::new(text.slice(..), highlight_config, loader),
                    );
                    let callback = move |editor: &mut Editor, compositor: &mut Compositor| {
                        let Some(syntax) = syntax else {
                            log::info!("highlighting picker item failed");
                            return;
                        };
                        let picker = match compositor.find::<Overlay<Self>>() {
                            Some(Overlay { content, .. }) => Some(content),
                            None => compositor
                                .find::<Overlay<DynamicPicker<T, D>>>()
                                .map(|overlay| &mut overlay.content.file_picker),
                        };
                        let Some(picker) = picker
                        else {
                            log::info!("picker closed before syntax highlighting finished");
                            return;
                        };
                        // Try to find a document in the cache
                        let doc = match current_file {
                            PathOrId::Id(doc_id) => doc_mut!(editor, &doc_id),
                            PathOrId::Path(path) => match picker.preview_cache.get_mut(&path) {
                                Some(CachedPreview::Document(ref mut doc)) => doc,
                                _ => return,
                            },
                        };
                        doc.syntax = Some(syntax);
                    };
                    Callback::EditorCompositor(Box::new(callback))
                });
                let tmp: compositor::Callback = Box::new(move |_, ctx| {
                    ctx.jobs
                        .callback(job.map(|res| res.map_err(anyhow::Error::from)))
                });
                callback = Some(Box::new(tmp))
            }
        }

        // QUESTION: do we want to compute inlay hints in pickers too ? Probably not for now
        // but it could be interesting in the future

        EventResult::Consumed(callback)
    }

    fn render_picker(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let status = self.matcher.tick(10);
        let snapshot = self.matcher.snapshot();
        if status.changed {
            self.cursor = self
                .cursor
                .min(snapshot.matched_item_count().saturating_sub(1))
        }

        let text_style = cx.editor.theme.get("ui.text");
        let selected = cx.editor.theme.get("ui.text.focus");
        let highlight_style = cx.editor.theme.get("special").add_modifier(Modifier::BOLD);

        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        // don't like this but the lifetime sucks
        let block = Block::default().borders(Borders::ALL);

        // calculate the inner area inside the box
        let inner = block.inner(area);

        block.render(area, surface);

        // -- Render the input bar:

        let area = inner.clip_left(1).with_height(1);
        // render the prompt first since it will clear its background
        self.prompt.render(area, surface, cx);

        let count = format!(
            "{}{}/{}",
            if status.running || self.tmp_running {
                "(running) "
            } else {
                ""
            },
            snapshot.matched_item_count(),
            snapshot.item_count(),
        );
        surface.set_stringn(
            (area.x + area.width).saturating_sub(count.len() as u16 + 1),
            area.y,
            &count,
            (count.len()).min(area.width as usize),
            text_style,
        );

        // -- Separator
        let sep_style = cx.editor.theme.get("ui.background.separator");
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in inner.left()..inner.right() {
            if let Some(cell) = surface.get_mut(x, inner.y + 1) {
                cell.set_symbol(borders.horizontal).set_style(sep_style);
            }
        }

        // -- Render the contents:
        // subtract area of prompt from top
        let inner = inner.clip_top(2);
        let rows = inner.height as u32;
        let offset = self.cursor - (self.cursor % std::cmp::max(1, rows));
        let cursor = self.cursor.saturating_sub(offset);
        let end = offset
            .saturating_add(rows)
            .min(snapshot.matched_item_count());
        let mut indices = Vec::new();
        let mut matcher = MATCHER.lock();
        matcher.config = Config::DEFAULT;
        if self.file_fn.is_some() {
            matcher.config.set_match_paths()
        }

        let options = snapshot.matched_items(offset..end).map(|item| {
            let mut widths = self.widths.iter_mut();
            let mut matcher_index = 0;

            Row::new(self.columns.iter().map(|column| {
                let Some(Constraint::Length(max_width)) = widths.next() else {
                    unreachable!();
                };
                let mut cell = column.format(item.data, &self.editor_data);
                let width = if column.filter {
                    snapshot.pattern().column_pattern(matcher_index).indices(
                        item.matcher_columns[matcher_index].slice(..),
                        &mut matcher,
                        &mut indices,
                    );
                    indices.sort_unstable();
                    indices.dedup();
                    let mut indices = indices.drain(..);
                    let mut next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                    let mut span_list = Vec::new();
                    let mut current_span = String::new();
                    let mut current_style = Style::default();
                    let mut grapheme_idx = 0u32;
                    let mut width = 0;

                    let spans: &[Span] =
                        cell.content.lines.first().map_or(&[], |it| it.0.as_slice());
                    for span in spans {
                        // this looks like a bug on first glance, we are iterating
                        // graphemes but treating them as char indices. The reason that
                        // this is correct is that nucleo will only ever consider the first char
                        // of a grapheme (and discard the rest of the grapheme) so the indices
                        // returned by nucleo are essentially grapheme indecies
                        for grapheme in span.content.graphemes(true) {
                            let style = if grapheme_idx == next_highlight_idx {
                                next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                                span.style.patch(highlight_style)
                            } else {
                                span.style
                            };
                            if style != current_style {
                                if !current_span.is_empty() {
                                    span_list.push(Span::styled(current_span, current_style))
                                }
                                current_span = String::new();
                                current_style = style;
                            }
                            current_span.push_str(grapheme);
                            grapheme_idx += 1;
                        }
                        width += span.width();
                    }

                    span_list.push(Span::styled(current_span, current_style));
                    cell = Cell::from(Spans::from(span_list));
                    matcher_index += 1;
                    width
                } else {
                    cell.content
                        .lines
                        .first()
                        .map(|line| line.width())
                        .unwrap_or_default()
                };

                if width as u16 > *max_width {
                    *max_width = width as u16;
                }

                cell
            }))
        });

        let mut table = Table::new(options)
            .style(text_style)
            .highlight_style(selected)
            .highlight_symbol(" > ")
            .column_spacing(1)
            .widths(&self.widths);

        // -- Header
        // TODO: theme keys ui.picker.header.text, ui.picker.header.separator
        if self.columns.len() > 1 {
            let header_text_style = cx.editor.theme.get("ui.picker.header.text");
            let header_separator_style = cx.editor.theme.get("ui.picker.header.separator");

            table = table.header(
                Row::new(self.columns.iter().zip(self.widths.iter()).map(
                    |(column, constraint)| {
                        let separator_len = constraint.apply(inner.width);
                        let separator = borders.horizontal.repeat(separator_len as usize);

                        Cell::from(tui::text::Text {
                            lines: vec![
                                Span::styled(column.name, header_text_style).into(),
                                Span::styled(separator, header_separator_style).into(),
                            ],
                        })
                    },
                ))
                .height(2),
            );
        }

        use tui::widgets::TableState;

        table.render_table(
            inner,
            surface,
            &mut TableState {
                offset: 0,
                selected: Some(cursor as usize),
            },
            self.truncate_start,
        );
    }

    fn render_preview(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        let text = cx.editor.theme.get("ui.text");
        surface.clear_with(area, background);

        // don't like this but the lifetime sucks
        let block = Block::default().borders(Borders::ALL);

        // calculate the inner area inside the box
        let inner = block.inner(area);
        // 1 column gap on either side
        let margin = Margin::horizontal(1);
        let inner = inner.inner(&margin);
        block.render(area, surface);

        if let Some((path, range)) = self.current_file(cx.editor) {
            let preview = self.get_preview(path, cx.editor);
            let doc = match preview.document() {
                Some(doc)
                    if range.map_or(true, |(start, end)| {
                        start <= end && end <= doc.text().len_lines()
                    }) =>
                {
                    doc
                }
                _ => {
                    let alt_text = preview.placeholder();
                    let x = inner.x + inner.width.saturating_sub(alt_text.len() as u16) / 2;
                    let y = inner.y + inner.height / 2;
                    surface.set_stringn(x, y, alt_text, inner.width as usize, text);
                    return;
                }
            };

            let mut offset = ViewPosition::default();
            if let Some((start_line, end_line)) = range {
                let height = end_line - start_line;
                let text = doc.text().slice(..);
                let start = text.line_to_char(start_line);
                let middle = text.line_to_char(start_line + height / 2);
                if height < inner.height as usize {
                    let text_fmt = doc.text_format(inner.width, None);
                    let annotations = TextAnnotations::default();
                    (offset.anchor, offset.vertical_offset) = char_idx_at_visual_offset(
                        text,
                        middle,
                        // align to middle
                        -(inner.height as isize / 2),
                        0,
                        &text_fmt,
                        &annotations,
                    );
                    if start < offset.anchor {
                        offset.anchor = start;
                        offset.vertical_offset = 0;
                    }
                } else {
                    offset.anchor = start;
                }
            }

            let mut highlights = EditorView::doc_syntax_highlights(
                doc,
                offset.anchor,
                area.height,
                &cx.editor.theme,
            );
            for spans in EditorView::doc_diagnostics_highlights(doc, &cx.editor.theme) {
                if spans.is_empty() {
                    continue;
                }
                highlights = Box::new(helix_core::syntax::merge(highlights, spans));
            }
            let mut decorations = DecorationManager::default();

            if let Some((start, end)) = range {
                let style = cx
                    .editor
                    .theme
                    .try_get("ui.highlight")
                    .unwrap_or_else(|| cx.editor.theme.get("ui.selection"));
                let draw_highlight = move |renderer: &mut TextRenderer, pos: LinePos| {
                    if (start..=end).contains(&pos.doc_line) {
                        let area = Rect::new(
                            renderer.viewport.x,
                            renderer.viewport.y + pos.visual_line,
                            renderer.viewport.width,
                            1,
                        );
                        renderer.surface.set_style(area, style)
                    }
                };
                decorations.add_decoration(draw_highlight);
            }

            render_document(
                surface,
                inner,
                doc,
                offset,
                // TODO: compute text annotations asynchronously here (like inlay hints)
                &TextAnnotations::default(),
                highlights,
                &cx.editor.theme,
                decorations,
            );
        }
    }
}

impl<I: 'static + Send + Sync, D: 'static + Send + Sync> Component for Picker<I, D> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------+ +---------+
        // |prompt   | |preview  |
        // +---------+ |         |
        // |picker   | |         |
        // |         | |         |
        // +---------+ +---------+

        let render_preview = self.show_preview && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };

        let picker_area = area.with_width(picker_width);
        self.render_picker(picker_area, surface, cx);

        if render_preview {
            let preview_area = area.clip_left(picker_width);
            self.render_preview(preview_area, surface, cx);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        if let Event::IdleTimeout = event {
            return self.handle_idle_timeout(ctx);
        }
        // TODO: keybinds for scrolling preview

        let key_event = match event {
            Event::Key(event) => *event,
            Event::Paste(..) => return self.prompt_handle_event(event, ctx),
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        let close_fn = |picker: &mut Self| {
            // if the picker is very large don't store it as last_picker to avoid
            // excessive memory consumption
            let callback: compositor::Callback = if picker.matcher.snapshot().item_count() > 100_000
            {
                Box::new(|compositor: &mut Compositor, _ctx| {
                    // remove the layer
                    compositor.pop();
                })
            } else {
                // stop streaming in new items in the background, really we should
                // be restarting the stream somehow once the picker gets
                // reopened instead (like for an FS crawl) that would also remove the
                // need for the special case above but that is pretty tricky
                picker.version.fetch_add(1, atomic::Ordering::Relaxed);
                Box::new(|compositor: &mut Compositor, _ctx| {
                    // remove the layer
                    compositor.last_picker = compositor.pop();
                })
            };
            EventResult::Consumed(Some(callback))
        };

        // So that idle timeout retriggers
        ctx.editor.reset_idle_timer();

        match key_event {
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_by(1, Direction::Backward);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                self.move_by(1, Direction::Forward);
            }
            key!(PageDown) | ctrl!('d') => {
                self.page_down();
            }
            key!(PageUp) | ctrl!('u') => {
                self.page_up();
            }
            key!(Home) => {
                self.to_start();
            }
            key!(End) => {
                self.to_end();
            }
            key!(Esc) | ctrl!('c') => return close_fn(self),
            alt!(Enter) => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::Load);
                }
            }
            key!(Enter) => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::Replace);
                }
                return close_fn(self);
            }
            ctrl!('s') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::HorizontalSplit);
                }
                return close_fn(self);
            }
            ctrl!('v') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::VerticalSplit);
                }
                return close_fn(self);
            }
            ctrl!('t') => {
                self.toggle_preview();
            }
            _ => {
                self.prompt_handle_event(event, ctx);
            }
        }

        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let block = Block::default().borders(Borders::ALL);
        // calculate the inner area inside the box
        let inner = block.inner(area);

        // prompt area
        let area = inner.clip_left(1).with_height(1);

        self.prompt.cursor(area, editor)
    }

    fn required_size(&mut self, (width, height): (u16, u16)) -> Option<(u16, u16)> {
        self.completion_height = height.saturating_sub(4);
        Some((width, height))
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
impl<T: 'static + Send + Sync, D> Drop for Picker<T, D> {
    fn drop(&mut self) {
        // ensure we cancel any ongoing background threads streaming into the picker
        self.version.fetch_add(1, atomic::Ordering::Relaxed);
    }
}

type PickerCallback<T> = Box<dyn Fn(&mut Context, &T, Action)>;

fn parse_query(
    column_names: &[&'static str],
    primary_column: usize,
    input: &str,
) -> HashMap<&'static str, String> {
    let mut fields: HashMap<&'static str, String> = HashMap::new();
    let primary_field = column_names[primary_column];
    let mut escaped = false;
    let mut quoted = false;
    let mut in_field = false;
    let mut field = None;
    let mut text = String::new();

    macro_rules! finish_field {
        () => {
            let key = field.take().unwrap_or(primary_field);

            if let Some(pattern) = fields.get_mut(key) {
                pattern.push(' ');
                pattern.push_str(&text);
                text.clear();
            } else {
                fields.insert(key, std::mem::take(&mut text));
            }
        };
    }

    for ch in input.chars() {
        match ch {
            // Backslash escaping for `%` and `"`
            '\\' => escaped = !escaped,
            _ if escaped => {
                if ch != '%' && ch != '"' {
                    text.push('\\');
                }
                text.push(ch);
                escaped = false;
            }
            // Double quoting
            '"' => quoted = !quoted,
            ' ' if quoted => {
                text.push('\\');
                text.push(' ');
            }
            '%' | ':' if quoted => text.push(ch),
            // Space either completes the current word if no field is specified
            // or field if one is specified.
            '%' | ' ' if !text.is_empty() => {
                finish_field!();
                in_field = ch == '%';
            }
            '%' => in_field = true,
            ' ' => (),
            ':' if in_field => {
                field = column_names
                    .iter()
                    .position(|name| name == &text)
                    .map(|idx| column_names[idx]);
                text.clear();
                in_field = false;
            }
            _ => text.push(ch),
        }
    }

    if !in_field && !text.is_empty() {
        finish_field!();
    }

    fields
}

/// Returns a new list of options to replace the contents of the picker
/// when called with the current picker query,
pub type DynQueryCallback<T, D> =
    Box<dyn Fn(String, &mut Editor, &Injector<T, D>) -> BoxFuture<'static, anyhow::Result<()>>>;

/// A picker that updates its contents via a callback whenever the
/// query string changes. Useful for live grep, workspace symbols, etc.
pub struct DynamicPicker<T: 'static + Send + Sync, D: 'static + Send + Sync> {
    file_picker: Picker<T, D>,
    query_callback: DynQueryCallback<T, D>,
    query: String,
    hook: tokio::sync::mpsc::Sender<String>,
}

impl<T: Send + Sync, D: Send + Sync> DynamicPicker<T, D> {
    pub fn new(file_picker: Picker<T, D>, query_callback: DynQueryCallback<T, D>) -> Self {
        let hook: DynamicPickerHook<T, D> = DynamicPickerHook {
            last_query: String::new(),
            query: None,
            phantom_data: Default::default(),
        };

        Self {
            file_picker,
            query_callback,
            query: String::new(),
            hook: hook.spawn(),
        }
    }
}

impl<T: Send + Sync + 'static, D: Send + Sync + 'static> Component for DynamicPicker<T, D> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        self.file_picker.render(area, surface, cx);
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let event_result = self.file_picker.handle_event(event, cx);
        let current_query = self.file_picker.primary_query();

        if self.query != *current_query {
            self.query = current_query.to_string();
            helix_event::send_blocking(&self.hook, self.query.clone());
        }

        event_result
    }

    fn cursor(&self, area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        self.file_picker.cursor(area, ctx)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.file_picker.required_size(viewport)
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}

struct DynamicPickerHook<T: 'static + Send + Sync, D: 'static + Send + Sync> {
    last_query: String,
    query: Option<String>,
    phantom_data: std::marker::PhantomData<(T, D)>,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> AsyncHook for DynamicPickerHook<T, D> {
    /// The new query line
    type Event = String;

    fn handle_event(&mut self, query: String, _timeout: Option<Instant>) -> Option<Instant> {
        if query == self.last_query {
            // If the search query reverts to the last one we requested, no need to
            // make a new request.
            self.query = None;
            None
        } else {
            self.query = Some(query);

            Some(Instant::now() + Duration::from_millis(275))
        }
    }

    fn finish_debounce(&mut self) {
        let Some(query) = self.query.take() else { return };
        self.last_query = query.clone();

        dispatch_blocking(move |editor, compositor| {
            let Some(Overlay { content: dyn_picker, .. }) = compositor.find::<Overlay<DynamicPicker<T, D>>>() else {
                return;
            };
            // Increment the version number to cancel any ongoing requests.
            dyn_picker
                .file_picker
                .version
                .fetch_add(1, atomic::Ordering::Relaxed);
            dyn_picker.file_picker.matcher.restart(false);
            dyn_picker.file_picker.tmp_running = true;
            let injector = dyn_picker.file_picker.injector();
            let get_options = (dyn_picker.query_callback)(query, editor, &injector);
            tokio::spawn(async move {
                if let Err(err) = get_options.await {
                    // TODO: better message
                    log::error!("Failed to do dynamic request: {err}");
                }

                crate::job::dispatch(|editor, compositor| {
                    let Some(Overlay { content: dyn_picker, .. }) = compositor.find::<Overlay<DynamicPicker<T, D>>>() else {
                        return;
                    };
                    dyn_picker.file_picker.tmp_running = false;
                    editor.reset_idle_timer();
                }).await;
            });
        })
    }
}

#[cfg(test)]
mod test {
    use helix_core::hashmap;

    use super::*;

    #[test]
    fn parse_query_test() {
        let columns = &["primary", "field1", "field2"];
        let primary_column = 0;

        // Basic field splitting
        assert_eq!(
            parse_query(columns, primary_column, "hello world"),
            hashmap!(
                "primary" => "hello world".to_string(),
            )
        );
        assert_eq!(
            parse_query(columns, primary_column, "hello %field1:world %field2:!"),
            hashmap!(
                "primary" => "hello".to_string(),
                "field1" => "world".to_string(),
                "field2" => "!".to_string(),
            )
        );
        assert_eq!(
            parse_query(columns, primary_column, "%field1:abc %field2:def xyz"),
            hashmap!(
                "primary" => "xyz".to_string(),
                "field1" => "abc".to_string(),
                "field2" => "def".to_string(),
            )
        );

        // Trailing space is trimmed
        assert_eq!(
            parse_query(columns, primary_column, "hello "),
            hashmap!(
                "primary" => "hello".to_string(),
            )
        );

        // Trailing fields are trimmed.
        assert_eq!(
            parse_query(columns, primary_column, "hello %foo"),
            hashmap!(
                "primary" => "hello".to_string(),
            )
        );

        // Quoting
        assert_eq!(
            parse_query(columns, primary_column, "hello %field1:\"a b c\""),
            hashmap!(
                "primary" => "hello".to_string(),
                "field1" => "a\\ b\\ c".to_string(),
            )
        );

        // Escaping
        assert_eq!(
            parse_query(columns, primary_column, "hello \\%field1:world"),
            hashmap!(
                "primary" => "hello %field1:world".to_string(),
            )
        );
        assert_eq!(
            parse_query(columns, primary_column, "foo\\("),
            hashmap!(
                "primary" => "foo\\(".to_string(),
            )
        );
        assert_eq!(
            // hello %field1:"a\"b"
            parse_query(columns, primary_column, "hello %field1:\"a\\\"b\""),
            hashmap!(
                "primary" => "hello".to_string(),
                "field1" => "a\"b".to_string(),
            )
        );
    }
}
