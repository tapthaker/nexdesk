use super::app::SetupState;

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
    pub fn next(self, role: Option<&str>) -> Self {
        match self {
            Self::Welcome => Self::Role,
            Self::Role => match role {
                Some("server") => Self::Screens,
                _ => Self::Network,
            },
            Self::Network | Self::Screens => Self::Certificates,
            Self::Certificates => {
                if cfg!(target_os = "macos") {
                    Self::Permissions
                } else {
                    Self::Service
                }
            }
            Self::Permissions => Self::Service,
            Self::Service => Self::Done,
            Self::Done => Self::Done,
        }
    }

    pub fn prev(self, role: Option<&str>) -> Self {
        match self {
            Self::Welcome => Self::Welcome,
            Self::Role => Self::Welcome,
            Self::Network | Self::Screens => Self::Role,
            Self::Certificates => match role {
                Some("server") => Self::Screens,
                _ => Self::Network,
            },
            Self::Permissions => Self::Certificates,
            Self::Service => {
                if cfg!(target_os = "macos") {
                    Self::Permissions
                } else {
                    Self::Certificates
                }
            }
            Self::Done => Self::Service,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Role => "Role Selection",
            Self::Network => "Network Configuration",
            Self::Screens => "Screen Arrangement",
            Self::Certificates => "Certificate Setup",
            Self::Permissions => "Permissions",
            Self::Service => "Install Service",
            Self::Done => "Complete",
        }
    }

    pub fn number(self) -> usize {
        match self {
            Self::Welcome => 1,
            Self::Role => 2,
            Self::Network | Self::Screens => 3,
            Self::Certificates => 4,
            Self::Permissions => 5,
            Self::Service => {
                if cfg!(target_os = "macos") {
                    6
                } else {
                    5
                }
            }
            Self::Done => {
                if cfg!(target_os = "macos") {
                    7
                } else {
                    6
                }
            }
        }
    }

    pub fn total_steps() -> usize {
        if cfg!(target_os = "macos") {
            6
        } else {
            5
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupAction {
    Quit,
    Next,
    Up,
    Down,
    Left,
    Right,
    RefreshDiscovery,
    ToggleNetworkMode,
    EnterCharacter(char),
    DeleteCharacter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupEffect {
    None,
    Exit,
    ApplyAndAdvance,
    RefreshDiscovery,
}

/// Apply one semantic setup action without terminal or platform I/O.
pub fn reduce(state: &mut SetupState, action: SetupAction) -> SetupEffect {
    match action {
        SetupAction::Quit => SetupEffect::Exit,
        SetupAction::Next => {
            if state.step == Step::Done {
                SetupEffect::Exit
            } else {
                SetupEffect::ApplyAndAdvance
            }
        }
        SetupAction::Right => {
            if state.step == Step::Screens {
                state.edge_selection = 1;
                SetupEffect::None
            } else {
                SetupEffect::ApplyAndAdvance
            }
        }
        SetupAction::Left => {
            if state.step == Step::Screens {
                state.edge_selection = 0;
            } else {
                state.step = state.step.prev(state.config.role.as_deref());
            }
            SetupEffect::None
        }
        SetupAction::Up => {
            match state.step {
                Step::Role => {
                    state.role_selection = state.role_selection.saturating_sub(1);
                }
                Step::Network if state.use_discovery => {
                    state.peer_selection = state.peer_selection.saturating_sub(1);
                }
                Step::Screens => state.edge_selection = 2,
                _ => {}
            }
            SetupEffect::None
        }
        SetupAction::Down => {
            match state.step {
                Step::Role => state.role_selection = (state.role_selection + 1).min(1),
                Step::Network if state.use_discovery && !state.discovered_peers.is_empty() => {
                    state.peer_selection =
                        (state.peer_selection + 1).min(state.discovered_peers.len() - 1);
                }
                Step::Screens => state.edge_selection = 3,
                _ => {}
            }
            SetupEffect::None
        }
        SetupAction::RefreshDiscovery if state.step == Step::Network && state.use_discovery => {
            SetupEffect::RefreshDiscovery
        }
        SetupAction::EnterCharacter(character)
            if state.step == Step::Network && !state.use_discovery =>
        {
            state.manual_addr.push(character);
            SetupEffect::None
        }
        SetupAction::DeleteCharacter => {
            if state.step == Step::Network && !state.use_discovery {
                state.manual_addr.pop();
            } else {
                state.step = state.step.prev(state.config.role.as_deref());
            }
            SetupEffect::None
        }
        SetupAction::ToggleNetworkMode if state.step == Step::Network => {
            state.use_discovery = !state.use_discovery;
            SetupEffect::None
        }
        SetupAction::RefreshDiscovery
        | SetupAction::ToggleNetworkMode
        | SetupAction::EnterCharacter(_) => SetupEffect::None,
    }
}

pub fn advance_after_apply(state: &mut SetupState) {
    state.step = state.step.next(state.config.role.as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_paths_and_back_navigation_are_pure() {
        assert_eq!(Step::Role.next(Some("server")), Step::Screens);
        assert_eq!(Step::Role.next(Some("client")), Step::Network);
        assert_eq!(Step::Certificates.prev(Some("server")), Step::Screens);
        assert_eq!(Step::Certificates.prev(Some("client")), Step::Network);
    }
}
