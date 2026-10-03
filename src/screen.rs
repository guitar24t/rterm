//! Shadow terminal state for a session.
//!
//! While a client is attached, PTY output is forwarded to it byte-for-byte;
//! nothing here sits in that path. In parallel the same bytes are fed into an
//! in-memory terminal emulator so that, when a client (re)attaches, we can
//! reproduce the screen, the scrollback and the terminal modes the programs in
//! the session expect. `snapshot` produces that replay and `reset_sequence`
//! undoes the session's modes when a client leaves, so the user's own shell is
//! not left with mouse reporting, an alternate screen or kitty key encoding.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::Duration;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape, CursorStyle, NamedColor, Processor, Timeout,
};
use alacritty_terminal::vte::{self, Params, Perform};

/// Attribute flags that are expressed with SGR.
const SGR_FLAGS: Flags = Flags::INVERSE
    .union(Flags::BOLD)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::DIM)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

const DEFAULT_FG: Color = Color::Named(NamedColor::Foreground);
const DEFAULT_BG: Color = Color::Named(NamedColor::Background);

/// Disables the emulator's buffering of synchronized updates (mode 2026).
/// We need the shadow state to be current at all times, not after a timeout.
#[derive(Default)]
struct NoSync;

impl Timeout for NoSync {
    fn set_timeout(&mut self, _: Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

#[derive(Default)]
struct EventState {
    replies: Vec<u8>,
    title: Option<String>,
}

#[derive(Clone, Default)]
struct Events(Rc<RefCell<EventState>>);

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        let mut s = self.0.borrow_mut();
        match event {
            Event::PtyWrite(text) => s.replies.extend_from_slice(text.as_bytes()),
            Event::Title(t) => s.title = Some(t),
            Event::ResetTitle => s.title = None,
            _ => {}
        }
    }
}

struct Size {
    rows: usize,
    cols: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// One kitty keyboard-protocol flag stack (the main and alternate screens
/// each have their own).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct KittyStack {
    entries: Vec<u16>,
    /// Flags modified with `CSI = ... u` while nothing was pushed.
    base: u16,
}

impl KittyStack {
    const MAX_DEPTH: usize = 16;

    fn push(&mut self, flags: u16) {
        if self.entries.len() >= Self::MAX_DEPTH {
            self.entries.remove(0);
        }
        self.entries.push(flags);
    }

    fn pop(&mut self, n: u16) {
        let n = usize::from(n.max(1));
        self.entries.truncate(self.entries.len().saturating_sub(n));
        if self.entries.is_empty() {
            self.base = 0;
        }
    }

    fn set(&mut self, flags: u16, mode: u16) {
        let cur = self.entries.last_mut().unwrap_or(&mut self.base);
        match mode {
            1 => *cur = flags,
            2 => *cur |= flags,
            3 => *cur &= !flags,
            _ => {}
        }
    }

    /// Recreate this stack on a terminal whose stack is empty.
    fn restore(&self, out: &mut String) {
        if self.base != 0 {
            let _ = write!(out, "\x1b[={};1u", self.base);
        }
        for f in &self.entries {
            let _ = write!(out, "\x1b[>{f}u");
        }
    }

    /// Undo what `restore` (plus any later pushes) did.
    fn unwind(&self, out: &mut String) {
        if !self.entries.is_empty() {
            let _ = write!(out, "\x1b[<{}u", self.entries.len());
        }
        if self.base != 0 || !self.entries.is_empty() {
            out.push_str("\x1b[=0;1u");
        }
    }
}

/// Terminal state that the emulator does not expose, tracked by scanning the
/// same output stream.
#[derive(Debug, Default)]
struct Extra {
    rows: u16,
    alt: bool,
    kitty: [KittyStack; 2],
    modify_other_keys: u16,
    /// 1-based inclusive scroll margins, `None` when they span the screen.
    scroll_region: Option<(u16, u16)>,
    /// Alternate scroll mode (1007) if a program set it explicitly; its
    /// default differs between terminals, so we only replay explicit changes.
    alternate_scroll: Option<bool>,
}

impl Extra {
    fn kitty(&mut self) -> &mut KittyStack {
        &mut self.kitty[self.alt as usize]
    }
}

