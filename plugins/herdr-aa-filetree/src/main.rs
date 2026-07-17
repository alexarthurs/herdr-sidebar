use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::widgets::{Block, Paragraph};

fn main() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let result = run(&mut terminal);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
    loop {
        terminal.draw(|frame| {
            let body = Paragraph::new(
                "herdr-aa-filetree — VS Code-style file explorer for herdr.\n\
                 \n\
                 Not implemented yet. Press q to quit.",
            )
            .block(Block::bordered().title(" Explorer "));
            frame.render_widget(body, frame.area());
        })?;
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press
                && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            {
                return Ok(());
            }
        }
    }
}
