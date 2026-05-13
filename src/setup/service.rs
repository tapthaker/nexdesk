use ratatui::{prelude::*, widgets::*};

use super::app::SetupState;

pub fn render(frame: &mut Frame, area: Rect, state: &SetupState) {
    let mut lines = vec![
        Line::from(""),
        Line::from("  Install as Background Service"),
        Line::from(""),
    ];

    if state.service_installed {
        lines.push(Line::from(Span::styled(
            "  Service installed successfully!",
            Style::default().fg(Color::Green),
        )));
        lines.push(Line::from(""));

        #[cfg(target_os = "linux")]
        {
            lines.push(Line::from("  The service is enabled and running."));
        }
        #[cfg(target_os = "macos")]
        {
            lines.push(Line::from(
                "  The service will start automatically on login.",
            ));
        }
    } else {
        let platform = if cfg!(target_os = "macos") {
            "macOS LaunchAgent"
        } else if cfg!(target_os = "linux") {
            "systemd user service"
        } else {
            "background service"
        };

        lines.push(Line::from(format!(
            "  Nexdesk will be installed as a {}.",
            platform
        )));
        lines.push(Line::from("  It will start automatically on login."));
        #[cfg(target_os = "linux")]
        if state.config.role.as_deref() == Some("server") {
            lines.push(Line::from("  If UFW is active, setup will allow UDP 4242"));
            lines.push(Line::from("  when passwordless sudo is available."));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Press Enter to install and save configuration.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Service Installation "),
    );
    frame.render_widget(paragraph, area);
}