impl Perform for Extra {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let p: Vec<u16> = params.iter().map(|sub| sub[0]).collect();
        let arg = |i: usize, default: u16| p.get(i).copied().filter(|&v| v != 0).unwrap_or(default);
        match (action, intermediates) {
            ('u', [b'>']) => {
                let flags = p.first().copied().unwrap_or(0);
                self.kitty().push(flags);
            }
            ('u', [b'<']) => {
                let n = arg(0, 1);
                self.kitty().pop(n);
            }
            ('u', [b'=']) => {
                let flags = p.first().copied().unwrap_or(0);
                let mode = arg(1, 1);
                self.kitty().set(flags, mode);
            }
            ('m', [b'>']) => match p.first().copied().unwrap_or(0) {
                4 => self.modify_other_keys = p.get(1).copied().unwrap_or(0),
                0 if p.len() <= 1 => self.modify_other_keys = 0,
                _ => {}
            },
            ('n', [b'>']) if p.first() == Some(&4) => self.modify_other_keys = 0,
            ('r', []) => {
                let top = arg(0, 1);
                let bottom = arg(1, self.rows).min(self.rows);
                if top < bottom {
                    self.scroll_region = (top > 1 || bottom < self.rows).then_some((top, bottom));
                }
            }
            ('h' | 'l', [b'?']) => {
                let on = action == 'h';
                for &mode in &p {
                    match mode {
                        1049 => self.alt = on,
                        1007 => self.alternate_scroll = Some(on),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            *self = Extra {
                rows: self.rows,
                ..Default::default()
            };
        }
    }
}

/// The SGR-visible style of a cell.
#[derive(Clone, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

impl Pen {
    fn default_pen() -> Pen {
        Pen {
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            flags: Flags::empty(),
            underline: None,
        }
    }

    fn of(cell: &Cell) -> Pen {
        Pen {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & SGR_FLAGS,
            underline: cell.underline_color(),
        }
    }

    fn sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        let f = self.flags;
        if f.contains(Flags::BOLD) {
            out.push_str(";1");
        }
        if f.contains(Flags::DIM) {
            out.push_str(";2");
        }
        if f.contains(Flags::ITALIC) {
            out.push_str(";3");
        }
        if f.contains(Flags::UNDERLINE) {
            out.push_str(";4");
        } else if f.contains(Flags::DOUBLE_UNDERLINE) {
            out.push_str(";4:2");
        } else if f.contains(Flags::UNDERCURL) {
            out.push_str(";4:3");
        } else if f.contains(Flags::DOTTED_UNDERLINE) {
            out.push_str(";4:4");
        } else if f.contains(Flags::DASHED_UNDERLINE) {
            out.push_str(";4:5");
        }
        if f.contains(Flags::INVERSE) {
            out.push_str(";7");
        }
        if f.contains(Flags::HIDDEN) {
            out.push_str(";8");
        }
        if f.contains(Flags::STRIKEOUT) {
            out.push_str(";9");
        }
        color_sgr(out, self.fg, 30, 38);
        color_sgr(out, self.bg, 40, 48);
        if let Some(c) = self.underline {
            match c {
                Color::Spec(rgb) => {
                    let _ = write!(out, ";58;2;{};{};{}", rgb.r, rgb.g, rgb.b);
                }
                Color::Indexed(i) => {
                    let _ = write!(out, ";58;5;{i}");
                }
                Color::Named(n) if (n as usize) < 16 => {
                    let _ = write!(out, ";58;5;{}", n as usize);
                }
                Color::Named(_) => {}
            }
        }
        out.push('m');
    }
}

fn color_sgr(out: &mut String, color: Color, base: usize, extended: usize) {
    match color {
        Color::Named(n) => {
            let idx = n as usize;
            let idx =
                if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&idx) {
                    idx - NamedColor::DimBlack as usize
                } else {
                    idx
                };
            if idx < 8 {
                let _ = write!(out, ";{}", base + idx);
            } else if idx < 16 {
                let _ = write!(out, ";{}", base + 60 + idx - 8);
            }
            // Foreground/Background/Cursor/etc. are the terminal defaults.
        }
        Color::Indexed(i) => {
            let _ = write!(out, ";{extended};5;{i}");
        }
        Color::Spec(rgb) => {
            let _ = write!(out, ";{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
    }
}

fn is_blank(cell: &Cell) -> bool {
    (cell.c == ' ' || cell.c == '\t')
        && cell.bg == DEFAULT_BG
        && !cell.flags.intersects(
            Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT | Flags::WIDE_CHAR_SPACER,
        )
        && cell.zerowidth().is_none_or(|z| z.is_empty())
        && cell.hyperlink().is_none()
}

/// Characters whose rendered width terminals commonly disagree on; after
/// printing one we re-sync the column explicitly.
fn width_is_ambiguous(cell: &Cell) -> bool {
    let c = cell.c as u32;
    cell.zerowidth().is_some_and(|z| !z.is_empty()) || (0x2600..0x2800).contains(&c) || c >= 0x1F000
}

/// Emits rows of cells as text + SGR, tracking the outer terminal's pen.
struct Painter {
    out: String,
    pen: Pen,
    link: Option<Hyperlink>,
}

impl Painter {
    fn new() -> Painter {
        Painter {
            out: String::new(),
            pen: Pen::default_pen(),
            link: None,
        }
    }

    fn set_pen(&mut self, pen: Pen) {
        if pen != self.pen {
            pen.sgr(&mut self.out);
            self.pen = pen;
        }
    }

    fn set_link(&mut self, link: Option<Hyperlink>) {
        if link != self.link {
            match &link {
                Some(l) => {
                    let _ = write!(self.out, "\x1b]8;id={};{}\x1b\\", l.id(), l.uri());
                }
                None => self.out.push_str("\x1b]8;;\x1b\\"),
            }
            self.link = link;
        }
    }

    fn reset_style(&mut self) {
        self.set_link(None);
        self.set_pen(Pen::default_pen());
    }

    fn put_cell(&mut self, cell: &Cell) {
        self.set_pen(Pen::of(cell));
        self.set_link(cell.hyperlink());
        let c = if cell.c == '\t' || (cell.c as u32) < 0x20 || cell.c == '\x7f' {
            ' '
        } else {
            cell.c
        };
        self.out.push(c);
        if let Some(z) = cell.zerowidth() {
            self.out.extend(z.iter());
        }
    }

    /// Print one grid row starting at the cursor's current column.
    /// `full`: print every column even if trailing cells are blank (needed so
    /// that a soft-wrapped row actually wraps in the outer terminal).
    /// `min_cells`: print at least this many cells.
    fn row(&mut self, row: &Row<Cell>, cols: usize, full: bool, min_cells: usize) {
        let end = if full {
            cols
        } else {
            let mut end = cols;
            while end > 0 && is_blank(&row[Column(end - 1)]) {
                end -= 1;
            }
            end.max(min_cells.min(cols))
        };
        let mut col = 0;
        while col < end {
            let cell = &row[Column(col)];
            if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                // A wide char that did not fit; the outer terminal will wrap
                // it to the next line by itself.
                col += 1;
                continue;
            }
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                // Orphaned spacer (its wide char was overwritten).
                let mut blank = cell.clone();
                blank.c = ' ';
                blank.flags.remove(Flags::WIDE_CHAR_SPACER);
                self.put_cell(&blank);
                col += 1;
                continue;
            }
            self.put_cell(cell);
            col += if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            if col < end && width_is_ambiguous(cell) {
                let _ = write!(self.out, "\x1b[{}G", col + 1);
            }
        }
    }

