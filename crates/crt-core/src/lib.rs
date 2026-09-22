//! CRT Core - Terminal emulation and PTY management
//!
//! This crate provides:
//! - Terminal grid state (via alacritty_terminal)
//! - ANSI escape sequence parsing (via vte)
//! - PTY process management (via portable-pty)

pub mod pty;

pub use pty::{Pty, PtyBackend, ShellType, SpawnOptions, WakeFn};

// Re-export alacritty_terminal types needed for rendering
pub use alacritty_terminal::event::Event as TerminalEvent;
pub use alacritty_terminal::grid::Scroll;
pub use alacritty_terminal::index::Side;
pub use alacritty_terminal::index::{Column, Line, Point};
pub use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
pub use alacritty_terminal::term::TermMode;
pub use alacritty_terminal::term::{
    LineDamageBounds, RenderableContent, RenderableCursor, TermDamage,
    cell::Cell,
    cell::Flags as CellFlags,
    color::{self, Colors},
};
pub use alacritty_terminal::vte::ansi::Color as AnsiColor;
pub use alacritty_terminal::vte::ansi::CursorShape;
pub use alacritty_terminal::vte::ansi::NamedColor;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use crossbeam_queue::SegQueue;

/// What part of the visible grid changed since the last `take_damage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextInvalidation {
    /// Nothing changed
    None,
    /// Only these viewport lines changed (0 = top of the visible screen)
    Lines(Vec<usize>),
    /// Everything changed (scroll, resize, mode switch, first frame)
    Full,
}

impl TextInvalidation {
    pub fn is_none(&self) -> bool {
        matches!(self, TextInvalidation::None)
    }
}

/// Semantic zone type from OSC 133 shell integration
///
/// OSC 133 sequences mark boundaries between prompt, input, and output regions.
/// This allows the terminal to apply different rendering (e.g., glow on prompt/input only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SemanticZone {
    /// No OSC 133 zone information (shell doesn't support it or before first marker)
    #[default]
    Unknown,
    /// Prompt region (between OSC 133;A and OSC 133;B)
    Prompt,
    /// User input region (between OSC 133;B and OSC 133;C)
    Input,
    /// Command output region (between OSC 133;C and next OSC 133;A)
    Output,
}

/// Shell events that can trigger theme effects
///
/// These events are detected from terminal output and can be used
/// to trigger visual effects defined in theme CSS (::on-bell, etc.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellEvent {
    /// Bell character received (BEL, 0x07)
    Bell,
    /// Command completed successfully (OSC 133;D with exit code 0)
    CommandSuccess,
    /// Command failed (OSC 133;D with non-zero exit code)
    CommandFail(i32),
}
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{self, Config as TermConfig, Term};
use alacritty_terminal::vte::ansi;

/// Terminal event handler that collects events for the application.
///
/// Uses a lock-free `SegQueue` instead of `Mutex<Vec>` to avoid contention
/// on the rendering hot path where `take_events()` is called every frame.
#[derive(Clone)]
pub struct TerminalEventProxy {
    queue: Arc<SegQueue<Event>>,
}

impl TerminalEventProxy {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(SegQueue::new()),
        }
    }

    /// Take all pending events (lock-free drain)
    pub fn take_events(&self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(event) = self.queue.pop() {
            events.push(event);
        }
        events
    }
}

impl Default for TerminalEventProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl EventListener for TerminalEventProxy {
    fn send_event(&self, event: Event) {
        self.queue.push(event);
    }
}

/// Terminal size in characters
#[derive(Debug, Clone, Copy)]
pub struct Size {
    pub columns: usize,
    pub lines: usize,
}

impl Size {
    pub fn new(columns: usize, lines: usize) -> Self {
        Self { columns, lines }
    }
}

/// CRT Terminal wrapper around alacritty_terminal
pub struct Terminal {
    term: Term<TerminalEventProxy>,
    event_proxy: TerminalEventProxy,
    parser: ansi::Processor,
    size: Size,
    /// Semantic zones per line (from OSC 133), keyed by absolute line index
    /// (`history_size + screen_line` at the time the marker was seen) so the
    /// entries stay attached to their text as the grid scrolls.
    line_zones: BTreeMap<i64, SemanticZone>,
    /// Current semantic zone state (for marking new content)
    current_zone: SemanticZone,
    /// Pending shell events for theme triggers (bell, command success/fail)
    pending_shell_events: Vec<ShellEvent>,
    /// Bytes held back from the previous chunk because they might be the
    /// start of an OSC 133 sequence split across two reads.
    osc_carry: Vec<u8>,
}

impl Terminal {
    /// Create a new terminal with the given size
    pub fn new(size: Size) -> Self {
        let config = TermConfig::default();
        let term_size = TermSize::new(size.columns, size.lines);
        let event_proxy = TerminalEventProxy::new();
        let term = Term::new(config, &term_size, event_proxy.clone());
        let parser = ansi::Processor::new();

        Self {
            term,
            event_proxy,
            parser,
            size,
            line_zones: BTreeMap::new(),
            current_zone: SemanticZone::Unknown,
            pending_shell_events: Vec::new(),
            osc_carry: Vec::new(),
        }
    }

    /// Get terminal dimensions
    pub fn size(&self) -> Size {
        self.size
    }

