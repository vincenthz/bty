//! Grid to bytes: VT that redraws it on another terminal, and plain text.
//!
//! rio-vt's own formatter covers the visible screen only. A repaint here
//! also needs scrollback, soft wraps (so the viewer reflows like we do),
//! hyperlinks and combining marks, so it walks the grid itself and reuses
//! rio's SGR writer for styles.

use rio_vt::crosswords::grid::{Dimensions, Grid};
use rio_vt::crosswords::pos::{CharsetIndex, Column, Line, StandardCharset};
use rio_vt::crosswords::square::{Square, Wide};
use rio_vt::crosswords::style::Style;
use std::fmt::Write as _;

type Row = rio_vt::crosswords::grid::row::Row<Square>;

fn wraps(row: &Row, cols: usize) -> bool {
    cols > 0 && row[Column(cols - 1)].wrapline()
}

fn link(grid: &Grid<Square>, sq: Square) -> Option<(&str, &str)> {
    let extras = grid.extras_table.get(sq.extras_id_checked()?)?;
    extras.hyperlink.as_ref().map(|h| (h.id(), h.uri()))
}

fn zerowidth(grid: &Grid<Square>, sq: Square) -> &[char] {
    sq.extras_id_checked().and_then(|id| grid.extras_table.get(id)).map_or(&[], |e| &e.zerowidth)
}

/// Whether a cell shows anything: a glyph, a background, or a link.
fn content(grid: &Grid<Square>, sq: Square) -> bool {
    let c = sq.c();
    (c != ' ' && c != '\0') || grid.style_of(&sq).renders_on_blank() || sq.has_hyperlink()
}

/// Columns of a row worth drawing: all of a wrapped one, else up to its last content.
fn width(grid: &Grid<Square>, row: &Row, cols: usize) -> usize {
    if wraps(row, cols) {
        return cols;
    }
    (0..cols).rev().find(|&x| content(grid, row[Column(x)])).map_or(0, |x| x + 1)
}

fn blank(grid: &Grid<Square>, line: i32) -> bool {
    width(grid, &grid[Line(line)], grid.columns()) == 0
}

/// Writes lines `from..to` one after another: soft-wrapped ones run on so
/// the receiver wraps them too, others end with CRLF. Ends with pen and
/// link reset. `trim` drops trailing blank lines. Returns the rows written.
pub fn lines(out: &mut String, grid: &Grid<Square>, from: i32, mut to: i32, trim: bool) -> usize {
    while trim && to > from && blank(grid, to - 1) {
        to -= 1;
    }
    let cols = grid.columns();
    let mut style = Style::default();
    let mut open: Option<(&str, &str)> = None;
    for line in from..to {
        let row = &grid[Line(line)];
        let end = width(grid, row, cols);
        for x in 0..end {
            let sq = row[Column(x)];
            if matches!(sq.wide(), Wide::Spacer | Wide::LeadingSpacer) {
                continue;
            }
            let s = grid.style_of(&sq);
            if s != style {
                rio_vt::crosswords::formatter::write_sgr(out, &s);
                style = s;
            }
            let l = link(grid, sq);
            if l != open {
                match l {
                    Some((id, uri)) => {
                        let _ = write!(out, "\x1b]8;id={id};{uri}\x1b\\");
                    }
                    None => out.push_str("\x1b]8;;\x1b\\"),
                }
                open = l;
            }
            let c = sq.c();
            out.push(if c == '\0' { ' ' } else { c });
            out.extend(zerowidth(grid, sq));
        }
        if line + 1 < to && !wraps(row, cols) {
            out.push_str("\r\n");
        }
    }
    if open.is_some() {
        out.push_str("\x1b]8;;\x1b\\");
    }
    out.push_str("\x1b[0m");
    (to - from).max(0) as usize
}

/// Scrollback lines to send: the last `history` of them.
pub fn history_from(grid: &Grid<Square>, history: usize) -> i32 {
    -(grid.history_size().min(history) as i32)
}