    fn goto(&mut self, line: usize, col: usize) {
        let _ = write!(self.out, "\x1b[{};{}H", line + 1, col + 1);
    }

    /// Paint the primary screen plus its scrollback by printing every line in
    /// order, so the history lands in the outer terminal's native scrollback.
    fn primary(&mut self, grid: &Grid<Cell>, max_history: usize) {
        let rows = grid.screen_lines() as i32;
        let cols = grid.columns();
        let history = grid.history_size().min(max_history) as i32;
        let mut prev_wrapped = false;
        for i in -history..rows {
            let row = &grid[Line(i)];
            let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            self.row(row, cols, wrapped, usize::from(prev_wrapped));
            if !wrapped && i != rows - 1 {
                if self.pen.bg != DEFAULT_BG {
                    // Avoid background-color-erase painting the new line.
                    self.set_pen(Pen::default_pen());
                }
                self.out.push_str("\r\n");
            }
            prev_wrapped = wrapped;
        }
    }

    /// Paint the alternate screen (no history) row by row.
    fn alternate(&mut self, grid: &Grid<Cell>) {
        let cols = grid.columns();
        for i in 0..grid.screen_lines() {
            let row = &grid[Line(i as i32)];
            if (0..cols).all(|c| is_blank(&row[Column(c)])) {
                continue;
            }
            self.goto(i, 0);
            self.row(row, cols, false, 0);
        }
    }

