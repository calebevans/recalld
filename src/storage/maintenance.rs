//! Receipt-bearing legacy maintenance. Skip outcomes never reserve a receipt.
use super::control::WriteReceipt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecaySkip {
    Unchanged,
    Stale,
    Exempt,
}

#[derive(Clone, Debug)]
pub enum DecayMutationResult {
    Committed(WriteReceipt),
    Skipped(DecaySkip),
}
