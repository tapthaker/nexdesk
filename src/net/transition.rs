use std::collections::{HashMap, HashSet};

use crate::cursor::edge;
use crate::net::protocol::{Direction, Message, ScreenLayout};

// --- Constants ---

const EDGE_DWELL_THRESHOLD: u32 = 50;
const SERVER_EDGE_COOLDOWN: u32 = 125;
const INSET: i32 = 20;
const CLIENT_EDGE_DWELL: u32 = 8;

/// Polls before first repeat fires (~500ms at 2ms poll interval)
const KEY_REPEAT_DELAY: u32 = 250;
/// Polls between subsequent repeats (~30ms ≈ 33 repeats/sec)
const KEY_REPEAT_INTERVAL: u32 = 15;

// Evdev keycodes for the safety escape combo (Ctrl+Alt+Escape)
const KEY_ESC: u32 = 1;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_RIGHTALT: u32 = 100;

/// Modifier keys that should NOT generate repeat events.
fn is_modifier(keycode: u32) -> bool {
    matches!(
        keycode,
        29 |   // KEY_LEFTCTRL
        42 |   // KEY_LEFTSHIFT
        54 |   // KEY_RIGHTSHIFT
        56 |   // KEY_LEFTALT
        58 |   // KEY_CAPSLOCK
        97 |   // KEY_RIGHTCTRL
        100 |  // KEY_RIGHTALT
        125 |  // KEY_LEFTMETA
        126    // KEY_RIGHTMETA
    )
}

// --- Server Transition ---

#[derive(Debug)]
pub enum ServerOutput {
    Idle,
    Activate { messages: Vec<Message>, grab: bool },
    Forward { messages: Vec<Message> },
    /// Safety escape: Ctrl+Alt+Escape pressed, force-release grab
    ForceRelease,
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
    pressed_keys: HashSet<u32>,
    /// Polls each non-modifier key has been held, for generating repeats.
    key_hold_polls: HashMap<u32, u32>,
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
            pressed_keys: HashSet::new(),
            key_hold_polls: HashMap::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    fn update_pressed_keys(&mut self, key_events: &[Message]) {
        for msg in key_events {
            if let Message::KeyEvent { keycode, pressed, .. } = msg {
                if *pressed {
                    self.pressed_keys.insert(*keycode);
                } else {
                    self.pressed_keys.remove(keycode);
                }
            }
        }
    }

    pub fn is_escape_combo(&self) -> bool {
        let has_ctrl = self.pressed_keys.contains(&KEY_LEFTCTRL)
            || self.pressed_keys.contains(&KEY_RIGHTCTRL);
        let has_alt = self.pressed_keys.contains(&KEY_LEFTALT)
            || self.pressed_keys.contains(&KEY_RIGHTALT);
        let has_esc = self.pressed_keys.contains(&KEY_ESC);
        has_ctrl && has_alt && has_esc
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
            Direction::Right => (INSET, ph / 2),
            Direction::Left => (pw - 1 - INSET, ph / 2),
            Direction::Down => (pw / 2, INSET),
            Direction::Up => (pw / 2, ph - 1 - INSET),
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
            self.pressed_keys.clear();
            self.key_hold_polls.clear();
            return ServerOutput::ForceRelease;
        }

        let clamped_x = mx.clamp(0, sw as i32 - 1);
        let clamped_y = my.clamp(0, sh as i32 - 1);

