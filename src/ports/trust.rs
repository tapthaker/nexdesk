use color_eyre::eyre::Result;

/// Persistence boundary for peer certificate trust decisions.
pub trait TrustStore: Send + Sync {
    /// Return whether the normalized identity represented by `fingerprint` is trusted.
    fn is_trusted(&self, fingerprint: &str) -> Result<bool>;

    /// Persist trust for `fingerprint`. Implementations must be idempotent.
    fn trust(&self, fingerprint: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysTrusted;

    impl TrustStore for AlwaysTrusted {
        fn is_trusted(&self, _fingerprint: &str) -> Result<bool> {
            Ok(true)
        }

        fn trust(&self, _fingerprint: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn trust_store_is_a_focused_object_safe_port() {
        let store: &dyn TrustStore = &AlwaysTrusted;
        assert!(store.is_trusted("fingerprint").unwrap());
        store.trust("fingerprint").unwrap();
    }
}
