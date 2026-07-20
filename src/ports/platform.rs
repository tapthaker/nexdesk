use color_eyre::eyre::Result;

/// Source for the local graphical session's lock state.
pub trait LocalSessionLockSource: Send + Sync {
    fn is_locked(&self) -> Result<bool>;
}

/// Process-lifetime guard returned by a platform sleep inhibitor.
pub trait SleepInhibitor: Send {}

impl<T: Send> SleepInhibitor for T {}

/// Platform boundary for display wake requests and idle-sleep inhibition.
pub trait DisplaySessionControl: Send + Sync {
    fn inhibit_idle_sleep(&self) -> Result<Box<dyn SleepInhibitor>>;
    fn wake_display(&self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct UnlockedSession;

    impl LocalSessionLockSource for UnlockedSession {
        fn is_locked(&self) -> Result<bool> {
            Ok(false)
        }
    }

    struct NoopDisplayControl;

    impl DisplaySessionControl for NoopDisplayControl {
        fn inhibit_idle_sleep(&self) -> Result<Box<dyn SleepInhibitor>> {
            Ok(Box::new(()))
        }

        fn wake_display(&self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn local_session_lock_port_is_object_safe() {
        let source: &dyn LocalSessionLockSource = &UnlockedSession;
        assert!(!source.is_locked().unwrap());
    }

    #[test]
    fn display_control_port_is_object_safe() {
        let control: &dyn DisplaySessionControl = &NoopDisplayControl;
        let _guard = control.inhibit_idle_sleep().unwrap();
        control.wake_display().unwrap();
    }
}
