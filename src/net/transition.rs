use std::collections::HashSet;

use crate::cursor::edge;
use crate::net::protocol::{Direction, Message, ScreenLayout};

// --- Constants ---

const EDGE_DWELL_THRESHOLD: u32 = 50;
const SERVER_EDGE_COOLDOWN: u32 = 125;
const INSET: i32 = 20;
const CLIENT_EDGE_DWELL: u32 = 8;

// Do not synthesize keyboard repeat in nexdesk. The client OS will repeat
// naturally after a forwarded key-down. Synthesizing repeat here is dangerous:
// if the Linux capturer misses a key-up during a switch-back/grab transition,
// the stale pressed key becomes an endless stream of key-downs on the client.

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
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u8>,
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
            pressed_buttons: HashSet::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
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

    pub fn update_button(&mut self, button: u8, pressed: bool) {
        if pressed {
            self.pressed_buttons.insert(button);
        } else {
            self.pressed_buttons.remove(&button);
        }
    }

    pub fn release_remote_inputs(&mut self) -> Vec<Message> {
        let mut keys: Vec<u32> = self.pressed_keys.drain().collect();
        keys.sort_unstable();
        let mut buttons: Vec<u8> = self.pressed_buttons.drain().collect();
        buttons.sort_unstable();

        let mut releases = keys
            .into_iter()
            .map(|keycode| Message::KeyEvent {
                keycode,
                pressed: false,
                modifiers: 0,
            })
            .collect::<Vec<_>>();
        releases.extend(buttons.into_iter().map(|button| Message::MouseButton {
            button,
            pressed: false,
        }));
        releases
    }

    /// Activate immediately (layer-shell: edge detection is instant, no dwell needed).
    pub fn activate_instant(&mut self, direction: Direction) -> Vec<Message> {
        self.active = true;
        self.last_buttons = 0;
        self.last_x = 0;
        self.last_y = 0;
        self.edge_dwell = 0;
        self.edge_cooldown = 0;
        self.pressed_buttons.clear();

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

    fn push_key_events(&mut self, key_events: Vec<Message>, messages: &mut Vec<Message>) {
        // No synthetic repeats. Forward only real press/release transitions.
        messages.extend(key_events);
    }

    pub fn poll_active_keys(&mut self, key_events: Vec<Message>) -> ServerOutput {
        if !self.active {
            return ServerOutput::Idle;
        }

        self.update_pressed_keys(&key_events);
        if self.is_escape_combo() {
            self.active = false;
            return ServerOutput::ForceRelease {
                messages: self.release_remote_inputs(),
            };
        }
        if self.shortcut_direction().is_some() {
            self.active = false;
            self.edge_cooldown = SERVER_EDGE_COOLDOWN;
            return ServerOutput::ShortcutRelease {
                messages: self.release_remote_inputs(),
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
            return ServerOutput::ForceRelease {
                messages: self.release_remote_inputs(),
            };
        }

        if let Some(dir) = self.shortcut_direction() {
            if self.active {
                self.active = false;
                self.edge_cooldown = SERVER_EDGE_COOLDOWN;
                return ServerOutput::ShortcutRelease {
                    messages: self.release_remote_inputs(),
                };
            }

            if self.edge_cooldown == 0 {
                self.active = true;
                self.last_buttons = buttons;
                self.pressed_buttons.clear();
                self.last_x = mx;
                self.last_y = my;
                self.edge_dwell = 0;

                let pw = self.peer_screen.width as i32;
                let ph = self.peer_screen.height as i32;
                let (rx, ry) = match dir {
                    Direction::Right => (INSET, ph / 2),
                    Direction::Left => (pw - 1 - INSET, ph / 2),
                    Direction::Down => (pw / 2, INSET),
                    Direction::Up => (pw / 2, ph - 1 - INSET),
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
                self.pressed_buttons.clear();
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
                        self.update_button(bit, now);
                        messages.push(Message::MouseButton {
                            button: bit,
                            pressed: now,
                        });
                    }
                }
                self.last_buttons = buttons;
            }

            // Keyboard events: forward originals and synthesize repeats.
            self.push_key_events(key_events, &mut messages);

            ServerOutput::Forward { messages }
        }
    }

    pub fn update_peer_screen(&mut self, screen: ScreenLayout) {
        self.peer_screen = screen;
    }

    /// Deactivate because the client hit its return edge.
    ///
    /// Any keys currently considered down on the remote side must be released
    /// before we stop forwarding input; otherwise the client OS can be left
    /// with a synthetic key-down that never gets its key-up.
    pub fn on_switch_back(&mut self) -> Vec<Message> {
        self.active = false;
        self.edge_cooldown = SERVER_EDGE_COOLDOWN;
        self.release_remote_inputs()
    }

    pub fn deactivate(&mut self) {
        self.active = false;
    }

    pub fn deactivate_for_shortcut(&mut self) -> Vec<Message> {
        self.active = false;
        self.edge_cooldown = SERVER_EDGE_COOLDOWN;
        self.release_remote_inputs()
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

    pub fn update_screen_size(&mut self, w: u32, h: u32) {
        self.screen_w = w;
        self.screen_h = h;
        self.cursor_x = self.cursor_x.clamp(0, w as i32 - 1);
        self.cursor_y = self.cursor_y.clamp(0, h as i32 - 1);
    }

    /// Stop accepting remote input because the server reclaimed control.
    pub fn release_control(&mut self) {
        self.active = false;
        self.first_move = false;
        self.edge_dwell = 0;
        self.switch_back_edge = None;
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
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    enum GeneratedClientEvent {
        Message(Message),
        Resize { width: u32, height: u32 },
    }

    fn direction_strategy() -> impl Strategy<Value = Direction> {
        prop_oneof![
            Just(Direction::Left),
            Just(Direction::Right),
            Just(Direction::Up),
            Just(Direction::Down),
        ]
    }

    #[derive(Clone, Debug)]
    enum GeneratedServerEvent {
        Poll {
            x: i32,
            y: i32,
            buttons: u8,
            keys: Vec<Message>,
        },
        PollActiveKeys(Vec<Message>),
        ActivateInstant(Direction),
        SwitchBack,
        ShortcutDeactivate,
    }

    fn key_event_strategy() -> impl Strategy<Value = Message> {
        (0u32..=255, any::<bool>(), any::<u16>()).prop_map(|(keycode, pressed, modifiers)| {
            Message::KeyEvent {
                keycode,
                pressed,
                modifiers,
            }
        })
    }

    fn server_event_strategy() -> impl Strategy<Value = GeneratedServerEvent> {
        prop_oneof![
            (
                -100i32..=2_100,
                -100i32..=1_200,
                0u8..=7,
                prop::collection::vec(key_event_strategy(), 0..5),
            )
                .prop_map(|(x, y, buttons, keys)| GeneratedServerEvent::Poll {
                    x,
                    y,
                    buttons,
                    keys,
                }),
            prop::collection::vec(key_event_strategy(), 0..5)
                .prop_map(GeneratedServerEvent::PollActiveKeys),
            direction_strategy().prop_map(GeneratedServerEvent::ActivateInstant),
            Just(GeneratedServerEvent::SwitchBack),
            Just(GeneratedServerEvent::ShortcutDeactivate),
        ]
    }

    fn client_event_strategy() -> impl Strategy<Value = GeneratedClientEvent> {
        prop_oneof![
            direction_strategy().prop_map(|direction| GeneratedClientEvent::Message(
                Message::SwitchScreen { direction }
            )),
            (-10_000i32..=10_000, -10_000i32..=10_000)
                .prop_map(|(x, y)| { GeneratedClientEvent::Message(Message::MouseMove { x, y }) }),
            (0u8..=7, any::<bool>()).prop_map(|(button, pressed)| {
                GeneratedClientEvent::Message(Message::MouseButton { button, pressed })
            }),
            (-100i16..=100, -100i16..=100).prop_map(|(dx, dy)| {
                GeneratedClientEvent::Message(Message::MouseScroll {
                    dx: f64::from(dx),
                    dy: f64::from(dy),
                    phase: crate::net::protocol::ScrollPhase::None,
                })
            }),
            (0u32..=255, any::<bool>(), any::<u16>()).prop_map(|(keycode, pressed, modifiers)| {
                GeneratedClientEvent::Message(Message::KeyEvent {
                    keycode,
                    pressed,
                    modifiers,
                })
            }),
            (1u32..=16_384, 1u32..=16_384)
                .prop_map(|(width, height)| { GeneratedClientEvent::Resize { width, height } }),
            any::<u64>().prop_map(
                |timestamp| GeneratedClientEvent::Message(Message::Heartbeat { timestamp })
            ),
        ]
    }

    fn peer_screen() -> ScreenLayout {
        ScreenLayout {
            width: 2560,
            height: 1440,
        }
    }

    // ===== Server Tests =====

    fn apply_remote_input_messages(
        messages: &[Message],
        keys: &mut HashSet<u32>,
        buttons: &mut HashSet<u8>,
    ) {
        for message in messages {
            match message {
                Message::KeyEvent {
                    keycode, pressed, ..
                } => {
                    if *pressed {
                        keys.insert(*keycode);
                    } else {
                        keys.remove(keycode);
                    }
                }
                Message::MouseButton { button, pressed } => {
                    if *pressed {
                        buttons.insert(*button);
                    } else {
                        buttons.remove(button);
                    }
                }
                _ => {}
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn generated_server_sequences_preserve_grab_and_held_input_safety(
            events in prop::collection::vec(server_event_strategy(), 0..256),
        ) {
            let mut transition = ServerTransition::new(None, peer_screen());
            let mut remote_keys = HashSet::new();
            let mut remote_buttons = HashSet::new();
            let mut grabbed = false;

            for event in events {
                let output = match event {
                    GeneratedServerEvent::Poll { x, y, buttons, keys } => {
                        Some(transition.poll(x, y, 1920, 1080, buttons, keys))
                    }
                    GeneratedServerEvent::PollActiveKeys(keys) => {
                        Some(transition.poll_active_keys(keys))
                    }
                    GeneratedServerEvent::ActivateInstant(direction) => {
                        if transition.is_active() {
                            continue;
                        }
                        let messages = transition.activate_instant(direction);
                        apply_remote_input_messages(
                            &messages,
                            &mut remote_keys,
                            &mut remote_buttons,
                        );
                        None
                    }
                    GeneratedServerEvent::SwitchBack => {
                        let messages = transition.on_switch_back();
                        apply_remote_input_messages(
                            &messages,
                            &mut remote_keys,
                            &mut remote_buttons,
                        );
                        grabbed = false;
                        prop_assert!(transition.pressed_keys.is_empty());
                        prop_assert!(transition.pressed_buttons.is_empty());
                        None
                    }
                    GeneratedServerEvent::ShortcutDeactivate => {
                        let messages = transition.deactivate_for_shortcut();
                        apply_remote_input_messages(
                            &messages,
                            &mut remote_keys,
                            &mut remote_buttons,
                        );
                        grabbed = false;
                        prop_assert!(transition.pressed_keys.is_empty());
                        prop_assert!(transition.pressed_buttons.is_empty());
                        None
                    }
                };

                if let Some(output) = output {
                    match output {
                        ServerOutput::Idle => {}
                        ServerOutput::Activate { messages, grab } => {
                            prop_assert!(grab, "poll activation must request an input grab");
                            grabbed = grab;
                            apply_remote_input_messages(
                                &messages,
                                &mut remote_keys,
                                &mut remote_buttons,
                            );
                        }
                        ServerOutput::Forward { messages } => apply_remote_input_messages(
                            &messages,
                            &mut remote_keys,
                            &mut remote_buttons,
                        ),
                        ServerOutput::ShortcutRelease { messages }
                        | ServerOutput::ForceRelease { messages } => {
                            apply_remote_input_messages(
                                &messages,
                                &mut remote_keys,
                                &mut remote_buttons,
                            );
                            grabbed = false;
                            prop_assert!(transition.pressed_keys.is_empty());
                            prop_assert!(transition.pressed_buttons.is_empty());
                        }
                    }
                }

                if !transition.is_active() {
                    prop_assert!(!grabbed, "inactive server retained an input grab");
                    prop_assert!(remote_keys.is_empty(), "inactive server left remote keys held");
                    prop_assert!(
                        remote_buttons.is_empty(),
                        "inactive server left remote buttons held"
                    );
                }
            }
        }
    }

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

    // ===== Client Tests =====

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn generated_client_sequences_preserve_cursor_and_input_safety(
            initial_width in 1u32..=16_384,
            initial_height in 1u32..=16_384,
            events in prop::collection::vec(client_event_strategy(), 0..256),
        ) {
            let mut transition = ClientTransition::new(initial_width, initial_height);
            let mut width = initial_width;
            let mut height = initial_height;

            for event in events {
                match event {
                    GeneratedClientEvent::Resize { width: next_width, height: next_height } => {
                        transition.update_screen_size(next_width, next_height);
                        width = next_width;
                        height = next_height;
                    }
                    GeneratedClientEvent::Message(message) => {
                        let was_active = transition.active;
                        let is_press = matches!(
                            &message,
                            Message::KeyEvent { pressed: true, .. }
                                | Message::MouseButton { pressed: true, .. }
                        );
                        let output = transition.handle(message);

                        match output {
                            ClientOutput::InjectMove { x, y } => {
                                prop_assert!(was_active);
                                prop_assert!((0..width as i32).contains(&x));
                                prop_assert!((0..height as i32).contains(&y));
                            }
                            ClientOutput::SwitchBack { inject: Some((x, y)), .. } => {
                                prop_assert!(was_active);
                                prop_assert!((0..width as i32).contains(&x));
                                prop_assert!((0..height as i32).contains(&y));
                                prop_assert!(!transition.active);
                            }
                            ClientOutput::Forward(_) if is_press => {
                                prop_assert!(was_active, "inactive client forwarded an input press");
                            }
                            ClientOutput::Activate => prop_assert!(transition.active),
                            ClientOutput::Ignore
                            | ClientOutput::Forward(_)
                            | ClientOutput::SwitchBack { inject: None, .. } => {}
                        }
                    }
                }

                prop_assert!((0..width as i32).contains(&transition.cursor_x));
                prop_assert!((0..height as i32).contains(&transition.cursor_y));
            }
        }
    }

    #[test]
    fn client_forced_release_stops_remote_input() {
        let mut transition = ClientTransition::new(1920, 1080);
        assert!(matches!(
            transition.handle(Message::SwitchScreen {
                direction: Direction::Right,
            }),
            ClientOutput::Activate
        ));

        transition.release_control();

        assert!(matches!(
            transition.handle(Message::MouseButton {
                button: 0,
                pressed: true,
            }),
            ClientOutput::Ignore
        ));
        assert!(matches!(
            transition.handle(Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            }),
            ClientOutput::Ignore
        ));
    }

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
        // key-down messages. The client OS handles natural key repeat.
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