    /// Put the cursor where the grid has it, including a pending auto-wrap.
    fn cursor(&mut self, grid: &Grid<Cell>, line_offset: usize) {
        let c = &grid.cursor;
        let line = (c.point.line.0.max(0) as usize).saturating_sub(line_offset);
        let col = c.point.column.0;
        if c.input_needs_wrap {
            let row = &grid[c.point.line];
            let mut start = col;
            if start > 0 && row[Column(start)].flags.contains(Flags::WIDE_CHAR_SPACER) {
                start -= 1;
            }
            self.goto(line, start);
            self.put_cell(&row[Column(start)]);
        } else {
            self.goto(line, col);
        }
    }
}

fn last_content_line(grid: &Grid<Cell>) -> Option<usize> {
    let cols = grid.columns();
    (0..grid.screen_lines())
        .rev()
        .find(|&i| (0..cols).any(|c| !is_blank(&grid[Line(i as i32)][Column(c)])))
}

fn mode_seq(out: &mut String, on: bool, seq_on: &str) {
    if on {
        out.push_str(seq_on);
    }
}

pub struct Screen {
    term: Term<Events>,
    parser: Processor<NoSync>,
    scanner: vte::Parser,
    extra: Extra,
    events: Events,
}

impl Screen {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Screen {
        let (rows, cols) = clamp_size(rows, cols);
        let events = Events::default();
        let config = Config {
            scrolling_history: scrollback,
            kitty_keyboard: false,
            ..Default::default()
        };
        let term = Term::new(
            config,
            &Size {
                rows: rows.into(),
                cols: cols.into(),
            },
            events.clone(),
        );
        Screen {
            term,
            parser: Processor::new(),
            scanner: vte::Parser::new(),
            extra: Extra {
                rows,
                ..Default::default()
            },
            events,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.scanner.advance(&mut self.extra, bytes);
        self.parser.advance(&mut self.term, bytes);
    }

    /// Answers the emulator generated for terminal queries (cursor position,
    /// device attributes). Only useful while no real terminal is attached.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.events.0.borrow_mut().replies)
    }

    /// Kitty keyboard flags in effect on the active screen.
    #[cfg(unix)]
    pub fn kitty_flags(&self) -> u16 {
        let k = &self.extra.kitty[self.extra.alt as usize];
        k.entries.last().copied().unwrap_or(k.base)
    }

