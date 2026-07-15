use std::io;
#[cfg(unix)]
use std::io::IsTerminal;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use color_eyre::eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};

use crate::config::NexdeskConfig;
use crate::net::discovery::{BrowseHandle, DiscoveredPeer};

use super::{certificates, network, permissions, role, screens, service, welcome};

const MAX_MANUAL_ADDR_BYTES: usize = 512;

fn push_manual_addr_char(value: &mut String, ch: char) -> bool {
    if ch.is_control() || value.len().saturating_add(ch.len_utf8()) > MAX_MANUAL_ADDR_BYTES {
        return false;
    }
    value.push(ch);
    true
}

fn normalize_manual_addr(value: &str) -> Option<String> {
    if value.len() > MAX_MANUAL_ADDR_BYTES || value.chars().any(char::is_control) {
        return None;
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn service_client_addr_arg(addr: &str) -> Result<String> {
    normalize_manual_addr(addr).ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "Invalid configured server address {:?}. Run `nexdesk daemon setup` and choose a server address again.",
            crate::status::terminal_safe(addr, crate::status::MAX_STATUS_DISPLAY_BYTES)
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    Role,
    Network,
    Screens,
    Certificates,
    Permissions,
    Service,
    Done,
}

impl Step {
    fn next(self, role: Option<&str>) -> Self {
        match self {
            Step::Welcome => Step::Role,
            Step::Role => match role {
                Some("server") => Step::Screens,
                _ => Step::Network,
            },
            Step::Network => Step::Certificates,
            Step::Screens => Step::Certificates,
            Step::Certificates => {
                if cfg!(target_os = "macos") {
                    Step::Permissions
                } else {
                    Step::Service
                }
            }
            Step::Permissions => Step::Service,
            Step::Service => Step::Done,
            Step::Done => Step::Done,
        }
    }

    fn prev(self, role: Option<&str>) -> Self {
        match self {
            Step::Welcome => Step::Welcome,
            Step::Role => Step::Welcome,
            Step::Network => Step::Role,
            Step::Screens => Step::Role,
            Step::Certificates => match role {
                Some("server") => Step::Screens,
                _ => Step::Network,
            },
            Step::Permissions => Step::Certificates,
            Step::Service => {
                if cfg!(target_os = "macos") {
                    Step::Permissions
                } else {
                    Step::Certificates
                }
            }
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
            Step::Permissions => "Permissions",
            Step::Service => "Install Service",
            Step::Done => "Complete",
        }
    }

    fn number(self, _role: Option<&str>) -> usize {
        match self {
            Step::Welcome => 1,
            Step::Role => 2,
            Step::Network => 3,
            Step::Screens => 3,
            Step::Certificates => 4,
            Step::Permissions => 5,
            Step::Service => {
                if cfg!(target_os = "macos") {
                    6
                } else {
                    5
                }
            }
            Step::Done => {
                if cfg!(target_os = "macos") {
                    7
                } else {
                    6
                }
            }
        }
    }

    fn total_steps() -> usize {
        if cfg!(target_os = "macos") {
            6
        } else {
            5
        }
    }
}

pub struct SetupState {
    pub step: Step,
    pub config: NexdeskConfig,
    pub role_selection: usize,
    pub edge_selection: usize,
    pub discovered_peers: Vec<DiscoveredPeer>,
    pub peer_selection: usize,
    pub manual_addr: String,
    pub use_discovery: bool,
    pub service_installed: bool,
    pub fingerprint: Option<String>,
    pub accessibility_granted: bool,
    accessibility_prompted: bool,
    peer_receiver: Option<std::sync::mpsc::Receiver<DiscoveredPeer>>,
    _browse_handle: Option<BrowseHandle>,
}

struct TerminalCleanup {
    active: bool,
}

impl TerminalCleanup {
    fn armed() -> Self {
        Self { active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        if self.active {
            disable_raw_mode().ok();
            let mut stdout = io::stdout();
            execute!(stdout, LeaveAlternateScreen).ok();
        }
    }
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
            peer_selection: 0,
            manual_addr: String::new(),
            use_discovery: true,
            service_installed: false,
            fingerprint: None,
            accessibility_granted: crate::input::is_accessibility_granted(),
            accessibility_prompted: false,
            peer_receiver: None,
            _browse_handle: None,
        })
    }
}

