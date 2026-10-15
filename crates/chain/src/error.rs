use std::time::Duration;

use alloy::{primitives::TxHash, transports::TransportError};

/// Errors from [`ChainClient`](crate::ChainClient); only `Rpc` and `Timeout` are retried.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    /// Transport, HTTP or JSON-RPC failure at the node.
    #[error("{op}: rpc {host} failed (http status {}, {attempts} attempt(s)): {source}", status_text(*.status))]
    Rpc {
        op: &'static str,
        host: String,
        status: Option<u16>,
        attempts: u32,
        #[source]
        source: TransportError,
    },
    /// A request, or the wait for a transaction's receipt, ran out of time.
    #[error("{op}: rpc {host} timed out ({waited:?} per attempt, tx {}, {attempts} attempt(s))", tx_text(*.tx))]
    Timeout {
        op: &'static str,
        host: String,
        waited: Duration,
        tx: Option<TxHash>,
        attempts: u32,
    },
    /// The call or transaction reverted; `reason` is the decoded error when known.
    #[error("{op} reverted: {reason} (tx {})", tx_text(*.tx))]
    Revert {
        op: &'static str,
        reason: String,
        tx: Option<TxHash>,
    },
    /// Bad configuration or arguments.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The node or contract answered with data this client cannot use.
    #[error("{op}: unexpected response from rpc {host}: {detail}")]
    Unexpected {
        op: &'static str,
        host: String,
        detail: String,
    },
}

impl ChainError {
    /// Whether another attempt might succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Rpc { .. } | Self::Timeout { .. })
    }

    pub(crate) fn with_attempts(mut self, n: u32) -> Self {
        if let Self::Rpc { attempts, .. } | Self::Timeout { attempts, .. } = &mut self {
            *attempts = n;
        }
        self
    }
}

fn status_text(status: Option<u16>) -> String {
    status.map_or_else(|| "none".to_owned(), |s| s.to_string())
}

fn tx_text(tx: Option<TxHash>) -> String {
    tx.map_or_else(|| "none".to_owned(), |tx| tx.to_string())
}
