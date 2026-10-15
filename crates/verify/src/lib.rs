//! Settlement for staked games: the encodings in `docs/SETTLEMENT.md`, session keys, forced-move
//! encryption, the transcript format, and the replay that the CLI and the SP1 program share.

pub mod encode;
pub mod force;
pub mod keys;
pub mod replay;
pub mod transcript;

pub use alloy_primitives;
pub use encode::PublicValues;
pub use replay::{Decrypt, VerifyError, check_result, verify};
pub use transcript::{Transcript, TranscriptBuilder};
