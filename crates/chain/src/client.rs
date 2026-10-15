use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alloy::{
    eips::BlockNumberOrTag,
    network::{EthereumWallet, NetworkTransactionBuilder, TransactionBuilder},
    primitives::{Address, B256, TxHash},
    providers::{Provider, RootProvider},
    rpc::{
        client::RpcClient,
        types::{TransactionReceipt, TransactionRequest},
    },
    signers::{SignerSync, local::PrivateKeySigner},
    sol_types::{SolInterface, decode_revert_reason},
    transports::{HttpError, RpcError, TransportError, TransportErrorKind, http::reqwest},
};
use tokio::sync::Mutex;
use tracing::warn;
use zeroize::Zeroizing;

use crate::{
    ChainError,
    bindings::{IERC20::IERC20Errors, IWerewolfErrors, IWerewolfEscrow, OpenZeppelinErrors},
};

const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(8);
const RECEIPT_POLL: Duration = Duration::from_millis(500);

/// Settings for [`ChainClient::connect`].
#[derive(Clone)]
pub struct ChainConfig {
    /// JSON-RPC HTTP endpoint; only its host and port appear in errors and logs.
    pub rpc_url: reqwest::Url,
    /// Chain id the endpoint must report.
    pub chain_id: u64,
    /// The escrow contract.
    pub escrow: Address,
    /// File holding the signer's hex private key; `None` makes a read-only client.
    pub key_file: Option<PathBuf>,
    /// Bound on every HTTP request.
    pub request_timeout: Duration,
    /// Bound on each attempt's wait for a transaction receipt.
    pub receipt_timeout: Duration,
    /// Attempts per call, counting the first.
    pub max_attempts: u32,
    /// Largest block span per `eth_getLogs` request.
    pub max_log_range: u64,
}

impl ChainConfig {
    /// Config with default timeouts, retries and log range.
    pub fn new(
        rpc_url: reqwest::Url,
        chain_id: u64,
        escrow: Address,
        key_file: Option<PathBuf>,
    ) -> Self {
        Self {
            rpc_url,
            chain_id,
            escrow,
            key_file,
            request_timeout: Duration::from_secs(10),
            receipt_timeout: Duration::from_secs(30),
            max_attempts: 5,
            max_log_range: 5_000,
        }
    }
}

/// Outcome of [`ChainClient::send`].
#[derive(Debug)]
pub enum Sent {
    /// One of the call's transactions mined successfully.
    Mined(Box<TransactionReceipt>),
    /// A retry found the effect already on chain.
    AlreadyApplied,
}

/// Client for one escrow on one chain. Clones share the connection and the write queue.
#[derive(Clone)]
pub struct ChainClient {
    pub(crate) provider: RootProvider,
    pub(crate) escrow: IWerewolfEscrow::IWerewolfEscrowInstance<RootProvider>,
    pub(crate) host: Arc<str>,
    /// Full endpoint, kept only to scrub it from errors; it can carry an API key.
    rpc_url: Arc<str>,
    pub(crate) max_log_range: u64,
    signer: Option<PrivateKeySigner>,
    chain_id: u64,
    request_timeout: Duration,
    receipt_timeout: Duration,
    max_attempts: u32,
    /// One write at a time, so a retry can safely reuse its nonce.
    write_lock: Arc<Mutex<()>>,
}

impl ChainClient {
    /// Connects, then checks the chain id and that the escrow has code.
    pub async fn connect(config: ChainConfig) -> Result<Self, ChainError> {
        if config.max_attempts == 0 || config.max_log_range == 0 {
            return Err(ChainError::InvalidInput(
                "max_attempts and max_log_range must be positive".to_owned(),
            ));
        }
        let signer = match &config.key_file {
            Some(path) => Some(read_key(path).await?),
            None => None,
        };
        let host = match (config.rpc_url.host_str(), config.rpc_url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_owned(),
            (None, _) => "<no host>".to_owned(),
        };
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|err| {
                ChainError::InvalidInput(format!("http client for rpc {host}: {err}"))
            })?;
        let rpc_url = config.rpc_url.as_str().into();
        let provider =
            RootProvider::new(RpcClient::builder().http_with_client(http, config.rpc_url));
        let client = Self {
            escrow: IWerewolfEscrow::new(config.escrow, provider.clone()),
            provider,
            host: host.into(),
            rpc_url,
            max_log_range: config.max_log_range,
            signer,
            chain_id: config.chain_id,
            request_timeout: config.request_timeout,
            receipt_timeout: config.receipt_timeout,
            max_attempts: config.max_attempts,
            write_lock: Arc::default(),
        };