    /// Get the number of columns
    pub fn columns(&self) -> usize {
        self.term.columns()
    }

    /// Get the number of visible lines
    pub fn screen_lines(&self) -> usize {
        self.term.screen_lines()
    }

    /// Process input bytes through the terminal parser
    ///
    /// Selection is preserved across output processing to support copy/paste
    /// during active shell output (e.g., during builds, long-running commands).
    ///
    /// OSC 133 markers are handled *at their position in the stream*: bytes
    /// before a marker are parsed first so the cursor line recorded for the
    /// zone is correct, and a marker split across two reads is reassembled.
    pub fn process_input(&mut self, bytes: &[u8]) {
        // Preserve selection across output processing
        // Alacritty_terminal clears selection when lines are cleared or screen is modified,
        // but we want to keep it for copy/paste convenience
        let saved_selection = self.term.selection.clone();

        if self.osc_carry.is_empty() {
            self.process_with_markers(bytes);
        } else {
            let mut joined = std::mem::take(&mut self.osc_carry);
            joined.extend_from_slice(bytes);
            self.process_with_markers(&joined);
        }

        // Restore selection if it was cleared during processing
        if saved_selection.is_some() && self.term.selection.is_none() {
            self.term.selection = saved_selection;
        }

        self.prune_zones();
    }

    /// Feed `bytes` to the parser, handling OSC 133 markers in stream order.
    fn process_with_markers(&mut self, bytes: &[u8]) {
        let mut fed = 0;
        let mut i = 0;
        while i < bytes.len() {
            match scan_osc133_at(bytes, i) {
                Osc133Scan::None => i += 1,
                Osc133Scan::Marker {
                    cmd,
                    exit_code,
                    end,
                } => {
                    // Parse everything before the marker so the cursor is where
                    // the shell expects when the marker is recorded.
                    self.parser.advance(&mut self.term, &bytes[fed..i]);
                    self.handle_osc133(cmd, exit_code);
                    fed = i;
                    i = end;
                }
                Osc133Scan::Incomplete => {
                    // Possible marker cut off by the read boundary: feed what we
                    // have before it and hold the rest for the next chunk.
                    self.parser.advance(&mut self.term, &bytes[fed..i]);
                    self.osc_carry.extend_from_slice(&bytes[i..]);
                    return;
                }
            }
        }
        self.parser.advance(&mut self.term, &bytes[fed..]);
    }

    /// Drop zone entries that have scrolled far out of the retained history.
    fn prune_zones(&mut self) {
        const KEEP_HISTORY_LINES: i64 = 2000;
        let floor = self.term.grid().history_size() as i64 - KEEP_HISTORY_LINES;
        if floor > 0
            && self
                .line_zones
                .first_key_value()
                .is_some_and(|(k, _)| *k < floor)
        {
            self.line_zones = self.line_zones.split_off(&floor);
        }
    }

    /// Absolute index of a screen line: stable across scrolling until the
    /// scrollback buffer is full.
    fn absolute_line(&self, screen_line: i32) -> i64 {
        self.term.grid().history_size() as i64 + screen_line as i64
    }

    /// Handle an OSC 133 command
    fn handle_osc133(&mut self, cmd: u8, exit_code: Option<i32>) {
        // Get current cursor line from terminal
        let screen_line = self.term.grid().cursor.point.line.0;
        let line = self.absolute_line(screen_line);

        match cmd {
            b'A' => {
                // Prompt start
                self.current_zone = SemanticZone::Prompt;
                self.line_zones.insert(line, SemanticZone::Prompt);
                log::debug!("OSC 133;A: Prompt start at line {}", line);
            }
            b'B' => {
                // Command start (end of prompt, user input begins)
                self.current_zone = SemanticZone::Input;
                self.line_zones.insert(line, SemanticZone::Input);
                log::debug!("OSC 133;B: Input start at line {}", line);
            }
            b'C' => {
                // Output start (command executed)
                self.current_zone = SemanticZone::Output;
                self.line_zones.insert(line, SemanticZone::Output);
                log::debug!("OSC 133;C: Output start at line {}", line);
            }
            b'D' => {
                // Output end with exit code - emit command success/fail event
                let code = exit_code.unwrap_or(0);
                log::debug!(
                    "OSC 133;D: Output end at line {}, exit code: {}",
                    line,
                    code
                );
                if code == 0 {
                    self.pending_shell_events.push(ShellEvent::CommandSuccess);
                } else {
                    self.pending_shell_events
                        .push(ShellEvent::CommandFail(code));
                }
            }
            _ => {}
        }
    }

    /// Get semantic zone for a given line
    ///
    /// Returns Unknown if no OSC 133 marker has been seen for this line.
    pub fn get_line_zone(&self, line: i32) -> SemanticZone {
        self.line_zones
            .get(&self.absolute_line(line))
            .copied()
            .unwrap_or(SemanticZone::Unknown)
    }

    /// Check if any OSC 133 zones have been detected
    ///
    /// Returns true if the shell has sent at least one OSC 133 sequence,
    /// indicating it supports semantic prompts.
    pub fn has_semantic_zones(&self) -> bool {
        !self.line_zones.is_empty()
    }

