use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::sync::Arc;

use color_eyre::eyre::{eyre, Result, WrapErr};

use crate::net::protocol;
use crate::ports::{PairingPrompt, PairingPromptFuture};

fn normalize_pairing_input(input: &str) -> Result<String> {
    let code = input.trim().to_string();
    if code.is_empty() {
        return Err(eyre!("Pairing code cannot be empty"));
    }
    if code.len() != protocol::OTP_DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(eyre!(
            "Invalid pairing code: expected {} decimal digits",
            protocol::OTP_DIGITS
        ));
    }
    Ok(code)
}

fn write_pairing_prompt(mut writer: impl Write) -> Result<()> {
    write!(writer, "Enter pairing code: ").wrap_err("Failed to write pairing prompt")?;
    writer.flush().wrap_err("Failed to flush pairing prompt")
}

trait PairingTerminal: Send + Sync {
    fn is_interactive(&self) -> bool;
    fn write_prompt(&self) -> Result<()>;
    fn read_line(&self) -> Result<String>;
}

struct SystemPairingTerminal;

impl PairingTerminal for SystemPairingTerminal {
    fn is_interactive(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn write_prompt(&self) -> Result<()> {
        write_pairing_prompt(std::io::stderr())
    }

    fn read_line(&self) -> Result<String> {
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .wrap_err("Failed to read pairing code from stdin")?;
        Ok(input)
    }
}

#[derive(Clone)]
pub struct TerminalPairingPrompt {
    terminal: Arc<dyn PairingTerminal>,
}

impl TerminalPairingPrompt {
    pub fn new() -> Self {
        Self {
            terminal: Arc::new(SystemPairingTerminal),
        }
    }

    #[cfg(test)]
    fn with_terminal(terminal: Arc<dyn PairingTerminal>) -> Self {
        Self { terminal }
    }
}

impl Default for TerminalPairingPrompt {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingPrompt for TerminalPairingPrompt {
    fn prompt(&self, addr: SocketAddr) -> PairingPromptFuture<'_> {
        let terminal = self.terminal.clone();
        Box::pin(async move {
            if !terminal.is_interactive() {
                return Err(eyre!(
                    "Server fingerprint is not trusted and no interactive terminal is available for pairing. Run `nexdesk connect {}` from a terminal once, enter the pairing code, then restart the background service.",
                    addr
                ));
            }

            tokio::task::spawn_blocking(move || {
                terminal.write_prompt()?;
                normalize_pairing_input(&terminal.read_line()?)
            })
            .await
            .wrap_err("Pairing prompt task failed")?
        })
    }
}

pub async fn prompt_pairing_code(addr: SocketAddr) -> Result<String> {
    TerminalPairingPrompt::new().prompt(addr).await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    struct FakeTerminal {
        interactive: bool,
        input: Mutex<String>,
        prompts: AtomicUsize,
        reads: AtomicUsize,
    }

    impl FakeTerminal {
        fn new(interactive: bool, input: &str) -> Self {
            Self {
                interactive,
                input: Mutex::new(input.to_string()),
                prompts: AtomicUsize::new(0),
                reads: AtomicUsize::new(0),
            }
        }
    }

    impl PairingTerminal for FakeTerminal {
        fn is_interactive(&self) -> bool {
            self.interactive
        }

        fn write_prompt(&self) -> Result<()> {
            self.prompts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn read_line(&self) -> Result<String> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(lock_recover(&self.input).clone())
        }
    }

    fn lock_recover(mutex: &Mutex<String>) -> MutexGuard<'_, String> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn addr() -> SocketAddr {
        "127.0.0.1:4242".parse().unwrap()
    }

    #[tokio::test]
    async fn interactive_prompt_reads_and_normalizes_code_without_real_stdio() {
        let terminal = Arc::new(FakeTerminal::new(true, " 123456\n"));
        let prompt = TerminalPairingPrompt::with_terminal(terminal.clone());

        assert_eq!(prompt.prompt(addr()).await.unwrap(), "123456");
        assert_eq!(terminal.prompts.load(Ordering::SeqCst), 1);
        assert_eq!(terminal.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn noninteractive_prompt_fails_without_reading_stdio() {
        let terminal = Arc::new(FakeTerminal::new(false, "123456"));
        let prompt = TerminalPairingPrompt::with_terminal(terminal.clone());

        let error = prompt.prompt(addr()).await.unwrap_err();
        assert!(error.to_string().contains("no interactive terminal"));
        assert_eq!(terminal.prompts.load(Ordering::SeqCst), 0);
        assert_eq!(terminal.reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pairing_prompt_is_flushed_and_user_visible() {
        let mut output = Vec::new();
        write_pairing_prompt(&mut output).unwrap();
        assert_eq!(output, b"Enter pairing code: ");
    }

    #[test]
    fn invalid_pairing_codes_are_rejected_before_send() {
        assert_eq!(normalize_pairing_input(" 123456\n").unwrap(), "123456");
        assert!(normalize_pairing_input("\n").is_err());
        assert!(normalize_pairing_input("12345").is_err());
        assert!(normalize_pairing_input("1234567").is_err());
        assert!(normalize_pairing_input("12a456").is_err());
    }
}