        let chain_id = client
            .read("eth_chainId", || client.provider.get_chain_id())
            .await?;
        if chain_id != config.chain_id {
            return Err(ChainError::InvalidInput(format!(
                "rpc {} serves chain {chain_id}, expected {}",
                client.host, config.chain_id
            )));
        }
        let code = client
            .read("eth_getCode", || client.provider.get_code_at(config.escrow))
            .await?;
        if code.is_empty() {
            return Err(ChainError::InvalidInput(format!(
                "no contract at escrow {} on chain {chain_id}",
                config.escrow
            )));
        }
        Ok(client)
    }

    /// The signer's address, if the client has one.
    pub fn address(&self) -> Option<Address> {
        self.signer.as_ref().map(PrivateKeySigner::address)
    }

    /// The escrow contract address.
    pub fn escrow_address(&self) -> Address {
        *self.escrow.address()
    }

    /// The chain id checked at connect.
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// The node's latest block number.
    pub async fn block_number(&self) -> Result<u64, ChainError> {
        self.read("eth_blockNumber", || self.provider.get_block_number())
            .await
    }

    /// The latest block's timestamp; deadlines like `joinBy` are checked against chain time.
    pub async fn latest_timestamp(&self) -> Result<u64, ChainError> {
        let op = "eth_getBlockByNumber";
        let block = self
            .read(op, || {
                self.provider.get_block_by_number(BlockNumberOrTag::Latest)
            })
            .await?;
        Ok(block
            .ok_or_else(|| self.unexpected(op, "no latest block"))?
            .header
            .timestamp)
    }

    /// Signs a raw 32-byte digest, with no EIP-191 prefix, as `r ‖ s ‖ v` with `v` 27 or 28.
    pub fn sign_digest(&self, digest: B256) -> Result<[u8; 65], ChainError> {
        let signature = self
            .signer("sign_digest")?
            .sign_hash_sync(&digest)
            .map_err(|err| ChainError::InvalidInput(format!("cannot sign digest: {err}")))?;
        Ok(signature.as_bytes())
    }

    /// Sends `tx` from the signer, retrying transport failures and receipt timeouts.
    ///
    /// Every attempt reuses the first one's nonce with higher fees, so at most one of them can mine.
    /// Before each retry it checks the earlier attempts' receipts, then asks `applied` whether the
    /// effect is already on chain.
    pub async fn send<Fut>(
        &self,
        op: &'static str,
        tx: TransactionRequest,
        applied: impl Fn() -> Fut,
    ) -> Result<Sent, ChainError>
    where
        Fut: Future<Output = Result<bool, ChainError>>,
    {
        let signer = self.signer(op)?;
        let wallet = EthereumWallet::from(signer.clone());
        let _queue = self.write_lock.lock().await;
        let mut pinned: Option<TransactionRequest> = None;
        let mut sent: Vec<TxHash> = Vec::new();
        let mut retries = 0;
        loop {
            let attempt = async {
                if retries > 0 {
                    for &hash in &sent {
                        let receipt = self
                            .provider
                            .get_transaction_receipt(hash)
                            .await
                            .map_err(|err| self.rpc_error(op, err))?;
                        if let Some(receipt) = receipt {
                            return mined(op, receipt);
                        }
                    }
                    if applied().await? {
                        return Ok(Sent::AlreadyApplied);
                    }
                }
                let request = match pinned.take() {
                    Some(previous) => bump_fees(previous),
                    None => self.fill(op, tx.clone(), signer.address()).await?,
                };
                pinned = Some(request.clone());
                let envelope = request.build(&wallet).await.map_err(|err| {
                    ChainError::InvalidInput(format!("{op}: cannot build transaction: {err}"))
                })?;
                let hash = *envelope.tx_hash();
                sent.push(hash);
                self.provider
                    .send_tx_envelope(envelope)
                    .await
                    .map(drop)
                    .map_err(|err| self.rpc_error(op, err))?;
                mined(op, self.wait_receipt(op, hash).await?)
            };
            match attempt.await {
                Ok(outcome) => return Ok(outcome),
                Err(err) => self.pause_or_fail(op, &mut retries, err).await?,
            }
        }
    }

    /// Runs an idempotent read with retries.
    pub(crate) async fn read<T, E, Fut>(
        &self,
        op: &'static str,
        call: impl Fn() -> Fut,
    ) -> Result<T, ChainError>
    where
        E: Classify,
        Fut: IntoFuture<Output = Result<T, E>>,
    {
        let mut retries = 0;
        loop {
            match call().await {
                Ok(value) => return Ok(value),
                Err(err) => {
                    self.pause_or_fail(op, &mut retries, err.classify(self, op))
                        .await?;
                }
            }
        }
    }

    pub(crate) fn signer_address(&self, op: &'static str) -> Result<Address, ChainError> {
        self.signer(op).map(PrivateKeySigner::address)
    }

    pub(crate) fn unexpected(&self, op: &'static str, detail: impl Into<String>) -> ChainError {
        ChainError::Unexpected {
            op,
            host: self.host.to_string(),
            detail: detail.into(),
        }
    }

    /// Backs off with full jitter before the next attempt, or returns `err` if final or out of tries.
    async fn pause_or_fail(
        &self,
        op: &'static str,
        retries: &mut u32,
        err: ChainError,
    ) -> Result<(), ChainError> {
        let err = err.with_attempts(*retries + 1);
        if !err.is_retryable() || *retries + 1 >= self.max_attempts {
            return Err(err);
        }
        let delay = backoff(*retries);
        warn!(op, rpc = %self.host, attempt = *retries + 1, ?delay, %err, "chain call failed, retrying");
        tokio::time::sleep(delay).await;
        *retries += 1;
        Ok(())
    }

    fn signer(&self, op: &'static str) -> Result<&PrivateKeySigner, ChainError> {
        self.signer
            .as_ref()
            .ok_or_else(|| ChainError::InvalidInput(format!("{op} needs a signer key file")))
    }

    /// Pins nonce, gas and fees; a revert in gas estimation fails the call without a transaction.
    async fn fill(
        &self,
        op: &'static str,
        tx: TransactionRequest,
        from: Address,
    ) -> Result<TransactionRequest, ChainError> {
        let tx = tx.with_from(from).with_chain_id(self.chain_id);
        let gas = self
            .provider
            .estimate_gas(tx.clone())
            .await
            .map_err(|err| self.rpc_error(op, err))?;
        let fees = self
            .provider
            .estimate_eip1559_fees()
            .await
            .map_err(|err| self.rpc_error(op, err))?;
        let nonce = self
            .provider
            .get_transaction_count(from)
            .pending()
            .await
            .map_err(|err| self.rpc_error(op, err))?;
        // Headroom for state that moves between estimation and inclusion.
        Ok(tx
            .with_gas_limit(gas.saturating_mul(6) / 5)
            .with_nonce(nonce)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas))
    }

    async fn wait_receipt(
        &self,
        op: &'static str,
        hash: TxHash,
    ) -> Result<TransactionReceipt, ChainError> {
        tokio::time::timeout(self.receipt_timeout, self.poll_receipt(op, hash))
            .await
            .map_err(|_| ChainError::Timeout {
                op,
                host: self.host.to_string(),
                waited: self.receipt_timeout,
                tx: Some(hash),
                attempts: 1,
            })?
    }

    async fn poll_receipt(
        &self,
        op: &'static str,
        hash: TxHash,
    ) -> Result<TransactionReceipt, ChainError> {
        loop {
            let receipt = self
                .provider
                .get_transaction_receipt(hash)
                .await
                .map_err(|err| self.rpc_error(op, err))?;
            if let Some(receipt) = receipt {
                return Ok(receipt);
            }
            tokio::time::sleep(RECEIPT_POLL).await;
        }
    }

    fn rpc_error(&self, op: &'static str, err: TransportError) -> ChainError {
        if let Some(payload) = err.as_error_resp()
            && payload.message.contains("revert")
        {
            let reason = payload
                .as_revert_data()
                .map_or_else(|| payload.message.to_string(), |data| decode_revert(&data));
            return ChainError::Revert {
                op,
                reason,
                tx: None,
            };
        }
        let err = self.scrub_url(err);
        let status = match &err {
            RpcError::Transport(TransportErrorKind::HttpError(http)) => Some(http.status),
            RpcError::Transport(TransportErrorKind::HttpErrorWithRetryAfter { error, .. }) => {
                Some(error.status)
            }
            RpcError::Transport(TransportErrorKind::Custom(inner))
                if inner
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(reqwest::Error::is_timeout) =>
            {
                return ChainError::Timeout {
                    op,
                    host: self.host.to_string(),
                    waited: self.request_timeout,
                    tx: None,
                    attempts: 1,
                };
            }
            _ => None,
        };
        ChainError::Rpc {
            op,
            host: self.host.to_string(),
            status,
            attempts: 1,
            source: err,
        }
    }

    /// Removes the endpoint URL that reqwest errors quote, since it can carry an API key.
    fn scrub_url(&self, err: TransportError) -> TransportError {
        let scrub = |mut http: HttpError| {
            http.body = http.body.replace(&*self.rpc_url, "<rpc url>");
            http
        };
        let kind = match err {
            RpcError::Transport(TransportErrorKind::Custom(inner)) => {
                let inner: Box<dyn std::error::Error + Send + Sync> =
                    match inner.downcast::<reqwest::Error>() {
                        Ok(http) => Box::new(http.without_url()),
                        Err(other) => other,
                    };
                TransportErrorKind::Custom(inner)
            }
            RpcError::Transport(TransportErrorKind::HttpError(http)) => {
                TransportErrorKind::HttpError(scrub(http))
            }
            RpcError::Transport(TransportErrorKind::HttpErrorWithRetryAfter {
                error,
                retry_after,
            }) => TransportErrorKind::HttpErrorWithRetryAfter {
                error: scrub(error),
                retry_after,
            },
            other => return other,
        };
        RpcError::Transport(kind)
    }
}