    pub fn title(&self) -> Option<String> {
        self.events.0.borrow().title.clone()
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.screen_lines() as u16, self.term.columns() as u16)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = clamp_size(rows, cols);
        if (rows, cols) == self.size() {
            return;
        }
        self.term.resize(Size {
            rows: rows.into(),
            cols: cols.into(),
        });
        self.extra.rows = rows;
        self.extra.scroll_region = None;
    }

    /// Run `f` against the primary grid, even while the alternate screen is
    /// active (the emulator only exposes the active grid).
    fn with_primary<R>(&mut self, f: impl FnOnce(&Grid<Cell>) -> R) -> R {
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            return f(self.term.grid());
        }
        let alt = self.term.grid().clone();
        self.term.swap_alt();
        let r = f(self.term.grid());
        // Swapping back clears the alternate grid; put the saved copy back.
        self.term.swap_alt();
        *self.term.grid_mut() = alt;
        r
    }

    /// Bytes that make a terminal of the same size, sitting at a shell
    /// prompt, look and behave like this session. Up to `max_history` lines
    /// of scrollback are replayed into the terminal's native scrollback.
    pub fn snapshot(&mut self, max_history: usize) -> Vec<u8> {
        let mode = *self.term.mode();
        let alt = mode.contains(TermMode::ALT_SCREEN);
        let rows = self.term.screen_lines();
        let mut p = Painter::new();

        p.out.push_str("\x1b[?2026h\x1b[?25l\x1b[0m");
        // Push whatever is on the outer screen into its scrollback, then
        // start from a clean screen.
        p.out.push_str(&"\r\n".repeat(rows));
        p.out.push_str("\x1b[H\x1b[J");

        let main_kitty = self.extra.kitty[0].clone();
        let primary_cursor = self.with_primary(|grid| {
            p.primary(grid, max_history);
            p.reset_style();
            grid.cursor.clone()
        });
        main_kitty.restore(&mut p.out);

        let grid_line_offset = |extra: &Extra| -> usize {
            if mode.contains(TermMode::ORIGIN) {
                extra
                    .scroll_region
                    .map_or(0, |(top, _)| usize::from(top - 1))
            } else {
                0
            }
        };

        if alt {
            // Park the cursor where the primary screen has it so the outer
            // terminal saves that position on entering the alternate screen.
            let point = primary_cursor.point;
            p.goto(point.line.0.max(0) as usize, point.column.0);
            p.out.push_str("\x1b[?1049h\x1b[H\x1b[2J");
            p.alternate(self.term.grid());
            p.reset_style();
            self.extra.kitty[1].restore(&mut p.out);
        }

        if let Some((top, bottom)) = self.extra.scroll_region {
            let _ = write!(p.out, "\x1b[{top};{bottom}r");
        }
        if mode.contains(TermMode::ORIGIN) {
            p.out.push_str("\x1b[?6h");
        }
        let grid = self.term.grid();
        p.cursor(grid, grid_line_offset(&self.extra));
        p.set_pen(Pen::of(&grid.cursor.template));
        p.set_link(grid.cursor.template.hyperlink());

        let o = &mut p.out;
        mode_seq(o, !mode.contains(TermMode::LINE_WRAP), "\x1b[?7l");
        mode_seq(o, mode.contains(TermMode::INSERT), "\x1b[4h");
        mode_seq(o, mode.contains(TermMode::LINE_FEED_NEW_LINE), "\x1b[20h");
        mode_seq(o, mode.contains(TermMode::APP_CURSOR), "\x1b[?1h");
        mode_seq(o, mode.contains(TermMode::APP_KEYPAD), "\x1b=");
        mode_seq(
            o,
            mode.contains(TermMode::MOUSE_REPORT_CLICK),
            "\x1b[?1000h",
        );
        mode_seq(o, mode.contains(TermMode::MOUSE_DRAG), "\x1b[?1002h");
        mode_seq(o, mode.contains(TermMode::MOUSE_MOTION), "\x1b[?1003h");
        mode_seq(o, mode.contains(TermMode::SGR_MOUSE), "\x1b[?1006h");
        mode_seq(o, mode.contains(TermMode::UTF8_MOUSE), "\x1b[?1005h");
        mode_seq(o, mode.contains(TermMode::FOCUS_IN_OUT), "\x1b[?1004h");
        mode_seq(o, mode.contains(TermMode::BRACKETED_PASTE), "\x1b[?2004h");
        if let Some(on) = self.extra.alternate_scroll {
            o.push_str(if on { "\x1b[?1007h" } else { "\x1b[?1007l" });
        }
        if self.extra.modify_other_keys != 0 {
            let _ = write!(o, "\x1b[>4;{}m", self.extra.modify_other_keys);
        }
        let style = self.term.cursor_style();
        if style != CursorStyle::default() {
            let _ = write!(o, "\x1b[{} q", decscusr(style));
        }
        if let Some(title) = self.title() {
            let title: String = title.chars().filter(|c| !c.is_control()).collect();
            let _ = write!(o, "\x1b]2;{title}\x07");
        }
        mode_seq(o, mode.contains(TermMode::SHOW_CURSOR), "\x1b[?25h");
        o.push_str("\x1b[?2026l");
        p.out.into_bytes()
    }

    /// Bytes that return a terminal showing this session to a sane state for
    /// the user's own shell, leaving the cursor on a fresh line below the
    /// session's content.
    pub fn reset_sequence(&mut self) -> Vec<u8> {
        let mode = *self.term.mode();
        let mut o = String::from("\x1b[?2026l\x1b[0m");
        if self.term.grid().cursor.template.hyperlink().is_some() {
            o.push_str("\x1b]8;;\x1b\\");
        }
        if self.extra.scroll_region.is_some() {
            o.push_str("\x1b7\x1b[r\x1b8");
        }
        mode_seq(&mut o, mode.contains(TermMode::ORIGIN), "\x1b[?6l");
        if mode.contains(TermMode::ALT_SCREEN) {
            self.extra.kitty[1].unwind(&mut o);
            o.push_str("\x1b[?1049l");
        }
        self.extra.kitty[0].unwind(&mut o);
        if self.extra.modify_other_keys != 0 {
            o.push_str("\x1b[>4m");
        }
        if self.extra.alternate_scroll == Some(false) {
            // Most terminals enable it by default.
            o.push_str("\x1b[?1007h");
        }
        mode_seq(&mut o, mode.contains(TermMode::INSERT), "\x1b[4l");
        mode_seq(
            &mut o,
            mode.contains(TermMode::LINE_FEED_NEW_LINE),
            "\x1b[20l",
        );
        mode_seq(&mut o, mode.contains(TermMode::APP_CURSOR), "\x1b[?1l");
        mode_seq(&mut o, mode.contains(TermMode::APP_KEYPAD), "\x1b>");
        mode_seq(
            &mut o,
            mode.intersects(TermMode::MOUSE_MODE),
            "\x1b[?1000l\x1b[?1002l\x1b[?1003l",
        );
        mode_seq(&mut o, mode.contains(TermMode::SGR_MOUSE), "\x1b[?1006l");
        mode_seq(&mut o, mode.contains(TermMode::UTF8_MOUSE), "\x1b[?1005l");
        mode_seq(&mut o, mode.contains(TermMode::FOCUS_IN_OUT), "\x1b[?1004l");
        mode_seq(
            &mut o,
            mode.contains(TermMode::BRACKETED_PASTE),
            "\x1b[?2004l",
        );
        mode_seq(&mut o, !mode.contains(TermMode::LINE_WRAP), "\x1b[?7h");
        if self.term.cursor_style() != CursorStyle::default() {
            o.push_str("\x1b[0 q");
        }
        o.push_str("\x1b[?25h");

        // Move below everything on the (primary) screen.
        let (cursor_line, last) =
            self.with_primary(|g| (g.cursor.point.line.0.max(0) as usize, last_content_line(g)));
        match last {
            Some(last) if last >= cursor_line => {
                let _ = write!(o, "\x1b[{};1H\r\n", last + 1);
            }
            _ => {
                let _ = write!(o, "\x1b[{};1H", cursor_line + 1);
            }
        }
        o.into_bytes()
    }
}

