/// Supplies identifiers for outgoing file transfers.
///
/// Production uses operating-system-seeded randomness through `rand`; tests can
/// inject a deterministic sequence without changing runtime behavior.
pub trait TransferIdSource {
    fn next_transfer_id(&self) -> u64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RandomTransferIdSource;

impl TransferIdSource for RandomTransferIdSource {
    fn next_transfer_id(&self) -> u64 {
        rand::random()
    }
}