    /// Get the current semantic zone state
    pub fn current_zone(&self) -> SemanticZone {
        self.current_zone
    }

    /// Get access to renderable content (cells, cursor, etc.)
    pub fn renderable_content(&self) -> term::RenderableContent<'_> {
        self.term.renderable_content()
    }

    /// Get the cursor information
    pub fn cursor(&self) -> term::RenderableCursor {
        self.renderable_content().cursor
    }

    /// Check if cursor should be visible (based on SHOW_CURSOR mode)
    pub fn cursor_mode_visible(&self) -> bool {
        self.term.mode().contains(TermMode::SHOW_CURSOR)
    }

    /// Get terminal mode flags
    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Take pending terminal events
    pub fn take_events(&self) -> Vec<Event> {
        self.event_proxy.take_events()
    }

    /// Take pending shell events (bell, command success/fail)
    ///
    /// These events can trigger theme visual effects.
    pub fn take_shell_events(&mut self) -> Vec<ShellEvent> {
        std::mem::take(&mut self.pending_shell_events)
    }

    /// Resize the terminal
    pub fn resize(&mut self, size: Size) {
        self.size = size;
        let term_size = TermSize::new(size.columns, size.lines);
        self.term.resize(term_size);
    }

    /// Access the underlying Term for advanced operations
    pub fn inner(&self) -> &Term<TerminalEventProxy> {
        &self.term
    }

    /// Mutable access to the underlying Term
    pub fn inner_mut(&mut self) -> &mut Term<TerminalEventProxy> {
        &mut self.term
    }

    /// Get damage information since last reset
    ///
    /// Returns which parts of the terminal have changed and need redrawing.
    /// Call `reset_damage()` after rendering to clear the damage state.
    pub fn damage(&mut self) -> TermDamage<'_> {
        self.term.damage()
    }

    /// Reset damage state after rendering
    ///
    /// Call this after you've rendered the damaged regions to clear the
    /// damage tracking for the next frame.
    pub fn reset_damage(&mut self) {
        self.term.reset_damage();
    }

    /// Check if any damage exists (needs redraw)
    pub fn has_damage(&mut self) -> bool {
        match self.term.damage() {
            TermDamage::Full => true,
            TermDamage::Partial(iter) => iter.count() > 0,
        }
    }

    /// Get the set of damaged line indices (0-based visible lines).
    ///
    /// Returns `None` for full damage (caller should re-render everything).
    /// Returns `Some(set)` with the indices of lines that changed for partial damage.
    pub fn damaged_line_set(&mut self) -> Option<Vec<usize>> {
        match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(iter) => {
                let lines: Vec<usize> = iter.map(|bounds| bounds.line).collect();
                if lines.is_empty() {
                    // No damage at all — return empty set (not None)
                    Some(Vec::new())
                } else {
                    Some(lines)
                }
            }
        }
    }

    /// Take and reset the damage accumulated since the previous call.
    ///
    /// This is the one invalidation source for the text layer: alacritty
    /// tracks cursor movement, selection, attribute-only changes, scrolling
    /// and resizes, so nothing else needs to hash the grid. Lines are
    /// viewport-relative (already adjusted for `display_offset`).
    pub fn take_damage(&mut self) -> TextInvalidation {
        let result = match self.term.damage() {
            TermDamage::Full => TextInvalidation::Full,
            TermDamage::Partial(iter) => {
                let lines: Vec<usize> = iter.map(|bounds| bounds.line).collect();
                if lines.is_empty() {
                    TextInvalidation::None
                } else {
                    TextInvalidation::Lines(lines)
                }
            }
        };
        self.term.reset_damage();
        result
    }

    /// Mark the whole grid as needing a redraw (e.g. after a theme change).
    pub fn mark_full_damage(&mut self) {
        // Resizing to the current size is the public way to fully damage a Term.
        let size = TermSize::new(self.term.columns(), self.term.screen_lines());
        self.term.resize(size);
    }

    /// Start a new selection at the given point
    pub fn start_selection(&mut self, point: Point, selection_type: SelectionType) {
        use alacritty_terminal::index::Side;
        use alacritty_terminal::selection::Selection;
        self.term.selection = Some(Selection::new(selection_type, point, Side::Left));
    }

    /// Update the selection end point
    pub fn update_selection(&mut self, point: Point) {
        use alacritty_terminal::index::Side;
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, Side::Right);
        }
    }

    /// Clear the current selection
    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Check if a selection exists
    pub fn has_selection(&self) -> bool {
        self.term.selection.is_some()
    }

    /// Get the selection as text, if any
    pub fn selection_to_string(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    /// Scroll the terminal viewport
    ///
    /// Use `Scroll::Delta(n)` to scroll by n lines (positive = up into history)
    /// Use `Scroll::PageUp`, `Scroll::PageDown`, `Scroll::Top`, `Scroll::Bottom`
    pub fn scroll(&mut self, scroll: alacritty_terminal::grid::Scroll) {
        self.term.scroll_display(scroll);
    }

    /// Get the current display offset (how far scrolled into history)
    /// Returns 0 when at the bottom (live output), >0 when scrolled back
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Check if the terminal is scrolled back (not showing live output)
    pub fn is_scrolled_back(&self) -> bool {
        self.display_offset() > 0
    }

    /// Scroll to the bottom (show live output)
    pub fn scroll_to_bottom(&mut self) {
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
    }

    /// Get total number of lines including history
    pub fn total_lines(&self) -> usize {
        self.term.grid().total_lines()
    }

    /// Get history size (lines above visible area)
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Get all lines as text (history + visible), returns Vec of (line_index, text)
    /// Line indices are relative to the grid: negative = history, 0+ = visible
    pub fn all_lines_text(&self) -> Vec<(i32, String)> {
        let grid = self.term.grid();
        let history_size = grid.history_size() as i32;
        let screen_lines = self.term.screen_lines() as i32;
        let cols = self.columns();
        let mut lines = Vec::with_capacity((history_size + screen_lines) as usize);

        // History lines (negative indices, from oldest to newest)
        for i in (0..history_size).rev() {
            let line_idx = -(i + 1);
            let row = &grid[alacritty_terminal::index::Line(line_idx)];
            let mut text = String::with_capacity(cols);
            for cell in row.into_iter() {
                text.push(cell.c);
            }
            let trimmed_len = text.trim_end().len();
            text.truncate(trimmed_len);
            lines.push((line_idx, text));
        }

        // Visible lines (0 to screen_lines-1)
        for i in 0..screen_lines {
            let row = &grid[alacritty_terminal::index::Line(i)];
            let mut text = String::with_capacity(cols);
            for cell in row.into_iter() {
                text.push(cell.c);
            }
            let trimmed_len = text.trim_end().len();
            text.truncate(trimmed_len);
            lines.push((i, text));
        }

        lines
    }

    /// Check if bracketed paste mode is enabled
    pub fn bracketed_paste_enabled(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }
}

