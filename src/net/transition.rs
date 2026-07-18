use std::collections::HashSet;

use crate::cursor::edge;
use crate::net::protocol::{Direction, Message, ScreenLayout};

// --- Constants ---

const EDGE_DWELL_THRESHOLD: u32 = 50;
const SERVER_EDGE_COOLDOWN: u32 = 125;
const INSET: i32 = 20;
const CLIENT_EDGE_DWELL: u32 = 8;
const CLIENT_CURSOR_SYNC_MARGIN: i32 = 64;

fn lower_inset(len: i32) -> i32 {
    if len <= 0 {
        0
    } else {
        INSET.clamp(0, len - 1)
    }
}

fn upper_inset(len: i32) -> i32 {
    if len <= 0 {
        0
    } else {
        (len - 1 - INSET).clamp(0, len - 1)
    }
}

fn clamp_with_inset(value: i32, len: i32) -> i32 {
    if len <= 0 {
        return 0;
    }
    let lo = lower_inset(len);
    let hi = upper_inset(len);
    if lo <= hi {
        value.clamp(lo, hi)
    } else {
        value.clamp(0, len - 1)
    }
}

// Do not synthesize keyboard repeat in the transition state. Capture backends
// forward source-generated repeat events, preserving the configured delay/rate.
// A timer here would risk endless repeats if a key-up were missed during a
// switch-back or grab transition.

// Evdev keycodes for the safety escape combo (Ctrl+Alt+Escape)
const KEY_ESC: u32 = 1;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_RIGHTALT: u32 = 100;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_DOWN: u32 = 108;

// --- Server Transition ---

#[derive(Debug)]
pub enum ServerOutput {
    Idle,
    Activate {
        messages: Vec<Message>,
        grab: bool,
    },
    Forward {
        messages: Vec<Message>,
    },
    ShortcutRelease {
        messages: Vec<Message>,
    },
    /// Safety escape: Ctrl+Alt+Escape pressed, force-release grab
    ForceRelease {
        messages: Vec<Message>,
    },
}

pub struct ServerTransition {
    trigger_edge: Option<Direction>,
    peer_screen: ScreenLayout,
    active: bool,
    last_x: i32,
    last_y: i32,
    last_buttons: u8,
    edge_cooldown: u32,
    edge_dwell: u32,
    /// After reclaiming local control, require evidence that the pointer left
    /// the transfer edge before another handoff can begin.
    edge_armed: bool,
    pressed_keys: HashSet<u32>,
}