/// Maps a call's error into [`ChainError`].
pub(crate) trait Classify {
    fn classify(self, client: &ChainClient, op: &'static str) -> ChainError;
}

impl Classify for TransportError {
    fn classify(self, client: &ChainClient, op: &'static str) -> ChainError {
        client.rpc_error(op, self)
    }
}

impl Classify for alloy::contract::Error {
    fn classify(self, client: &ChainClient, op: &'static str) -> ChainError {
        match self {
            Self::TransportError(err) => client.rpc_error(op, err),
            other => client.unexpected(op, other.to_string()),
        }
    }
}

fn mined(op: &'static str, receipt: TransactionReceipt) -> Result<Sent, ChainError> {
    if receipt.status() {
        Ok(Sent::Mined(Box::new(receipt)))
    } else {
        Err(ChainError::Revert {
            op,
            reason: "reverted on chain".to_owned(),
            tx: Some(receipt.transaction_hash),
        })
    }
}

/// Raises both fee caps by 25%; pools replace a pending transaction only above about 10%.
fn bump_fees(tx: TransactionRequest) -> TransactionRequest {
    let bump = |fee: Option<u128>| fee.map(|fee| fee + fee / 4 + 1);
    TransactionRequest {
        max_fee_per_gas: bump(tx.max_fee_per_gas),
        max_priority_fee_per_gas: bump(tx.max_priority_fee_per_gas),
        ..tx
    }
}

/// Full jitter: uniform in `[0, min(cap, base × 2^retry))`.
fn backoff(retry: u32) -> Duration {
    BACKOFF_BASE
        .saturating_mul(1 << retry.min(16))
        .min(BACKOFF_CAP)
        .mul_f64(rand::random::<f64>())
}

fn decode_revert(data: &[u8]) -> String {
    if let Ok(err) = IWerewolfErrors::IWerewolfErrorsErrors::abi_decode(data) {
        return format!("{err:?}");
    }
    if let Ok(err) = IERC20Errors::abi_decode(data) {
        return format!("{err:?}");
    }
    if let Ok(err) = OpenZeppelinErrors::OpenZeppelinErrorsErrors::abi_decode(data) {
        return format!("{err:?}");
    }
    decode_revert_reason(data)
        .unwrap_or_else(|| format!("unknown error {}", alloy::hex::encode_prefixed(data)))
}

async fn read_key(path: &Path) -> Result<PrivateKeySigner, ChainError> {
    let text =
        Zeroizing::new(tokio::fs::read_to_string(path).await.map_err(|err| {
            ChainError::InvalidInput(format!("key file {}: {err}", path.display()))
        })?);
    // The parse error can quote the key, so it is dropped.
    text.trim().parse().map_err(|_| {
        ChainError::InvalidInput(format!(
            "key file {}: not a hex secp256k1 private key",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{U256, keccak256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn client_with(signer: PrivateKeySigner) -> ChainClient {
        let url: reqwest::Url = "http://127.0.0.1:1".parse().expect("static url");
        let provider = RootProvider::new_http(url);
        ChainClient {
            escrow: IWerewolfEscrow::new(Address::ZERO, provider.clone()),
            provider,
            host: "127.0.0.1:1".into(),
            rpc_url: "http://127.0.0.1:1".into(),
            max_log_range: 1,
            signer: Some(signer),
            chain_id: 31337,
            request_timeout: Duration::from_secs(1),
            receipt_timeout: Duration::from_secs(1),
            max_attempts: 1,
            write_lock: Arc::default(),
        }
    }

    #[test]
    fn signed_digest_recovers_to_signer() {
        let signer = PrivateKeySigner::random();
        let client = client_with(signer.clone());
        let digest = keccak256(b"werewolf/receipt/v1");

        let bytes = client.sign_digest(digest).expect("signer present");

        assert!(matches!(bytes[64], 27 | 28));
        let signature = alloy::primitives::Signature::from_raw(&bytes).expect("65-byte signature");
        assert_eq!(
            signature
                .recover_address_from_prehash(&digest)
                .expect("recoverable"),
            signer.address()
        );
    }

    #[test]
    fn backoff_stays_under_cap() {
        for retry in 0..40 {
            let ceiling = BACKOFF_BASE
                .saturating_mul(1 << retry.min(16))
                .min(BACKOFF_CAP);
            assert!(backoff(retry) <= ceiling);
        }
    }

    #[test]
    fn decodes_erc20_and_string_reverts() {
        let err = IERC20Errors::ERC20InsufficientAllowance(
            crate::bindings::IERC20::ERC20InsufficientAllowance {
                spender: Address::ZERO,
                allowance: U256::ZERO,
                needed: U256::from(7),
            },
        );
        assert!(decode_revert(&err.abi_encode()).contains("ERC20InsufficientAllowance"));
        let reason = alloy::sol_types::Revert::from("no seats left");
        assert!(
            decode_revert(&alloy::sol_types::SolError::abi_encode(&reason))
                .contains("no seats left")
        );
    }

    #[tokio::test]
    async fn bad_key_file_error_hides_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("key");
        std::fs::write(&path, "0xdeadbeefSECRET").expect("write key file");

        let err = read_key(&path).await.expect_err("not a key");

        assert!(!err.to_string().contains("SECRET") && !err.to_string().contains("deadbeef"));
    }

    #[test]
    fn decodes_escrow_errors() {
        use alloy::sol_types::SolError;
        let err = IWerewolfErrors::WrongRound {};
        assert!(decode_revert(&err.abi_encode()).contains("WrongRound"));
        let err = OpenZeppelinErrors::ReentrancyGuardReentrantCall {};
        assert!(decode_revert(&err.abi_encode()).contains("ReentrancyGuardReentrantCall"));
        let err = OpenZeppelinErrors::SafeERC20FailedOperation {
            token: Address::ZERO,
        };
        assert!(decode_revert(&err.abi_encode()).contains("SafeERC20FailedOperation"));
    }

    /// Answers every request with `response`, or never answers when it is `None`.
    async fn fake_rpc(response: Option<&'static str>) -> ChainConfig {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!(
            "http://{}/v2/SECRET",
            listener.local_addr().expect("local addr")
        );
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((mut stream, _)) = listener.accept().await {
                let Some(response) = response else {
                    held.push(stream);
                    continue;
                };
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).await;
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let mut config = ChainConfig::new(url.parse().expect("url"), 31337, Address::ZERO, None);
        config.request_timeout = Duration::from_millis(100);
        config.max_attempts = 2;
        config
    }

    /// Fails if the error, its `Debug` form or any source in its chain shows the URL's key.
    fn assert_url_hidden(err: &ChainError) {
        let mut shown = format!("{err:?}");
        let mut source: Option<&dyn std::error::Error> = Some(err);
        while let Some(err) = source {
            shown.push_str(&err.to_string());
            source = err.source();
        }
        assert!(!shown.contains("SECRET"), "{shown}");
    }

    #[tokio::test]
    async fn refused_connection_hides_rpc_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);
        let url = format!("http://{addr}/v2/SECRET").parse().expect("url");
        let mut config = ChainConfig::new(url, 31337, Address::ZERO, None);
        config.max_attempts = 1;

        let err = ChainClient::connect(config)
            .await
            .err()
            .expect("nothing listens");

        assert!(
            matches!(err, ChainError::Rpc { status: None, .. }),
            "{err:?}"
        );
        assert_url_hidden(&err);
    }

    #[tokio::test]
    async fn truncated_error_body_hides_rpc_url() {
        let config = fake_rpc(Some(
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial",
        ))
        .await;

        let err = ChainClient::connect(config)
            .await
            .err()
            .expect("502 fails connect");

        assert!(
            matches!(
                err,
                ChainError::Rpc {
                    status: Some(502),
                    ..
                }
            ),
            "{err:?}"
        );
        assert_url_hidden(&err);
    }

    #[tokio::test]
    async fn http_5xx_is_retried_then_reported_with_status() {
        let config = fake_rpc(Some(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ))
        .await;

        let err = ChainClient::connect(config)
            .await
            .err()
            .expect("503 fails connect");

        assert!(
            matches!(
                err,
                ChainError::Rpc {
                    status: Some(503),
                    attempts: 2,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_url_hidden(&err);
    }

    #[tokio::test]
    async fn hung_rpc_times_out() {
        let config = fake_rpc(None).await;

        let err = ChainClient::connect(config)
            .await
            .err()
            .expect("hung rpc fails connect");

        assert!(
            matches!(
                err,
                ChainError::Timeout {
                    attempts: 2,
                    tx: None,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_url_hidden(&err);
    }

    #[tokio::test]
    async fn calls_are_send() {
        fn assert_send<T: Send>(_: T) {}
        let client = client_with(PrivateKeySigner::random());
        assert_send(client.create(Default::default()));
        assert_send(client.join(U256::ZERO, B256::ZERO));
        assert_send(client.checkpoint(U256::ZERO, 0));
        assert_send(client.force_action(U256::ZERO, 0, Default::default()));
        assert_send(client.forced_actions(
            U256::ZERO,
            0,
            0,
            crate::LogPosition { block: 0, index: 0 },
        ));
        assert_send(
            client.send("noop", TransactionRequest::default(), || async {
                Ok(false)
            }),
        );
        assert_send(
            client
                .watch(
                    U256::ZERO,
                    &[],
                    0,
                    Duration::from_secs(1),
                    Default::default(),
                )
                .expect("watcher")
                .next(),
        );
    }
}
