use std::io;
#[cfg(unix)]
use std::io::IsTerminal;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};

use crate::config::NexdeskConfig;
use crate::net::discovery::{BrowseHandle, DiscoveredPeer};

use super::flow::{advance_after_apply, reduce, SetupAction, SetupEffect, Step};
use super::{certificates, network, permissions, role, screens, service, welcome};

const DISCOVERY_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

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
    discovery_started_at: Option<Instant>,
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
            discovery_started_at: None,
        })
    }
}

fn restart_peer_browsing(state: &mut SetupState) {
    state._browse_handle = None;
    state.peer_receiver = None;
    state.discovered_peers.clear();
    state.peer_selection = 0;
    state.discovery_started_at = Some(Instant::now());
    match crate::net::discovery::start_browsing() {
        Ok((receiver, handle)) => {
            state.peer_receiver = Some(receiver);
            state._browse_handle = Some(handle);
        }
        Err(error) => tracing::warn!("Failed to refresh peer discovery: {}", error),
    }
}

fn discovery_refresh_due(state: &SetupState, now: Instant) -> bool {
    state.step == Step::Network
        && state.use_discovery
        && state.discovered_peers.is_empty()
        && state.discovery_started_at.is_some_and(|started| {
            now.saturating_duration_since(started) >= DISCOVERY_REFRESH_INTERVAL
        })
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
        let need_reopen = if !io::stdin().is_terminal() {
            true
        } else if flags >= 0 && (flags & libc::O_ACCMODE == libc::O_RDONLY) {
            true
        } else {
            false
        };
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
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = SetupState::new()?;

    loop {
        if discovery_refresh_due(&state, Instant::now()) {
            restart_peer_browsing(&mut state);
        }

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
                merge_discovered_peer(&mut state.discovered_peers, peer);
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
            let title = if state.step == Step::Done {
                " Nexdesk Setup - Complete ".to_string()
            } else {
                format!(
                    " Nexdesk Setup - Step {}/{}: {} ",
                    state.step.number(),
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
                " ↑/↓ Select | R: Refresh | Tab: Switch mode | Enter: Next | Backspace: Back | q: Quit "
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
                let action = match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => SetupAction::Quit,
                    KeyCode::Enter => SetupAction::Next,
                    KeyCode::Right => SetupAction::Right,
                    KeyCode::Left => SetupAction::Left,
                    KeyCode::Up => SetupAction::Up,
                    KeyCode::Down => SetupAction::Down,
                    KeyCode::Char('r') | KeyCode::Char('R')
                        if state.step == Step::Network && state.use_discovery =>
                    {
                        SetupAction::RefreshDiscovery
                    }
                    KeyCode::Char(character) => SetupAction::EnterCharacter(character),
                    KeyCode::Backspace => SetupAction::DeleteCharacter,
                    KeyCode::Tab => SetupAction::ToggleNetworkMode,
                    _ => continue,
                };
                match reduce(&mut state, action) {
                    SetupEffect::None => {}
                    SetupEffect::Exit => break,
                    SetupEffect::RefreshDiscovery => restart_peer_browsing(&mut state),
                    SetupEffect::ApplyAndAdvance => {
                        apply_step_with_terminal(&mut terminal, &mut state).await?;
                        advance_after_apply(&mut state);
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

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

fn merge_discovered_peer(peers: &mut Vec<DiscoveredPeer>, peer: DiscoveredPeer) {
    if let Some(existing) = peers
        .iter_mut()
        .find(|existing| existing.fingerprint == peer.fingerprint)
    {
        *existing = peer;
    } else {
        peers.push(peer);
    }
}

fn apply_network_selection(state: &mut SetupState) -> Result<()> {
    if state.use_discovery {
        let peer = state
            .discovered_peers
            .get(state.peer_selection)
            .ok_or_else(|| eyre!("Select a discovered server before continuing"))?;
        state.config.server_addr = Some(peer.addr.to_string());
        state.config.server_fingerprint = Some(peer.fingerprint.clone());
    } else if state.manual_addr.is_empty() {
        return Err(eyre!("Enter a server address before continuing"));
    } else {
        state.config.server_addr = Some(state.manual_addr.clone());
        state.config.server_fingerprint = None;
    }
    Ok(())
}

fn service_arguments(state: &SetupState) -> Vec<String> {
    match state.config.role.as_deref() {
        Some("server") => vec!["serve".to_string()],
        _ if state.use_discovery => vec!["connect".to_string()],
        _ => match &state.config.server_addr {
            Some(addr) => vec!["connect".to_string(), addr.clone()],
            None => vec!["connect".to_string()],
        },
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
            // Start with a fresh mDNS daemon and multicast sockets when the
            // client enters discovery. The browser will refresh periodically
            // until a peer is found.
            if state.role_selection == 1 {
                restart_peer_browsing(state);
            } else {
                state._browse_handle = None;
                state.peer_receiver = None;
                state.discovered_peers.clear();
                state.discovery_started_at = None;
            }
        }
        Step::Network => apply_network_selection(state)?,
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
            if state.config.role.as_deref() != Some("server") {
                let addr = state
                    .config
                    .server_addr
                    .as_deref()
                    .ok_or_else(|| eyre!("No server selected"))?;
                let fingerprint =
                    crate::net::quic::pair(addr, state.config.server_fingerprint.as_deref())
                        .await?;
                state.config.server_fingerprint = Some(fingerprint);
            }
            // The daemon starts immediately during installation, so persist its
            // fingerprint target before allowing the service manager to launch it.
            state.config.save()?;
            let arguments = service_arguments(state);
            let args = arguments.iter().map(String::as_str).collect::<Vec<_>>();
            if let Err(e) = crate::daemon::install_service(&args) {
                tracing::warn!("Failed to install service: {}", e);
            } else {
                state.service_installed = true;
            }
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

    fn state() -> SetupState {
        SetupState {
            step: Step::Network,
            config: NexdeskConfig::default(),
            role_selection: 1,
            edge_selection: 1,
            discovered_peers: Vec::new(),
            peer_selection: 0,
            manual_addr: String::new(),
            use_discovery: true,
            service_installed: false,
            fingerprint: None,
            accessibility_granted: true,
            accessibility_prompted: false,
            peer_receiver: None,
            _browse_handle: None,
            discovery_started_at: None,
        }
    }

    #[test]
    fn empty_discovery_refreshes_on_a_bounded_interval() {
        let mut state = state();
        let started = Instant::now();
        state.discovery_started_at = Some(started);

        assert!(!discovery_refresh_due(
            &state,
            started + DISCOVERY_REFRESH_INTERVAL - Duration::from_millis(1)
        ));
        assert!(discovery_refresh_due(
            &state,
            started + DISCOVERY_REFRESH_INTERVAL
        ));
        state.discovered_peers.push(DiscoveredPeer {
            name: "desk".to_string(),
            platform: "linux".to_string(),
            addr: "192.0.2.10:4242".parse().unwrap(),
            fingerprint: "AA:BB".to_string(),
        });
        assert!(!discovery_refresh_due(
            &state,
            started + DISCOVERY_REFRESH_INTERVAL
        ));
    }

    #[test]
    fn discovered_identity_replaces_its_stale_address() {
        let mut peers = vec![DiscoveredPeer {
            name: "desk".to_string(),
            platform: "linux".to_string(),
            addr: "192.0.2.10:4242".parse().unwrap(),
            fingerprint: "AA:BB:CC".to_string(),
        }];
        merge_discovered_peer(
            &mut peers,
            DiscoveredPeer {
                name: "desk".to_string(),
                platform: "linux".to_string(),
                addr: "192.0.2.20:4242".parse().unwrap(),
                fingerprint: "AA:BB:CC".to_string(),
            },
        );

        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].addr, "192.0.2.20:4242".parse().unwrap());
    }

    #[test]
    fn discovery_selection_persists_identity_but_service_rediscovers_address() {
        let mut state = state();
        state.discovered_peers.push(DiscoveredPeer {
            name: "desk".to_string(),
            platform: "linux".to_string(),
            addr: "192.0.2.10:4242".parse().unwrap(),
            fingerprint: "AA:BB:CC".to_string(),
        });

        apply_network_selection(&mut state).unwrap();

        assert_eq!(state.config.server_addr.as_deref(), Some("192.0.2.10:4242"));
        assert_eq!(state.config.server_fingerprint.as_deref(), Some("AA:BB:CC"));
        assert_eq!(service_arguments(&state), ["connect"]);
    }

    #[test]
    fn manual_selection_pins_address_and_clears_discovered_identity() {
        let mut state = state();
        state.use_discovery = false;
        state.manual_addr = "192.0.2.20:4242".to_string();
        state.config.server_fingerprint = Some("OLD:FINGERPRINT".to_string());

        apply_network_selection(&mut state).unwrap();

        assert_eq!(state.config.server_fingerprint, None);
        assert_eq!(service_arguments(&state), ["connect", "192.0.2.20:4242"]);
    }

    #[test]
    fn network_selection_requires_a_peer_or_manual_address() {
        let mut state = state();
        assert!(apply_network_selection(&mut state).is_err());
        state.use_discovery = false;
        assert!(apply_network_selection(&mut state).is_err());
    }
}