        if !self.active {
            let at_edge = edge::detect_edge(clamped_x, clamped_y, sw, sh)
                .filter(|d| self.trigger_edge.map_or(true, |e| *d == e));

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
                    Direction::Right => (INSET, my.clamp(INSET, ph - 1 - INSET)),
                    Direction::Left => (pw - 1 - INSET, my.clamp(INSET, ph - 1 - INSET)),
                    Direction::Down => (mx.clamp(INSET, pw - 1 - INSET), INSET),
                    Direction::Up => (mx.clamp(INSET, pw - 1 - INSET), ph - 1 - INSET),
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
            let dx = mx - self.last_x;
            let dy = my - self.last_y;
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

            // Keyboard events: forward originals and update hold counters
            for key_msg in &key_events {
                if let Message::KeyEvent { keycode, pressed, .. } = key_msg {
                    if *pressed {
                        self.key_hold_polls.entry(*keycode).or_insert(0);
                    } else {
                        self.key_hold_polls.remove(keycode);
                    }
                }
            }
            for key_msg in key_events {
                messages.push(key_msg);
            }

            // Generate repeat events for non-modifier keys held past the delay
            for (&keycode, polls) in &mut self.key_hold_polls {
                *polls += 1;
                if !is_modifier(keycode)
                    && *polls > KEY_REPEAT_DELAY
                    && (*polls - KEY_REPEAT_DELAY) % KEY_REPEAT_INTERVAL == 0
                {
                    messages.push(Message::KeyEvent {
                        keycode,
                        pressed: true,
                        modifiers: 0,
                    });
                }
            }

            ServerOutput::Forward { messages }
        }
    }

    pub fn update_peer_screen(&mut self, screen: ScreenLayout) {
        self.peer_screen = screen;
    }

    pub fn on_switch_back(&mut self) {
        self.active = false;
        self.edge_cooldown = SERVER_EDGE_COOLDOWN;
        self.key_hold_polls.clear();
    }

    pub fn deactivate(&mut self) {
        self.active = false;
        self.key_hold_polls.clear();
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
    InjectMove { x: i32, y: i32 },
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

    pub fn update_screen_size(&mut self, w: u32, h: u32) {
        self.screen_w = w;
        self.screen_h = h;
        self.cursor_x = self.cursor_x.clamp(0, w as i32 - 1);
        self.cursor_y = self.cursor_y.clamp(0, h as i32 - 1);
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
            Message::MouseMove { x, y } if self.active => {
                if self.first_move {
                    self.cursor_x = x;
                    self.cursor_y = y;
                    self.first_move = false;
                } else {
                    self.cursor_x += x;
                    self.cursor_y += y;
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
            _ => ClientOutput::Ignore,
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn server_screen() -> ScreenLayout {
        ScreenLayout {
            width: 1920,
            height: 1080,
        }
    }

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
            assert!(matches!(out, ServerOutput::Idle), "should be idle at dwell {}", i);
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
            assert!(matches!(messages[0], Message::SwitchScreen { direction: Direction::Right }));
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
    fn server_button_change() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        st.poll(1919, 500, 1920, 1080, 0, vec![]);

        // Press button 0
        let out = st.poll(1919, 500, 1920, 1080, 1, vec![]);
        if let ServerOutput::Forward { messages } = out {
            assert!(messages.iter().any(|m| matches!(m, Message::MouseButton { button: 0, pressed: true })));
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
            assert!(messages.iter().any(|m| matches!(m, Message::KeyEvent { keycode: 42, .. })));
        } else {
            panic!("Expected Forward");
        }
    }

    #[test]
    fn server_switch_back_sets_cooldown() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        st.poll(1919, 500, 1920, 1080, 0, vec![]);
        assert!(st.is_active());

        st.on_switch_back();
        assert!(!st.is_active());

        // Should be idle during cooldown even at edge
        for _ in 0..SERVER_EDGE_COOLDOWN {
            let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
            assert!(matches!(out, ServerOutput::Idle));
        }
    }

    #[test]
    fn server_cooldown_prevents_retriggering() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        for _ in 0..EDGE_DWELL_THRESHOLD - 1 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        st.poll(1919, 500, 1920, 1080, 0, vec![]);
        st.on_switch_back();

        // During cooldown, dwell shouldn't accumulate
        for _ in 0..SERVER_EDGE_COOLDOWN {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }
        // Cooldown just expired, dwell should be 0
        // Need full dwell again
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
            Message::KeyEvent { keycode: KEY_LEFTCTRL, pressed: true, modifiers: 0 },
            Message::KeyEvent { keycode: KEY_LEFTALT, pressed: true, modifiers: 0 },
            Message::KeyEvent { keycode: KEY_ESC, pressed: true, modifiers: 0 },
        ];
        let out = st.poll(1919, 500, 1920, 1080, 0, keys);
        assert!(matches!(out, ServerOutput::ForceRelease));
        assert!(!st.is_active());
    }

    #[test]
    fn server_escape_alone_does_not_force_release() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        let keys = vec![
            Message::KeyEvent { keycode: KEY_ESC, pressed: true, modifiers: 0 },
        ];
        let out = st.poll(1919, 500, 1920, 1080, 0, keys);
        assert!(matches!(out, ServerOutput::Forward { .. }));
        assert!(st.is_active());
    }

    #[test]
    fn server_escape_combo_ignored_when_inactive() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        let keys = vec![
            Message::KeyEvent { keycode: KEY_LEFTCTRL, pressed: true, modifiers: 0 },
            Message::KeyEvent { keycode: KEY_LEFTALT, pressed: true, modifiers: 0 },
            Message::KeyEvent { keycode: KEY_ESC, pressed: true, modifiers: 0 },
        ];
        let out = st.poll(500, 500, 1920, 1080, 0, keys);
        assert!(matches!(out, ServerOutput::Idle));
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
                assert!(inject.is_some(), "SwitchBack must include inject coordinates");
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
        assert!(matches!(out, ClientOutput::Forward(Message::MouseButton { button: 0, pressed: true })));
    }

    #[test]
    fn client_scroll_forwarding() {
        let mut ct = ClientTransition::new(1920, 1080);
        ct.handle(Message::SwitchScreen {
            direction: Direction::Right,
        });
        let out = ct.handle(Message::MouseScroll { dx: 0, dy: -3 });
        assert!(matches!(out, ClientOutput::Forward(Message::MouseScroll { dx: 0, dy: -3 })));
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
        assert!(matches!(out, ClientOutput::Forward(Message::KeyEvent { keycode: 42, pressed: true, modifiers: 0 })));
    }

    // ===== Key Repeat Tests =====

    #[test]
    fn server_key_repeat_after_delay() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        // Press a non-modifier key (KEY_A = 30)
        let key = vec![Message::KeyEvent { keycode: 30, pressed: true, modifiers: 0 }];
        st.poll(1919, 500, 1920, 1080, 0, key);

        // Hold until we see a repeat event (should happen within delay + interval polls)
        let mut found_repeat = false;
        for _ in 0..(KEY_REPEAT_DELAY + KEY_REPEAT_INTERVAL + 10) {
            let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
            if let ServerOutput::Forward { messages } = out {
                if messages.iter().any(|m| matches!(m, Message::KeyEvent { keycode: 30, pressed: true, .. })) {
                    found_repeat = true;
                    break;
                }
            }
        }
        assert!(found_repeat, "expected repeat event after delay + interval");
    }

    #[test]
    fn server_modifier_keys_do_not_repeat() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        // Press Shift (modifier, keycode 42)
        let key = vec![Message::KeyEvent { keycode: 42, pressed: true, modifiers: 0 }];
        st.poll(1919, 500, 1920, 1080, 0, key);

        // Hold past the delay
        for _ in 0..KEY_REPEAT_DELAY + KEY_REPEAT_INTERVAL + 10 {
            let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
            if let ServerOutput::Forward { messages } = out {
                // Should never have a repeat for keycode 42
                let repeats: Vec<_> = messages.iter().filter(|m| {
                    matches!(m, Message::KeyEvent { keycode: 42, pressed: true, .. })
                }).collect();
                assert!(repeats.is_empty(), "modifier keys should not repeat");
            }
        }
    }

    #[test]
    fn server_key_release_stops_repeat() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        activate_server(&mut st);

        // Press KEY_A
        let key = vec![Message::KeyEvent { keycode: 30, pressed: true, modifiers: 0 }];
        st.poll(1919, 500, 1920, 1080, 0, key);

        // Hold past the delay
        for _ in 1..KEY_REPEAT_DELAY + 5 {
            st.poll(1919, 500, 1920, 1080, 0, vec![]);
        }

        // Release the key
        let key = vec![Message::KeyEvent { keycode: 30, pressed: false, modifiers: 0 }];
        st.poll(1919, 500, 1920, 1080, 0, key);

        // Further polls should NOT produce repeats for keycode 30
        for _ in 0..KEY_REPEAT_INTERVAL * 3 {
            let out = st.poll(1919, 500, 1920, 1080, 0, vec![]);
            if let ServerOutput::Forward { messages } = out {
                let repeats: Vec<_> = messages.iter().filter(|m| {
                    matches!(m, Message::KeyEvent { keycode: 30, pressed: true, .. })
                }).collect();
                assert!(repeats.is_empty(), "released key should not repeat");
            }
        }
    }

    // ===== activate_instant Tests =====

    #[test]
    fn server_activate_instant_right() {
        let mut st = ServerTransition::new(Some(Direction::Right), peer_screen());
        let messages = st.activate_instant(Direction::Right);
        assert!(st.is_active());
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0], Message::SwitchScreen { direction: Direction::Right }));
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
