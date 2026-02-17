use ratatui::{prelude::*, widgets::*};

use super::app::SetupState;

pub fn render(frame: &mut Frame, area: Rect, state: &SetupState) {
    let mut lines = vec![
        Line::from(""),
        Line::from("  Network Configuration"),
        Line::from(""),
    ];

    if state.use_discovery {
        lines.push(Line::from(Span::styled(
            "  ▸ Auto-discover peers via mDNS",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from("    Manual IP entry"));
        lines.push(Line::from(""));

        if state.discovered_peers.is_empty() {
            lines.push(Line::from(Span::styled(
                "  Searching for peers...",
                Style::default().fg(Color::Yellow),
            )));
        } else {
            lines.push(Line::from("  Discovered peers:"));
            for peer in &state.discovered_peers {
                lines.push(Line::from(format!("    - {}", peer)));
            }
        }
    } else {
        lines.push(Line::from("    Auto-discover peers via mDNS"));
        lines.push(Line::from(Span::styled(
            "  ▸ Manual IP entry",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "  Server address: {}▏",
            state.manual_addr
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Tab to switch mode, Enter to continue.",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Network "));
    frame.render_widget(paragraph, area);
}
