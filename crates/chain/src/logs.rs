use std::time::Duration;

use alloy::{
    primitives::{B256, U256},
    providers::Provider,
    rpc::types::{Filter, Log},
    sol_types::{SolEvent, SolEventInterface},
};
use tokio::time::{Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{
    ChainClient, ChainError,
    bindings::IWerewolfEscrow::{
        ActionForced, Challenged, IWerewolfEscrowEvents, Joined, RandomnessFulfilled,
        ReceiptDisputed,
    },
};

/// Where a log sits on chain; orders by block, then log index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogPosition {
    pub block: u64,
    pub index: u64,
}

impl ChainClient {
    /// `Joined` logs for a game, from `from_block` to the head.
    pub async fn joined(
        &self,
        game_id: U256,
        from_block: u64,
    ) -> Result<Vec<(Joined, LogPosition)>, ChainError> {
        let filter = self.event_filter::<Joined>().topic1(word(game_id));
        self.events("joined", &filter, from_block, None).await
    }

    /// `ActionForced` logs for a game's `round`, from `from_block` up to and including `upto`.
    pub async fn forced_actions(
        &self,
        game_id: U256,
        round: u32,
        from_block: u64,
        upto: LogPosition,
    ) -> Result<Vec<(ActionForced, LogPosition)>, ChainError> {
        let filter = self
            .event_filter::<ActionForced>()
            .topic1(word(game_id))
            .topic2(word(U256::from(round)));
        let mut forced = self
            .events("forced_actions", &filter, from_block, Some(upto.block))
            .await?;
        forced.retain(|(_, at)| *at <= upto);
        Ok(forced)
    }

    /// `RandomnessFulfilled` logs for a game, from `from_block` to the head.
    pub async fn randomness_fulfilled(
        &self,
        game_id: U256,
        from_block: u64,
    ) -> Result<Vec<(RandomnessFulfilled, LogPosition)>, ChainError> {
        let filter = self
            .event_filter::<RandomnessFulfilled>()
            .topic1(word(game_id));
        self.events("randomness_fulfilled", &filter, from_block, None)
            .await
    }

    /// `Challenged` logs for a game, from `from_block` to the head.
    pub async fn challenges(
        &self,
        game_id: U256,
        from_block: u64,
    ) -> Result<Vec<(Challenged, LogPosition)>, ChainError> {
        let filter = self.event_filter::<Challenged>().topic1(word(game_id));
        self.events("challenges", &filter, from_block, None).await
    }

    /// `ReceiptDisputed` logs for a game, from `from_block` to the head.
    pub async fn receipt_disputes(
        &self,
        game_id: U256,
        from_block: u64,
    ) -> Result<Vec<(ReceiptDisputed, LogPosition)>, ChainError> {
        let filter = self.event_filter::<ReceiptDisputed>().topic1(word(game_id));
        self.events("receipt_disputes", &filter, from_block, None)
            .await
    }

    /// Watches a game's escrow events whose signature hash is in `events`, from `from_block`.
    pub fn watch(
        &self,
        game_id: U256,
        events: &[B256],
        from_block: u64,
        interval: Duration,
        cancel: CancellationToken,
    ) -> Result<LogWatcher, ChainError> {
        if interval.is_zero() {
            return Err(ChainError::InvalidInput(
                "watch interval must be positive".to_owned(),
            ));
        }
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Ok(LogWatcher {
            client: self.clone(),
            filter: Filter::new()
                .address(self.escrow_address())
                .event_signature(events.to_vec())
                .topic1(word(game_id)),
            next_block: from_block,
            ticker,
            cancel,
        })
    }

    pub(crate) fn event_filter<E: SolEvent>(&self) -> Filter {
        Filter::new()
            .address(self.escrow_address())
            .event_signature(E::SIGNATURE_HASH)
    }

