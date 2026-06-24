//! Health report computation module.
//!
//! Pure domain logic for computing decay health reports. These functions
//! have no HTTP/axum dependencies and accept FSRS configuration as a
//! parameter rather than instantiating calculators directly.

pub mod report;