impl ServerTransition {
    pub fn new(trigger_edge: Option<Direction>, peer_screen: ScreenLayout) -> Self {
        Self {
            trigger_edge,
            peer_screen,
            active: false,
            last_x: 0,
            last_y: 0,
            last_buttons: 0,
            edge_cooldown: 0,
            edge_dwell: 0,
            edge_armed: true,
            pressed_keys: HashSet::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn edge_is_armed(&self) -> bool {
        self.edge_armed
    }

    pub fn rearm_edge(&mut self) {
        if !self.active {
            self.edge_armed = true;
            self.edge_dwell = 0;
        }
    }

    fn update_pressed_keys(&mut self, key_events: &[Message]) {
        for msg in key_events {
            if let Message::KeyEvent {
                keycode, pressed, ..
            } = msg
            {
                if *pressed {
                    self.pressed_keys.insert(*keycode);
                } else {
                    self.pressed_keys.remove(keycode);
                }
            }
        }
    }

    pub fn is_escape_combo(&self) -> bool {
        let has_ctrl =
            self.pressed_keys.contains(&KEY_LEFTCTRL) || self.pressed_keys.contains(&KEY_RIGHTCTRL);
        let has_alt =
            self.pressed_keys.contains(&KEY_LEFTALT) || self.pressed_keys.contains(&KEY_RIGHTALT);
        let has_esc = self.pressed_keys.contains(&KEY_ESC);
        has_ctrl && has_alt && has_esc
    }

    pub fn shortcut_direction(&self) -> Option<Direction> {
        let has_ctrl =
            self.pressed_keys.contains(&KEY_LEFTCTRL) || self.pressed_keys.contains(&KEY_RIGHTCTRL);
        let has_alt =
            self.pressed_keys.contains(&KEY_LEFTALT) || self.pressed_keys.contains(&KEY_RIGHTALT);
        let has_shift = self.pressed_keys.contains(&KEY_LEFTSHIFT)
            || self.pressed_keys.contains(&KEY_RIGHTSHIFT);

        if !(has_ctrl && has_alt && has_shift) {
            return None;
        }

        let direction = if self.pressed_keys.contains(&KEY_RIGHT) {
            Direction::Right
        } else if self.pressed_keys.contains(&KEY_LEFT) {
            Direction::Left
        } else if self.pressed_keys.contains(&KEY_UP) {
            Direction::Up
        } else if self.pressed_keys.contains(&KEY_DOWN) {
            Direction::Down
        } else {
            return None;
        };

        if self.trigger_edge.is_none_or(|edge| edge == direction) {
            Some(direction)
        } else {
            None
        }
    }

    fn release_pressed_keys(&mut self) -> Vec<Message> {
        let mut keys: Vec<u32> = self.pressed_keys.iter().copied().collect();
        keys.sort_unstable();
        self.pressed_keys.clear();

        keys.into_iter()
            .map(|keycode| Message::KeyEvent {
                keycode,
                pressed: false,
                modifiers: 0,
            })
            .collect()
    }

    /// Activate immediately (layer-shell: edge detection is instant, no dwell needed).
    pub fn activate_instant(&mut self, direction: Direction) -> Vec<Message> {
        self.active = true;
        self.last_buttons = 0;
        self.last_x = 0;
        self.last_y = 0;
        self.edge_dwell = 0;
        self.edge_cooldown = 0;

        let pw = self.peer_screen.width as i32;
        let ph = self.peer_screen.height as i32;
        let (rx, ry) = match direction {
            Direction::Right => (lower_inset(pw), ph / 2),
            Direction::Left => (upper_inset(pw), ph / 2),
            Direction::Down => (pw / 2, lower_inset(ph)),
            Direction::Up => (pw / 2, upper_inset(ph)),
        };

        vec![
            Message::SwitchScreen { direction },
            Message::MouseMove { x: rx, y: ry },
        ]
    }

    /// Update key state from a single event (for event-driven mode).
    pub fn update_key(&mut self, keycode: u32, pressed: bool) {
        if pressed {
            self.pressed_keys.insert(keycode);
        } else {
            self.pressed_keys.remove(&keycode);
        }
    }

    fn push_key_events(&mut self, key_events: Vec<Message>, messages: &mut Vec<Message>) {
        // Forward real press, release, and source-generated repeat key-downs.
        messages.extend(key_events);
    }

    pub fn poll_active_keys(&mut self, key_events: Vec<Message>) -> ServerOutput {
        if !self.active {
            return ServerOutput::Idle;
        }

        self.update_pressed_keys(&key_events);
        if self.is_escape_combo() {
            self.active = false;
            self.edge_armed = false;
            return ServerOutput::ForceRelease {
                messages: self.release_pressed_keys(),
            };
        }
        if self.shortcut_direction().is_some() {
            self.active = false;
            self.edge_armed = false;
            self.edge_cooldown = SERVER_EDGE_COOLDOWN;
            return ServerOutput::ShortcutRelease {
                messages: self.release_pressed_keys(),
            };
        }

        let mut messages = Vec::new();
        self.push_key_events(key_events, &mut messages);
        ServerOutput::Forward { messages }
    }

    pub fn poll(
        &mut self,
        mx: i32,
        my: i32,
        sw: u32,
        sh: u32,
        buttons: u8,
        key_events: Vec<Message>,
    ) -> ServerOutput {
        self.update_pressed_keys(&key_events);

        // Safety escape: Ctrl+Alt+Escape always force-releases
        if self.active && self.is_escape_combo() {
            self.active = false;
            self.edge_armed = false;
            return ServerOutput::ForceRelease {
                messages: self.release_pressed_keys(),
            };
        }

        if sw == 0 || sh == 0 {
            self.edge_dwell = 0;
            if self.active {
                self.active = false;
                self.edge_armed = false;
                return ServerOutput::ForceRelease {
                    messages: self.release_pressed_keys(),
                };
            }
            return ServerOutput::Idle;
        }

        if let Some(dir) = self.shortcut_direction() {
            if self.active {
                self.active = false;
                self.edge_armed = false;
                self.edge_cooldown = SERVER_EDGE_COOLDOWN;
                return ServerOutput::ShortcutRelease {
                    messages: self.release_pressed_keys(),
                };
            }

            if self.edge_cooldown == 0 {
                self.active = true;
                self.last_buttons = buttons;
                self.last_x = mx;
                self.last_y = my;
                self.edge_dwell = 0;

                let pw = self.peer_screen.width as i32;
                let ph = self.peer_screen.height as i32;
                let (rx, ry) = match dir {
                    Direction::Right => (lower_inset(pw), ph / 2),
                    Direction::Left => (upper_inset(pw), ph / 2),
                    Direction::Down => (pw / 2, lower_inset(ph)),
                    Direction::Up => (pw / 2, upper_inset(ph)),
                };

                return ServerOutput::Activate {
                    messages: vec![
                        Message::SwitchScreen { direction: dir },
                        Message::MouseMove { x: rx, y: ry },
                    ],
                    grab: true,
                };
            }
        }

        let clamped_x = mx.clamp(0, sw as i32 - 1);
        let clamped_y = my.clamp(0, sh as i32 - 1);

        if !self.active {
            let at_edge = edge::detect_edge(clamped_x, clamped_y, sw, sh)
                .filter(|d| self.trigger_edge.is_none_or(|e| *d == e));

            if !self.edge_armed {
                if at_edge.is_some() {
                    self.edge_dwell = 0;
                    return ServerOutput::Idle;
                }
                self.rearm_edge();
            }

            if self.edge_cooldown > 0 {
                self.edge_cooldown -= 1;
                self.edge_dwell = 0;
                return ServerOutput::Idle;
            }

            if let Some(dir) = at_edge {
                self.edge_dwell += 1;
                if self.edge_dwell < EDGE_DWELL_THRESHOLD {
                    return ServerOutput::Idle;
                }
                self.edge_dwell = 0;
                self.active = true;
                self.last_buttons = buttons;
                self.last_x = mx;
                self.last_y = my;

                let pw = self.peer_screen.width as i32;
                let ph = self.peer_screen.height as i32;
                let (rx, ry) = match dir {
                    Direction::Right => (lower_inset(pw), clamp_with_inset(my, ph)),
                    Direction::Left => (upper_inset(pw), clamp_with_inset(my, ph)),
                    Direction::Down => (clamp_with_inset(mx, pw), lower_inset(ph)),
                    Direction::Up => (clamp_with_inset(mx, pw), upper_inset(ph)),
                };

                let messages = vec![
                    Message::SwitchScreen { direction: dir },
                    Message::MouseMove { x: rx, y: ry },
                ];
                ServerOutput::Activate {
                    messages,
                    grab: true,
                }
            } else {
                self.edge_dwell = 0;
                ServerOutput::Idle
            }
        } else {
            let mut messages = Vec::new();

            // Mouse movement (relative deltas)
            let dx = mx.saturating_sub(self.last_x);
            let dy = my.saturating_sub(self.last_y);
            if dx != 0 || dy != 0 {
                messages.push(Message::MouseMove { x: dx, y: dy });
                self.last_x = mx;
                self.last_y = my;
            }

            // Mouse button changes
            if buttons != self.last_buttons {
                for bit in 0..3u8 {
                    let was = (self.last_buttons >> bit) & 1 != 0;
                    let now = (buttons >> bit) & 1 != 0;
                    if was != now {
                        messages.push(Message::MouseButton {
                            button: bit,
                            pressed: now,
                        });
                    }
                }
                self.last_buttons = buttons;
            }

            // Forward keyboard events, including source-generated repeats.
            self.push_key_events(key_events, &mut messages);

            ServerOutput::Forward { messages }
        }
    }

    pub fn update_peer_screen(&mut self, screen: ScreenLayout) {
        if screen.width > 0 && screen.height > 0 {
            self.peer_screen = screen;
        }
    }

    /// Deactivate because the client hit its return edge.
    ///
    /// Any keys currently considered down on the remote side must be released
    /// before we stop forwarding input; otherwise the client OS can be left
    /// with a synthetic key-down that never gets its key-up.
    #[cfg(test)]
    pub fn on_switch_back(&mut self) -> Vec<Message> {
        self.active = false;
        self.edge_armed = false;
        self.edge_cooldown = SERVER_EDGE_COOLDOWN;
        self.release_pressed_keys()
    }

    pub fn deactivate(&mut self) {
        self.active = false;
        self.edge_armed = false;
    }

    pub fn deactivate_for_shortcut(&mut self) -> Vec<Message> {
        self.active = false;
        self.edge_armed = false;
        self.edge_cooldown = SERVER_EDGE_COOLDOWN;
        self.release_pressed_keys()
    }

    /// Reclaim control on the server and explicitly reset the client's active
    /// screen state. Used when local safety takes priority over remote input.
    pub fn reset_to_local(&mut self) -> Vec<Message> {
        self.active = false;
        self.edge_armed = false;
        self.edge_cooldown = SERVER_EDGE_COOLDOWN;
        let mut messages = vec![Message::ReleaseScreen];
        messages.extend(self.release_pressed_keys());
        messages
    }
}

// --- Client Transition ---

fn opposite(dir: Direction) -> Direction {
    match dir {
        Direction::Left => Direction::Right,
        Direction::Right => Direction::Left,
        Direction::Up => Direction::Down,
        Direction::Down => Direction::Up,
    }
}

#[derive(Debug)]
pub enum ClientOutput {
    Ignore,
    Activate,
    Deactivate,
    InjectMove {
        x: i32,
        y: i32,
    },
    Forward(Message),
    SwitchBack {
        direction: Direction,
        inject: Option<(i32, i32)>,
    },
}

pub struct ClientTransition {
    cursor_x: i32,
    cursor_y: i32,
    screen_w: u32,
    screen_h: u32,
    active: bool,
    first_move: bool,
    edge_cooldown: u32,
    edge_dwell: u32,
    switch_back_edge: Option<Direction>,
}

impl ClientTransition {
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        Self {
            cursor_x: screen_w as i32 / 2,
            cursor_y: screen_h as i32 / 2,
            screen_w,
            screen_h,
            active: false,
            first_move: false,
            edge_cooldown: 0,
            edge_dwell: 0,
            switch_back_edge: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn update_screen_size(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let changed = (w, h) != (self.screen_w, self.screen_h);
        self.screen_w = w;
        self.screen_h = h;
        self.cursor_x = self.cursor_x.clamp(0, w as i32 - 1);
        self.cursor_y = self.cursor_y.clamp(0, h as i32 - 1);
        if changed {
            // Topology changes can relocate the OS cursor onto an edge. Require
            // fresh movement after the resize rather than switching back from
            // dwell accumulated on a display that no longer exists.
            self.edge_dwell = 0;
            if self.active {
                self.edge_cooldown = 50;
            }
        }
    }

    pub fn needs_cursor_sync(&self) -> bool {
        if !self.active {
            return false;
        }
        match self.switch_back_edge {
            Some(Direction::Left) => self.cursor_x <= CLIENT_CURSOR_SYNC_MARGIN,
            Some(Direction::Right) => {
                self.cursor_x >= self.screen_w as i32 - 1 - CLIENT_CURSOR_SYNC_MARGIN
            }
            Some(Direction::Up) => self.cursor_y <= CLIENT_CURSOR_SYNC_MARGIN,
            Some(Direction::Down) => {
                self.cursor_y >= self.screen_h as i32 - 1 - CLIENT_CURSOR_SYNC_MARGIN
            }
            None => false,
        }
    }

    /// Reconcile the modeled cursor with the position accepted by the client
    /// OS. This matters for non-rectangular multi-display layouts, where an OS
    /// may constrain a requested point out of a gap between displays.
    pub fn sync_cursor_position(&mut self, x: i32, y: i32) {
        if !self.active || self.screen_w == 0 || self.screen_h == 0 {
            return;
        }
        self.cursor_x = x.clamp(0, self.screen_w as i32 - 1);
        self.cursor_y = y.clamp(0, self.screen_h as i32 - 1);

        let actual_edge =
            edge::detect_edge(self.cursor_x, self.cursor_y, self.screen_w, self.screen_h);
        if actual_edge != self.switch_back_edge {
            self.edge_dwell = 0;
        }
    }

    pub fn handle(&mut self, message: Message) -> ClientOutput {
        match message {
            Message::SwitchScreen { direction } => {
                self.active = true;
                self.first_move = true;
                self.edge_cooldown = 50;
                self.edge_dwell = 0;
                self.switch_back_edge = Some(opposite(direction));
                ClientOutput::Activate
            }
            Message::ReleaseScreen => {
                self.active = false;
                self.first_move = false;
                self.edge_dwell = 0;
                self.switch_back_edge = None;
                ClientOutput::Deactivate
            }
            Message::MouseMove { x, y } if self.active => {
                if self.first_move {
                    self.cursor_x = x;
                    self.cursor_y = y;
                    self.first_move = false;
                } else {
                    self.cursor_x = self.cursor_x.saturating_add(x);
                    self.cursor_y = self.cursor_y.saturating_add(y);
                }
                self.cursor_x = self.cursor_x.clamp(0, self.screen_w as i32 - 1);
                self.cursor_y = self.cursor_y.clamp(0, self.screen_h as i32 - 1);

                if self.edge_cooldown > 0 {
                    self.edge_cooldown -= 1;
                    self.edge_dwell = 0;
                } else if let Some(dir) =
                    edge::detect_edge(self.cursor_x, self.cursor_y, self.screen_w, self.screen_h)
                {
                    if Some(dir) == self.switch_back_edge {
                        self.edge_dwell += 1;
                        if self.edge_dwell >= CLIENT_EDGE_DWELL {
                            self.active = false;
                            self.edge_dwell = 0;
                            return ClientOutput::SwitchBack {
                                direction: dir,
                                inject: Some((self.cursor_x, self.cursor_y)),
                            };
                        }
                    } else {
                        self.edge_dwell = 0;
                    }
                } else {
                    self.edge_dwell = 0;
                }

                ClientOutput::InjectMove {
                    x: self.cursor_x,
                    y: self.cursor_y,
                }
            }
            Message::MouseButton { .. } if self.active => ClientOutput::Forward(message),
            Message::MouseScroll { .. } if self.active => ClientOutput::Forward(message),
            Message::KeyEvent { .. } if self.active => ClientOutput::Forward(message),

            // A switch-back can deactivate the client before the server's
            // synthetic release messages arrive on the input stream. Still
            // forward releases while inactive so macOS/Linux does not keep a
            // key or mouse button stuck down (observed as endless "." input).
            Message::KeyEvent { pressed: false, .. } => ClientOutput::Forward(message),
            Message::MouseButton { pressed: false, .. } => ClientOutput::Forward(message),
            _ => ClientOutput::Ignore,
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_screen() -> ScreenLayout {
        ScreenLayout {
            width: 2560,
            height: 1440,
        }
    }

    // ===== Server Tests =====

    #[test]
    fn server_edge_respects_trigger_filter() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        // Cursor at left edge should NOT trigger when trigger_edge is Right
        for _ in 0..EDGE_DWELL_THRESHOLD + 10 {
            let out = st.poll(0, 500, 1920, 1080, 0, vec![]);
            assert!(matches!(out, ServerOutput::Idle));
        }
        assert!(!st.is_active());
    }

    #[test]
    fn server_dwell_increments_and_triggers() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        // Dwell at right edge
        for i in 0..EDGE_DWELL_THRESHOLD - 1 {
            let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
            assert!(
                matches!(out, ServerOutput::Idle),
                "should be idle at dwell {}",
                i
            );
        }
        // One more should trigger
        let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
        assert!(matches!(out, ServerOutput::Activate { .. }));
        assert!(st.is_active());
    }

    #[test]
    fn server_dwell_resets_on_move_away() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        // Partial dwell (half the threshold)
        for _ in 0..EDGE_DWELL_THRESHOLD / 2 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        // Move away from edge
        st.poll(500, 500, 1920, 1080, 0, vec![]);
        // Dwell again - should need full threshold
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
            assert!(matches!(out, ServerOutput::Idle));
        }
        let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
        assert!(matches!(out, ServerOutput::Activate { .. }));
    }

    #[test]
    fn server_initial_placement_right() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        // Re-create to get the Activate output
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
        if let ServerOutput::Activate { messages, .. } = out {
            assert_eq!(messages.len(), 2);
            assert!(matches!(
                messages[0],
                Message::SwitchScreen {
                    direction: Direction::Right
                }
            ));
            // Placement: x=INSET, y clamped
            if let Message::MouseMove { x, y } = messages[1] {
                assert_eq!(x, INSET);
                assert_eq!(y, 500); // 500 is within valid range
            } else {
                panic!("Expected MouseMove");
            }
        } else {
            panic!("Expected Activate");
        }
    }

