use ratatui::{prelude::*, widgets::*};

pub fn render(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Welcome to Nexdesk!",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Nexdesk lets you share your mouse, keyboard, and clipboard"),
        Line::from("  between macOS and Linux machines on your local network."),
        Line::from(""),
        Line::from("  This wizard will guide you through setting up nexdesk."),
        Line::from(""),
        Line::from(Span::styled(
            "  Press → or Enter to continue.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph =
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Welcome "));
    frame.render_widget(paragraph, area);
}
