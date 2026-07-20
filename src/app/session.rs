use std::time::Duration;

use color_eyre::eyre::Report;

/// Why the composition root should restart the current process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartReason {
    UpdateInstalled { version: String },
    LatencyWatchdog,
}

/// Outcome from dispatching one top-level Nexdesk command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    RestartRequested(RestartReason),
}

/// Terminal outcome from one client or server session attempt.
///
/// Application code returns lifecycle intent instead of exiting the process.
/// The binary or service composition root decides how to apply that intent.
#[derive(Debug)]
pub enum SessionExit {
    Cancelled,
    Disconnected,
    RetryAfter(Duration),
    RestartRequested(RestartReason),
    Fatal(Report),
}

impl SessionExit {
    pub fn fatal(error: impl Into<Report>) -> Self {
        Self::Fatal(error.into())
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::eyre;

    use super::*;

    #[test]
    fn retry_outcome_preserves_requested_delay() {
        let outcome = SessionExit::RetryAfter(Duration::from_secs(2));
        assert!(matches!(
            outcome,
            SessionExit::RetryAfter(delay) if delay == Duration::from_secs(2)
        ));
    }

    #[test]
    fn restart_outcome_carries_a_typed_reason() {
        let reason = RestartReason::UpdateInstalled {
            version: "v1.2.3".to_string(),
        };
        let session_outcome = SessionExit::RestartRequested(reason.clone());
        let run_outcome = RunOutcome::RestartRequested(reason);

        assert!(matches!(
            session_outcome,
            SessionExit::RestartRequested(RestartReason::UpdateInstalled { version })
                if version == "v1.2.3"
        ));
        assert!(matches!(
            run_outcome,
            RunOutcome::RestartRequested(RestartReason::UpdateInstalled { version })
                if version == "v1.2.3"
        ));
    }

    #[test]
    fn fatal_outcome_retains_the_error_report() {
        let outcome = SessionExit::fatal(eyre!("handshake failed"));
        let SessionExit::Fatal(error) = outcome else {
            panic!("expected fatal outcome");
        };
        assert_eq!(error.to_string(), "handshake failed");
    }
}
