use std::time::Duration;

use crossterm::event::{self, Event, KeyCode};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    widgets::{Block, Borders, Paragraph},
};

pub fn run() -> std::io::Result<()> {
    ratatui::run(|terminal| {
        let mut quit = false;
        while !quit {
            terminal.draw(render)?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
            {
                quit = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc);
            }
        }
        Ok(())
    })
}

pub fn render(frame: &mut Frame) {
    let [body, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    frame.render_widget(
        Paragraph::new("No worktrees loaded")
            .block(Block::default().borders(Borders::ALL).title("ewtm")),
        body,
    );
    frame.render_widget(Paragraph::new("q / Esc  quit"), footer);
}

#[cfg(test)]
mod tests {
    use super::render;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn render_is_deterministic() {
        let backend = TestBackend::new(32, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(render).unwrap();
        let lines = terminal
            .backend()
            .buffer()
            .content()
            .chunks(32)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![
                format!("┌ewtm{}┐", "─".repeat(26)),
                format!("│No worktrees loaded{}│", " ".repeat(11)),
                format!("│{}│", " ".repeat(30)),
                format!("└{}┘", "─".repeat(30)),
                format!("q / Esc  quit{}", " ".repeat(19)),
            ]
        );
    }
}
