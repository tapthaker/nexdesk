use ratatui::{prelude::*, widgets::*};

use super::app::SetupState;

pub fn render(frame: &mut Frame, area: Rect, state: &SetupState) {
    let mut lines = vec![
        Line::from(""),
        Line::from("  Accessibility Permission"),
        Line::from(""),
    ];

    if state.accessibility_granted {
        lines.push(Line::from(Span::styled(
            "  Accessibility access granted!",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from("  Nexdesk can capture and inject input events."));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Press Enter to continue.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  Waiting for Accessibility permission...",
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from("  Nexdesk needs Accessibility access to capture and inject"));
        lines.push(Line::from("  keyboard and mouse events."));
        lines.push(Line::from(""));
        lines.push(Line::from("  A system dialog should have appeared. If not:"));
        lines.push(Line::from(Span::styled(
            "    1. Open System Settings > Privacy & Security > Accessibility",
            Style::default().fg(Color::Cyan),
        )));
        lines.push(Line::from(Span::styled(
            "    2. Enable the toggle for nexdesk (or your terminal app)",
            Style::default().fg(Color::Cyan),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  This screen will update automatically once granted.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Permissions "));
    frame.render_widget(paragraph, area);
}
