//! Markdown → ratatui lines for assistant messages (plan 38 slice 1).
//!
//! `pulldown-cmark` drives a small block/inline walker that emits styled
//! [`Line`]s: headings, emphasis, inline code, ordered/unordered lists, block
//! quotes, box-drawing tables, and indented code blocks. Fenced code blocks that
//! name a supported language are syntax-highlighted with `synoptic` (plan 38
//! slice 7, 关键决定 2): a tight styles.md-safe palette — see [`token_style`].
//!
//! Emphasis carries real weight (plan 114): a bold span is the brand accent, so
//! a conclusion the model marked up stands out from the prose it sits in, while
//! code spans stay one quiet colour and never outshine it.
//!
//! Streaming uses a claw-style safe-boundary buffer ([`find_stream_safe_boundary`]):
//! only the part of the stream that ends on a stable boundary (a blank line, or a
//! closed code fence) is rendered as markdown; the still-forming tail is shown
//! raw so a half-written table or fence never reflows mid-stream. Once the tail
//! stabilizes (or the message finalizes) it renders as markdown too.

use pulldown_cmark::Alignment;
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use crate::render::BRAND;
use crate::text_layout::wrap;

/// Code (inline spans and fenced blocks) carries a foreground only. A filled
/// grey chip behind every span made dense technical prose read as a wall of
/// blocks — the `30s`/`status=0` noise outweighed the sentence's conclusion
/// (plan 114) — and the same fill padded each fence into a ragged rectangle.
const CODE_FG: Color = Color::Cyan;
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
/// Fenced blocks are set off by an indent instead of that fill.
const CODE_INDENT: &str = "  ";
/// Bullets by nesting depth. A single `•` at every level flattened the nesting
/// the model wrote; `-` and `·` keep the levels apart the way cc/codex do.
const BULLETS: [&str; 3] = ["• ", "- ", "· "];
/// Tab stop for code blocks — passed to synoptic (which expands tabs) and used
/// to expand tabs in the unhighlighted path, so both keep indentation.
const TAB_WIDTH: usize = 4;

/// One character carrying the inline style it was emitted with. The inline
/// builder accumulates these, then wraps them into [`Line`]s at width.
type Chars = Vec<(char, Style)>;

/// Render a complete (finalized) markdown string to styled lines. Used for a
/// sealed assistant cell — everything is stable, so the whole text is parsed.
pub fn markdown_lines(md: &str, width: usize) -> Vec<Line<'static>> {
    let normalized = normalize_nested_fences(md);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut r = Renderer::new(width.max(1));
    for ev in Parser::new_ext(&normalized, opts) {
        r.event(ev);
    }
    r.finish();
    r.out
}

/// Render a still-streaming assistant message: markdown for the stable prefix
/// (up to the last safe boundary), the forming tail raw so it cannot reflow.
/// The boundary is recomputed from the accumulated text each frame, so no
/// streaming state is threaded through the pure render path.
pub fn assistant_stream_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    match find_stream_safe_boundary(text) {
        Some(split) => {
            let mut lines = markdown_lines(&text[..split], width);
            // The tail is whatever has arrived since the last stable boundary —
            // a partial paragraph, or the open lines of an unclosed fence. Show
            // it as plain wrapped text; it becomes markdown once it stabilizes.
            let tail = text[split..].trim_matches('\n');
            if !tail.is_empty() {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                lines.extend(wrap(tail, width).into_iter().map(Line::from));
            }
            lines
        }
        // Nothing stable yet: the whole message is still forming, show it raw.
        None => wrap(text.trim_matches('\n'), width)
            .into_iter()
            .map(Line::from)
            .collect(),
    }
}

/// Accumulates markdown events into styled lines. Block elements push their
/// wrapped content onto `out`; inline elements build `inline` until a block
/// boundary flushes it.
struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    /// The inline content of the current block (or the current table cell).
    inline: Chars,
    /// Active inline styles (Strong/Emphasis/Link/…); the effective style is
    /// their fold. A stack so nested emphasis composes.
    styles: Vec<Style>,
    /// Continuation prefix per open block level (list-item indent, quote bar).
    /// A wrapped block's non-first lines all carry this; the first line swaps
    /// the innermost level for `marker` when one is pending.
    prefix: Vec<Span<'static>>,
    /// The first-line marker (bullet / ordinal) for the innermost list item,
    /// consumed by the next flush so only that line shows it.
    marker: Option<Span<'static>>,
    /// Open list levels; `Some(n)` is an ordered list's next ordinal.
    lists: Vec<Option<u64>>,
    /// Items opened so far at each list level, so a loose list can tell its
    /// first item (which needs no gap above it) from the rest.
    list_items: Vec<u64>,
    /// The open list items, innermost last.
    items: Vec<OpenItem>,
    quote_depth: usize,
    /// Whether any top-level block has been emitted, so the next one is
    /// preceded by a blank separator (blocks nested in lists/quotes are tight).
    blocks_emitted: bool,
    /// The fenced code block currently open (raw text accumulates here, not as
    /// inline styled chars).
    code: Option<String>,
    /// Destinations of the open links, so the URL can follow the link text —
    /// a terminal cannot click it, and dropping it loses the address entirely.
    link_urls: Vec<String>,
    /// The open code block's info string (its language, e.g. `rust`), used to
    /// pick a syntax highlighter at flush (plan 38 slice 7). None for an
    /// indented block or a bare fence.
    code_lang: Option<String>,
    /// The table currently open.
    table: Option<TableAcc>,
}

