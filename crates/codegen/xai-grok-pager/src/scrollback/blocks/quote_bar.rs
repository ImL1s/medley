//! Rendered blockquote-bar detection for selection/copy metadata.
//!
//! The markdown renderer rewrites each `>` quote marker to a `│` bar styled
//! `blockquote_outer` (xai-grok-markdown parse.rs), so the decoration becomes
//! ordinary span content and would otherwise leak into drag-select copies.
//! The helpers here detect that prefix on a rendered row and exclude it from
//! selection via [`Selectable::Spans`] — the same decoration-exclusion
//! machinery tool headers and diff gutters use. Shared by
//! [`MarkdownContent::output`](super::markdown_content::MarkdownContent::output)
//! and [`ThinkingBlock`](super::ThinkingBlock)'s render paths.

use std::borrow::Cow;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::scrollback::types::Selectable;
use crate::theme::Theme;

/// Per-render quote-bar stripping context: the raw-mode gate plus the bar
/// style the rendered lines were actually painted with.
///
/// #430: this used to derive the style from its own read of the global,
/// separate from the read that painted the spans it compares against. Two
/// reads of one process-global that must agree is a race, and when they
/// disagreed the bar went unrecognised and the `|` gutter silently re-entered
/// copied text. The style now travels with the wrapped lines
/// ([`WrappedLines::bar_style`](super::markdown_content::WrappedLines)), so
/// the comparison source and the paint source are the same stored value.
#[derive(Clone, Copy)]
pub(crate) struct QuoteBarStrip {
    /// `None` = stripping disabled (raw mode shows source `>` markers).
    bar_style: Option<Style>,
}

impl QuoteBarStrip {
    /// Build from the wrap snapshot the lines came from.
    ///
    /// Taking `WrappedLines` rather than a loose `Style` is the point: the
    /// only way to obtain one is from `with_wrapped_lines`, so the style used
    /// for bar detection cannot be anything other than the style those lines
    /// were painted with. A caller cannot reintroduce #430's second read here
    /// without fabricating a snapshot.
    pub(crate) fn for_snapshot(
        enabled: bool,
        wrapped: &super::markdown_content::WrappedLines<'_>,
    ) -> Self {
        Self {
            bar_style: enabled.then_some(wrapped.bar_style),
        }
    }

    /// Selection metadata for one rendered row; see [`quote_prefix_selectable`].
    pub(crate) fn selectable(&self, line: &mut Line<'static>) -> Selectable {
        match self.bar_style {
            Some(bar_style) => quote_prefix_selectable(line, bar_style),
            None => Selectable::All,
        }
    }
}

/// The exact ratatui style the renderer paints parser-generated blockquote
/// bars with, for one theme: `md_style` sets
/// `blockquote_outer = fg(md_muted).dimmed()` and a `Reset` fg is dropped in
/// the anstyle round-trip, leaving DIM alone. Mirrors pager-render
/// theme/md_style.rs `blockquote_outer` (breadcrumbed there); the end-to-end
/// tests below trip if either side drifts.
///
/// Takes the theme rather than reading the global: the caller stores the
/// result beside the lines it painted, so both come from one read (#430).
pub(crate) fn quote_bar_style_for(theme: &Theme) -> Style {
    let muted = theme.md_muted;
    let style = Style::default().add_modifier(Modifier::DIM);
    if muted == Color::Reset {
        style
    } else {
        style.fg(muted)
    }
}

