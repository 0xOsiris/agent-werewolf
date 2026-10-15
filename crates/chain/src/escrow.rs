use alloy::{
    primitives::{Address, B256, Bytes, U256},
    rpc::types::{Filter, TransactionRequest},
    sol_types::SolEvent,
};

use crate::{
    ChainClient, ChainError, LogPosition, Sent,
    bindings::{
        IERC20,
        IWerewolfEscrow::{
            ActionForced, Challenged, Checkpointed, Claimed, CreateParams, DisputeAnswered,
            Finalized, GameCreated, GameView, Joined, Proposed, Proved, ReceiptDisputed, Started,
        },
    },
    logs::word,
};

/// Reads.
impl ChainClient {
    /// The game's full on-chain state.
    pub async fn game_info(&self, game_id: U256) -> Result<GameView, ChainError> {
        let call = self.escrow.gameInfo(game_id);
        self.read("gameInfo", || call.call()).await
    }

    /// Seat addresses and session keys, in seat order.
    pub async fn seats(&self, game_id: U256) -> Result<(Vec<Address>, Vec<B256>), ChainError> {
        let call = self.escrow.seats(game_id);
        let seats = self.read("seats", || call.call()).await?;
        Ok((seats.players, seats.sessionKeys))
    }

    /// What `account` can claim from the game now.
    pub async fn claimable(&self, game_id: U256, account: Address) -> Result<U256, ChainError> {
        let call = self.escrow.claimable(game_id, account);
        self.read("claimable", || call.call()).await
    }

    /// The contract's receipt digest for one recorded action.
    pub async fn receipt_digest(
        &self,
        game_id: U256,
        turn_id: u64,
        seat: u32,
        act_hash: B256,
    ) -> Result<B256, ChainError> {
        let call = self.escrow.receiptDigest(game_id, turn_id, seat, act_hash);
        self.read("receiptDigest", || call.call()).await
    }

    /// `account`'s balance of `token`.
    pub async fn token_balance(
        &self,
        token: Address,
        account: Address,
    ) -> Result<U256, ChainError> {
        let erc20 = IERC20::new(token, &self.provider);
        let call = erc20.balanceOf(account);
        self.read("balanceOf", || call.call()).await
    }
}

/// Operator writes.
impl ChainClient {
    /// Creates a game after approving the `stake × maxSeats` bond.
    pub async fn create(
        &self,
        params: CreateParams,
    ) -> Result<(GameCreated, LogPosition), ChainError> {
        let operator = self.signer_address("create")?;
        let bond = U256::from(params.stake) * U256::from(params.maxSeats);
        self.ensure_allowance(params.token, bond).await?;
        let seed_commit = params.seedCommit;
        let filter = self
            .event_filter::<GameCreated>()
            .topic2(operator.into_word());
        let tx = self.escrow.create(params).into_transaction_request();
        self.send_for_event("create", tx, filter, |e: &GameCreated| {
            e.params.seedCommit == seed_commit
        })
        .await
    }

    /// Fixes the roster and requests randomness.
    pub async fn start(&self, game_id: U256) -> Result<(Started, LogPosition), ChainError> {
        let filter = self.event_filter::<Started>().topic1(word(game_id));
        let tx = self.escrow.start(game_id).into_transaction_request();
        self.send_for_event("start", tx, filter, |_: &Started| true)
            .await
    }

    /// Closes `round` to forced moves; the position bounds [`forced_actions`](Self::forced_actions).
    pub async fn checkpoint(
        &self,
        game_id: U256,
        round: u32,
    ) -> Result<(Checkpointed, LogPosition), ChainError> {
        let filter = self
            .event_filter::<Checkpointed>()
            .topic1(word(game_id))
            .topic2(word(U256::from(round)));
        let tx = self
            .escrow
            .checkpoint(game_id, round)
            .into_transaction_request();
        self.send_for_event("checkpoint", tx, filter, |_: &Checkpointed| true)
            .await
    }