/// A table under construction: alignments from the `Table` tag, the header
/// cells, then body rows. Cells are the inline styled chars of each `TableCell`.
/// One open list item: where its content starts in `out`, and whether the
/// source wrote the list loose. Loose means the item's blocks arrive wrapped in
/// paragraphs — the author left blank lines — and only then do the blocks inside
/// it get blank lines here. A tight nested list must stay tight.
struct OpenItem {
    start: usize,
    loose: bool,
}

struct TableAcc {
    aligns: Vec<Alignment>,
    head: Vec<Chars>,
    rows: Vec<Vec<Chars>>,
    in_head: bool,
    row: Vec<Chars>,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            inline: Vec::new(),
            styles: Vec::new(),
            prefix: Vec::new(),
            marker: None,
            lists: Vec::new(),
            list_items: Vec::new(),
            items: Vec::new(),
            quote_depth: 0,
            blocks_emitted: false,
            link_urls: Vec::new(),
            code: None,
            code_lang: None,
            table: None,
        }
    }

    /// The folded effective style of the active inline stack.
    fn eff(&self) -> Style {
        let mut s = Style::default();
        for st in &self.styles {
            s = s.patch(*st);
        }
        s
    }

    /// Column width available to content after the current block prefix.
    fn content_width(&self) -> usize {
        let used: usize = self.prefix.iter().map(span_width).sum();
        self.width.saturating_sub(used).max(1)
    }

    /// Whether the inline buffer already ends with `text` — an autolink whose
    /// visible text is its own URL.
    fn inline_ends_with(&self, text: &str) -> bool {
        let tail: String = self
            .inline
            .iter()
            .rev()
            .take(text.chars().count())
            .map(|(c, _)| *c)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        tail == text
    }

    fn push_str(&mut self, s: &str, style: Style) {
        for c in s.chars() {
            self.inline.push((c, style));
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some(buf) = &mut self.code {
                    buf.push_str(&t);
                    return;
                }
                let st = self.eff();
                self.push_str(&t, st);
            }
            Event::Code(t) => {
                let st = self.eff().patch(Style::new().fg(CODE_FG));
                self.push_str(&t, st);
            }
            // A soft line break (single newline in the source) reflows as a
            // space; the wrapper re-breaks at width. A hard break forces a line.
            Event::SoftBreak => self.inline.push((' ', Style::default())),
            Event::HardBreak => self.inline.push(('\n', Style::default())),
            Event::Rule => {
                self.flush_para();
                self.block_gap();
                let w = self.content_width();
                self.out.push(prefixed(
                    &self.prefix,
                    vec![Span::styled("─".repeat(w), DIM)],
                ));
                self.blocks_emitted = true;
            }
            Event::TaskListMarker(done) => {
                let st = self.eff();
                self.push_str(if done { "[x] " } else { "[ ] " }, st);
            }
            // Raw HTML in assistant markdown is rare; show it dim rather than
            // dropping it, so a stray tag is at least visible.
            Event::Html(h) | Event::InlineHtml(h) => {
                if self.code.is_some() {
                    return;
                }
                let text = h.trim_end_matches('\n').to_string();
                let st = self.eff().patch(DIM);
                self.push_str(&text, st);
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            // A paragraph sitting directly under an item marker means a loose
            // list — its items are blank-line separated in the source, and they
            // read that way here too, except above the first (the gap before the
            // list already covers that one).
            Tag::Paragraph => {
                if self.marker.is_some() {
                    // Reaching a paragraph before the item's marker was consumed
                    // is what identifies a loose list.
                    if let Some(item) = self.items.last_mut() {
                        item.loose = true;
                    }
                    if self.blocks_emitted && self.list_items.last().is_some_and(|&n| n > 1) {
                        self.out.push(Line::default());
                    }
                } else {
                    self.block_gap();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_para();
                self.block_gap();
                self.styles.push(heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_para();
                self.block_gap();
                self.prefix.push(Span::styled("│ ".to_string(), DIM));
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_para();
                self.code = Some(String::new());
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(info) => Some(info.to_string()),
                    CodeBlockKind::Indented => None,
                };
            }
            Tag::List(first) => {
                self.flush_para();
                self.block_gap();
                self.lists.push(first);
                self.list_items.push(0);
            }
            Tag::Item => {
                if let Some(n) = self.list_items.last_mut() {
                    *n += 1;
                }
                self.items.push(OpenItem {
                    start: self.out.len(),
                    loose: false,
                });
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last().copied() {
                    Some(Some(n)) => {
                        if let Some(slot) = self.lists.last_mut() {
                            *slot = Some(n + 1);
                        }
                        format!("{n}. ")
                    }
                    _ => BULLETS[depth.min(BULLETS.len() - 1)].to_string(),
                };
                let w = display_width(&marker);
                // The outermost marker carries the accent; nested ones stay dim
                // so depth reads as receding, not as another thing to look at.
                let style = if depth == 0 {
                    Style::new().fg(BRAND)
                } else {
                    DIM
                };
                self.marker = Some(Span::styled(marker, style));
                self.prefix.push(Span::raw(" ".repeat(w)));
            }
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            // Bold is how a model marks its verdict; give it the accent colour
            // so it carries across a screen of prose, not just a weight the
            // terminal may or may not render heavier.
            Tag::Strong => self
                .styles
                .push(Style::new().add_modifier(Modifier::BOLD).fg(BRAND)),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            // A link renders its text underlined and keeps its URL (appended at
            // the end tag). An image has no useful address to show, so its URL
            // is dropped.
            Tag::Link { dest_url, .. } => {
                self.link_urls.push(dest_url.to_string());
                self.styles
                    .push(Style::new().add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { .. } => self
                .styles
                .push(Style::new().add_modifier(Modifier::UNDERLINED)),
            Tag::Table(aligns) => {
                self.flush_para();
                self.table = Some(TableAcc {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    in_head: false,
                    row: Vec::new(),
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                }
                self.inline.clear();
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.in_head = false;
                    t.row.clear();
                }
            }
            Tag::TableCell => self.inline.clear(),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_para(),
            TagEnd::Heading(_) => {
                self.flush_para();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_para();
                self.prefix.pop();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => self.flush_code(),
            TagEnd::List(_) => {
                self.lists.pop();
                self.list_items.pop();
            }
            TagEnd::Item => {
                self.flush_para();
                self.prefix.pop();
                self.items.pop();
                self.marker = None;
            }
            TagEnd::Link => {
                self.styles.pop();
                // An autolink's text is already the URL; printing it twice is
                // noise, not information.
                if let Some(url) = self.link_urls.pop()
                    && !url.is_empty()
                    && !self.inline_ends_with(&url)
                {
                    self.push_str(&format!(" ({url})"), DIM);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Image => {
                self.styles.pop();
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.render_table(t);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(t) = &mut self.table {
                    if t.in_head {
                        t.head.push(cell);
                    } else {
                        t.row.push(cell);
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self) {
        // A stream that ends mid-paragraph (no trailing block close) still has
        // pending inline; commit it.
        self.flush_para();
    }

    /// A blank separator before the next top-level block. Blocks nested inside a
    /// list or quote are tight (no blank), matching how markdown reads in a
    /// terminal.
    /// The blank line between two blocks. At the top level every pair gets one.
    /// Inside a list or quote only a block that follows content the same item
    /// already emitted does — a gap above an item's first line would open a hole
    /// under its own bullet.
    fn block_gap(&mut self) {
        if !self.blocks_emitted {
            return;
        }
        let nested = !self.lists.is_empty() || self.quote_depth > 0;
        let item_has_content = self
            .items
            .last()
            .is_some_and(|item| item.loose && self.out.len() > item.start);
        if !nested || item_has_content {
            self.out.push(Line::default());
        }
    }

    /// Wrap and emit the accumulated inline content as a block. The first line
    /// carries the pending list marker (if any); wrapped continuations carry the
    /// plain prefix. A no-op when there is nothing to emit.
    fn flush_para(&mut self) {
        if self.inline.is_empty() && self.marker.is_none() {
            return;
        }
        let chars = std::mem::take(&mut self.inline);
        let content_w = self.content_width();
        let wrapped = wrap_words(&chars, content_w);
        let mut marker = self.marker.take();
        for line_chars in wrapped {
            let prefix = match marker.take() {
                Some(m) => first_prefix(&self.prefix, m),
                None => self.prefix.clone(),
            };
            self.out.push(prefixed(&prefix, coalesce(line_chars)));
        }
        self.blocks_emitted = true;
    }

    /// Emit the open code block: syntax-highlighted source (plan 38 slice 7)
    /// over a dim background, hard-wrapped to the content width and padded to a
    /// clean rectangle. Highlighting runs per source line via `synoptic` when the
    /// fence names a supported language; otherwise the source shows plain.
    fn flush_code(&mut self) {
        let buf = self.code.take().unwrap_or_default();
        let lang = self.code_lang.take();
        let body = buf.strip_suffix('\n').unwrap_or(&buf);
        if body.is_empty() {
            return;
        }
        self.block_gap();
        // Set off by an indent, not by a filled rectangle: a grey slab sized to
        // the longest line drew the eye before the prose explaining it did.
        self.prefix.push(Span::raw(CODE_INDENT.to_string()));
        let avail = self.content_width();
        // One styled `Chars` per source line, then hard-wrap each to width
        // (code never reflows on spaces), preserving per-token styles.
        let frags: Vec<Chars> = highlight_code(body, lang.as_deref())
            .into_iter()
            .flat_map(|line| hard_wrap_chars(&line, avail))
            .collect();
        for frag in frags {
            self.out.push(prefixed(&self.prefix, coalesce(frag)));
        }
        self.prefix.pop();
        self.blocks_emitted = true;
    }

    /// Render a collected table with box-drawing borders. Column widths grow to
    /// fit content, then shrink (widest first) to fit the available width; cells
    /// that still overflow are truncated with `…`.
    fn render_table(&mut self, t: TableAcc) {
        let ncols = t
            .aligns
            .len()
            .max(t.head.len())
            .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }
        self.block_gap();

        let mut colw = vec![0usize; ncols];
        let mut fit = |cells: &[Chars]| {
            for (i, c) in cells.iter().enumerate() {
                colw[i] = colw[i].max(chars_width(c));
            }
        };
        fit(&t.head);
        for row in &t.rows {
            fit(row);
        }

        // Fit within the content width: 1 leading border + per column a border,
        // two padding spaces, and the content (`│ x │ y │` → 3*ncols + 1 chrome).
        let budget = self
            .content_width()
            .saturating_sub(3 * ncols + 1)
            .max(ncols);
        while colw.iter().sum::<usize>() > budget {
            let Some(i) = widest_col(&colw) else { break };
            if colw[i] <= 1 {
                break;
            }
            colw[i] -= 1;
        }

        let prefix = self.prefix.clone();
        self.out
            .push(prefixed(&prefix, vec![border('┌', '┬', '┐', &colw)]));
        if !t.head.is_empty() {
            let line = table_row(&t.head, &colw, &t.aligns, true);
            self.out.push(prefixed(&prefix, line));
            self.out
                .push(prefixed(&prefix, vec![border('├', '┼', '┤', &colw)]));
        }
        for row in &t.rows {
            let line = table_row(row, &colw, &t.aligns, false);
            self.out.push(prefixed(&prefix, line));
        }
        self.out
            .push(prefixed(&prefix, vec![border('└', '┴', '┘', &colw)]));
        self.blocks_emitted = true;
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    // The top two levels carry the accent so a section title outranks the bold
    // spans inside it; deeper ones stay bold-only.
    let base = Style::new().add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => base.fg(BRAND),
        _ => base,
    }
}

/// The first-line prefix of a list item: the enclosing levels' indents with the
/// innermost swapped for the bullet/ordinal marker (same display width, so
/// continuation lines line up under the text).
fn first_prefix(prefix: &[Span<'static>], marker: Span<'static>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = prefix.to_vec();
    match out.last_mut() {
        Some(last) => *last = marker,
        None => out.push(marker),
    }
    out
}

/// Prepend the block prefix to a line's content spans.
fn prefixed(prefix: &[Span<'static>], content: Vec<Span<'static>>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = prefix.to_vec();
    spans.extend(content);
    Line::from(spans)
}

fn span_width(s: &Span) -> usize {
    display_width(&s.content)
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn chars_width(chars: &Chars) -> usize {
    chars.iter().map(|(c, _)| c.width().unwrap_or(0)).sum()
}

/// Coalesce a wrapped line's styled chars into the minimal run of spans (one
/// per maximal same-style run).
fn coalesce(chars: Chars) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for (c, st) in chars {
        match cur {
            Some(s) if s == st => buf.push(c),
            _ => {
                if let Some(s) = cur {
                    spans.push(Span::styled(std::mem::take(&mut buf), s));
                }
                buf.push(c);
                cur = Some(st);
            }
        }
    }
    if let Some(s) = cur {
        spans.push(Span::styled(buf, s));
    }
    spans
}

/// Word-wrap styled chars to `width` display columns: break at spaces, hard-break
/// words longer than a line, and honour forced newlines (`\n`, from hard breaks).
/// CJK runs have no spaces, so they hard-break per character — matching the
/// transcript's CJK-aware wrapping.
fn wrap_words(chars: &Chars, width: usize) -> Vec<Chars> {
    let width = width.max(1);
    let mut out: Vec<Chars> = Vec::new();
    let mut seg: Chars = Vec::new();
    for &(c, st) in chars {
        if c == '\n' {
            out.extend(wrap_segment(&seg, width));
            seg.clear();
        } else {
            seg.push((c, st));
        }
    }
    out.extend(wrap_segment(&seg, width));
    out
}

/// Wrap one newline-free segment. Always returns at least one (possibly empty)
/// line so a blank paragraph / empty list item still occupies a row.
fn wrap_segment(chars: &Chars, width: usize) -> Vec<Chars> {
    let words = split_words(chars);
    let mut lines: Vec<Chars> = Vec::new();
    let mut cur: Chars = Vec::new();
    let mut cur_w = 0usize;
    for word in words {
        let ww = chars_width(&word);
        if ww > width {
            // A single word longer than a line: flush, then hard-break it.
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut chunk: Chars = Vec::new();
            let mut chunk_w = 0usize;
            for (c, st) in word {
                let cw = c.width().unwrap_or(0);
                if chunk_w + cw > width && !chunk.is_empty() {
                    lines.push(std::mem::take(&mut chunk));
                    chunk_w = 0;
                }
                chunk.push((c, st));
                chunk_w += cw;
            }
            cur = chunk;
            cur_w = chunk_w;
            continue;
        }
        let sep = usize::from(!cur.is_empty());
        if cur_w + sep + ww > width {
            lines.push(std::mem::take(&mut cur));
            cur = word;
            cur_w = ww;
        } else {
            if sep == 1 {
                // Carry the run's own style across the space when both sides
                // agree, so a bold phrase stays one span; at a style boundary
                // the separator stays plain (an underline must not stretch).
                let style = match (cur.last(), word.first()) {
                    (Some((_, left)), Some((_, right))) if left == right => *left,
                    _ => Style::default(),
                };
                cur.push((' ', style));
                cur_w += 1;
            }
            cur.extend(word);
            cur_w += ww;
        }
    }
    lines.push(cur);
    lines
}

/// Split on ASCII spaces, dropping the spaces (runs collapse to one separator at
/// wrap time). Each word keeps its per-char styles.
fn split_words(chars: &Chars) -> Vec<Chars> {
    let mut words: Vec<Chars> = Vec::new();
    let mut w: Chars = Vec::new();
    for &(c, st) in chars {
        if c == ' ' {
            if !w.is_empty() {
                words.push(std::mem::take(&mut w));
            }
        } else {
            w.push((c, st));
        }
    }
    if !w.is_empty() {
        words.push(w);
    }
    words
}

/// Hard-wrap one source line of styled chars at `width` columns (CJK-aware),
/// breaking purely at the column boundary — code does not reflow on spaces.
/// Always yields at least one (possibly empty) line so a blank line keeps its row.
fn hard_wrap_chars(chars: &Chars, width: usize) -> Vec<Chars> {
    let width = width.max(1);
    let mut lines: Vec<Chars> = Vec::new();
    let mut line: Chars = Vec::new();
    let mut cols = 0usize;
    for &(c, st) in chars {
        let cw = c.width().unwrap_or(0);
        if cols + cw > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            cols = 0;
        }
        line.push((c, st));
        cols += cw;
    }
    lines.push(line);
    lines
}

/// Syntax-highlight a code block body into one styled `Chars` per source line.
/// Each char carries its token colour; with no supported language (or a bare
/// fence) every char is plain.
fn highlight_code(body: &str, lang: Option<&str>) -> Vec<Chars> {
    let base = Style::default();
    let lines: Vec<String> = body.split('\n').map(str::to_string).collect();
    let highlighter = lang.and_then(lang_to_ext).map(|ext| {
        let mut h = synoptic::from_extension(ext, TAB_WIDTH).expect("from_extension is total");
        h.run(&lines);
        h
    });
    lines
        .iter()
        .enumerate()
        .map(|(y, raw)| match &highlighter {
            Some(h) => h
                .line(y, raw)
                .into_iter()
                .flat_map(|tok| {
                    let (text, style) = match tok {
                        synoptic::TokOpt::Some(t, kind) => (t, base.patch(token_style(&kind))),
                        synoptic::TokOpt::None(t) => (t, base),
                    };
                    text.chars().map(move |c| (c, style)).collect::<Chars>()
                })
                .collect(),
            // No highlighter: expand tabs to spaces to match synoptic's tab
            // handling (a raw '\t' is zero-width, so ratatui drops it and the
            // indentation vanishes — an unhighlighted block would otherwise lose
            // the indent a highlighted one keeps). Same reason toolrow sanitizes.
            None => raw
                .replace('\t', &" ".repeat(TAB_WIDTH))
                .chars()
                .map(|c| (c, base))
                .collect(),
        })
        .collect()
}

/// Map a synoptic token kind to a styles.md-safe style (plan 38 slice 7). The
/// palette is tight on purpose — cyan for structure, green for strings, magenta
/// for keywords, dim for comments — no yellow/blue/black/white foregrounds that
/// theme unreliably. Magenta does double duty as the brand accent, but inside an
/// indented code block it reads as a keyword, not chrome. Operators and digits
/// stay plain: colouring them lit up every `.`, `/` and `=1` in a shell block.
fn token_style(kind: &str) -> Style {
    match kind {
        "keyword" | "boolean" => Style::new().fg(Color::Magenta),
        "string" => Style::new().fg(Color::Green),
        "comment" => DIM,
        "function" | "struct" | "namespace" | "tag" | "attribute" | "type" | "key" | "header"
        | "heading" => Style::new().fg(Color::Cyan),
        _ => Style::default(),
    }
}

/// Normalize a fence info string (`rust`, `py`, `c++`, `bash,ignore`) to an
/// extension `synoptic::from_extension` recognizes, or `None` to skip
/// highlighting for unknown / bare-fence blocks.
fn lang_to_ext(lang: &str) -> Option<&'static str> {
    let first = lang
        .trim()
        .split([',', ' ', '\t'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    Some(match first.as_str() {
        "rust" | "rs" => "rs",
        "python" | "py" => "py",
        "javascript" | "js" | "node" | "jsx" | "mjs" => "js",
        "typescript" | "ts" | "tsx" => "ts",
        "bash" | "sh" | "shell" | "zsh" | "console" => "sh",
        "c" | "h" => "c",
        "cpp" | "c++" | "cxx" | "cc" | "hpp" => "cpp",
        "csharp" | "cs" => "cs",
        "go" | "golang" => "go",
        "java" => "java",
        "kotlin" | "kt" => "kt",
        "ruby" | "rb" => "rb",
        "php" => "php",
        "swift" => "swift",
        "scala" => "scala",
        "lua" => "lua",
        "haskell" | "hs" => "hs",
        "json" => "json",
        "yaml" | "yml" => "yml",
        "toml" => "toml",
        "css" => "css",
        "html" | "htm" | "xhtml" => "html",
        "xml" => "xml",
        "sql" => "sql",
        "markdown" | "md" => "md",
        "diff" | "patch" => "diff",
        _ => return None,
    })
}

fn widest_col(colw: &[usize]) -> Option<usize> {
    colw.iter()
        .enumerate()
        .max_by_key(|(_, w)| **w)
        .map(|(i, _)| i)
}

/// A table border line (`┌───┬───┐` etc.) spanning `w+2` per column for the pad.
fn border(left: char, mid: char, right: char, colw: &[usize]) -> Span<'static> {
    let mut s = String::new();
    s.push(left);
    for (i, w) in colw.iter().enumerate() {
        s.push_str(&"─".repeat(w + 2));
        s.push(if i + 1 == colw.len() { right } else { mid });
    }
    Span::styled(s, DIM)
}

/// One table row: `│ ` (dim) + aligned/truncated cell + ` ` per column, closed
/// by `│`. Header cells render bold.
fn table_row(
    cells: &[Chars],
    colw: &[usize],
    aligns: &[Alignment],
    header: bool,
) -> Vec<Span<'static>> {
    let empty: Chars = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, &w) in colw.iter().enumerate() {
        spans.push(Span::styled("│ ".to_string(), DIM));
        let cell = cells.get(i).unwrap_or(&empty);
        let align = aligns.get(i).copied().unwrap_or(Alignment::None);
        spans.extend(fit_cell(cell, w, align, header));
        spans.push(Span::raw(" ".to_string()));
    }
    spans.push(Span::styled("│".to_string(), DIM));
    spans
}

/// Truncate a cell to `w` columns (with `…`), then pad per alignment. Header
/// cells get bold layered on.
fn fit_cell(cell: &Chars, w: usize, align: Alignment, header: bool) -> Vec<Span<'static>> {
    let mut chars = truncate_chars(cell, w);
    if header {
        for c in &mut chars {
            c.1 = c.1.patch(Style::new().add_modifier(Modifier::BOLD));
        }
    }
    let pad = w.saturating_sub(chars_width(&chars));
    let (left, right) = match align {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        _ => (0, pad),
    };
    let mut spans = Vec::new();
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    spans.extend(coalesce(chars));
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }
    spans
}

/// Truncate styled chars to `w` display columns, replacing the cut with `…`.
fn truncate_chars(chars: &Chars, w: usize) -> Chars {
    if chars_width(chars) <= w {
        return chars.clone();
    }
    let w = w.max(1);
    let mut out: Chars = Vec::new();
    let mut used = 0usize;
    for &(c, st) in chars {
        let cw = c.width().unwrap_or(0);
        if used + cw > w.saturating_sub(1) {
            out.push(('…', st));
            break;
        }
        out.push((c, st));
        used += cw;
    }
    out
}

// ---------------------------------------------------------------------------
// Streaming safe boundary + nested-fence normalization (ported from claw-code's
// render.rs — the same logic, minus its ANSI specifics; see plan 38 参考结论).
// ---------------------------------------------------------------------------

/// The byte offset up to which `markdown` is safe to render without a later
/// delta reflowing it: the end of the last blank line at top level, or the end
/// of a line that closes an open code fence. `None` when nothing is stable yet.
fn find_stream_safe_boundary(markdown: &str) -> Option<usize> {
    let mut open_fence: Option<FenceMarker> = None;
    let mut last_boundary = None;
    let mut cursor = 0usize;
    for line in markdown.split_inclusive('\n') {
        let start = cursor;
        cursor += line.len();
        let bare = line.trim_end_matches('\n');
        if let Some(opener) = open_fence {
            if line_closes_fence(bare, opener) {
                open_fence = None;
                last_boundary = Some(start + line.len());
            }
            continue;
        }
        if let Some(opener) = parse_fence_opener(bare) {
            open_fence = Some(opener);
            continue;
        }
        if bare.trim().is_empty() {
            last_boundary = Some(start + line.len());
        }
    }
    last_boundary
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FenceMarker {
    character: char,
    length: usize,
}

fn parse_fence_opener(line: &str) -> Option<FenceMarker> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let character = rest.chars().next()?;
    if character != '`' && character != '~' {
        return None;
    }
    let length = rest.chars().take_while(|c| *c == character).count();
    if length < 3 {
        return None;
    }
    // A backtick fence's info string may not contain a backtick.
    if character == '`' && rest[length..].contains('`') {
        return None;
    }
    Some(FenceMarker { character, length })
}

fn line_closes_fence(line: &str, opener: FenceMarker) -> bool {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let length = rest.chars().take_while(|c| *c == opener.character).count();
    if length < opener.length {
        return false;
    }
    rest[length..].chars().all(|c| c == ' ' || c == '\t')
}

/// Wrap a fenced code block whose body contains fence markers of equal-or-greater
/// length in a longer fence, so `CommonMark` doesn't treat the inner marker as
/// the close. LLMs emit nested triple-backtick examples constantly; without this
/// the outer block breaks at the first inner fence.
fn normalize_nested_fences(markdown: &str) -> String {
    struct FenceLine {
        ch: char,
        len: usize,
        has_info: bool,
        indent: usize,
    }

    fn parse(line: &str) -> Option<FenceLine> {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        let indent = trimmed.chars().take_while(|c| *c == ' ').count();
        if indent > 3 {
            return None;
        }
        let rest = &trimmed[indent..];
        let ch = rest.chars().next()?;
        if ch != '`' && ch != '~' {
            return None;
        }
        let len = rest.chars().take_while(|c| *c == ch).count();
        if len < 3 {
            return None;
        }
        let after = &rest[len..];
        if ch == '`' && after.contains('`') {
            return None;
        }
        Some(FenceLine {
            ch,
            len,
            has_info: !after.trim().is_empty(),
            indent,
        })
    }

    let lines: Vec<&str> = markdown.split_inclusive('\n').collect();
    let info: Vec<Option<FenceLine>> = lines.iter().map(|l| parse(l)).collect();

    // Pair openers with closers via a stack; a labelled fence is always an
    // opener, a bare fence closes a compatible top-of-stack or else opens.
    let mut stack: Vec<usize> = Vec::new();
    let mut pairs: Vec<(usize, usize, usize)> = Vec::new(); // (opener, closer, max inner len)
    for (i, fi) in info.iter().enumerate() {
        let Some(fl) = fi else { continue };
        if fl.has_info {
            stack.push(i);
            continue;
        }
        let closes = stack.last().is_some_and(|&top| {
            let t = info[top].as_ref().unwrap();
            t.ch == fl.ch && fl.len >= t.len
        });
        if closes {
            let opener = stack.pop().unwrap();
            let inner_max = info[opener + 1..i]
                .iter()
                .filter_map(|f| f.as_ref().map(|f| f.len))
                .max()
                .unwrap_or(0);
            pairs.push((opener, i, inner_max));
        } else {
            stack.push(i);
        }
    }

    // A pair needs rewriting when its fence isn't longer than the longest fence
    // nested inside it.
    let mut new_len: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (opener, closer, inner_max) in pairs {
        let fl = info[opener].as_ref().unwrap();
        if fl.len <= inner_max {
            new_len.insert(opener, inner_max + 1);
            new_len.insert(closer, inner_max + 1);
        }
    }
    if new_len.is_empty() {
        return markdown.to_string();
    }

    let mut out = String::with_capacity(markdown.len() + new_len.len() * 4);
    for (i, line) in lines.iter().enumerate() {
        match new_len.get(&i) {
            Some(&len) => {
                let fl = info[i].as_ref().unwrap();
                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                let tail_info = &trimmed[fl.indent + fl.len..];
                let trailing = &line[trimmed.len()..];
                out.push_str(&" ".repeat(fl.indent));
                out.push_str(&fl.ch.to_string().repeat(len));
                out.push_str(tail_info);
                out.push_str(trailing);
            }
            None => out.push_str(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten a line to its plain text (styles dropped), for structural checks.
    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(text).collect()
    }

    #[test]
    fn paragraph_reflows_soft_breaks_and_wraps_cjk() {
        // A single newline inside a paragraph is a soft break → space (reflow).
        assert_eq!(
            texts(&markdown_lines("hello\nworld", 40)),
            vec!["hello world"]
        );
        // CJK counts as width 2 and hard-breaks per char at the boundary.
        assert_eq!(texts(&markdown_lines("你好世界", 4)), vec!["你好", "世界"]);
    }

    #[test]
    fn emphasis_and_inline_code_carry_styles() {
        let lines = markdown_lines("a **b** `c`", 40);
        assert_eq!(texts(&lines), vec!["a b c"]);
        // Spans: "a " (plain), "b" (bold accent), " " (plain), "c" (code fg).
        let spans = &lines[0].spans;
        let bold = spans.iter().find(|s| s.content.as_ref() == "b").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(bold.style.fg, Some(BRAND));
        let code = spans.iter().find(|s| s.content.as_ref() == "c").unwrap();
        assert_eq!(code.style.fg, Some(CODE_FG));
        // No filled chip behind a code span: the colour alone marks it.
        assert_eq!(code.style.bg, None);
    }

    #[test]
    fn headings_are_bold_and_separated() {
        let lines = markdown_lines("# Title\n\nbody", 40);
        assert_eq!(texts(&lines), vec!["Title", "", "body"]);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        // The top two levels take the accent; deeper ones are bold only.
        assert_eq!(lines[0].spans[0].style.fg, Some(BRAND));
        assert_eq!(markdown_lines("### Sub", 40)[0].spans[0].style.fg, None);
        assert!(
            !lines[2].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn bullet_and_ordered_lists_get_markers_and_indent() {
        let lines = markdown_lines("- one\n- two", 40);
        assert_eq!(texts(&lines), vec!["• one", "• two"]);

        let lines = markdown_lines("1. first\n2. second", 40);
        assert_eq!(texts(&lines), vec!["1. first", "2. second"]);
    }

    /// A wrapped list item indents its continuation under the text, not the
    /// marker, so the bullet column stays clean.
    #[test]
    fn list_item_wraps_with_hanging_indent() {
        let lines = markdown_lines("- alpha beta gamma", 9);
        assert_eq!(texts(&lines), vec!["• alpha", "  beta", "  gamma"]);
    }

    #[test]
    fn block_quote_gets_a_dim_bar() {
        let lines = markdown_lines("> quoted", 40);
        assert_eq!(texts(&lines), vec!["│ quoted"]);
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn code_block_is_indented_and_keeps_source() {
        let lines = markdown_lines("```rust\nlet x = 1;\n```", 40);
        assert_eq!(texts(&lines), vec!["  let x = 1;"]);
        // Set off by the indent alone — no filled rectangle behind it.
        assert!(lines[0].spans.iter().all(|s| s.style.bg.is_none()));
    }

    /// A fenced block with a supported language is syntax-highlighted (plan 38
    /// slice 7): keyword magenta, string green, comment dim, no background.
    #[test]
    fn code_block_syntax_highlights_by_language() {
        let lines = markdown_lines("```rust\nlet s = \"hi\"; // note\n```", 40);
        let spans = &lines[0].spans;
        let find = |needle: &str| {
            spans
                .iter()
                .find(|s| s.content.contains(needle))
                .unwrap_or_else(|| panic!("span with {needle:?}: {spans:?}"))
        };
        assert_eq!(find("let").style.fg, Some(Color::Magenta));
        assert_eq!(find("hi").style.fg, Some(Color::Green));
        assert!(find("note").style.add_modifier.contains(Modifier::DIM));
        assert!(spans.iter().all(|s| s.style.bg.is_none()));
        assert!(spans.iter().all(|s| s.style.fg != Some(Color::Yellow)));
    }

    /// A bare fence (no language) or an unknown language is not highlighted —
    /// every span is plain.
    #[test]
    fn code_block_without_language_is_plain() {
        for md in ["```\nlet x = 1;\n```", "```nope\nlet x = 1;\n```"] {
            let lines = markdown_lines(md, 40);
            assert!(
                lines[0].spans.iter().all(|s| s.style.fg.is_none()),
                "unhighlighted: {md}"
            );
            assert!(lines[0].spans.iter().all(|s| s.style.bg.is_none()));
        }
    }

    /// An unhighlighted (unknown-language / bare) code block expands tabs to
    /// spaces so its indentation survives — a raw '\t' is zero-width and would
    /// otherwise be dropped, unlike a highlighted block where synoptic expands
    /// tabs. Both keep the indent, consistently.
    #[test]
    fn unhighlighted_code_block_expands_tabs() {
        let lines = markdown_lines("```makefile\n\tall:\n```", 40);
        assert_eq!(
            texts(&lines)[0],
            "      all:",
            "two-column indent + a tab expanded to 4 spaces"
        );
    }

    #[test]
    fn lang_to_ext_normalizes_names_and_rejects_unknown() {
        assert_eq!(lang_to_ext("rust"), Some("rs"));
        assert_eq!(lang_to_ext("python"), Some("py"));
        assert_eq!(lang_to_ext("c++"), Some("cpp"));
        // Info strings carry attributes after the language name.
        assert_eq!(lang_to_ext("bash,ignore"), Some("sh"));
        assert_eq!(lang_to_ext("TypeScript"), Some("ts"));
        assert_eq!(lang_to_ext("brainfuck"), None);
        assert_eq!(lang_to_ext(""), None);
    }

    #[test]
    fn token_style_uses_the_safe_palette() {
        assert_eq!(token_style("keyword").fg, Some(Color::Magenta));
        assert_eq!(token_style("string").fg, Some(Color::Green));
        assert_eq!(token_style("type").fg, Some(Color::Cyan));
        // Punctuation and numbers keep the plain foreground — see the doc above.
        assert_eq!(token_style("operator"), Style::default());
        assert_eq!(token_style("digit"), Style::default());
        assert!(token_style("comment").add_modifier.contains(Modifier::DIM));
        assert_eq!(token_style("whatever"), Style::default());
        // No banned foregrounds anywhere in the map.
        for kind in ["keyword", "string", "function", "comment", "type"] {
            let fg = token_style(kind).fg;
            assert!(fg != Some(Color::Yellow) && fg != Some(Color::Blue));
        }
    }

    /// Nesting reads as nesting: each level gets its own glyph, so a sub-point
    /// is visibly subordinate instead of another `•` at a different indent.
    #[test]
    fn nested_bullets_change_glyph_by_depth() {
        let lines = markdown_lines("- a\n  - b\n    - c\n      - d", 40);
        assert_eq!(texts(&lines), vec!["• a", "  - b", "    · c", "      · d"]);
        // The outer marker is the accent, deeper ones recede.
        assert_eq!(lines[0].spans[0].style.fg, Some(BRAND));
        assert!(lines[1].spans[1].style.add_modifier.contains(Modifier::DIM));
    }

    /// A loose list (blank lines in the source) keeps them; a tight one stays
    /// tight. Same markup, same rhythm the model wrote.
    #[test]
    fn loose_list_keeps_its_blank_lines_and_tight_stays_tight() {
        assert_eq!(texts(&markdown_lines("- a\n- b", 40)), vec!["• a", "• b"]);
        assert_eq!(
            texts(&markdown_lines("- a\n\n- b", 40)),
            vec!["• a", "", "• b"]
        );
    }

    /// A second block inside one item — a nested list, another paragraph — is
    /// separated from the item's first line, which is what makes a long review
    /// item readable instead of one slab.
    #[test]
    fn blocks_inside_an_item_are_separated() {
        let lines = markdown_lines("- head\n\n  - sub\n\n    detail", 40);
        assert_eq!(
            texts(&lines),
            vec!["• head", "", "  - sub", "", "    detail"]
        );
    }

    /// A link keeps its address (a terminal cannot click the text), but an
    /// autolink whose text is already the URL does not print it twice.
    #[test]
    fn links_keep_their_url_without_repeating_an_autolink() {
        assert_eq!(
            texts(&markdown_lines("see [docs](https://x.dev/a)", 40)),
            vec!["see docs (https://x.dev/a)"]
        );
        assert_eq!(
            texts(&markdown_lines("<https://x.dev/a>", 40)),
            vec!["https://x.dev/a"]
        );
    }

    /// A code fence containing a nested triple-backtick example is upgraded to a
    /// longer fence so the whole block stays one unit.
    #[test]
    fn nested_fences_stay_one_block() {
        let md = "````\n```\ninner\n```\n````";
        let lines = markdown_lines(md, 40);
        // The inner backticks render as literal code content, not a broken
        // block; every line carries the block indent and nothing else.
        assert_eq!(texts(&lines), vec!["  ```", "  inner", "  ```"]);
        assert!(
            lines
                .iter()
                .all(|l| l.spans.iter().all(|s| s.style.bg.is_none()))
        );
    }

    #[test]
    fn table_renders_box_drawing_with_aligned_cells() {
        let md = "| A | B |\n|---|---|\n| 1 | 22 |";
        let lines = markdown_lines(md, 40);
        assert_eq!(
            texts(&lines),
            vec![
                "┌───┬────┐",
                "│ A │ B  │",
                "├───┼────┤",
                "│ 1 │ 22 │",
                "└───┴────┘",
            ]
        );
        // Header cell is bold.
        let header = &lines[1];
        let a = header
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "A")
            .unwrap();
        assert!(a.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn right_alignment_pads_on_the_left() {
        let md = "| n |\n|--:|\n| 5 |\n| 100 |";
        let lines = markdown_lines(md, 40);
        assert_eq!(
            texts(&lines),
            vec![
                "┌─────┐",
                "│   n │",
                "├─────┤",
                "│   5 │",
                "│ 100 │",
                "└─────┘"
            ]
        );
    }

    #[test]
    fn empty_input_renders_nothing() {
        assert!(markdown_lines("", 40).is_empty());
        assert!(markdown_lines("   \n  ", 40).is_empty());
    }

    #[test]
    fn safe_boundary_stops_at_blank_line_and_closed_fence() {
        // Boundary after the blank line; the second paragraph is still forming.
        let md = "para one\n\npara two";
        let split = find_stream_safe_boundary(md).unwrap();
        assert_eq!(&md[..split], "para one\n\n");

        // Inside an open fence there is no boundary until it closes.
        assert_eq!(find_stream_safe_boundary("```\ncode\nmore"), None);
        let closed = "```\ncode\n```\n";
        let split = find_stream_safe_boundary(closed).unwrap();
        assert_eq!(&closed[..split], closed);

        // Nothing stable yet.
        assert_eq!(find_stream_safe_boundary("half a line"), None);
    }

    /// Streaming shows the stable prefix as markdown and the forming tail raw,
    /// so a half-written block never reflows. The tail's `**` is literal until
    /// the stream stabilizes.
    #[test]
    fn stream_lines_render_stable_prefix_and_raw_tail() {
        let text = "# Done\n\nnow **bol";
        let lines = assistant_stream_lines(text, 40);
        let got = texts(&lines);
        assert_eq!(got.first().map(String::as_str), Some("Done")); // heading rendered
        assert!(got.contains(&"now **bol".to_string()), "tail raw: {got:?}");
        // The heading is bold (markdown), proving the prefix went through the
        // parser while the tail stayed literal.
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn stream_lines_all_raw_before_first_boundary() {
        let lines = assistant_stream_lines("still forming a sentence", 40);
        assert_eq!(texts(&lines), vec!["still forming a sentence"]);
    }
}