/// Result of probing for an OSC 133 sequence at one offset.
enum Osc133Scan {
    /// No sequence starts here
    None,
    /// A complete sequence: command byte, optional exit code, and end offset (exclusive)
    Marker {
        cmd: u8,
        exit_code: Option<i32>,
        end: usize,
    },
    /// The chunk ends inside what may be an OSC 133 sequence
    Incomplete,
}

/// Longest OSC 133 sequence we recognise, parameters included
/// (`ESC ] 133 ; D ; <exit code> ; aid=<pid> ESC \`). Also bounds how much of
/// a chunk's tail is carried over to the next read.
const OSC133_MAX_LEN: usize = 128;

/// Probe `bytes[i..]` for `ESC ] 133 ; X [; digits] [; params] (BEL | ESC \)`.
fn scan_osc133_at(bytes: &[u8], i: usize) -> Osc133Scan {
    const PREFIX: &[u8] = b"\x1b]133;";
    let rest = &bytes[i..];
    if rest.is_empty() || rest[0] != 0x1b {
        return Osc133Scan::None;
    }
    // Does the available data still agree with the prefix?
    let cmp = rest.len().min(PREFIX.len());
    if rest[..cmp] != PREFIX[..cmp] {
        return Osc133Scan::None;
    }
    if rest.len() <= PREFIX.len() {
        return Osc133Scan::Incomplete;
    }
    let cmd = rest[PREFIX.len()];
    let mut pos = PREFIX.len() + 1;
    let mut exit_code = None;
    if cmd == b'D' && rest.get(pos) == Some(&b';') {
        let digits_start = pos + 1;
        let mut end = digits_start;
        while end < rest.len() && rest[end].is_ascii_digit() {
            end += 1;
        }
        if end > digits_start {
            exit_code = std::str::from_utf8(&rest[digits_start..end])
                .ok()
                .and_then(|s| s.parse::<i32>().ok());
        }
        pos = end;
    }
    // Optional `;key=value` parameters (WezTerm `aid=`, kitty `cl=`, ...):
    // printable bytes up to the terminator. They carry nothing we use.
    if rest.get(pos) == Some(&b';') {
        while pos < rest.len() && pos < OSC133_MAX_LEN && (0x20..0x7f).contains(&rest[pos]) {
            pos += 1;
        }
    }
    // Terminator: BEL or ESC \ (ST). Anything else is not a marker we handle.
    match rest.get(pos) {
        Some(0x07) => Osc133Scan::Marker {
            cmd,
            exit_code,
            end: i + pos + 1,
        },
        Some(0x1b) => match rest.get(pos + 1) {
            Some(b'\\') => Osc133Scan::Marker {
                cmd,
                exit_code,
                end: i + pos + 2,
            },
            Some(_) => Osc133Scan::None,
            None => Osc133Scan::Incomplete,
        },
        Some(_) => Osc133Scan::None,
        None if pos < OSC133_MAX_LEN => Osc133Scan::Incomplete,
        None => Osc133Scan::None,
    }
}

/// A terminal connected to a PTY backend running a shell.
///
/// Generic over the PTY backend to enable testing with mock PTY implementations.
pub struct ShellTerminalGeneric<P: PtyBackend> {
    terminal: Terminal,
    pty: P,
    /// Cached working directory (querying it costs a syscall, or a fork on
    /// platforms without libproc); refreshed at most every `CWD_CACHE_TTL`.
    cwd_cache: RefCell<Option<(Instant, Option<PathBuf>)>>,
}