/// Whether nothing at all is on the visible screen.
pub fn blank_screen(grid: &Grid<Square>) -> bool {
    use rio_vt::crosswords::grid::GridSquare;
    let cols = grid.columns();
    (0..grid.screen_lines() as i32).all(|l| (0..cols).all(|x| grid[Line(l)][Column(x)].is_empty()))
}

/// A screen on a terminal just cleared: up to `history` lines pushed into
/// the receiver's scrollback, then the visible rows.
pub fn screen(out: &mut String, grid: &Grid<Square>, history: usize) {
    let rows = grid.screen_lines();
    let from = history_from(grid, history);
    out.push_str("\x1b[H");
    if from < 0 {
        let n = lines(out, grid, from, 0, false);
        // Scroll exactly those rows off the top, leaving the screen blank.
        let _ = write!(out, "\x1b[{rows};1H{}\x1b[H\x1b[J", "\n".repeat(n.min(rows)));
    }
    lines(out, grid, 0, rows as i32, true);
}

/// Cursor, then what DECSC saves with it: charsets and pen. `top` is the
/// first row of the scroll region when origin mode is on.
pub fn cursor(out: &mut String, grid: &Grid<Square>, top: i32) {
    let c = &grid.cursor;
    let (row, col) = (c.pos.row.0, c.pos.col.0);
    let _ = write!(out, "\x1b[{};{}H", row - top + 1, col + 1);
    // A cursor waiting to wrap sits past its cell: drawing that cell again puts it there.
    let sq = grid[c.pos];
    if c.should_wrap && !matches!(sq.wide(), Wide::Spacer | Wide::LeadingSpacer) {
        rio_vt::crosswords::formatter::write_sgr(out, &grid.style_of(&sq));
        let ch = sq.c();
        out.push(if ch == '\0' { ' ' } else { ch });
        out.extend(zerowidth(grid, sq));
    }
    for (i, intro) in
        [(CharsetIndex::G0, '('), (CharsetIndex::G1, ')'), (CharsetIndex::G2, '*'), (CharsetIndex::G3, '+')]
    {
        if c.charsets[i] == StandardCharset::SpecialCharacterAndLineDrawing {
            let _ = write!(out, "\x1b{intro}0");
        }
    }
    rio_vt::crosswords::formatter::write_sgr(out, &c.pending_style);
}

/// Plain text of lines `from..to`, soft wraps joined, trailing blanks dropped.
pub fn plain(grid: &Grid<Square>, from: i32, to: i32) -> String {
    let cols = grid.columns();
    let mut out: Vec<String> = vec![String::new()];
    for line in from..to {
        let row = &grid[Line(line)];
        let text = out.last_mut().expect("never empty");
        for x in 0..cols {
            let sq = row[Column(x)];
            if sq.wide() == Wide::Spacer {
                continue;
            }
            let c = sq.c();
            text.push(if c == '\0' { ' ' } else { c });
            text.extend(zerowidth(grid, sq));
        }
        if !wraps(row, cols) {
            text.truncate(text.trim_end().len());
            out.push(String::new());
        }
    }
    while out.last().is_some_and(|l| l.trim_end().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// One visible row as text, padded to the screen width.
pub fn row(grid: &Grid<Square>, line: i32) -> String {
    let cols = grid.columns();
    let row = &grid[Line(line)];
    let mut text = String::new();
    for x in 0..cols {
        let sq = row[Column(x)];
        if sq.wide() != Wide::Spacer {
            let c = sq.c();
            text.push(if c == '\0' { ' ' } else { c });
            text.extend(zerowidth(grid, sq));
        }
    }
    let text = text.trim_end();
    format!("{text:<cols$}")
}

/// `file://` URL for OSC 7, percent-encoding what isn't plainly a path.
pub fn file_url(path: &std::path::Path) -> String {
    let mut url = String::from("file://");
    for &b in path.as_os_str().as_encoded_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            url.push(b as char);
        } else {
            let _ = write!(url, "%{b:02X}");
        }
    }
    url
}
