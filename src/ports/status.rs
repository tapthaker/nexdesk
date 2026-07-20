use color_eyre::eyre::Result;

use crate::status::RuntimeStatus;

/// Sink for observable runtime status transitions.
pub trait StatusSink: Send + Sync {
    fn write(&self, status: RuntimeStatus) -> Result<()>;
}
