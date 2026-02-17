use std::io;

use color_eyre::eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};

use crate::net::protocol::Direction;

/// Render the visual monitor arrangement layout.
///
/// Selection indices: 0=left, 1=right, 2=top, 3=bottom.
pub fn render_edge_layout(frame: &mut Frame, area: Rect, selection: usize) {
    let sel = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let this = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);

    let top = if selection == 2 { sel } else { dim };
    let bot = if selection == 3 { sel } else { dim };
    let lft = if selection == 0 { sel } else { dim };
    let rht = if selection == 1 { sel } else { dim };

    // Padding to align center/top/bottom boxes with the center column
    let pad = "                "; // 16 spaces
    let gap = "   ";              // 3 spaces between boxes
    let ind = "  ";               // 2 spaces indent for left box

    let lines = vec![
        Line::from(""),
        Line::from("  Where is the other machine's screen relative to yours?"),
        Line::from(""),
        // Top box
        Line::from(Span::styled(format!("{pad}┌─────────┐"), top)),
        Line::from(Span::styled(format!("{pad}│  Other  │"), top)),
        Line::from(Span::styled(format!("{pad}└─────────┘"), top)),
        // Middle row
        Line::from(vec![
            Span::styled(format!("{ind}┌─────────┐"), lft),
            Span::raw(gap),
            Span::styled("┌─────────┐", this),
            Span::raw(gap),
            Span::styled("┌─────────┐", rht),
        ]),
        Line::from(vec![
            Span::styled(format!("{ind}│  Other  │"), lft),
            Span::raw(gap),
            Span::styled("│  This   │", this),
            Span::raw(gap),
            Span::styled("│  Other  │", rht),
        ]),
        Line::from(vec![
            Span::styled(format!("{ind}└─────────┘"), lft),
            Span::raw(gap),
            Span::styled("└─────────┘", this),
            Span::raw(gap),
            Span::styled("└─────────┘", rht),
        ]),
        // Bottom box
        Line::from(Span::styled(format!("{pad}┌─────────┐"), bot)),
        Line::from(Span::styled(format!("{pad}│  Other  │"), bot)),
        Line::from(Span::styled(format!("{pad}└─────────┘"), bot)),
        Line::from(""),
        Line::from(Span::styled(
            "  Use arrow keys to select, Enter to confirm.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Screen Arrangement "));
    frame.render_widget(paragraph, area);
}

/// Standalone fullscreen TUI for picking the screen edge.
///
/// Returns the chosen `Direction`. Esc/q cancels with an error.
pub fn pick_edge() -> Result<Direction> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_picker(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

fn run_picker(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<Direction> {
    let mut selection: usize = 1; // default: right

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(ratatui::layout::Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(3),
                ])
                .split(area);

            let header = Block::default()
                .title(" Nexdesk - Screen Setup ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            frame.render_widget(header, chunks[0]);

            render_edge_layout(frame, chunks[1], selection);

            let footer =
                Paragraph::new(" ←↑↓→ Select edge | Enter: Confirm | q/Esc: Cancel ")
                    .alignment(Alignment::Center)
                    .block(Block::default().borders(Borders::ALL));
            frame.render_widget(footer, chunks[2]);
        })?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        return Err(color_eyre::eyre::eyre!("Edge selection cancelled"));
                    }
                    KeyCode::Left => selection = 0,
                    KeyCode::Right => selection = 1,
                    KeyCode::Up => selection = 2,
                    KeyCode::Down => selection = 3,
                    KeyCode::Enter => {
                        return Ok(match selection {
                            0 => Direction::Left,
                            1 => Direction::Right,
                            2 => Direction::Up,
                            _ => Direction::Down,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
}