/// Byte length of a rendered blockquote prefix at the start of `line`:
/// `bar_style`-styled bars separated by exactly one space, through the space
/// before content (`│ text` → 4, `│ │ deep` → 8), or the whole line for
/// bar-only rows (blank line inside a quote: `│`, `│ │`, optional trailing
/// space).
///
/// Every prefix bar must carry `bar_style` — a differently-styled bar is
/// quote CONTENT (e.g. source `> │ box art`), which ends the prefix and then
/// trips the interior-bar rule. Returns `None` for anything else: any `│`
/// after the prefix marks table rows, literal box art, or literal-bar quote
/// content (same interior-bar rule `is_table_line` uses in the wrap layer),
/// so the row conservatively stays fully selectable.
///
/// Shape must agree with the looser `blockquote_prefix_len` in pager-render's
/// wrapping.rs (which re-injects this prefix on wrapped continuation rows,
/// preserving the bar spans + style this scanner keys on).
fn rendered_quote_prefix_len(line: &Line<'_>, bar_style: Style) -> Option<usize> {
    const BAR: char = '\u{2502}';
    const BAR_LEN: usize = '\u{2502}'.len_utf8();
    let mut chars = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .peekable();
    let mut len = 0usize;
    loop {
        match chars.next() {
            Some((BAR, style)) if style == bar_style => len += BAR_LEN,
            _ => return None,
        }
        match chars.next() {
            None => return Some(len),
            Some((' ', _)) => {
                len += 1;
                match chars.peek() {
                    None => return Some(len),
                    Some((BAR, style)) if *style == bar_style => continue,
                    Some(_) => break,
                }
            }
            Some(_) => return None,
        }
    }
    if chars.any(|(c, _)| c == BAR) {
        return None;
    }
    Some(len)
}

/// Split `line`'s spans at `byte_offset` (splitting a straddling span in two)
/// and return the number of spans covering `0..byte_offset`.
fn split_spans_at(line: &mut Line<'static>, byte_offset: usize) -> usize {
    let mut acc = 0usize;
    for i in 0..line.spans.len() {
        let end = acc + line.spans[i].content.len();
        if end == byte_offset {
            return i + 1;
        }
        if end > byte_offset {
            let local = byte_offset - acc;
            let span = &mut line.spans[i];
            let tail: Cow<'static, str> = match &mut span.content {
                Cow::Borrowed(s) => {
                    let (head, tail) = s.split_at(local);
                    span.content = Cow::Borrowed(head);
                    Cow::Borrowed(tail)
                }
                Cow::Owned(s) => Cow::Owned(s.split_off(local)),
            };
            let style = span.style;
            line.spans.insert(
                i + 1,
                Span {
                    content: tail,
                    style,
                },
            );
            return i + 1;
        }
        acc = end;
    }
    line.spans.len()
}

/// Selection metadata for a pretty-mode markdown row: when the row is a
/// parser-generated blockquote line, exclude the `│ ` prefix (all nesting
/// levels) from copy by splitting the straddling span at the prefix boundary
/// and returning a `Selectable::Spans` range past it. Bar-only rows (blank
/// quote lines) return an empty end range so multi-line copies keep the
/// blank line. Returns `Selectable::All` for every other row.
///
/// The bar must be the FIRST span: quotes indented under other constructs
/// (e.g. a list item's `- > quoted`, bullet span first) keep their prefix in
/// copies — an accepted conservative false negative, like interior bars.
fn quote_prefix_selectable(line: &mut Line<'static>, bar_style: Style) -> Selectable {
    // Only parser-generated bars are a lone 1-char span carrying the
    // blockquote_outer style; a literal "│ " in prose or code stays glued to
    // its content span or carries a different style, so it is left intact.
    let genuine = line
        .spans
        .first()
        .is_some_and(|s| s.content.as_ref() == "\u{2502}" && s.style == bar_style);
    if !genuine {
        return Selectable::All;
    }
    let Some(prefix_len) = rendered_quote_prefix_len(line, bar_style) else {
        return Selectable::All;
    };
    let prefix_spans = split_spans_at(line, prefix_len);
    Selectable::Spans(prefix_spans..line.spans.len())
}

#[cfg(test)]
mod tests {
    use super::super::markdown_content::{MARKDOWN_BODY_RANGE, MarkdownContent};
    use super::*;
    use crate::scrollback::text_selection::{ActiveTextDrag, RangeHit};
    use crate::scrollback::types::{
        BlockLine, BlockOutput, derive_selection_text, line_plain_text, selectable_cols,
    };

    /// A bar style from a locally built theme. Unit fixtures below construct
    /// their own lines with it and pass the same value in, so the assertion is
    /// self-consistent under any ambient theme — the #166 remedy, applied to a
    /// selectability test rather than a colour one.
    fn test_bar_style() -> Style {
        quote_bar_style_for(&Theme::groknight())
    }

