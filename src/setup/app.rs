use std::io;

use color_eyre::eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};

use crate::config::NexdeskConfig;

use super::{welcome, role, network, screens, certificates, service};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    Role,
    Network,
    Screens,
    Certificates,
    Service,
    Done,
}

impl Step {
    fn next(self) -> Self {
        match self {
            Step::Welcome => Step::Role,
            Step::Role => Step::Network,
            Step::Network => Step::Screens,
            Step::Screens => Step::Certificates,
            Step::Certificates => Step::Service,
            Step::Service => Step::Done,
            Step::Done => Step::Done,
        }
    }

    fn prev(self) -> Self {
        match self {
            Step::Welcome => Step::Welcome,
            Step::Role => Step::Welcome,
            Step::Network => Step::Role,
            Step::Screens => Step::Network,
            Step::Certificates => Step::Screens,
            Step::Service => Step::Certificates,
            Step::Done => Step::Service,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Welcome",
            Step::Role => "Role Selection",
            Step::Network => "Network Configuration",
            Step::Screens => "Screen Arrangement",
            Step::Certificates => "Certificate Setup",
            Step::Service => "Install Service",
            Step::Done => "Complete",
        }
    }

    fn number(self) -> usize {
        match self {
            Step::Welcome => 1,
            Step::Role => 2,
            Step::Network => 3,
            Step::Screens => 4,
            Step::Certificates => 5,
            Step::Service => 6,
            Step::Done => 7,
        }
    }
}

pub struct SetupState {
    pub step: Step,
    pub config: NexdeskConfig,
    pub role_selection: usize,
    pub edge_selection: usize,
    pub discovered_peers: Vec<String>,
    pub manual_addr: String,
    pub use_discovery: bool,
    pub service_installed: bool,
    pub fingerprint: Option<String>,
}

impl SetupState {
    fn new() -> Result<Self> {
        let config = NexdeskConfig::load()?;
        Ok(Self {
            step: Step::Welcome,
            config,
            role_selection: 0,
            edge_selection: 1, // default: right
            discovered_peers: Vec::new(),
            manual_addr: String::new(),
            use_discovery: true,
            service_installed: false,
            fingerprint: None,
        })
    }
}

pub async fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = SetupState::new()?;

    loop {
        terminal.draw(|frame| {
            let area = frame.area();

            // Header
            let chunks = Layout::default()
                .direction(ratatui::layout::Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(3),
                ])
                .split(area);

            // Title bar
            let title = format!(
                " Nexdesk Setup - Step {}/6: {} ",
                state.step.number(),
                state.step.title()
            );
            let header = Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            frame.render_widget(header, chunks[0]);

            // Content
            let content_area = chunks[1];
            match state.step {
                Step::Welcome => welcome::render(frame, content_area),
                Step::Role => role::render(frame, content_area, &state),
                Step::Network => network::render(frame, content_area, &state),
                Step::Screens => screens::render(frame, content_area, &state),
                Step::Certificates => certificates::render(frame, content_area, &state),
                Step::Service => service::render(frame, content_area, &state),
                Step::Done => render_done(frame, content_area),
            };

            // Footer
            let nav = if state.step == Step::Done {
                " Press 'q' to exit "
            } else {
                " ←/→ Navigate | Enter: Next | q: Quit "
            };
            let footer = Paragraph::new(nav)
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
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Right | KeyCode::Enter => {
                        if state.step == Step::Done {
                            break;
                        }
                        // Apply current step
                        apply_step(&mut state)?;
                        state.step = state.step.next();
                    }
                    KeyCode::Left => {
                        state.step = state.step.prev();
                    }
                    KeyCode::Up => {
                        match state.step {
                            Step::Role => {
                                state.role_selection = state.role_selection.saturating_sub(1);
                            }
                            Step::Screens => {
                                state.edge_selection = state.edge_selection.saturating_sub(1);
                            }
                            _ => {}
                        }
                    }
                    KeyCode::Down => {
                        match state.step {
                            Step::Role => {
                                state.role_selection = (state.role_selection + 1).min(1);
                            }
                            Step::Screens => {
                                state.edge_selection = (state.edge_selection + 1).min(3);
                            }
                            _ => {}
                        }
                    }
                    KeyCode::Char(c) => {
                        if state.step == Step::Network && !state.use_discovery {
                            state.manual_addr.push(c);
                        }
                    }
                    KeyCode::Backspace => {
                        if state.step == Step::Network && !state.use_discovery {
                            state.manual_addr.pop();
                        }
                    }
                    KeyCode::Tab => {
                        if state.step == Step::Network {
                            state.use_discovery = !state.use_discovery;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    Ok(())
}

fn apply_step(state: &mut SetupState) -> Result<()> {
    match state.step {
        Step::Role => {
            state.config.role = Some(
                if state.role_selection == 0 { "server" } else { "client" }.to_string(),
            );
        }
        Step::Network => {
            if !state.use_discovery && !state.manual_addr.is_empty() {
                state.config.server_addr = Some(state.manual_addr.clone());
            }
        }
        Step::Screens => {
            let edge = match state.edge_selection {
                0 => "left",
                1 => "right",
                2 => "top",
                3 => "bottom",
                _ => "right",
            };
            state.config.switch_edge = Some(edge.to_string());
        }
        Step::Certificates => {
            let (cert, _) = crate::net::tls::load_or_generate_certs()?;
            state.fingerprint = Some(crate::net::tls::fingerprint(&cert));
        }
        Step::Service => {
            if let Err(e) = crate::daemon::install_service() {
                tracing::warn!("Failed to install service: {}", e);
            } else {
                state.service_installed = true;
            }
            state.config.save()?;
        }
        _ => {}
    }
    Ok(())
}

fn render_done(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Setup complete!",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Your configuration has been saved."),
        Line::from("  Nexdesk is ready to use."),
        Line::from(""),
        Line::from("  Run 'nexdesk serve' or 'nexdesk connect <peer>' to get started."),
    ];
    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" Done "));
    frame.render_widget(paragraph, area);
}
