use ratatui::{prelude::*, widgets::*};

use super::app::SetupState;

pub fn render(frame: &mut Frame, area: Rect, state: &SetupState) {
    let mut lines = vec![
        Line::from(""),
        Line::from("  Certificate Setup"),
        Line::from(""),
    ];

    if let Some(fp) = &state.fingerprint {
        lines.push(Line::from(Span::styled(
            "  Certificate generated!",
            Style::default().fg(Color::Green),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from("  Your fingerprint:"));
        lines.push(Line::from(Span::styled(
            format!("  {}", fp),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(
            "  Share this fingerprint with peers to verify identity.",
        ));
    } else {
        lines.push(Line::from(
            "  A self-signed TLS certificate will be generated for",
        ));
        lines.push(Line::from("  secure communication between your machines."));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Press Enter to generate the certificate.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Certificates "),
    );
    frame.render_widget(paragraph, area);
}