pub async fn run() -> Result<()> {
    // When invoked via `curl | sh`, stdin may be a pipe or /dev/tty opened
    // read-only by the shell's `<` redirect. On macOS, kqueue returns EINVAL
    // for /dev/tty fds, so we resolve the actual PTY device path (e.g.
    // /dev/ttys001) via ttyname() on stdout/stderr and reopen that O_RDWR.
    // We keep _tty_guard alive so the fd isn't closed.
    #[cfg(unix)]
    let _tty_guard = {
        let stdin_fd = io::stdin().as_raw_fd();
        let flags = unsafe { libc::fcntl(stdin_fd, libc::F_GETFL) };
        let need_reopen = !io::stdin().is_terminal()
            || (flags >= 0 && (flags & libc::O_ACCMODE == libc::O_RDONLY));
        if need_reopen {
            // Try to get the actual PTY device path via ttyname on stdin,
            // stdout, or stderr. On macOS, kqueue returns EINVAL for /dev/tty
            // fds, so we need the real PTY device (e.g. /dev/ttys001).
            let tty_path = unsafe {
                let mut path: Option<String> = None;
                for fd in [stdin_fd, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                    let name = libc::ttyname(fd);
                    if !name.is_null() {
                        let s = std::ffi::CStr::from_ptr(name)
                            .to_string_lossy()
                            .into_owned();
                        if s != "/dev/tty" {
                            path = Some(s);
                            break;
                        }
                    }
                }
                path.unwrap_or_else(|| "/dev/tty".to_string())
            };
            let tty = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&tty_path)?;
            unsafe {
                libc::dup2(tty.as_raw_fd(), libc::STDIN_FILENO);
            }
            Some(tty)
        } else {
            None
        }
    };

    enable_raw_mode()?;
    let mut terminal_cleanup = TerminalCleanup::armed();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = SetupState::new()?;

    loop {
        // On the Permissions step, trigger the prompt once and poll for status
        if state.step == Step::Permissions {
            if !state.accessibility_prompted {
                state.accessibility_prompted = true;
                state.accessibility_granted = crate::input::request_accessibility();
            } else if !state.accessibility_granted {
                state.accessibility_granted = crate::input::is_accessibility_granted();
            }
        }

        // Drain any newly discovered peers from the mDNS browse task
        if let Some(rx) = &state.peer_receiver {
            while let Ok(peer) = rx.try_recv() {
                if !state.discovered_peers.iter().any(|p| p.addr == peer.addr) {
                    state.discovered_peers.push(peer);
                }
            }
        }

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
            let role = state.config.role.as_deref();
            let title = if state.step == Step::Done {
                " Nexdesk Setup - Complete ".to_string()
            } else {
                format!(
                    " Nexdesk Setup - Step {}/{}: {} ",
                    state.step.number(role),
                    Step::total_steps(),
                    state.step.title()
                )
            };
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
                Step::Permissions => permissions::render(frame, content_area, &state),
                Step::Service => service::render(frame, content_area, &state),
                Step::Done => render_done(frame, content_area),
            };

            // Footer
            let nav = if state.step == Step::Done {
                " Press 'q' to exit "
            } else if state.step == Step::Screens {
                " ←↑↓→ Select edge | Enter: Next | Backspace: Back | q: Quit "
            } else if state.step == Step::Network {
                " ↑/↓ Select peer | Tab: Switch mode | Enter: Next | Backspace: Back | q: Quit "
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
                    KeyCode::Enter => {
                        if state.step == Step::Done {
                            break;
                        }
                        apply_step_with_terminal(&mut terminal, &mut state).await?;
                        let role = state.config.role.as_deref().map(String::from);
                        state.step = state.step.next(role.as_deref());
                    }
                    KeyCode::Right => {
                        if state.step == Step::Screens {
                            state.edge_selection = 1; // right
                        } else {
                            apply_step_with_terminal(&mut terminal, &mut state).await?;
                            let role = state.config.role.as_deref().map(String::from);
                            state.step = state.step.next(role.as_deref());
                        }
                    }
                    KeyCode::Left => {
                        if state.step == Step::Screens {
                            state.edge_selection = 0; // left
                        } else {
                            let role = state.config.role.as_deref().map(String::from);
                            state.step = state.step.prev(role.as_deref());
                        }
                    }
                    KeyCode::Up => {
                        match state.step {
                            Step::Role => {
                                state.role_selection = state.role_selection.saturating_sub(1);
                            }
                            Step::Network if state.use_discovery => {
                                state.peer_selection = state.peer_selection.saturating_sub(1);
                            }
                            Step::Screens => {
                                state.edge_selection = 2; // top
                            }
                            _ => {}
                        }
                    }
                    KeyCode::Down => {
                        match state.step {
                            Step::Role => {
                                state.role_selection = (state.role_selection + 1).min(1);
                            }
                            Step::Network
                                if state.use_discovery && !state.discovered_peers.is_empty() =>
                            {
                                state.peer_selection = (state.peer_selection + 1)
                                    .min(state.discovered_peers.len() - 1);
                            }
                            Step::Screens => {
                                state.edge_selection = 3; // bottom
                            }
                            _ => {}
                        }
                    }
                    KeyCode::Char(c) => {
                        if state.step == Step::Network && !state.use_discovery {
                            push_manual_addr_char(&mut state.manual_addr, c);
                        }
                    }
                    KeyCode::Backspace => {
                        if state.step == Step::Network && !state.use_discovery {
                            state.manual_addr.pop();
                        } else {
                            let role = state.config.role.as_deref().map(String::from);
                            state.step = state.step.prev(role.as_deref());
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
    terminal_cleanup.disarm();

    Ok(())
}

async fn apply_step_with_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut SetupState,
) -> Result<()> {
    let suspend_tui = state.step == Step::Service && state.config.role.as_deref() != Some("server");

    if suspend_tui {
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    }

    let result = apply_step(state).await;

    if suspend_tui {
        execute!(terminal.backend_mut(), EnterAlternateScreen)?;
        enable_raw_mode()?;
        terminal.clear()?;
    }

    result
}

fn service_args_for_config(config: &NexdeskConfig) -> Result<Vec<String>> {
    match config.role.as_deref() {
        Some("server") => Ok(vec![
            "serve".to_string(),
            "--port".to_string(),
            if config.port == 0 { 4242 } else { config.port }.to_string(),
        ]),
        Some("client") => Ok(match &config.server_addr {
            Some(addr) => vec!["connect".to_string(), service_client_addr_arg(addr)?],
            None => vec!["connect".to_string()],
        }),
        Some(role) => Err(color_eyre::eyre::eyre!(
            "Invalid configured role {:?}. Run `nexdesk daemon setup` and choose server or client.",
            crate::status::terminal_safe(role, crate::status::MAX_STATUS_DISPLAY_BYTES)
        )),
        None => Err(color_eyre::eyre::eyre!(
            "No role configured. Run `nexdesk daemon setup` and choose server or client."
        )),
    }
}

async fn apply_step(state: &mut SetupState) -> Result<()> {
    match state.step {
        Step::Role => {
            state.config.role = Some(
                if state.role_selection == 0 {
                    "server"
                } else {
                    "client"
                }
                .to_string(),
            );
            // Stop any previous discovery
            state._browse_handle = None;
            state.peer_receiver = None;
            state.discovered_peers.clear();
            // Start mDNS discovery when role is client
            if state.role_selection == 1 {
                match crate::net::discovery::start_browsing() {
                    Ok((rx, handle)) => {
                        state.peer_receiver = Some(rx);
                        state._browse_handle = Some(handle);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to start peer discovery: {}", e);
                    }
                }
            }
        }
        Step::Network => {
            if state.use_discovery {
                if let Some(peer) = state.discovered_peers.get(state.peer_selection) {
                    state.config.server_addr = Some(peer.addr.to_string());
                }
            } else {
                state.config.server_addr = normalize_manual_addr(&state.manual_addr);
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
            let arg_values: Vec<String> = service_args_for_config(&state.config)?;
            let args: Vec<&str> = arg_values.iter().map(String::as_str).collect();
            state.config.save()?;
            if state.config.role.as_deref() != Some("server") {
                if let Some(addr) = state.config.server_addr.as_deref() {
                    crate::net::quic::pair(addr).await?;
                }
            }
            crate::daemon::install_service(&args)?;
            state.service_installed = true;
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
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Your configuration has been saved."),
        Line::from("  Nexdesk is ready to use."),
        Line::from(""),
        Line::from("  Run 'nexdesk serve' or 'nexdesk connect <peer>' to get started."),
    ];
    let paragraph =
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Done "));
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_address_input_is_bounded_and_control_free() {
        let mut value = String::new();
        assert!(push_manual_addr_char(&mut value, 'a'));
        assert!(!push_manual_addr_char(&mut value, '\n'));
        assert_eq!(value, "a");

        value = "x".repeat(MAX_MANUAL_ADDR_BYTES);
        assert!(!push_manual_addr_char(&mut value, 'y'));
        assert_eq!(value.len(), MAX_MANUAL_ADDR_BYTES);
    }

    #[test]
    fn manual_address_is_trimmed_before_config_persistence() {
        assert_eq!(
            normalize_manual_addr("  example.local:4242  ").as_deref(),
            Some("example.local:4242")
        );
        assert_eq!(normalize_manual_addr("   "), None);
        assert_eq!(normalize_manual_addr("host:4242\n"), None);
        assert_eq!(
            normalize_manual_addr(&"x".repeat(MAX_MANUAL_ADDR_BYTES + 1)),
            None
        );
    }

    #[test]
    fn service_arguments_require_an_explicit_valid_role() {
        let mut config = NexdeskConfig {
            role: Some("server".into()),
            port: 5555,
            ..Default::default()
        };
        assert_eq!(
            service_args_for_config(&config).unwrap(),
            vec!["serve", "--port", "5555"]
        );
        config.port = 0;
        assert_eq!(
            service_args_for_config(&config).unwrap(),
            vec!["serve", "--port", "4242"]
        );

        config.role = Some("client".into());
        config.server_addr = Some("127.0.0.1:4242".into());
        assert_eq!(
            service_args_for_config(&config).unwrap(),
            vec!["connect", "127.0.0.1:4242"]
        );

        config.server_addr = Some("host:4242\n".into());
        assert!(service_args_for_config(&config).is_err());

        config.role = None;
        assert!(service_args_for_config(&config).is_err());
        config.role = Some("invalid\x1b[31m".into());
        let err = service_args_for_config(&config).unwrap_err().to_string();
        assert!(err.contains("Invalid configured role"));
        assert!(!err.contains('\u{1b}'));
    }
}
