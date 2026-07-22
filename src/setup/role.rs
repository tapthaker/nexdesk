use ratatui::{prelude::*, widgets::*};

use super::app::SetupState;

pub fn render(frame: &mut Frame, area: Rect, state: &SetupState) {
    let roles = [
        (
            "Server",
            "Captures input and sends it to clients. Run this on your primary machine.",
        ),
        (
            "Client",
            "Receives input from the server. Run this on secondary machines.",
        ),
    ];

    let mut lines = vec![
        Line::from(""),
        Line::from("  Select this machine's role:"),
        Line::from(""),
    ];

    for (i, (name, desc)) in roles.iter().enumerate() {
        let marker = if i == state.role_selection {
            "▸ "
        } else {
            "  "
        };
        let style = if i == state.role_selection {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        lines.push(Line::from(Span::styled(
            format!("  {} {}", marker, name),
            style,
        )));
        lines.push(Line::from(Span::styled(
            format!("      {}", desc),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));
    }

    lines.push(Line::from(Span::styled(
        "  Use ↑/↓ to select, Enter to confirm.",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Role Selection "),
    );
    frame.render_widget(paragraph, area);
}
