//! Minimal-viable ratatui browser for a scanned [`Tree`].
//!
//! Subset of `browser.zig`. v0 supports navigation, sorting by disk size
//! descending, and delete-with-confirm. Deferred:
//! - Sort column switching (always disk-blocks desc)
//! - Help / info / refresh / search overlays
//! - Show-hidden toggle (no hidden filter applied yet)
//! - Apparent vs disk size toggle (shows disk blocks * 512)
//! - Hardlink shared-counts display
//! - Per-dir mtime aggregation

use std::io;
use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::delete;
use crate::model::{EType, EntryId, NodeKind, Tree};

pub struct Browser {
    tree: Tree,
    root_path: PathBuf,
    current_dir: EntryId,
    parent_stack: Vec<EntryId>,
    /// Sorted children of `current_dir`.
    items: Vec<EntryId>,
    selected: usize,
    confirm_delete: Option<EntryId>,
    status_msg: Option<String>,
}

impl Browser {
    pub fn new(tree: Tree, root_path: PathBuf) -> Self {
        let root = tree.root;
        let mut b = Self {
            tree,
            root_path,
            current_dir: root,
            parent_stack: Vec::new(),
            items: Vec::new(),
            selected: 0,
            confirm_delete: None,
            status_msg: None,
        };
        b.load_dir();
        b
    }

    pub fn run(mut self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let result = self.event_loop(&mut terminal);

        disable_raw_mode()?;
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
        result
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> io::Result<()> {
        loop {
            terminal.draw(|f| self.render(f))?;
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if self.handle_key(key.code) {
                    return Ok(());
                }
            }
        }
    }

    fn load_dir(&mut self) {
        let mut items = Vec::new();
        let mut cur = self
            .tree
            .get(self.current_dir)
            .as_dir()
            .expect("current_dir must be a directory")
            .sub;
        while !cur.is_none() {
            items.push(cur);
            cur = self.tree.get(cur).common.next;
        }
        // Sort by disk blocks desc; tie-break by name asc.
        items.sort_by(|&a, &b| {
            let na = self.tree.get(a);
            let nb = self.tree.get(b);
            nb.common
                .blocks
                .cmp(&na.common.blocks)
                .then_with(|| na.common.name.cmp(&nb.common.name))
        });
        self.items = items;
        self.selected = 0;
    }

    /// Returns `true` when the event loop should exit.
    fn handle_key(&mut self, code: KeyCode) -> bool {
        if let Some(target) = self.confirm_delete {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let parent = self.current_dir;
                    match delete::delete_entry(&mut self.tree, parent, target, &self.root_path) {
                        Ok(()) => self.status_msg = Some("deleted".to_string()),
                        Err(e) => self.status_msg = Some(format!("delete error: {e}")),
                    }
                    self.confirm_delete = None;
                    self.load_dir();
                }
                _ => {
                    self.confirm_delete = None;
                    self.status_msg = Some("delete cancelled".to_string());
                }
            }
            return false;
        }

        match code {
            KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + 10).min(self.items.len().saturating_sub(1));
            }
            KeyCode::Home => {
                self.selected = 0;
            }
            KeyCode::End => {
                self.selected = self.items.len().saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(&id) = self.items.get(self.selected) {
                    if self.tree.get(id).as_dir().is_some() {
                        self.parent_stack.push(self.current_dir);
                        self.current_dir = id;
                        self.load_dir();
                        self.status_msg = None;
                    }
                }
            }
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                if let Some(parent) = self.parent_stack.pop() {
                    self.current_dir = parent;
                    self.load_dir();
                    self.status_msg = None;
                }
            }
            KeyCode::Char('d') => {
                if let Some(&id) = self.items.get(self.selected) {
                    self.confirm_delete = Some(id);
                }
            }
            _ => {}
        }
        false
    }

    fn render(&self, f: &mut Frame) {
        let area = f.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        // Header.
        let header_text = format!(" ncdu-rs --- {} ", self.current_path_display());
        let header = Paragraph::new(header_text).style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
        f.render_widget(header, layout[0]);

        // Item list.
        let dir_total_blocks = self.tree.get(self.current_dir).common.blocks.max(1);
        let items: Vec<ListItem> = self
            .items
            .iter()
            .map(|&id| {
                let n = self.tree.get(id);
                let bytes = n.common.blocks.saturating_mul(512);
                let pct = (n.common.blocks.saturating_mul(100)) / dir_total_blocks;
                let bar_str = render_bar(pct as usize, 10);
                let suffix = match (n.common.etype, &n.kind) {
                    (EType::Dir, _) => "/",
                    (_, NodeKind::Link(_)) => "@",
                    (EType::Pattern, _) => " <excluded>",
                    (EType::OtherFs, _) => " <other fs>",
                    (EType::KernFs, _) => " <kernfs>",
                    (EType::Err, _) => " <error>",
                    _ => "",
                };
                let line = format!(
                    " {:>10}  [{}] {:>3}%  {}{}",
                    fmt_size(bytes),
                    bar_str,
                    pct.min(100),
                    n.common.name,
                    suffix
                );
                ListItem::new(Line::from(Span::raw(line)))
            })
            .collect();

        let title = format!(
            "Contents ({} items, total {})",
            self.items.len(),
            fmt_size(self.tree.get(self.current_dir).common.blocks * 512)
        );
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White));
        let mut state = ListState::default();
        if !self.items.is_empty() {
            state.select(Some(self.selected));
        }
        f.render_stateful_widget(list, layout[1], &mut state);

        // Footer.
        let footer_text = if let Some(target) = self.confirm_delete {
            let n = self.tree.get(target);
            format!(
                " DELETE \"{}\" ({})? Press y to confirm, any other key to cancel ",
                n.common.name,
                fmt_size(n.common.blocks * 512)
            )
        } else if let Some(msg) = &self.status_msg {
            format!(" {} ", msg)
        } else {
            " q quit | enter/l open | bksp/h up | j/k move | d delete | PgUp/PgDn ".to_string()
        };
        let footer_style = if self.confirm_delete.is_some() {
            Style::default()
                .bg(Color::Red)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        };
        f.render_widget(Paragraph::new(footer_text).style(footer_style), layout[2]);
    }

    fn current_path_display(&self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(self.parent_stack.len() + 1);
        for &p in &self.parent_stack {
            parts.push(self.tree.get(p).common.name.to_string());
        }
        parts.push(self.tree.get(self.current_dir).common.name.to_string());
        parts.join("/")
    }
}

fn fmt_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", v, UNITS[i])
}

fn render_bar(pct: usize, width: usize) -> String {
    let filled = ((pct * width) / 100).min(width);
    let mut s = String::with_capacity(width);
    for _ in 0..filled {
        s.push('#');
    }
    for _ in filled..width {
        s.push(' ');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_size_thresholds() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1024), "1.0 KiB");
        assert_eq!(fmt_size(1024 * 1024), "1.0 MiB");
        assert_eq!(fmt_size(1536 * 1024 * 1024), "1.5 GiB");
    }

    #[test]
    fn bar_renders_proportional() {
        assert_eq!(render_bar(0, 10), "          ");
        assert_eq!(render_bar(100, 10), "##########");
        assert_eq!(render_bar(50, 10), "#####     ");
    }
}