/// How long a cached working directory stays valid.
const CWD_CACHE_TTL: Duration = Duration::from_secs(1);

/// Backward-compatible alias for the concrete PTY implementation
pub type ShellTerminal = ShellTerminalGeneric<Pty>;

impl ShellTerminal {
    /// Create a new shell terminal with the given size
    pub fn new(size: Size) -> anyhow::Result<Self> {
        let terminal = Terminal::new(size);
        let pty = Pty::spawn(None, size.columns as u16, size.lines as u16)?;

        Ok(Self::from_parts(terminal, pty))
    }

    /// Create a new shell terminal with a specific working directory
    pub fn with_cwd(size: Size, cwd: std::path::PathBuf) -> anyhow::Result<Self> {
        let terminal = Terminal::new(size);
        let pty = Pty::spawn_with_cwd(None, size.columns as u16, size.lines as u16, Some(cwd))?;

        Ok(Self::from_parts(terminal, pty))
    }

    /// Create a new shell terminal with a specific shell
    pub fn with_shell(size: Size, shell: &str) -> anyhow::Result<Self> {
        let terminal = Terminal::new(size);
        let pty = Pty::spawn(Some(shell), size.columns as u16, size.lines as u16)?;

        Ok(Self::from_parts(terminal, pty))
    }

    /// Create a new shell terminal with full spawn options
    ///
    /// This enables semantic prompt support (OSC 133) for command success/fail detection.
    pub fn with_options(size: Size, options: SpawnOptions) -> anyhow::Result<Self> {
        let terminal = Terminal::new(size);
        let pty = Pty::spawn_with_options(size.columns as u16, size.lines as u16, options)?;

        Ok(Self::from_parts(terminal, pty))
    }

    /// Get access to the PTY
    pub fn pty(&self) -> &Pty {
        &self.pty
    }
}

impl<P: PtyBackend> ShellTerminalGeneric<P> {
    /// Create a shell terminal with a custom PTY backend
    pub fn with_backend(size: Size, pty: P) -> Self {
        let terminal = Terminal::new(size);
        Self::from_parts(terminal, pty)
    }

    fn from_parts(terminal: Terminal, pty: P) -> Self {
        Self {
            terminal,
            pty,
            cwd_cache: RefCell::new(None),
        }
    }

    /// Get the current working directory of the shell process (cached for
    /// `CWD_CACHE_TTL`; call `invalidate_cwd` after a command completes to
    /// refresh sooner).
    pub fn working_directory(&self) -> Option<PathBuf> {
        let now = Instant::now();
        if let Some((at, cwd)) = self.cwd_cache.borrow().as_ref()
            && now.duration_since(*at) < CWD_CACHE_TTL
        {
            return cwd.clone();
        }
        let cwd = self.pty.working_directory();
        *self.cwd_cache.borrow_mut() = Some((now, cwd.clone()));
        cwd
    }

    /// Forget the cached working directory.
    pub fn invalidate_cwd(&self) {
        *self.cwd_cache.borrow_mut() = None;
    }

    /// Process any available PTY output through the terminal
    /// Returns true if any output was processed
    pub fn process_pty_output(&mut self) -> bool {
        let output = self.pty.read_available();
        if output.is_empty() {
            return false;
        }
        // Escape-sequence trace for debugging; only built when it will be logged.
        if log::log_enabled!(log::Level::Debug) && output.len() < 2000 {
            use std::fmt::Write;
            let mut escaped = String::with_capacity(output.len() * 2);
            for &b in &output {
                if b == 0x1b {
                    escaped.push_str("ESC");
                } else if b == 0x07 {
                    escaped.push_str("BEL");
                } else if b < 32 {
                    let _ = write!(escaped, "^{}", (b + 64) as char);
                } else if b < 127 {
                    escaped.push(b as char);
                } else {
                    let _ = write!(escaped, "\\x{:02x}", b);
                }
            }
            log::debug!("PTY output ({} bytes): {}", output.len(), escaped);
        }
        self.terminal.process_input(&output);
        true
    }

    /// Send keyboard input to the PTY
    pub fn send_input(&self, data: &[u8]) {
        self.pty.write(data);
    }

    /// Resize both the terminal and PTY
    pub fn resize(&mut self, size: Size) {
        self.terminal.resize(size);
        self.pty.resize(size.columns as u16, size.lines as u16);
    }

