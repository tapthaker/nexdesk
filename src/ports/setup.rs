use std::future::Future;
use std::pin::Pin;

use color_eyre::eyre::Result;

pub type SetupFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Pairs setup with the selected server and returns its verified certificate identity.
pub trait SetupPairing: Send + Sync {
    fn pair<'a>(
        &'a self,
        address: &'a str,
        expected_fingerprint: Option<&'a str>,
    ) -> SetupFuture<'a, Result<String>>;
}

/// Installs the finalized setup arguments as the platform background service.
pub trait SetupServiceInstaller: Send + Sync {
    fn install(&self, arguments: &[String]) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct UnavailablePairing;

    impl SetupPairing for UnavailablePairing {
        fn pair<'a>(
            &'a self,
            _address: &'a str,
            _expected_fingerprint: Option<&'a str>,
        ) -> SetupFuture<'a, Result<String>> {
            Box::pin(async { Err(color_eyre::eyre::eyre!("pairing unavailable")) })
        }
    }

    struct UnavailableService;

    impl SetupServiceInstaller for UnavailableService {
        fn install(&self, _arguments: &[String]) -> Result<()> {
            Err(color_eyre::eyre::eyre!("service unavailable"))
        }
    }

    #[test]
    fn setup_ports_are_object_safe() {
        fn pairing(_: &dyn SetupPairing) {}
        fn service(_: &dyn SetupServiceInstaller) {}
        pairing(&UnavailablePairing);
        service(&UnavailableService);
    }
}
