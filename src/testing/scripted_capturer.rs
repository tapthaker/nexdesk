use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use color_eyre::eyre::{eyre, Result};

use crate::input::capture::{InputCapture, InputCaptureFactory};
use crate::net::protocol::Message;
use crate::testing::ObservationLog;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CaptureOperation {
    Create,
    MousePosition,
    ScreenSize,
    MouseButtons,
    PollKeyEvents,
    SetGrab,
    SetKeyboardGrab,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrabChange {
    All(bool),
    Keyboard(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub enum CaptureObservation {
    Created,
    MousePosition(i32, i32),
    ScreenSize(u32, u32),
    MouseButtons(u8),
    KeyEvents(Vec<String>),
    GrabChanged(GrabChange),
    Failed {
        operation: CaptureOperation,
        message: String,
    },
}

#[derive(Debug, Default)]
struct CaptureState {
    positions: VecDeque<(i32, i32)>,
    screen_sizes: VecDeque<(u32, u32)>,
    buttons: VecDeque<u8>,
    key_events: VecDeque<Vec<Message>>,
    grab_history: Vec<GrabChange>,
    failures: BTreeMap<CaptureOperation, VecDeque<String>>,
}

/// Stateful input capture fake used by deterministic server scenarios.
#[derive(Clone, Debug, Default)]
pub struct ScriptedCapturer {
    state: Arc<Mutex<CaptureState>>,
    observations: ObservationLog<CaptureObservation>,
}

impl ScriptedCapturer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_log(observations: ObservationLog<CaptureObservation>) -> Self {
        Self {
            state: Arc::new(Mutex::new(CaptureState::default())),
            observations,
        }
    }

    pub fn push_position(&self, x: i32, y: i32) {
        lock_recover(&self.state).positions.push_back((x, y));
    }

    pub fn push_screen_size(&self, width: u32, height: u32) {
        lock_recover(&self.state)
            .screen_sizes
            .push_back((width, height));
    }

    pub fn push_buttons(&self, buttons: u8) {
        lock_recover(&self.state).buttons.push_back(buttons);
    }

    pub fn push_key_events(&self, events: Vec<Message>) {
        lock_recover(&self.state).key_events.push_back(events);
    }

    pub fn fail_next(&self, operation: CaptureOperation, message: impl Into<String>) {
        lock_recover(&self.state)
            .failures
            .entry(operation)
            .or_default()
            .push_back(message.into());
    }

    pub fn grab_history(&self) -> Vec<GrabChange> {
        lock_recover(&self.state).grab_history.clone()
    }

    pub fn observations(&self) -> ObservationLog<CaptureObservation> {
        self.observations.clone()
    }

    pub fn remaining_positions(&self) -> usize {
        lock_recover(&self.state).positions.len()
    }

    pub fn remaining_screen_sizes(&self) -> usize {
        lock_recover(&self.state).screen_sizes.len()
    }

    pub fn remaining_buttons(&self) -> usize {
        lock_recover(&self.state).buttons.len()
    }

    pub fn remaining_key_polls(&self) -> usize {
        lock_recover(&self.state).key_events.len()
    }

    fn fail_if_scripted(&self, operation: CaptureOperation) -> Result<()> {
        let failure = {
            let mut state = lock_recover(&self.state);
            let message = state
                .failures
                .get_mut(&operation)
                .and_then(VecDeque::pop_front);
            if state
                .failures
                .get(&operation)
                .is_some_and(VecDeque::is_empty)
            {
                state.failures.remove(&operation);
            }
            message
        };
        let Some(message) = failure else {
            return Ok(());
        };
        self.observations.record(CaptureObservation::Failed {
            operation,
            message: message.clone(),
        });
        Err(eyre!(message))
    }

    fn unexpected(operation: CaptureOperation) -> color_eyre::Report {
        eyre!("ScriptedCapturer unexpected {operation:?} call: script is empty")
    }
}

impl InputCapture for ScriptedCapturer {
    fn mouse_position(&self) -> Result<(i32, i32)> {
        self.fail_if_scripted(CaptureOperation::MousePosition)?;
        let (x, y) = lock_recover(&self.state)
            .positions
            .pop_front()
            .ok_or_else(|| Self::unexpected(CaptureOperation::MousePosition))?;
        self.observations
            .record(CaptureObservation::MousePosition(x, y));
        Ok((x, y))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        self.fail_if_scripted(CaptureOperation::ScreenSize)?;
        let (width, height) = lock_recover(&self.state)
            .screen_sizes
            .pop_front()
            .ok_or_else(|| Self::unexpected(CaptureOperation::ScreenSize))?;
        self.observations
            .record(CaptureObservation::ScreenSize(width, height));
        Ok((width, height))
    }

    fn mouse_buttons(&self) -> Result<u8> {
        self.fail_if_scripted(CaptureOperation::MouseButtons)?;
        let buttons = lock_recover(&self.state)
            .buttons
            .pop_front()
            .ok_or_else(|| Self::unexpected(CaptureOperation::MouseButtons))?;
        self.observations
            .record(CaptureObservation::MouseButtons(buttons));
        Ok(buttons)
    }

    fn poll_key_events(&mut self) -> Result<Vec<Message>> {
        self.fail_if_scripted(CaptureOperation::PollKeyEvents)?;
        let events = lock_recover(&self.state)
            .key_events
            .pop_front()
            .ok_or_else(|| Self::unexpected(CaptureOperation::PollKeyEvents))?;
        self.observations.record(CaptureObservation::KeyEvents(
            events
                .iter()
                .map(crate::net::protocol::message_summary)
                .collect(),
        ));
        Ok(events)
    }

    fn set_grab(&mut self, grab: bool) -> Result<()> {
        self.fail_if_scripted(CaptureOperation::SetGrab)?;
        let change = GrabChange::All(grab);
        lock_recover(&self.state).grab_history.push(change);
        self.observations
            .record(CaptureObservation::GrabChanged(change));
        Ok(())
    }

    fn set_keyboard_grab(&mut self, grab: bool) -> Result<()> {
        self.fail_if_scripted(CaptureOperation::SetKeyboardGrab)?;
        let change = GrabChange::Keyboard(grab);
        lock_recover(&self.state).grab_history.push(change);
        self.observations
            .record(CaptureObservation::GrabChanged(change));
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ScriptedCaptureFactory {
    capturer: ScriptedCapturer,
}

impl ScriptedCaptureFactory {
    pub fn new(capturer: ScriptedCapturer) -> Self {
        Self { capturer }
    }

    pub fn capturer(&self) -> ScriptedCapturer {
        self.capturer.clone()
    }
}

impl InputCaptureFactory for ScriptedCaptureFactory {
    fn create(&self) -> Result<Box<dyn InputCapture>> {
        self.capturer.fail_if_scripted(CaptureOperation::Create)?;
        self.capturer
            .observations
            .record(CaptureObservation::Created);
        Ok(Box::new(self.capturer.clone()))
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_capture_state_and_records_grabs() {
        let capturer = ScriptedCapturer::new();
        capturer.push_position(12, 34);
        capturer.push_screen_size(2560, 1440);
        capturer.push_buttons(0b101);
        capturer.push_key_events(vec![Message::KeyEvent {
            keycode: 30,
            pressed: true,
            modifiers: 0,
        }]);
        let factory = ScriptedCaptureFactory::new(capturer.clone());
        let mut session = factory.create().unwrap();

        assert_eq!(session.mouse_position().unwrap(), (12, 34));
        assert_eq!(session.screen_size().unwrap(), (2560, 1440));
        assert_eq!(session.mouse_buttons().unwrap(), 0b101);
        assert_eq!(session.poll_key_events().unwrap().len(), 1);
        session.set_grab(true).unwrap();
        session.set_keyboard_grab(false).unwrap();

        assert_eq!(
            capturer.grab_history(),
            vec![GrabChange::All(true), GrabChange::Keyboard(false)]
        );
        assert_eq!(capturer.observations().snapshot().len(), 7);
    }

    #[test]
    fn scripts_failures_and_rejects_unscripted_reads() {
        let capturer = ScriptedCapturer::new();
        capturer.fail_next(CaptureOperation::MousePosition, "pointer unavailable");

        assert!(capturer
            .mouse_position()
            .unwrap_err()
            .to_string()
            .contains("pointer unavailable"));
        assert!(capturer
            .screen_size()
            .unwrap_err()
            .to_string()
            .contains("script is empty"));
        assert!(matches!(
            &capturer.observations().snapshot()[0].event,
            CaptureObservation::Failed {
                operation: CaptureOperation::MousePosition,
                ..
            }
        ));
    }

    #[test]
    fn factory_creation_failure_is_fifo() {
        let capturer = ScriptedCapturer::new();
        capturer.fail_next(CaptureOperation::Create, "device denied");
        let factory = ScriptedCaptureFactory::new(capturer);

        let error = match factory.create() {
            Ok(_) => panic!("expected capture creation to fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("device denied"));
        assert!(factory.create().is_ok());
    }
}
