use color_eyre::eyre::Result;

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
    fn display_control_port_is_object_safe() {
        let control: &dyn DisplaySessionControl = &NoopDisplayControl;
        let _guard = control.inhibit_idle_sleep().unwrap();
        control.wake_display().unwrap();
    }
}