    /// Get access to the terminal for rendering
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    /// Get mutable access to the terminal
    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.terminal
    }

    /// Take any pending terminal events (title changes, bells, etc.)
    pub fn take_events(&self) -> Vec<Event> {
        self.terminal.take_events()
    }

    /// Check for title change and return it, preserving other events
    /// Returns the title string from the most recent Title event
    pub fn check_title_change(&self) -> Option<String> {
        let (title, _) = self.check_events();
        title
    }

    /// Check for terminal events and return title changes and bell triggers
    /// Returns (Option<title>, bell_triggered)
    ///
    /// Note: For theme-triggerable events, prefer `take_shell_events()` which
    /// provides a unified `ShellEvent` enum including bell, command success/fail.
    pub fn check_events(&self) -> (Option<String>, bool) {
        let events = self.terminal.take_events();
        let mut title = None;
        let mut bell = false;

        if !events.is_empty() {
            log::debug!("Terminal events: {:?}", events);
        }

        for event in events {
            match event {
                Event::Title(t) => title = Some(t),
                Event::Bell => {
                    log::debug!("Bell event received from terminal");
                    bell = true;
                }
                _ => {} // Ignore other events
            }
        }

        (title, bell)
    }

    /// Take shell events for theme triggers (bell, command success/fail)
    ///
    /// This returns events that can trigger visual effects defined in theme CSS
    /// (::on-bell, ::on-command-success, ::on-command-fail).
    ///
    /// Also checks for Bell from alacritty_terminal and includes it as ShellEvent::Bell.
    /// Returns (shell_events, Option<title>) to combine with title checking.
    pub fn take_shell_events(&mut self) -> (Vec<ShellEvent>, Option<String>) {
        let mut shell_events = self.terminal.take_shell_events();

        // Also check terminal events for Bell and title
        let events = self.terminal.take_events();
        let mut title = None;

        for event in events {
            match event {
                Event::Title(t) => title = Some(t),
                Event::Bell => {
                    log::debug!("Bell event converted to ShellEvent");
                    shell_events.push(ShellEvent::Bell);
                }
                _ => {}
            }
        }

        (shell_events, title)
    }

    /// Start a new selection at the given point
    pub fn start_selection(&mut self, point: Point, selection_type: SelectionType) {
        self.terminal.start_selection(point, selection_type);
    }

    /// Update the selection end point
    pub fn update_selection(&mut self, point: Point) {
        self.terminal.update_selection(point);
    }

    /// Clear the current selection
    pub fn clear_selection(&mut self) {
        self.terminal.clear_selection();
    }

    /// Check if a selection exists
    pub fn has_selection(&self) -> bool {
        self.terminal.has_selection()
    }

    /// Get the selection as text, if any
    pub fn selection_to_string(&self) -> Option<String> {
        self.terminal.selection_to_string()
    }

    /// Scroll the terminal viewport
    pub fn scroll(&mut self, scroll: crate::Scroll) {
        self.terminal.scroll(scroll);
    }

    /// Get the current display offset (how far scrolled into history)
    pub fn display_offset(&self) -> usize {
        self.terminal.display_offset()
    }

    /// Check if the terminal is scrolled back (not showing live output)
    pub fn is_scrolled_back(&self) -> bool {
        self.terminal.is_scrolled_back()
    }

    /// Scroll to the bottom (show live output)
    pub fn scroll_to_bottom(&mut self) {
        self.terminal.scroll_to_bottom();
    }

    /// Check if bracketed paste mode is enabled
    pub fn bracketed_paste_enabled(&self) -> bool {
        self.terminal.bracketed_paste_enabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn create_terminal() {
        let term = Terminal::new(Size::new(80, 24));
        assert_eq!(term.columns(), 80);
        assert_eq!(term.screen_lines(), 24);
    }

    #[test]
    fn process_simple_text() {
        let mut term = Terminal::new(Size::new(80, 24));
        term.process_input(b"Hello, World!");
        // Text should be in the grid now
    }

    #[test]
    fn bell_event_triggered() {
        use alacritty_terminal::event::Event;

        let mut term = Terminal::new(Size::new(80, 24));

        // Clear any existing events
        term.take_events();

        // Send BEL character (0x07)
        term.process_input(b"\x07");

        // Check for Bell event
        let events = term.take_events();
        let has_bell = events.iter().any(|e| matches!(e, Event::Bell));
        assert!(has_bell, "Expected Bell event, got: {:?}", events);
    }

    #[test]
    fn shell_terminal_integration() {
        let mut shell = ShellTerminal::new(Size::new(80, 24)).expect("Failed to create shell");

        // Give the shell time to start and output prompt
        thread::sleep(Duration::from_millis(100));

        // Process initial output (shell prompt)
        shell.process_pty_output();

        // Send a command
        shell.send_input(b"echo test123\n");

        // Wait for output
        thread::sleep(Duration::from_millis(100));

        // Process the output
        shell.process_pty_output();

        // Terminal should now have content
        // (We're not checking specific content as it depends on shell)
    }

    #[test]
    fn take_damage_resets() {
        let mut term = Terminal::new(Size::new(80, 24));
        assert_eq!(term.take_damage(), TextInvalidation::Full);
        // Nothing happened since: no damage (cursor is re-damaged by alacritty
        // on every call, so at most its line is reported).
        match term.take_damage() {
            TextInvalidation::None | TextInvalidation::Lines(_) => {}
            TextInvalidation::Full => panic!("damage was not reset"),
        }
        term.process_input(b"hello");
        match term.take_damage() {
            TextInvalidation::Lines(lines) => assert!(lines.contains(&0)),
            other => panic!("expected partial damage, got {:?}", other),
        }
        term.process_input(b"\r\n".repeat(30).as_slice());
        assert_eq!(term.take_damage(), TextInvalidation::Full);
    }

    #[test]
    fn osc133_marker_split_across_reads_and_after_newline() {
        let mut term = Terminal::new(Size::new(80, 24));
        // Marker arrives after a newline in the same chunk: recorded on line 1
        term.process_input(b"prompt\r\n\x1b]133;A\x07");
        assert_eq!(term.get_line_zone(1), SemanticZone::Prompt);
        assert_eq!(term.get_line_zone(0), SemanticZone::Unknown);
        // Marker split across two reads
        term.process_input(b"\r\n\x1b]13");
        term.process_input(b"3;B\x07");
        assert_eq!(term.get_line_zone(2), SemanticZone::Input);
        assert_eq!(term.current_zone(), SemanticZone::Input);
    }

    #[test]
    fn osc133_zones_follow_scrolled_text() {
        let mut term = Terminal::new(Size::new(80, 4));
        term.process_input(b"\x1b]133;A\x07$ ");
        assert_eq!(term.get_line_zone(0), SemanticZone::Prompt);
        // Scroll the screen by two lines: the marked text is now on line -2
        // of the grid (history) and line 0 holds different text.
        term.process_input(b"\r\n\r\n\r\n\r\n\r\n");
        assert_eq!(term.get_line_zone(0), SemanticZone::Unknown);
        assert_eq!(term.get_line_zone(-2), SemanticZone::Prompt);
    }

    #[test]
    fn osc133_no_zones_initially() {
        let term = Terminal::new(Size::new(80, 24));
        assert!(!term.has_semantic_zones());
        assert_eq!(term.get_line_zone(0), SemanticZone::Unknown);
        assert_eq!(term.current_zone(), SemanticZone::Unknown);
    }

    #[test]
    fn osc133_prompt_start_with_bel() {
        let mut term = Terminal::new(Size::new(80, 24));

        // OSC 133;A with BEL terminator (prompt start)
        term.process_input(b"\x1b]133;A\x07");

        assert!(term.has_semantic_zones());
        assert_eq!(term.current_zone(), SemanticZone::Prompt);
        assert_eq!(term.get_line_zone(0), SemanticZone::Prompt);
    }

    #[test]
    fn osc133_prompt_start_with_st() {
        let mut term = Terminal::new(Size::new(80, 24));

        // OSC 133;A with ST terminator (ESC \)
        term.process_input(b"\x1b]133;A\x1b\\");

        assert!(term.has_semantic_zones());
        assert_eq!(term.current_zone(), SemanticZone::Prompt);
    }

    #[test]
    fn osc133_full_sequence() {
        let mut term = Terminal::new(Size::new(80, 24));

        // Simulate full shell integration sequence:
        // A = prompt start, B = command start, C = output start

        // Prompt start
        term.process_input(b"\x1b]133;A\x07");
        assert_eq!(term.current_zone(), SemanticZone::Prompt);

        // Some prompt text
        term.process_input(b"$ ");

        // Command start (user input begins)
        term.process_input(b"\x1b]133;B\x07");
        assert_eq!(term.current_zone(), SemanticZone::Input);

        // User types command and hits enter, then output starts
        term.process_input(b"ls -la\n");
        term.process_input(b"\x1b]133;C\x07");
        assert_eq!(term.current_zone(), SemanticZone::Output);

        // Output
        term.process_input(b"file1.txt\nfile2.txt\n");

        // Next prompt
        term.process_input(b"\x1b]133;A\x07");
        assert_eq!(term.current_zone(), SemanticZone::Prompt);
    }

    #[test]
    fn osc133_embedded_in_other_data() {
        let mut term = Terminal::new(Size::new(80, 24));

        // OSC 133 embedded in other text (as it would be from shell)
        term.process_input(b"some text\x1b]133;A\x07more text");

        assert!(term.has_semantic_zones());
        assert_eq!(term.current_zone(), SemanticZone::Prompt);
    }

    #[test]
    fn osc133_unknown_command_ignored() {
        let mut term = Terminal::new(Size::new(80, 24));

        // Unknown OSC 133 command (X) should be ignored
        term.process_input(b"\x1b]133;X\x07");

        // No zones should be set
        assert!(!term.has_semantic_zones());
        assert_eq!(term.current_zone(), SemanticZone::Unknown);
    }

    #[test]
    fn osc133_d_command_success() {
        let mut term = Terminal::new(Size::new(80, 24));

        // OSC 133;D;0 = command completed successfully
        term.process_input(b"\x1b]133;D;0\x07");

        let events = term.take_shell_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], ShellEvent::CommandSuccess);
    }

    #[test]
    fn osc133_d_command_fail() {
        let mut term = Terminal::new(Size::new(80, 24));

        // OSC 133;D;1 = command failed with exit code 1
        term.process_input(b"\x1b]133;D;1\x07");

        let events = term.take_shell_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], ShellEvent::CommandFail(1));
    }

    /// Regression: the v0.1.4 scanner only accepted a bare marker, so shell
    /// integrations that attach parameters (WezTerm `D;<code>;aid=<pid>`,
    /// kitty/fish `A;k=v`) lost their prompt zones and command events.
    #[test]
    fn osc133_markers_with_parameters_are_recognised() {
        let mut term = Terminal::new(Size::new(80, 24));
        term.process_input(b"\x1b]133;D;1;aid=4242\x07");
        assert_eq!(term.take_shell_events(), vec![ShellEvent::CommandFail(1)]);

        term.process_input(b"\x1b]133;D;0;aid=4242\x1b\\");
        assert_eq!(term.take_shell_events(), vec![ShellEvent::CommandSuccess]);

        let mut term = Terminal::new(Size::new(80, 24));
        term.process_input(b"\x1b]133;A;cl=m;aid=4242\x07$ ");
        assert_eq!(term.get_line_zone(0), SemanticZone::Prompt);
    }

    /// A marker with parameters may also be split across two PTY reads.
    #[test]
    fn osc133_marker_with_parameters_split_across_reads() {
        let mut term = Terminal::new(Size::new(80, 24));
        term.process_input(b"\x1b]133;D;3;ai");
        assert!(term.take_shell_events().is_empty());
        term.process_input(b"d=4242\x07");
        assert_eq!(term.take_shell_events(), vec![ShellEvent::CommandFail(3)]);
    }

    /// Parameters are bounded and printable: a stray prefix must not make
    /// the scanner hold back or swallow ordinary output.
    #[test]
    fn osc133_unterminated_parameters_are_not_a_marker() {
        let mut term = Terminal::new(Size::new(80, 24));
        let mut input = b"\x1b]133;A;".to_vec();
        input.extend(std::iter::repeat_n(b'x', 1024));
        term.process_input(&input);
        assert!(!term.has_semantic_zones());
    }

    #[test]
    fn osc133_d_command_fail_with_larger_code() {
        let mut term = Terminal::new(Size::new(80, 24));

        // OSC 133;D;127 = command failed with exit code 127 (command not found)
        term.process_input(b"\x1b]133;D;127\x07");

        let events = term.take_shell_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], ShellEvent::CommandFail(127));
    }

    #[test]
    fn osc133_d_no_exit_code_defaults_success() {
        let mut term = Terminal::new(Size::new(80, 24));

        // OSC 133;D without exit code should default to success (0)
        term.process_input(b"\x1b]133;D\x07");

        let events = term.take_shell_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], ShellEvent::CommandSuccess);
    }

    #[test]
    fn shell_events_clear_after_take() {
        let mut term = Terminal::new(Size::new(80, 24));

        term.process_input(b"\x1b]133;D;0\x07");
        let events = term.take_shell_events();
        assert_eq!(events.len(), 1);

        // Second take should be empty
        let events = term.take_shell_events();
        assert!(events.is_empty());
    }

    #[test]
    fn multiple_shell_events_accumulated() {
        let mut term = Terminal::new(Size::new(80, 24));

        // Multiple commands
        term.process_input(b"\x1b]133;D;0\x07");
        term.process_input(b"\x1b]133;D;1\x07");

        let events = term.take_shell_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], ShellEvent::CommandSuccess);
        assert_eq!(events[1], ShellEvent::CommandFail(1));
    }

    /// Mock PTY backend for deterministic testing without real shell processes
    pub struct MockPty {
        output_queue: std::cell::RefCell<std::collections::VecDeque<Vec<u8>>>,
        captured_input: std::cell::RefCell<Vec<u8>>,
        last_resize: std::cell::Cell<Option<(u16, u16)>>,
        shutdown_called: std::cell::Cell<bool>,
    }

    impl MockPty {
        /// Create a MockPty with pre-loaded output chunks
        pub fn with_output(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                output_queue: std::cell::RefCell::new(chunks.into()),
                captured_input: std::cell::RefCell::new(Vec::new()),
                last_resize: std::cell::Cell::new(None),
                shutdown_called: std::cell::Cell::new(false),
            }
        }

        /// Get all input that was written to this mock PTY
        pub fn captured_input(&self) -> Vec<u8> {
            self.captured_input.borrow().clone()
        }
    }

    impl PtyBackend for MockPty {
        fn write(&self, data: &[u8]) {
            self.captured_input.borrow_mut().extend_from_slice(data);
        }

        fn try_read(&self) -> Option<Vec<u8>> {
            self.output_queue.borrow_mut().pop_front()
        }

        fn read_available(&self) -> Vec<u8> {
            self.output_queue.borrow_mut().drain(..).flatten().collect()
        }

        fn resize(&self, cols: u16, rows: u16) {
            self.last_resize.set(Some((cols, rows)));
        }

        fn shutdown(&self) {
            self.shutdown_called.set(true);
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        fn working_directory(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    #[test]
    fn test_mock_pty_terminal() {
        let mock = MockPty::with_output(vec![]);
        let mut term = ShellTerminalGeneric::with_backend(Size::new(80, 24), mock);

        // Feed output directly to the terminal (bypassing PTY, as mock read_available returns empty)
        term.terminal_mut().process_input(b"Hello, world!\r\n");

        // Verify terminal content
        let content = term.terminal().renderable_content();
        let first_line: String = content
            .display_iter
            .take_while(|cell| cell.point.line.0 == 0)
            .map(|cell| cell.c)
            .collect::<String>()
            .trim_end()
            .to_string();
        assert!(
            first_line.contains("Hello, world!"),
            "Expected 'Hello, world!' in first line, got: '{}'",
            first_line
        );

        // Verify input capture
        term.send_input(b"ls\n");
        assert_eq!(term.pty.captured_input(), b"ls\n");

        // Verify resize tracking
        term.resize(Size::new(120, 40));
        assert_eq!(term.pty.last_resize.get(), Some((120, 40)));
    }
}