    #[test]
    fn server_initial_placement_left() {
        let mut st = ServerTransition::new(Some(Direction::Left), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(0, 500, 1920, 1080, 0, vec![]);
        }
        let out = st.poll(0, 500, 1920, 1080, 0, vec![]);
        if let ServerOutput::Activate { messages, .. } = out {
            if let Message::MouseMove { x, y } = messages[1] {
                assert_eq!(x, 2560 - 1 - INSET);
                assert_eq!(y, 500);
            } else {
                panic!("Expected MouseMove");
            }
        } else {
            panic!("Expected Activate");
        }
    }

    #[test]
    fn server_delta_when_active() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        // Trigger activation
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        st.poll(1919, 500, 1920, 1080, 0, vec![]);
        assert!(st.is_active());

        // Now send a delta
        let out = st.poll(1920, 510, 1920, 1080, 0, vec![]);
        if let ServerOutput::Forward { messages } = out {
            assert_eq!(messages.len(), 1);
            if let Message::MouseMove { x, y } = messages[0] {
                assert_eq!(x, 1); // 1920 - 1919
                assert_eq!(y, 10); // 510 - 500
            } else {
                panic!("Expected MouseMove");
            }
        } else {
            panic!("Expected Forward");
        }
    }

    #[test]
    fn server_delta_saturates_extreme_positions() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);
        st.last_x = i32::MIN;
        st.last_y = i32::MAX;

        let out = st.poll(i32::MAX, i32::MIN, 1920, 1080, 0, vec![]);
        assert!(matches!(
            out,
            ServerOutput::Forward { messages }
                if messages.iter().any(|m| matches!(
                    m,
                    Message::MouseMove { x: i32::MAX, y: i32::MIN }
                ))
        ));
    }

    #[test]
    fn server_button_change() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        st.poll(1919, 500, 1920, 1080, 0, vec![]);

        // Press button 0
        let out = st.poll(1919, 500, 1920, 1080, 1, vec![]);
        if let ServerOutput::Forward { messages } = out {
            assert!(messages.iter().any(|m| matches!(
                m,
                Message::MouseButton {
                    button: 0,
                    pressed: true
                }
            )));
        } else {
            panic!("Expected Forward");
        }
    }

    #[test]
    fn server_key_forwarding() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        st.poll(1919, 500, 1920, 1080, 0, vec![]);

        let key = Message::KeyEvent {
            keycode: 42,
            pressed: true,
            modifiers: 0,
        };
        let out = st.poll(1919, 500, 1920, 1080, 0, vec![key]);
        if let ServerOutput::Forward { messages } = out {
            assert!(messages
                .iter()
                .any(|m| matches!(m, Message::KeyEvent { keycode: 42, .. })));
        } else {
            panic!("Expected Forward");
        }
    }

    #[test]
    fn server_active_key_polling_forwards_keys() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        let out = st.poll_active_keys(vec![Message::KeyEvent {
            keycode: 115,
            pressed: true,
            modifiers: 0,
        }]);

        assert!(matches!(
            out,
            ServerOutput::Forward { ref messages }
                if messages.iter().any(|m| matches!(
                    m,
                    Message::KeyEvent { keycode: 115, pressed: true, .. }
                ))
        ));
    }

    #[test]
    fn server_switch_back_sets_cooldown() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        st.poll(1919, 500, 1920, 1080, 0, vec![]);
        assert!(st.is_active());

        let releases = st.on_switch_back();
        assert!(releases.is_empty());
        assert!(!st.is_active());

        // Should be idle during cooldown even at edge
        for _ in 0..SERVER_EDGE_COOLDOWN {
            let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
            assert!(matches!(out, ServerOutput::Idle));
        }
    }

    #[test]
    fn server_switch_back_releases_held_keys() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        st.poll(
            1919,
            500,
            1920,
            1080,
            0,
            vec![Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            }],
        );

        let releases = st.on_switch_back();
        assert_eq!(releases.len(), 1);
        assert!(matches!(
            releases[0],
            Message::KeyEvent {
                keycode: 30,
                pressed: false,
                ..
            }
        ));
        assert!(!st.is_active());
    }

    #[test]
    fn server_cooldown_prevents_retriggering() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        st.poll(1919, 500, 1920, 1080, 0, vec![]);
        let releases = st.on_switch_back();
        assert!(releases.is_empty());

        // Remaining at the edge must never re-arm, regardless of time.
        for _ in 0..SERVER_EDGE_COOLDOWN * 2 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        assert!(!st.edge_is_armed());

        // Moving away explicitly rearms the edge, then the normal cooldown
        // expires before a fresh dwell can activate it again.
        st.poll(1000, 500, 1920, 1080, 0, vec![]);
        assert!(st.edge_is_armed());
        for _ in 1..SERVER_EDGE_COOLDOWN {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        // Cooldown just expired, dwell should be 0. Need full dwell again.
        for i in 0..EDGE_DWELL_THRESHOLD - 1 {
            let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
            assert!(matches!(out, ServerOutput::Idle), "at dwell {}", i);
        }
        let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
        assert!(matches!(out, ServerOutput::Activate { .. }));
    }

    // ===== Safety Escape Tests =====

    fn activate_server(st: &mut ServerTransition) {
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        st.poll(1919, 500, 1920, 1080, 0, vec![]);
        assert!(st.is_active());
    }

    #[test]
    fn server_ctrl_alt_escape_force_releases() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        let keys = vec![
            Message::KeyEvent {
                keycode: KEY_LEFTCTRL,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_LEFTALT,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_ESC,
                pressed: true,
                modifiers: 0,
            },
        ];
        let out = st.poll(1919, 500, 1920, 1080, 0, keys);
        assert!(matches!(out, ServerOutput::ForceRelease { .. }));
        assert!(!st.is_active());
    }

    #[test]
    fn server_escape_alone_does_not_force_release() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        let keys = vec![Message::KeyEvent {
            keycode: KEY_ESC,
            pressed: true,
            modifiers: 0,
        }];
        let out = st.poll(1919, 500, 1920, 1080, 0, keys);
        assert!(matches!(out, ServerOutput::Forward { .. }));
        assert!(st.is_active());
    }

    #[test]
    fn server_escape_combo_ignored_when_inactive() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        let keys = vec![
            Message::KeyEvent {
                keycode: KEY_LEFTCTRL,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_LEFTALT,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_ESC,
                pressed: true,
                modifiers: 0,
            },
        ];
        let out = st.poll(500, 500, 1920, 1080, 0, keys);
        assert!(matches!(out, ServerOutput::Idle));
    }

    #[test]
    fn server_shortcut_activates_matching_edge() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        let keys = vec![
            Message::KeyEvent {
                keycode: KEY_LEFTCTRL,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_LEFTALT,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_LEFTSHIFT,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_RIGHT,
                pressed: true,
                modifiers: 0,
            },
        ];

        let out = st.poll(500, 500, 1920, 1080, 0, keys);
        if let ServerOutput::Activate { messages, .. } = out {
            assert!(matches!(
                messages[0],
                Message::SwitchScreen {
                    direction: Direction::Right
                }
            ));
            assert!(st.is_active());
        } else {
            panic!("Expected Activate");
        }
    }

    #[test]
    fn server_shortcut_respects_trigger_edge() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        let keys = vec![
            Message::KeyEvent {
                keycode: KEY_LEFTCTRL,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_LEFTALT,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_LEFTSHIFT,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_LEFT,
                pressed: true,
                modifiers: 0,
            },
        ];

        let out = st.poll(500, 500, 1920, 1080, 0, keys);
        assert!(matches!(out, ServerOutput::Idle));
        assert!(!st.is_active());
    }

    #[test]
    fn server_shortcut_releases_when_active() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        let keys = vec![
            Message::KeyEvent {
                keycode: KEY_LEFTCTRL,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_LEFTALT,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_LEFTSHIFT,
                pressed: true,
                modifiers: 0,
            },
            Message::KeyEvent {
                keycode: KEY_RIGHT,
                pressed: true,
                modifiers: 0,
            },
        ];

        let out = st.poll(1919, 500, 1920, 1080, 0, keys);
        if let ServerOutput::ShortcutRelease { messages } = out {
            assert!(messages.iter().any(|m| {
                matches!(
                    m,
                    Message::KeyEvent {
                        keycode: KEY_LEFTCTRL,
                        pressed: false,
                        ..
                    }
                )
            }));
            assert!(!st.is_active());
        } else {
            panic!("Expected ShortcutRelease");
        }
    }

    #[test]
    fn server_reset_to_local_deactivates_and_notifies_client() {
        let mut st = ServerTransition::new(
            Some(Direction::Right),
            ScreenLayout {
                width: 1920,
                height: 1080,
            },
        );
        activate_server(&mut st);
        st.update_key(KEY_LEFTCTRL, true);

        let messages = st.reset_to_local();

        assert!(!st.is_active());
        assert!(matches!(messages.first(), Some(Message::ReleaseScreen)));
        assert!(messages.iter().any(|message| matches!(
            message,
            Message::KeyEvent {
                keycode: KEY_LEFTCTRL,
                pressed: false,
                ..
            }
        )));
    }

    // ===== Client Tests =====

    #[test]
    fn client_inactive_ignores_mouse_move() {
        let mut ct = ClientTransition::new(1920, 1080);
        let out = ct.handle(Message::MouseMove { x: 100, y: 200 });
        assert!(matches!(out, ClientOutput::Ignore));
    }

    #[test]
    fn client_switch_screen_activates() {
        let mut ct = ClientTransition::new(1920, 1080);
        let out = ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        assert!(matches!(out, ClientOutput::Activate));
    }

    #[test]
    fn client_release_screen_deactivates_remote_control() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });

        let out = ct.handle(Message::ReleaseScreen);

        assert!(matches!(out, ClientOutput::Deactivate));
        assert!(!ct.active);
        assert_eq!(ct.switch_back_edge, None);
    }

    #[test]
    fn client_switch_screen_sets_correct_switch_back_edge() {
        let mut ct = ClientTransition::new(1920, 1080);
        // Server exits Right → cursor enters client's Left → switch_back_edge is Left
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        assert_eq!(ct.switch_back_edge, Some(Direction::Left));
    }

    #[test]
    fn client_first_move_is_absolute() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        let out = ct.handle(Message::MouseMove { x: 100, y: 200 });
        if let ClientOutput::InjectMove { x, y } = out {
            assert_eq!(x, 100);
            assert_eq!(y, 200);
        } else {
            panic!("Expected InjectMove");
        }
    }

    #[test]
    fn client_subsequent_moves_are_relative() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        ct.handle(Message::MouseMove { x: 100, y: 200 });
        let out = ct.handle(Message::MouseMove { x: 10, y: -5 });
        if let ClientOutput::InjectMove { x, y } = out {
            assert_eq!(x, 110);
            assert_eq!(y, 195);
        } else {
            panic!("Expected InjectMove");
        }
    }

    #[test]
    fn client_cursor_clamped_to_bounds() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        ct.handle(Message::MouseMove { x: 100, y: 100 });
        // Large delta pushing past bounds
        let out = ct.handle(Message::MouseMove { x: -5000, y: -5000 });
        if let ClientOutput::InjectMove { x, y } = out {
            assert_eq!(x, 0);
            assert_eq!(y, 0);
        } else {
            panic!("Expected InjectMove");
        }
    }

    #[test]
    fn client_cursor_sync_prevents_false_edge_in_display_gap() {
        let mut ct = ClientTransition::new(4030, 1440);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        ct.handle(Message::MouseMove { x: 1470, y: 1200 });
        ct.edge_cooldown = 0;

        // The modeled pointer can move into the rectangular union below a
        // shorter left display, while macOS constrains the real pointer to the
        // adjacent display at x=1470. Reconciliation must prevent false dwell
        // at the virtual desktop's x=0 edge.
        for _ in 0..CLIENT_EDGE_DWELL * 4 {
            if ct.needs_cursor_sync() {
                ct.sync_cursor_position(1470, 1200);
            }
            let out = ct.handle(Message::MouseMove { x: -500, y: 0 });
            assert!(matches!(out, ClientOutput::InjectMove { .. }));
        }
        assert!(ct.active);
    }

    #[test]
    fn client_cursor_sync_preserves_dwell_at_real_return_edge() {
        let mut ct = ClientTransition::new(4030, 1440);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        ct.handle(Message::MouseMove { x: 0, y: 500 });
        ct.edge_cooldown = 0;

        let mut switched = false;
        for _ in 0..CLIENT_EDGE_DWELL + 1 {
            ct.sync_cursor_position(0, 500);
            if matches!(
                ct.handle(Message::MouseMove { x: -1, y: 0 }),
                ClientOutput::SwitchBack { .. }
            ) {
                switched = true;
                break;
            }
        }
        assert!(switched);
    }

    #[test]
    fn client_relative_move_saturates_before_clamping() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        ct.handle(Message::MouseMove {
            x: i32::MAX,
            y: i32::MIN,
        });
        let out = ct.handle(Message::MouseMove { x: i32::MAX, y: -1 });
        if let ClientOutput::InjectMove { x, y } = out {
            assert_eq!(x, 1919);
            assert_eq!(y, 0);
        } else {
            panic!("Expected InjectMove");
        }
    }

    #[test]
    fn client_ignores_zero_sized_screen_update() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.update_screen_size(0, 1080);
        assert_eq!(ct.screen_w, 1920);
        assert_eq!(ct.screen_h, 1080);
        ct.update_screen_size(1920, 0);
        assert_eq!(ct.screen_w, 1920);
        assert_eq!(ct.screen_h, 1080);
    }

    #[test]
    fn client_display_resize_resets_stale_edge_dwell() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        ct.handle(Message::MouseMove { x: 100, y: 500 });
        for _ in 0..50 {
            ct.handle(Message::MouseMove { x: 0, y: 0 });
        }
        ct.handle(Message::MouseMove { x: -100, y: 0 });
        for _ in 1..CLIENT_EDGE_DWELL - 1 {
            assert!(matches!(
                ct.handle(Message::MouseMove { x: 0, y: 0 }),
                ClientOutput::InjectMove { .. }
            ));
        }

        // Closing a laptop lid can resize the desktop while the cursor model is
        // sitting on the old return edge. It must not complete that old dwell.
        ct.update_screen_size(1280, 720);
        assert!(matches!(
            ct.handle(Message::MouseMove { x: 0, y: 0 }),
            ClientOutput::InjectMove { .. }
        ));
        assert!(ct.is_active());
    }

    #[test]
    fn client_cooldown_prevents_edge_detection() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        // First move places cursor at left edge (the switch_back_edge)
        let out = ct.handle(Message::MouseMove { x: 0, y: 500 });
        // During cooldown, should inject, not switch back
        assert!(matches!(out, ClientOutput::InjectMove { .. }));
    }

    #[test]
    fn client_edge_dwell_required() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        // Absolute placement at safe position
        ct.handle(Message::MouseMove { x: 100, y: 500 });
        // Exhaust cooldown
        for _ in 0..50 {
            ct.handle(Message::MouseMove { x: 0, y: 0 });
        }
        // Now at left edge (cursor_x=100, move by -100 to get to 0)
        ct.handle(Message::MouseMove { x: -100, y: 0 });
        // First time at edge: dwell=1, not yet at threshold
        let out = ct.handle(Message::MouseMove { x: 0, y: 0 });
        assert!(matches!(out, ClientOutput::InjectMove { .. }));
    }

    #[test]
    fn client_only_opposite_edge_triggers_switch_back() {
        let mut ct = ClientTransition::new(1920, 1080);
        // Server exits Right → switch_back_edge = Left
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        // Place cursor at safe position
        ct.handle(Message::MouseMove { x: 960, y: 540 });
        // Exhaust cooldown
        for _ in 0..50 {
            ct.handle(Message::MouseMove { x: 0, y: 0 });
        }

        // Move to RIGHT edge (not the switch_back_edge)
        ct.handle(Message::MouseMove { x: 959, y: 0 }); // cursor at 1919
                                                        // Stay at right edge for many moves - should never switch back
        for _ in 0..CLIENT_EDGE_DWELL + 10 {
            let out = ct.handle(Message::MouseMove { x: 0, y: 0 });
            assert!(
                matches!(out, ClientOutput::InjectMove { .. }),
                "Right edge should not trigger switch-back when switch_back_edge is Left"
            );
        }
    }

    #[test]
    fn client_wrong_edge_no_switch_back() {
        let mut ct = ClientTransition::new(1920, 1080);
        // Server exits Left → switch_back_edge = Right
        ct.handle(Message::SwitchScreen {
            direction: Direction::Left,
        });
        ct.handle(Message::MouseMove { x: 960, y: 540 });
        // Exhaust cooldown
        for _ in 0..50 {
            ct.handle(Message::MouseMove { x: 0, y: 0 });
        }

        // Move to LEFT edge (wrong edge)
        ct.handle(Message::MouseMove { x: -960, y: 0 }); // cursor at 0
        for _ in 0..CLIENT_EDGE_DWELL + 10 {
            let out = ct.handle(Message::MouseMove { x: 0, y: 0 });
            assert!(matches!(out, ClientOutput::InjectMove { .. }));
        }
    }

    #[test]
    fn client_large_delta_no_instant_switch_back() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        // Absolute: place inset from left edge
        ct.handle(Message::MouseMove { x: INSET, y: 540 });
        // Exhaust cooldown
        for _ in 0..50 {
            ct.handle(Message::MouseMove { x: 0, y: 0 });
        }

        // One large delta jumps to left edge
        let out = ct.handle(Message::MouseMove { x: -5000, y: 0 });
        // Should NOT immediately switch back; dwell needs to accumulate
        assert!(
            matches!(out, ClientOutput::InjectMove { .. }),
            "Large delta should not cause instant switch-back"
        );
    }

    #[test]
    fn client_switch_back_includes_inject() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        // Place at safe position
        ct.handle(Message::MouseMove { x: 100, y: 500 });
        // Exhaust cooldown
        for _ in 0..50 {
            ct.handle(Message::MouseMove { x: 0, y: 0 });
        }

        // Move to left edge
        ct.handle(Message::MouseMove { x: -100, y: 0 });
        // Dwell at left edge until switch-back
        let mut switched = false;
        for _ in 0..CLIENT_EDGE_DWELL + 5 {
            let out = ct.handle(Message::MouseMove { x: 0, y: 0 });
            if let ClientOutput::SwitchBack { direction, inject } = out {
                assert_eq!(direction, Direction::Left);
                assert!(
                    inject.is_some(),
                    "SwitchBack must include inject coordinates"
                );
                switched = true;
                break;
            }
        }
        assert!(switched, "Should have triggered switch-back");
    }

    #[test]
    fn client_button_forwarding() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        let out = ct.handle(Message::MouseButton {
            button: 0,
            pressed: true,
        });
        assert!(matches!(
            out,
            ClientOutput::Forward(Message::MouseButton {
                button: 0,
                pressed: true
            })
        ));
    }

    #[test]
    fn client_scroll_forwarding() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        let out = ct.handle(Message::MouseScroll {
            dx: 0.0,
            dy: -3.0,
            phase: crate::net::protocol::ScrollPhase::None,
        });
        assert!(
            matches!(out, ClientOutput::Forward(Message::MouseScroll { dx, dy, .. }) if dx == 0.0 && dy == -3.0)
        );
    }

    #[test]
    fn client_key_forwarding() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        let out = ct.handle(Message::KeyEvent {
            keycode: 42,
            pressed: true,
            modifiers: 0,
        });
        assert!(matches!(
            out,
            ClientOutput::Forward(Message::KeyEvent {
                keycode: 42,
                pressed: true,
                modifiers: 0
            })
        ));
    }

    #[test]
    fn client_forwards_key_release_after_switch_back() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        ct.handle(Message::MouseMove { x: 100, y: 500 });
        for _ in 0..50 {
            ct.handle(Message::MouseMove { x: 0, y: 0 });
        }
        ct.handle(Message::MouseMove { x: -100, y: 0 });
        for _ in 0..CLIENT_EDGE_DWELL {
            ct.handle(Message::MouseMove { x: 0, y: 0 });
        }

        let out = ct.handle(Message::KeyEvent {
            keycode: 52, // KEY_DOT
            pressed: false,
            modifiers: 0,
        });
        assert!(matches!(
            out,
            ClientOutput::Forward(Message::KeyEvent {
                keycode: 52,
                pressed: false,
                modifiers: 0
            })
        ));
    }

    // ===== Key Repeat Tests =====

    #[test]
    fn server_does_not_synthesize_key_repeat() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        // Press a non-modifier key (KEY_A = 30)
        let key = vec![Message::KeyEvent {
            keycode: 30,
            pressed: true,
            modifiers: 0,
        }];
        st.poll(1919, 500, 1920, 1080, 0, key);

        // Holding the key without new physical events must not generate more
        // key-down messages. Source-generated repeats are forwarded separately.
        for _ in 0..500 {
            let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
            if let ServerOutput::Forward { messages } = out {
                assert!(
                    !messages.iter().any(|m| matches!(
                        m,
                        Message::KeyEvent {
                            keycode: 30,
                            pressed: true,
                            ..
                        }
                    )),
                    "server must not synthesize repeat key-downs"
                );
            }
        }
    }

    #[test]
    fn server_key_release_clears_hold_state() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        let key = vec![Message::KeyEvent {
            keycode: 30,
            pressed: true,
            modifiers: 0,
        }];
        st.poll(1919, 500, 1920, 1080, 0, key);

        let key = vec![Message::KeyEvent {
            keycode: 30,
            pressed: false,
            modifiers: 0,
        }];
        let out = st.poll(1919, 500, 1920, 1080, 0, key);
        assert!(matches!(
            out,
            ServerOutput::Forward { messages }
                if messages.iter().any(|m| matches!(
                    m,
                    Message::KeyEvent { keycode: 30, pressed: false, .. }
                ))
        ));
    }

    // ===== activate_instant Tests =====

    #[test]
    fn server_ignores_zero_sized_peer_screen_update() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        st.update_peer_screen(ScreenLayout {
            width: 0,
            height: 1440,
        });
        assert_eq!(st.peer_screen.width, 2560);
        assert_eq!(st.peer_screen.height, 1440);
        st.update_peer_screen(ScreenLayout {
            width: 1920,
            height: 0,
        });
        assert_eq!(st.peer_screen.width, 2560);
        assert_eq!(st.peer_screen.height, 1440);
    }

    #[test]
    fn server_activation_handles_tiny_peer_screen() {
        let mut st = ServerTransition::new(
            Some(Direction::Right),
            ScreenLayout {
                width: 10,
                height: 10,
            },
        );
        for _ in 0..EDGE_DWELL_THRESHOLD {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        assert!(st.is_active());
    }

    #[test]
    fn server_ignores_zero_sized_local_screen() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        let out = st.poll(0, 0, 0, 1080, 0, vec![]);
        assert!(matches!(out, ServerOutput::Idle));
        assert!(!st.is_active());
    }

    #[test]
    fn server_deactivates_on_zero_sized_local_screen() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);
        let out = st.poll(0, 0, 1920, 0, 0, vec![]);
        assert!(matches!(out, ServerOutput::ForceRelease { .. }));
        assert!(!st.is_active());
    }

    #[test]
    fn server_activate_instant_right() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        let messages = st.activate_instant(Direction::Right);
        assert!(st.is_active());
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            messages[0],
            Message::SwitchScreen {
                direction: Direction::Right
            }
        ));
        if let Message::MouseMove { x, y } = messages[1] {
            assert_eq!(x, INSET);
            assert_eq!(y, 1440 / 2); // peer_screen height / 2
        } else {
            panic!("Expected MouseMove");
        }
    }

    #[test]
    fn server_activate_instant_left() {
        let mut st = ServerTransition::new(Some(Direction::Left), peer_screen());
        let messages = st.activate_instant(Direction::Left);
        assert!(st.is_active());
        if let Message::MouseMove { x, y } = messages[1] {
            assert_eq!(x, 2560 - 1 - INSET);
            assert_eq!(y, 1440 / 2);
        } else {
            panic!("Expected MouseMove");
        }
    }

    #[test]
    fn server_activate_instant_down() {
        let mut st = ServerTransition::new(Some(Direction::Down), peer_screen());
        let messages = st.activate_instant(Direction::Down);
        assert!(st.is_active());
        if let Message::MouseMove { x, y } = messages[1] {
            assert_eq!(x, 2560 / 2);
            assert_eq!(y, INSET);
        } else {
            panic!("Expected MouseMove");
        }
    }

    #[test]
    fn server_activate_instant_up() {
        let mut st = ServerTransition::new(Some(Direction::Up), peer_screen());
        let messages = st.activate_instant(Direction::Up);
        assert!(st.is_active());
        if let Message::MouseMove { x, y } = messages[1] {
            assert_eq!(x, 2560 / 2);
            assert_eq!(y, 1440 - 1 - INSET);
        } else {
            panic!("Expected MouseMove");
        }
    }

    #[test]
    fn server_activate_instant_resets_state() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        // Simulate some accumulated state
        st.edge_dwell = 25;
        st.edge_cooldown = 50;
        st.last_x = 500;
        st.last_y = 300;
        st.last_buttons = 3;

        st.activate_instant(Direction::Right);
        assert_eq!(st.edge_dwell, 0);
        assert_eq!(st.edge_cooldown, 0);
        assert_eq!(st.last_x, 0);
        assert_eq!(st.last_y, 0);
        assert_eq!(st.last_buttons, 0);
    }

    // ===== update_key Tests =====

    #[test]
    fn server_update_key_tracks_state() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        st.update_key(30, true); // KEY_A pressed
        assert!(st.pressed_keys.contains(&30));

        st.update_key(30, false); // KEY_A released
        assert!(!st.pressed_keys.contains(&30));
    }

    #[test]
    fn server_update_key_escape_combo() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        st.update_key(KEY_LEFTCTRL, true);
        st.update_key(KEY_LEFTALT, true);
        assert!(!st.is_escape_combo());

        st.update_key(KEY_ESC, true);
        assert!(st.is_escape_combo());
    }
}