    /// Posts the result and reveals the seed.
    pub async fn propose(
        &self,
        game_id: U256,
        seed: B256,
        winner_team: u8,
        winners: U256,
        transcript_root: B256,
        transcript_uri: String,
    ) -> Result<(Proposed, LogPosition), ChainError> {
        let filter = self.event_filter::<Proposed>().topic1(word(game_id));
        let tx = self
            .escrow
            .propose(
                game_id,
                seed,
                winner_team,
                winners,
                transcript_root,
                transcript_uri,
            )
            .into_transaction_request();
        self.send_for_event("propose", tx, filter, |_: &Proposed| true)
            .await
    }

    /// Submits the SP1 proof of the replay.
    pub async fn prove(
        &self,
        game_id: U256,
        public_values: Bytes,
        proof: Bytes,
    ) -> Result<(Proved, LogPosition), ChainError> {
        let filter = self.event_filter::<Proved>().topic1(word(game_id));
        let tx = self
            .escrow
            .prove(game_id, public_values, proof)
            .into_transaction_request();
        self.send_for_event("prove", tx, filter, |_: &Proved| true)
            .await
    }

    /// Answers a receipt dispute with the leaf's Merkle proof.
    pub async fn answer_dispute(
        &self,
        game_id: U256,
        dispute_id: U256,
        proof: Vec<B256>,
    ) -> Result<(DisputeAnswered, LogPosition), ChainError> {
        let filter = self
            .event_filter::<DisputeAnswered>()
            .topic1(word(game_id))
            .topic2(word(dispute_id));
        let tx = self
            .escrow
            .answerDispute(game_id, dispute_id, proof)
            .into_transaction_request();
        self.send_for_event("answerDispute", tx, filter, |_: &DisputeAnswered| true)
            .await
    }

    /// Moves the game to its end state once its deadline has passed.
    pub async fn finalize(&self, game_id: U256) -> Result<(Finalized, LogPosition), ChainError> {
        let filter = self.event_filter::<Finalized>().topic1(word(game_id));
        let tx = self.escrow.finalize(game_id).into_transaction_request();
        self.send_for_event("finalize", tx, filter, |_: &Finalized| true)
            .await
    }

    /// Approves the escrow for `amount` of `token` unless the allowance already covers it.
    pub async fn ensure_allowance(&self, token: Address, amount: U256) -> Result<(), ChainError> {
        let owner = self.signer_address("approve")?;
        let spender = self.escrow_address();
        let erc20 = IERC20::new(token, &self.provider);
        let allowance = &erc20.allowance(owner, spender);
        let covered =
            move || async move { Ok(self.read("allowance", || allowance.call()).await? >= amount) };
        if covered().await? {
            return Ok(());
        }
        let tx = erc20.approve(spender, amount).into_transaction_request();
        self.send("approve", tx, covered).await.map(drop)
    }
}

/// Player writes.
impl ChainClient {
    /// Approves the stake, then takes the next seat.
    pub async fn join(
        &self,
        game_id: U256,
        session_key: B256,
    ) -> Result<(Joined, LogPosition), ChainError> {
        let player = self.signer_address("join")?;
        let game = self.game_info(game_id).await?;
        self.ensure_allowance(game.token, U256::from(game.stake))
            .await?;
        let filter = self
            .event_filter::<Joined>()
            .topic1(word(game_id))
            .topic2(player.into_word());
        let tx = self
            .escrow
            .join(game_id, session_key)
            .into_transaction_request();
        self.send_for_event("join", tx, filter, |_: &Joined| true)
            .await
    }

    /// Logs an encrypted move for `round` that the server must apply.
    pub async fn force_action(
        &self,
        game_id: U256,
        round: u32,
        payload: Bytes,
    ) -> Result<(ActionForced, LogPosition), ChainError> {
        let filter = self
            .event_filter::<ActionForced>()
            .topic1(word(game_id))
            .topic2(word(U256::from(round)));
        let tx = self
            .escrow
            .forceAction(game_id, round, payload.clone())
            .into_transaction_request();
        self.send_for_event("forceAction", tx, filter, |e: &ActionForced| {
            e.payload == payload
        })
        .await
    }