fn decscusr(style: CursorStyle) -> u8 {
    let base = match style.shape {
        CursorShape::Block | CursorShape::HollowBlock | CursorShape::Hidden => 1,
        CursorShape::Underline => 3,
        CursorShape::Beam => 5,
    };
    if style.blinking { base } else { base + 1 }
}

pub fn clamp_size(rows: u16, cols: u16) -> (u16, u16) {
    (rows.clamp(2, 1000), cols.clamp(4, 2000))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: u16 = 12;
    const COLS: u16 = 40;

    fn cells_match(a: &Cell, b: &Cell) -> bool {
        let norm = |c: char| if c == '\t' { ' ' } else { c };
        let mask = SGR_FLAGS | Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER;
        norm(a.c) == norm(b.c)
            && a.fg == b.fg
            && a.bg == b.bg
            && (a.flags & mask) == (b.flags & mask)
            && a.zerowidth().unwrap_or(&[]) == b.zerowidth().unwrap_or(&[])
            && a.underline_color() == b.underline_color()
            && a.hyperlink().map(|h| h.uri().to_owned())
                == b.hyperlink().map(|h| h.uri().to_owned())
    }

    fn row_text(grid: &Grid<Cell>, line: i32) -> String {
        let row = &grid[Line(line)];
        (0..grid.columns())
            .map(|c| row[Column(c)].c)
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn assert_grid_eq(a: &Grid<Cell>, b: &Grid<Cell>, what: &str) {
        for line in 0..a.screen_lines() as i32 {
            for col in 0..a.columns() {
                let (ca, cb) = (&a[Line(line)][Column(col)], &b[Line(line)][Column(col)]);
                assert!(
                    cells_match(ca, cb),
                    "{what}: cell {line},{col} differs:\n  want {ca:?}\n  got  {cb:?}\n  want row {:?}\n  got row  {:?}",
                    row_text(a, line),
                    row_text(b, line)
                );
            }
            let wa = a[Line(line)][Column(a.columns() - 1)]
                .flags
                .contains(Flags::WRAPLINE);
            let wb = b[Line(line)][Column(b.columns() - 1)]
                .flags
                .contains(Flags::WRAPLINE);
            assert_eq!(wa, wb, "{what}: wrap flag on line {line}");
        }
    }

    fn assert_history_eq(a: &Grid<Cell>, b: &Grid<Cell>) {
        let ha = a.history_size() as i32;
        let hb = b.history_size() as i32;
        assert!(
            hb >= ha,
            "restored history shorter ({hb}) than original ({ha})"
        );
        for i in 1..=ha {
            assert_eq!(row_text(a, -i), row_text(b, -i), "history line -{i}");
        }
    }

    /// Feed `input` to a session, replay its snapshot into a terminal that
    /// already has some content, and check that both now look the same.
    fn roundtrip(input: &[u8]) -> (Screen, Screen) {
        let mut session = Screen::new(ROWS, COLS, 1000);
        session.feed(input);
        let snap = session.snapshot(usize::MAX);

        let mut outer = Screen::new(ROWS, COLS, 2000);
        outer.feed(b"user@host:~$ ls\r\nfoo bar\r\nuser@host:~$ rterm attach\r\n");
        outer.feed(&snap);

        let ms = *session.term.mode() & !TermMode::ALTERNATE_SCROLL;
        let mo = *outer.term.mode() & !TermMode::ALTERNATE_SCROLL;
        assert_eq!(ms, mo, "modes differ");
        assert_eq!(
            session.extra.kitty, outer.extra.kitty,
            "kitty stacks differ"
        );
        assert_eq!(
            session.extra.scroll_region, outer.extra.scroll_region,
            "scroll region"
        );
        assert_eq!(
            session.extra.modify_other_keys,
            outer.extra.modify_other_keys
        );
        assert_eq!(session.extra.alternate_scroll, outer.extra.alternate_scroll);
        assert_eq!(session.title(), outer.title());
        assert_eq!(session.term.cursor_style(), outer.term.cursor_style());

        let (ga, gb) = (session.term.grid(), outer.term.grid());
        assert_grid_eq(ga, gb, "active screen");
        assert_eq!(ga.cursor.point, gb.cursor.point, "cursor position");
        assert_eq!(
            ga.cursor.input_needs_wrap, gb.cursor.input_needs_wrap,
            "pending wrap"
        );
        assert!(
            cells_match(&ga.cursor.template, &gb.cursor.template),
            "pen differs"
        );

        let pa = session.with_primary(|g| g.clone());
        let pb = outer.with_primary(|g| g.clone());
        assert_grid_eq(&pa, &pb, "primary screen");
        assert_history_eq(&pa, &pb);
        if session.term.mode().contains(TermMode::ALT_SCREEN) {
            assert_eq!(pa.cursor.point, pb.cursor.point, "primary cursor");
        }
        (session, outer)
    }

    #[test]
    fn plain_shell_output() {
        roundtrip(b"$ echo hi\r\nhi\r\n$ ");
    }

    #[test]
    fn colors_and_attributes() {
        roundtrip(
            b"\x1b[1;31mbold red\x1b[0m \x1b[4:3;58;2;1;2;3mcurly\x1b[0m \x1b[38;5;200;48;2;10;20;30mrgb\x1b[0m\r\n\
              \x1b[7minverse\x1b[27m \x1b[2;3;9mdim it strike\x1b[0m \x1b[92;104mbright\x1b[0m\r\n\
              \x1b[44m colored line with erase \x1b[K\x1b[0m\r\n\x1b[1;32mpen left on",
        );
    }

    #[test]
    fn scrollback_and_wrapping() {
        let mut input = Vec::new();
        for i in 0..100 {
            input.extend_from_slice(format!("line {i} ").as_bytes());
            if i % 7 == 0 {
                input.extend_from_slice("x".repeat(95).as_bytes());
            }
            input.extend_from_slice(b"\r\n");
        }
        input.extend_from_slice(b"$ ");
        roundtrip(&input);
    }

    #[test]
    fn exact_width_lines_and_pending_wrap() {
        let mut input = Vec::new();
        input.extend_from_slice("a".repeat(COLS as usize).as_bytes());
        input.extend_from_slice(b"\r\n");
        input.extend_from_slice("b".repeat(COLS as usize * 2).as_bytes());
        input.extend_from_slice(b"\r\n\r\n");
        input.extend_from_slice("c".repeat(COLS as usize).as_bytes());
        roundtrip(&input);
    }

    #[test]
    fn wrapped_line_followed_by_blank_continuation() {
        let mut input = Vec::new();
        input.extend_from_slice("w".repeat(COLS as usize).as_bytes());
        input.extend_from_slice(b" \x1b[D\x1b[K\r\nnext\r\n");
        roundtrip(&input);
    }

    #[test]
    fn wide_and_combining_chars() {
        let mut input = "漢字テスト e\u{301} ok 😀 x\r\n".as_bytes().to_vec();
        // Force a wide char to wrap from the last column.
        input.extend_from_slice("y".repeat(COLS as usize - 1).as_bytes());
        input.extend_from_slice("界 tail\r\n".as_bytes());
        roundtrip(&input);
    }

    #[test]
    fn scroll_region_and_cursor() {
        roundtrip(b"top\r\n\x1b[3;8r\x1b[5;3Hinside\x1b[2;3H");
    }

    #[test]
    fn origin_mode() {
        roundtrip(b"\x1b[4;9r\x1b[?6h\x1b[2;2Hrel");
    }

    #[test]
    fn modes_and_title() {
        roundtrip(
            b"\x1b]2;my title\x07\x1b[?2004h\x1b[?1h\x1b=\x1b[?1002h\x1b[?1006h\x1b[?1004h\
              \x1b[>1u\x1b[>5u\x1b[>4;2m\x1b[5 q\x1b[?1007l\x1b[?25lhidden cursor",
        );
    }

    #[test]
    fn alternate_screen() {
        let mut input = Vec::new();
        for i in 0..30 {
            input.extend_from_slice(format!("history {i}\r\n").as_bytes());
        }
        input.extend_from_slice(b"$ vim\r\n");
        input.extend_from_slice(b"\x1b[>3u\x1b[?1049h\x1b[>1u\x1b[H\x1b[2J\x1b[1;1H~\x1b[2;1H~\x1b[12;1H\x1b[7m-- INSERT --\x1b[0m\x1b[4;5H");
        let (mut session, mut outer) = roundtrip(&input);

        // Leaving the alternate screen must reveal the right primary screen.
        session.feed(b"\x1b[<1u\x1b[?1049l\x1b[<1u");
        outer.feed(b"\x1b[<1u\x1b[?1049l\x1b[<1u");
        assert_grid_eq(session.term.grid(), outer.term.grid(), "primary after exit");
        assert_eq!(
            session.term.grid().cursor.point,
            outer.term.grid().cursor.point
        );
        assert_eq!(session.extra.kitty, outer.extra.kitty);
    }

    #[test]
    fn hyperlinks() {
        roundtrip(b"see \x1b]8;;https://example.com\x1b\\the link\x1b]8;;\x1b\\ ok\r\n\x1b]8;id=x;file:///tmp\x1b\\open");
    }

    #[test]
    fn snapshot_preserves_alternate_screen_state() {
        let mut s = Screen::new(ROWS, COLS, 100);
        s.feed(b"before\r\n\x1b[?1049hALT CONTENT");
        let _ = s.snapshot(usize::MAX);
        let _ = s.reset_sequence();
        assert!(s.term.mode().contains(TermMode::ALT_SCREEN));
        // Entering the alternate screen keeps the cursor on line 1.
        assert_eq!(row_text(s.term.grid(), 1), "ALT CONTENT");
        s.feed(b"\x1b[?1049l");
        assert_eq!(row_text(s.term.grid(), 0), "before");
    }

    #[test]
    fn reset_restores_sane_terminal() {
        let mut session = Screen::new(ROWS, COLS, 100);
        session.feed(b"$ app\r\n\x1b[>1u\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1049h\x1b[>7u\x1b[?1h\x1b[3;5r\x1b[?25l");
        let snap = session.snapshot(usize::MAX);
        let reset = session.reset_sequence();

        let mut outer = Screen::new(ROWS, COLS, 100);
        outer.feed(&snap);
        outer.feed(&reset);
        let fresh = Screen::new(ROWS, COLS, 100);
        assert_eq!(
            *outer.term.mode() & !TermMode::ALTERNATE_SCROLL,
            *fresh.term.mode() & !TermMode::ALTERNATE_SCROLL
        );
        assert_eq!(outer.extra.kitty, fresh.extra.kitty);
        assert_eq!(outer.extra.scroll_region, None);
        // Cursor on the line after "$ app".
        assert_eq!(outer.term.grid().cursor.point.line, Line(1));
        assert_eq!(outer.term.grid().cursor.point.column, Column(0));
    }

    #[test]
    fn reset_moves_below_content() {
        let mut s = Screen::new(ROWS, COLS, 100);
        s.feed(b"one\r\ntwo\r\nthree\x1b[1;1H");
        let mut outer = Screen::new(ROWS, COLS, 100);
        outer.feed(&s.snapshot(usize::MAX));
        outer.feed(&s.reset_sequence());
        assert_eq!(outer.term.grid().cursor.point.line, Line(3));
    }

    #[test]
    fn replies_to_queries() {
        let mut s = Screen::new(ROWS, COLS, 100);
        s.feed(b"ab\x1b[6n\x1b[c");
        assert_eq!(s.take_replies(), b"\x1b[1;3R\x1b[?6c");
        assert!(s.take_replies().is_empty());
    }

    #[test]
    fn resize_reflows() {
        let mut s = Screen::new(ROWS, COLS, 100);
        s.feed("z".repeat(60).as_bytes());
        s.resize(ROWS, 80);
        assert_eq!(row_text(s.term.grid(), 0), "z".repeat(60));
        roundtrip(b"");
    }
}