    /// Decoded `E` logs in `[from, to]`; `to` defaults to the head.
    pub(crate) async fn events<E: SolEvent>(
        &self,
        op: &'static str,
        filter: &Filter,
        from: u64,
        to: Option<u64>,
    ) -> Result<Vec<(E, LogPosition)>, ChainError> {
        self.logs(op, filter, from, to)
            .await?
            .iter()
            .map(|log| self.decode(op, log))
            .collect()
    }

    pub(crate) fn decode<E: SolEvent>(
        &self,
        op: &'static str,
        log: &Log,
    ) -> Result<(E, LogPosition), ChainError> {
        let event = log
            .log_decode::<E>()
            .map_err(|err| self.unexpected(op, format!("cannot decode {}: {err}", E::SIGNATURE)))?;
        Ok((event.inner.data, self.position(op, log)?))
    }

    fn position(&self, op: &'static str, log: &Log) -> Result<LogPosition, ChainError> {
        match (log.block_number, log.log_index) {
            (Some(block), Some(index)) => Ok(LogPosition { block, index }),
            _ => Err(self.unexpected(op, "log without block number or index")),
        }
    }

    /// Logs in `[from, to]`, fetched in spans of at most `max_log_range` blocks.
    async fn logs(
        &self,
        op: &'static str,
        filter: &Filter,
        from: u64,
        to: Option<u64>,
    ) -> Result<Vec<Log>, ChainError> {
        let to = match to {
            Some(to) => to,
            None => self.block_number().await?,
        };
        let mut logs = Vec::new();
        let mut start = from;
        while start <= to {
            let end = to.min(start.saturating_add(self.max_log_range - 1));
            let span = filter.clone().from_block(start).to_block(end);
            logs.extend(self.read(op, || self.provider.get_logs(&span)).await?);
            start = end + 1;
        }
        Ok(logs)
    }
}

/// Polls one game's escrow logs at a fixed interval; see [`ChainClient::watch`].
pub struct LogWatcher {
    client: ChainClient,
    filter: Filter,
    next_block: u64,
    ticker: Interval,
    cancel: CancellationToken,
}

impl LogWatcher {
    /// The next non-empty batch in chain order, or `None` once cancelled. A failed poll is
    /// returned and the next call retries the same blocks.
    pub async fn next(
        &mut self,
    ) -> Option<Result<Vec<(IWerewolfEscrowEvents, LogPosition)>, ChainError>> {
        let cancel = self.cancel.clone();
        loop {
            let polled = tokio::select! {
                biased;
                () = cancel.cancelled() => return None,
                polled = self.poll() => polled,
            };
            match polled {
                Ok(batch) if batch.is_empty() => {}
                other => return Some(other),
            }
        }
    }

    async fn poll(&mut self) -> Result<Vec<(IWerewolfEscrowEvents, LogPosition)>, ChainError> {
        self.ticker.tick().await;
        let head = self.client.block_number().await?;
        if head < self.next_block {
            return Ok(Vec::new());
        }
        let batch = self
            .client
            .logs("watch", &self.filter, self.next_block, Some(head))
            .await?
            .iter()
            .map(|log| {
                let event = IWerewolfEscrowEvents::decode_log(&log.inner).map_err(|err| {
                    self.client
                        .unexpected("watch", format!("cannot decode log: {err}"))
                })?;
                Ok((event.data, self.client.position("watch", log)?))
            })
            .collect::<Result<_, ChainError>>()?;
        self.next_block = head + 1;
        Ok(batch)
    }
}

pub(crate) fn word(value: U256) -> B256 {
    value.into()
}

#[cfg(test)]
mod tests {
    use super::LogPosition;

    #[test]
    fn positions_order_by_block_then_index() {
        let at = |block, index| LogPosition { block, index };
        assert!(at(7, 3) < at(8, 0));
        assert!(at(8, 0) < at(8, 1));
        assert!(at(8, 1) <= at(8, 1));
    }
}
