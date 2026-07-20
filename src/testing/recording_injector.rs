use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use color_eyre::eyre::{eyre, Result};

use crate::input::inject::{InputInjector, InputInjectorFactory};
use crate::net::protocol::{self, Message, ScrollPhase};
use crate::testing::ObservationLog;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InjectorOperation {
    Create,
    Inject,
    MoveMouse,
    ScreenSize,
    SetCursorVisible,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RecordedInput {
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: u8,
        pressed: bool,
    },
    MouseScroll {
        dx: f64,
        dy: f64,
        phase: ScrollPhase,
    },
    KeyEvent {
        keycode: u32,
        pressed: bool,
        modifiers: u16,
    },
    Other(String),
}

impl From<&Message> for RecordedInput {
    fn from(message: &Message) -> Self {
        match message {
            Message::MouseMove { x, y } => Self::MouseMove { x: *x, y: *y },
            Message::MouseButton { button, pressed } => Self::MouseButton {
                button: *button,
                pressed: *pressed,
            },
            Message::MouseScroll { dx, dy, phase } => Self::MouseScroll {
                dx: *dx,
                dy: *dy,
                phase: *phase,
            },
            Message::KeyEvent {
                keycode,
                pressed,
                modifiers,
            } => Self::KeyEvent {
                keycode: *keycode,
                pressed: *pressed,
                modifiers: *modifiers,
            },
            other => Self::Other(protocol::message_summary(other)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum InjectorObservation {
    Created,
    Inject(RecordedInput),
    MoveMouse {
        x: i32,
        y: i32,
    },
    ScreenSize,
    CursorVisible(bool),
    Failed {
        operation: InjectorOperation,
        message: String,
    },
}

#[derive(Debug)]
struct InjectorState {
    screen_size: (u32, u32),
    injected: Vec<RecordedInput>,
    mouse_moves: Vec<(i32, i32)>,
    cursor_visibility: Vec<bool>,
    pressed_keys: BTreeSet<u32>,
    pressed_buttons: BTreeSet<u8>,
    failures: BTreeMap<InjectorOperation, VecDeque<String>>,
}

/// Stateful input injector fake used by deterministic client scenarios.
#[derive(Clone, Debug)]
pub struct RecordingInjector {
    state: Arc<Mutex<InjectorState>>,
    observations: ObservationLog<InjectorObservation>,
}

impl RecordingInjector {
    pub fn new(screen_size: (u32, u32)) -> Self {
        Self::with_log(screen_size, ObservationLog::new())
    }

    pub fn with_log(
        screen_size: (u32, u32),
        observations: ObservationLog<InjectorObservation>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(InjectorState {
                screen_size,
                injected: Vec::new(),
                mouse_moves: Vec::new(),
                cursor_visibility: Vec::new(),
                pressed_keys: BTreeSet::new(),
                pressed_buttons: BTreeSet::new(),
                failures: BTreeMap::new(),
            })),
            observations,
        }
    }

    pub fn fail_next(&self, operation: InjectorOperation, message: impl Into<String>) {
        lock_recover(&self.state)
            .failures
            .entry(operation)
            .or_default()
            .push_back(message.into());
    }

    pub fn set_screen_size(&self, screen_size: (u32, u32)) {
        lock_recover(&self.state).screen_size = screen_size;
    }

    pub fn injected(&self) -> Vec<RecordedInput> {
        lock_recover(&self.state).injected.clone()
    }

    pub fn mouse_moves(&self) -> Vec<(i32, i32)> {
        lock_recover(&self.state).mouse_moves.clone()
    }

    pub fn cursor_visibility(&self) -> Vec<bool> {
        lock_recover(&self.state).cursor_visibility.clone()
    }

    pub fn pressed_keys(&self) -> BTreeSet<u32> {
        lock_recover(&self.state).pressed_keys.clone()
    }

    pub fn pressed_buttons(&self) -> BTreeSet<u8> {
        lock_recover(&self.state).pressed_buttons.clone()
    }

    pub fn observations(&self) -> ObservationLog<InjectorObservation> {
        self.observations.clone()
    }

    fn take_failure(&self, operation: InjectorOperation) -> Option<String> {
        let mut state = lock_recover(&self.state);
        let failures = state.failures.get_mut(&operation)?;
        let failure = failures.pop_front();
        if failures.is_empty() {
            state.failures.remove(&operation);
        }
        failure
    }

    fn fail_if_scripted(&self, operation: InjectorOperation) -> Result<()> {
        let Some(message) = self.take_failure(operation) else {
            return Ok(());
        };
        self.observations.record(InjectorObservation::Failed {
            operation,
            message: message.clone(),
        });
        Err(eyre!(message))
    }
}

impl InputInjector for RecordingInjector {
    fn inject(&mut self, event: &Message) -> Result<()> {
        self.fail_if_scripted(InjectorOperation::Inject)?;
        let event = RecordedInput::from(event);
        self.observations
            .record(InjectorObservation::Inject(event.clone()));

        let mut state = lock_recover(&self.state);
        match &event {
            RecordedInput::KeyEvent {
                keycode, pressed, ..
            } => {
                if *pressed {
                    state.pressed_keys.insert(*keycode);
                } else {
                    state.pressed_keys.remove(keycode);
                }
            }
            RecordedInput::MouseButton { button, pressed } => {
                if *pressed {
                    state.pressed_buttons.insert(*button);
                } else {
                    state.pressed_buttons.remove(button);
                }
            }
            _ => {}
        }
        state.injected.push(event);
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        self.fail_if_scripted(InjectorOperation::MoveMouse)?;
        self.observations
            .record(InjectorObservation::MoveMouse { x, y });
        lock_recover(&self.state).mouse_moves.push((x, y));
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        self.fail_if_scripted(InjectorOperation::ScreenSize)?;
        self.observations.record(InjectorObservation::ScreenSize);
        Ok(lock_recover(&self.state).screen_size)
    }

    fn set_cursor_visible(&mut self, visible: bool) -> Result<()> {
        self.fail_if_scripted(InjectorOperation::SetCursorVisible)?;
        self.observations
            .record(InjectorObservation::CursorVisible(visible));
        lock_recover(&self.state).cursor_visibility.push(visible);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RecordingInjectorFactory {
    injector: RecordingInjector,
}

impl RecordingInjectorFactory {
    pub fn new(injector: RecordingInjector) -> Self {
        Self { injector }
    }

    pub fn injector(&self) -> RecordingInjector {
        self.injector.clone()
    }
}

impl InputInjectorFactory for RecordingInjectorFactory {
    fn create(&self) -> Result<Box<dyn InputInjector>> {
        self.injector.fail_if_scripted(InjectorOperation::Create)?;
        self.injector
            .observations
            .record(InjectorObservation::Created);
        Ok(Box::new(self.injector.clone()))
    }
}

fn lock_recover(mutex: &Mutex<InjectorState>) -> MutexGuard<'_, InjectorState> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_input_state_cursor_and_mouse_operations() {
        let injector = RecordingInjector::new((2560, 1440));
        let factory = RecordingInjectorFactory::new(injector.clone());
        let mut session_injector = factory.create().unwrap();

        session_injector
            .inject(&Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            })
            .unwrap();
        session_injector
            .inject(&Message::MouseButton {
                button: 0,
                pressed: true,
            })
            .unwrap();
        session_injector.move_mouse(100, 200).unwrap();
        session_injector.set_cursor_visible(false).unwrap();

        assert_eq!(session_injector.screen_size().unwrap(), (2560, 1440));
        assert_eq!(injector.pressed_keys(), BTreeSet::from([30]));
        assert_eq!(injector.pressed_buttons(), BTreeSet::from([0]));
        assert_eq!(injector.mouse_moves(), vec![(100, 200)]);
        assert_eq!(injector.cursor_visibility(), vec![false]);
        assert!(matches!(
            injector.observations().snapshot().first().unwrap().event,
            InjectorObservation::Created
        ));
    }

    #[test]
    fn releases_remove_pressed_state() {
        let mut injector = RecordingInjector::new((1920, 1080));
        for pressed in [true, false] {
            injector
                .inject(&Message::KeyEvent {
                    keycode: 30,
                    pressed,
                    modifiers: 0,
                })
                .unwrap();
            injector
                .inject(&Message::MouseButton { button: 0, pressed })
                .unwrap();
        }
        assert!(injector.pressed_keys().is_empty());
        assert!(injector.pressed_buttons().is_empty());
    }

    #[test]
    fn scripted_failure_is_consumed_by_matching_operation() {
        let mut injector = RecordingInjector::new((1920, 1080));
        injector.fail_next(InjectorOperation::Inject, "injection unavailable");

        let error = injector
            .inject(&Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "injection unavailable");
        assert!(injector.injected().is_empty());

        injector
            .inject(&Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            })
            .unwrap();
        assert_eq!(injector.pressed_keys(), BTreeSet::from([30]));
    }

    #[test]
    fn factory_creation_failure_is_scriptable() {
        let injector = RecordingInjector::new((1920, 1080));
        injector.fail_next(InjectorOperation::Create, "factory unavailable");
        let factory = RecordingInjectorFactory::new(injector);

        let error = factory.create().err().expect("factory should fail once");
        assert_eq!(error.to_string(), "factory unavailable");
        assert!(factory.create().is_ok());
    }
}
