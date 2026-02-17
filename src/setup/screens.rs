use ratatui::{prelude::*, widgets::*};

use super::app::SetupState;

pub fn render(frame: &mut Frame, area: Rect, state: &SetupState) {
    let edges = ["Left", "Right", "Top", "Bottom"];

    let mut lines = vec![
        Line::from(""),
        Line::from("  Which edge of this screen connects to the other machine?"),
        Line::from(""),
    ];

    for (i, edge) in edges.iter().enumerate() {
        let marker = if i == state.edge_selection { "▸ " } else { "  " };
        let style = if i == state.edge_selection {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        lines.push(Line::from(Span::styled(
            format!("  {} {}", marker, edge),
            style,
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from("  ┌─────────┐   ┌─────────┐"));
    lines.push(Line::from("  │         │───│         │"));
    lines.push(Line::from("  │  This   │   │  Other  │"));
    lines.push(Line::from("  │         │   │         │"));
    lines.push(Line::from("  └─────────┘   └─────────┘"));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Use ↑/↓ to select the edge, Enter to confirm.",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Screen Arrangement "));
    frame.render_widget(paragraph, area);
}