    fn find_line<'a>(out: &'a BlockOutput, needle: &str) -> &'a BlockLine {
        out.lines
            .iter()
            .find(|l| line_plain_text(&l.content).contains(needle))
            .unwrap_or_else(|| panic!("no output line contains {needle:?}"))
    }

    /// Source-scan guard (#430), same idiom as
    /// `colour_asserting_tests_do_not_use_theme_current` in `user.rs`.
    ///
    /// Selection metadata is decided by comparing painted span styles against
    /// the bar style. Deriving that style here from the process-global theme
    /// makes it a second read that has to agree with the read that painted the
    /// spans — and when they disagree the bar goes unrecognised and the `│`
    /// gutter silently re-enters copied text. The style must arrive from the
    /// wrap snapshot instead.
    ///
    /// This guards the *mechanism*, not the symptom, deliberately. The test
    /// that fired for #430 asserts selectability and calls no theme API at all,
    /// so no source scan over test bodies could have classified it as
    /// theme-sensitive without re-implementing the renderer. Extending a list
    /// of "theme-sensitive test names" would not have caught it; forbidding the
    /// second read does.
    #[test]
    fn selection_metadata_never_reads_the_global_theme() {
        // Split so this guard's own needle is not a match for itself.
        let needle = concat!("Theme::", "current()");
        let offenders: Vec<(usize, &str)> = include_str!("quote_bar.rs")
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(needle))
            .map(|(i, line)| (i + 1, line.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "selection metadata must take the bar style from the wrap snapshot \
             (WrappedLines::bar_style), never from a second read of the global \
             theme — that is the #430 defect. Offending lines: {offenders:?}"
        );
    }

    #[test]
    fn rendered_quote_prefix_len_shapes() {
        let bq = test_bar_style();
        let quote = Line::from(vec![Span::styled("│", bq), Span::raw(" text")]);
        assert_eq!(rendered_quote_prefix_len(&quote, bq), Some(4));

        let nested = Line::from(vec![
            Span::styled("│", bq),
            Span::raw(" "),
            Span::styled("│", bq),
            Span::raw(" deep"),
        ]);
        assert_eq!(rendered_quote_prefix_len(&nested, bq), Some(8));

        // Bar-only rows (blank quote line), optional trailing space.
        assert_eq!(
            rendered_quote_prefix_len(&Line::from(Span::styled("│", bq)), bq),
            Some(3)
        );
        let nested_blank = Line::from(vec![
            Span::styled("│", bq),
            Span::raw(" "),
            Span::styled("│", bq),
        ]);
        assert_eq!(rendered_quote_prefix_len(&nested_blank, bq), Some(7));
        assert_eq!(
            rendered_quote_prefix_len(&Line::from(vec![Span::styled("│", bq), Span::raw(" ")]), bq),
            Some(4)
        );

        // Unstyled bars are content, never a prefix.
        assert_eq!(rendered_quote_prefix_len(&Line::raw("│ text"), bq), None);
        assert_eq!(rendered_quote_prefix_len(&Line::raw("│ "), bq), None);

        // Interior bars mark table rows / box art — never a quote prefix,
        // even when the leading border span carries the identical style.
        let table_row = Line::from(vec![Span::styled("│", bq), Span::raw(" a │ b │")]);
        assert_eq!(rendered_quote_prefix_len(&table_row, bq), None);
        let blank_table_row = Line::from(vec![Span::styled("│", bq), Span::raw("   │   │")]);
        assert_eq!(rendered_quote_prefix_len(&blank_table_row, bq), None);
        assert_eq!(rendered_quote_prefix_len(&Line::raw("││"), bq), None);
        assert_eq!(rendered_quote_prefix_len(&Line::raw("── rule"), bq), None);
        assert_eq!(rendered_quote_prefix_len(&Line::raw("plain"), bq), None);
    }

    #[test]
    fn rendered_quote_prefix_len_rejects_content_bars_after_genuine_prefix() {
        let bq = test_bar_style();
        // Source `> │ box art`: genuine bar, then a literal (unstyled) bar as
        // the first content char — must not be consumed as a nesting level.
        let literal_second = Line::from(vec![Span::styled("│", bq), Span::raw(" │ box art")]);
        assert_eq!(rendered_quote_prefix_len(&literal_second, bq), None);

        // Degenerate `> │` (content is just a bar).
        let bar_only_content = Line::from(vec![Span::styled("│", bq), Span::raw(" │")]);
        assert_eq!(rendered_quote_prefix_len(&bar_only_content, bq), None);

        // Nested variant `> > │ deep`: two genuine bars, then a literal one.
        let nested_literal = Line::from(vec![
            Span::styled("│", bq),
            Span::raw(" "),
            Span::styled("│", bq),
            Span::raw(" │ deep"),
        ]);
        assert_eq!(rendered_quote_prefix_len(&nested_literal, bq), None);
    }

    #[test]
    fn quote_prefix_selectable_requires_bar_style() {
        // Same content, wrong style (no DIM): a literal bar span is not a
        // parser-generated quote bar and must stay fully selectable.
        let mut line = Line::from(vec![Span::raw("│"), Span::raw(" text")]);
        assert_eq!(
            quote_prefix_selectable(&mut line, test_bar_style()),
            Selectable::All
        );

        let mut line = Line::from(vec![
            Span::styled("│", test_bar_style()),
            Span::raw(" text"),
        ]);
        assert_eq!(
            quote_prefix_selectable(&mut line, test_bar_style()),
            Selectable::Spans(2..3)
        );
        // The glued " text" span was split so the prefix ends on a boundary.
        assert_eq!(line.spans[1].content.as_ref(), " ");
        assert_eq!(line.spans[2].content.as_ref(), "text");
    }

    #[test]
    fn quote_line_selection_excludes_bar_prefix() {
        let md = MarkdownContent::new("intro\n\n> QUOTE alpha\n\noutro");
        let out = md.output(80);

        let line = find_line(&out, "QUOTE");
        // Pretty mode renders the bar on screen…
        assert!(
            line_plain_text(&line.content).starts_with("│ "),
            "expected rendered bar prefix, got {:?}",
            line_plain_text(&line.content)
        );
        // …but the bar is excluded from selection/copy.
        assert!(
            matches!(line.selectable, Selectable::Spans(_)),
            "quote line should exclude its prefix, got {:?}",
            line.selectable
        );
        assert_eq!(derive_selection_text(line), "QUOTE alpha");
        // Non-quote lines stay fully selectable.
        assert!(matches!(
            find_line(&out, "intro").selectable,
            Selectable::All
        ));
        assert!(matches!(
            find_line(&out, "outro").selectable,
            Selectable::All
        ));
    }

    #[test]
    fn nested_quote_selection_excludes_all_bars() {
        let md = MarkdownContent::new("> outer line\n>\n> > NESTED deep");
        let out = md.output(80);

        let line = find_line(&out, "NESTED");
        assert!(line_plain_text(&line.content).starts_with("│ │ "));
        let text = derive_selection_text(line);
        assert_eq!(text, "NESTED deep");
        assert!(!text.contains('│'));
    }

    #[test]
    fn wrapped_quote_continuations_exclude_reinjected_prefix() {
        let md = MarkdownContent::new("> alpha bravo charlie delta echo foxtrot golf hotel india");
        let out = md.output(16);
        assert!(out.lines.len() > 1, "quote must wrap at width 16");

        for line in &out.lines {
            assert!(
                line_plain_text(&line.content).starts_with('│'),
                "every wrapped row repeats the bar: {:?}",
                line_plain_text(&line.content)
            );
            let text = derive_selection_text(line);
            assert!(!text.contains('│'), "copy text has a bar: {text:?}");
            assert!(
                !text.starts_with(' '),
                "copy text keeps prefix space: {text:?}"
            );
        }

        // Joiner-based reconstruction (the drag-copy join rule) is clean.
        let mut joined = String::new();
        for (i, line) in out.lines.iter().enumerate() {
            if i > 0 {
                joined.push_str(line.joiner.as_deref().unwrap_or("\n"));
            }
            joined.push_str(&derive_selection_text(line));
        }
        assert_eq!(
            joined,
            "alpha bravo charlie delta echo foxtrot golf hotel india"
        );
    }

    #[test]
    fn blank_quote_line_survives_drag_copy_as_blank() {
        let md = MarkdownContent::new("> QUOTE_A first\n>\n> QUOTE_B second");
        let out = md.output(80);
        assert_eq!(out.lines.len(), 3, "quote renders as three rows");

        // The bar-only middle row keeps an (empty) selectable range so it
        // stays in the selection model and contributes its newline.
        let mid = &out.lines[1];
        assert_eq!(line_plain_text(&mid.content), "│");
        assert!(
            matches!(&mid.selectable, Selectable::Spans(r) if r.is_empty()),
            "bar-only row should have an empty Spans range, got {:?}",
            mid.selectable
        );
        assert!(selectable_cols(&mid.content, &mid.selectable).is_some());
        assert_eq!(derive_selection_text(mid), "");

        // Full drag from the first to the last row copies a\n\nb.
        let drag = ActiveTextDrag {
            anchor: RangeHit {
                entry_idx: 0,
                range_id: MARKDOWN_BODY_RANGE,
                block_line_idx: 0,
                col_within_range: 0,
            },
            head: RangeHit {
                entry_idx: 0,
                range_id: MARKDOWN_BODY_RANGE,
                block_line_idx: 2,
                col_within_range: u16::MAX,
            },
            kind: Default::default(),
            anchor_content_width: None,
        };
        let text =
            crate::scrollback::text_selection::reconstruct_full_selection_text(&out.lines, &drag)
                .expect("drag reconstruction");
        assert_eq!(text, "QUOTE_A first\n\nQUOTE_B second");
    }

    #[test]
    fn literal_bar_at_quote_content_start_is_not_stripped() {
        // Quoted box-drawing output: the content's own bar must never be
        // consumed as a nesting level (that would DELETE user bytes from the
        // copy). The row degrades to the conservative interior-bar class.
        let md = MarkdownContent::new("> │ box art");
        let out = md.output(80);
        let line = find_line(&out, "box art");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "│ │ box art");
    }

    #[test]
    fn literal_bar_as_entire_quote_content_is_not_dropped() {
        // Degenerate `> │`: without the style-aware scan this classified as a
        // bar-only blank row and the content bar vanished from copies.
        let md = MarkdownContent::new("> │");
        let out = md.output(80);
        let line = find_line(&out, "│");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "│ │");
    }

    #[test]
    fn list_nested_quote_keeps_prefix() {
        // Bullet span precedes the bar, so the first-span guard skips the row
        // (documented conservative false negative on quote_prefix_selectable).
        let md = MarkdownContent::new("- > quoted text");
        let out = md.output(80);
        let line = find_line(&out, "quoted text");
        assert!(matches!(line.selectable, Selectable::All));
        assert!(derive_selection_text(line).contains("│ quoted text"));
    }

    #[test]
    fn literal_bar_in_paragraph_is_not_stripped() {
        // A plain paragraph starting with a literal bar (file-tree art) is a
        // single glued span without the quote-bar style — left fully selectable.
        let md = MarkdownContent::new("│ literal tree line");
        let out = md.output(80);
        let line = find_line(&out, "literal");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "│ literal tree line");
    }

    #[test]
    fn literal_bar_in_code_block_is_not_stripped() {
        let md = MarkdownContent::new("```\n│ box art\n└── tree\n```");
        let out = md.output(80);
        let line = find_line(&out, "box art");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "│ box art");
    }

    #[test]
    fn table_rows_keep_borders_in_copy() {
        let md = MarkdownContent::new("| a | b |\n|---|---|\n| CELL1 | CELL2 |");
        let out = md.output(40);
        let row = find_line(&out, "CELL1");
        assert!(matches!(row.selectable, Selectable::All));
        let text = derive_selection_text(row);
        assert!(
            text.starts_with('│') && text.ends_with('│'),
            "table row copy keeps its borders: {text:?}"
        );
    }

    #[test]
    fn raw_mode_quote_lines_stay_fully_selectable() {
        let mut md = MarkdownContent::new("> QUOTE alpha");
        md.set_raw_mode(true);
        let out = md.output(80);
        let line = find_line(&out, "QUOTE");
        assert!(matches!(line.selectable, Selectable::All));
        assert_eq!(derive_selection_text(line), "> QUOTE alpha");
    }
}