    /// Challenges the proposal after approving the `stake` bond.
    pub async fn challenge(&self, game_id: U256) -> Result<(Challenged, LogPosition), ChainError> {
        let challenger = self.signer_address("challenge")?;
        let game = self.game_info(game_id).await?;
        self.ensure_allowance(game.token, U256::from(game.stake))
            .await?;
        let filter = self
            .event_filter::<Challenged>()
            .topic1(word(game_id))
            .topic2(challenger.into_word());
        let tx = self.escrow.challenge(game_id).into_transaction_request();
        self.send_for_event("challenge", tx, filter, |_: &Challenged| true)
            .await
    }

    /// Disputes a receipt the transcript left out, after approving the `stake / 4` bond.
    pub async fn dispute_receipt(
        &self,
        game_id: U256,
        turn_id: u64,
        seat: u32,
        act_hash: B256,
        signature: Bytes,
    ) -> Result<(ReceiptDisputed, LogPosition), ChainError> {
        let disputer = self.signer_address("disputeReceipt")?;
        let game = self.game_info(game_id).await?;
        // Mirrors the contract's `_disputeBond`.
        let bond = (U256::from(game.stake) / U256::from(4)).max(U256::from(1));
        self.ensure_allowance(game.token, bond).await?;
        let leaf = self
            .receipt_digest(game_id, turn_id, seat, act_hash)
            .await?;
        let filter = self
            .event_filter::<ReceiptDisputed>()
            .topic1(word(game_id))
            .topic3(disputer.into_word());
        let tx = self
            .escrow
            .disputeReceipt(game_id, turn_id, seat, act_hash, signature)
            .into_transaction_request();
        self.send_for_event("disputeReceipt", tx, filter, |e: &ReceiptDisputed| {
            e.leaf == leaf
        })
        .await
    }

    /// Withdraws what the game owes the signer.
    pub async fn claim(&self, game_id: U256) -> Result<(Claimed, LogPosition), ChainError> {
        let account = self.signer_address("claim")?;
        let filter = self
            .event_filter::<Claimed>()
            .topic1(word(game_id))
            .topic2(account.into_word());
        let tx = self.escrow.claim(game_id).into_transaction_request();
        self.send_for_event("claim", tx, filter, |_: &Claimed| true)
            .await
    }
}

impl ChainClient {
    /// Sends `tx` and returns the `E` it logged that matches `pick`. A retry treats a matching `E`
    /// in `filter` since the call began as proof the effect is applied.
    async fn send_for_event<E: SolEvent>(
        &self,
        op: &'static str,
        tx: TransactionRequest,
        filter: Filter,
        pick: impl Fn(&E) -> bool,
    ) -> Result<(E, LogPosition), ChainError> {
        let from = self.block_number().await?;
        let (filter, pick) = (&filter, &pick);
        let applied =
            move || async move { Ok(self.find_event(op, filter, from, pick).await?.is_some()) };
        let event = match self.send(op, tx, applied).await? {
            Sent::Mined(receipt) => receipt
                .logs()
                .iter()
                .filter(|log| {
                    log.address() == self.escrow_address()
                        && log.topic0() == Some(&E::SIGNATURE_HASH)
                })
                .map(|log| self.decode::<E>(op, log))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .find(|(e, _)| pick(e)),
            Sent::AlreadyApplied => self.find_event(op, filter, from, pick).await?,
        };
        event.ok_or_else(|| self.unexpected(op, format!("no matching {} log", E::SIGNATURE)))
    }

    /// The first `E` in `filter` from block `from` to the head that matches `pick`.
    async fn find_event<E: SolEvent>(
        &self,
        op: &'static str,
        filter: &Filter,
        from: u64,
        pick: &impl Fn(&E) -> bool,
    ) -> Result<Option<(E, LogPosition)>, ChainError> {
        let events = self.events::<E>(op, filter, from, None).await?;
        Ok(events.into_iter().find(|(e, _)| pick(e)))
    }
}
