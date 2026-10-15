//! Plays one staked game against a local anvil, deploying the forge artifacts in `contracts/out/`.

use std::{error::Error, path::PathBuf, time::Duration};

use alloy::{
    hex,
    network::TransactionBuilder,
    node_bindings::Anvil,
    primitives::{Address, B256, Bytes, U256, aliases::U96, keccak256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::{SolEvent, SolValue},
};
use tokio_util::sync::CancellationToken;
use werewolf_chain::{
    ChainClient, ChainConfig, ChainError, Sent,
    bindings::{
        IERC20,
        IWerewolfEscrow::{ActionForced, CreateParams, IWerewolfEscrowEvents, Status},
    },
};

type TestResult = Result<(), Box<dyn Error>>;

const STAKE: u64 = 100;
const SEATS: u8 = 5;
const FUNDS: u64 = 1_000;
const WINDOW: u64 = 3_600;

fn artifact(name: &str) -> Result<serde_json::Value, Box<dyn Error>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../contracts/out/{name}.sol/{name}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("{}: {err}; run `forge build` in contracts/", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

async fn deploy(
    provider: &impl Provider,
    name: &str,
    args: &[u8],
) -> Result<Address, Box<dyn Error>> {
    let code = artifact(name)?["bytecode"]["object"]
        .as_str()
        .ok_or("artifact has no bytecode")?
        .to_owned();
    let mut code = hex::decode(code)?;
    code.extend_from_slice(args);
    let receipt = provider
        .send_transaction(TransactionRequest::default().with_deploy_code(code))
        .await?
        .get_receipt()
        .await?;
    Ok(receipt
        .contract_address
        .ok_or("deploy receipt has no contract address")?)
}

#[tokio::test]
#[ignore = "needs anvil and forge build"]
async fn staked_game_settles_and_pays_winners() -> TestResult {
    let anvil = Anvil::new().try_spawn()?;
    let keys: Vec<PrivateKeySigner> = anvil
        .keys()
        .iter()
        .cloned()
        .map(PrivateKeySigner::from)
        .collect();
    let deployer = ProviderBuilder::new()
        .wallet(keys[0].clone())
        .connect_http(anvil.endpoint_url());

    let token = deploy(&deployer, "MockERC20", &[]).await?;
    let randomness = deploy(&deployer, "PrevrandaoRandomness", &[]).await?;
    let verifier = deploy(&deployer, "SP1MockVerifier", &[]).await?;
    let windows = (randomness, verifier, B256::ZERO, WINDOW, WINDOW, WINDOW).abi_encode_params();
    let escrow = deploy(&deployer, "WerewolfEscrow", &windows).await?;

    let erc20 = IERC20::new(token, &deployer);
    let dir = tempfile::tempdir()?;
    let mut clients = Vec::new();
    for (i, key) in keys[1..=usize::from(SEATS) + 1].iter().enumerate() {
        erc20
            .mint(key.address(), U256::from(FUNDS))
            .send()
            .await?
            .get_receipt()
            .await?;
        let path = dir.path().join(format!("key{i}"));
        std::fs::write(&path, hex::encode(key.to_bytes()))?;
        let config = ChainConfig::new(anvil.endpoint_url(), anvil.chain_id(), escrow, Some(path));
        clients.push(ChainClient::connect(config).await?);
    }
    let operator = clients.remove(0);
    let players = clients;
    let balance = async |client: &ChainClient| -> Result<U256, ChainError> {
        client
            .token_balance(
                token,
                client
                    .address()
                    .ok_or(ChainError::InvalidInput("no signer".into()))?,
            )
            .await
    };

    // Create pulls the bond.
    let now = operator.latest_timestamp().await?;
    let seed = B256::repeat_byte(0x5e);
    let params = CreateParams {
        token,
        stake: U96::from(STAKE),
        minSeats: SEATS,
        maxSeats: SEATS,
        joinBy: now + WINDOW,
        playBy: now + 2 * WINDOW,
        // `abi.encode(uint8 wolves, ...)` pads `wolves` to a full word, as for uint256.
        rulesHash: keccak256((U256::from(2), true, true).abi_encode()),
        seedCommit: keccak256(seed),
        encKey: B256::repeat_byte(0xe1),
    };
    let (created, created_at) = operator.create(params).await?;
    let game = created.gameId;
    let from = created_at.block;
    assert_eq!(
        balance(&operator).await?,
        U256::from(FUNDS - STAKE * u64::from(SEATS))
    );

    // Five joins fill the seats in order.
    for (seat, player) in players.iter().enumerate() {
        let (joined, _) = player
            .join(game, B256::with_last_byte(seat as u8 + 1))
            .await?;
        assert_eq!(joined.seat as usize, seat);
    }
    assert_eq!(operator.joined(game, from).await?.len(), usize::from(SEATS));
    let again = players[0].join(game, B256::with_last_byte(9)).await;
    assert!(
        matches!(&again, Err(ChainError::Revert { reason, .. }) if reason.contains("GameFull")),
        "{again:?}"
    );

    // Start; the prevrandao provider fulfils in the same transaction.
    operator.start(game).await?;
    let fulfilled = operator.randomness_fulfilled(game, from).await?;
    let info = operator.game_info(game).await?;
    assert_eq!(info.status, Status::Started);
    assert_eq!(fulfilled.len(), 1);
    assert_eq!(fulfilled[0].0.randomness, info.randomness);

    // A round-0 force lands before checkpoint 0; a round-1 force lands after it.
    let forced0 = players[2]
        .force_action(game, 0, Bytes::from_static(b"round zero"))
        .await?;
    let (_, checkpoint0) = operator.checkpoint(game, 0).await?;
    let forced1 = players[3]
        .force_action(game, 1, Bytes::from_static(b"round one"))
        .await?;
    assert_eq!(
        operator.forced_actions(game, 0, from, checkpoint0).await?,
        std::slice::from_ref(&forced0)
    );
    assert!(
        operator
            .forced_actions(game, 1, from, checkpoint0)
            .await?
            .is_empty()
    );
    let (_, checkpoint1) = operator.checkpoint(game, 1).await?;
    assert_eq!(
        operator.forced_actions(game, 1, from, checkpoint1).await?,
        std::slice::from_ref(&forced1)
    );

    // The watcher sees both forces, then stops on cancel.
    let cancel = CancellationToken::new();
    let mut watcher = operator.watch(
        game,
        &[ActionForced::SIGNATURE_HASH],
        from,
        Duration::from_millis(50),
        cancel.clone(),
    )?;
    let batch = watcher.next().await.ok_or("watcher ended early")??;
    let payloads: Vec<_> = batch
        .iter()
        .filter_map(|(e, _)| match e {
            IWerewolfEscrowEvents::ActionForced(e) => Some(e.payload.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(payloads, [forced0.0.payload, forced1.0.payload]);
    cancel.cancel();
    assert!(watcher.next().await.is_none());

    // Seats 0 and 1 win; finalize after the challenge window.
    let winners = U256::from(0b11);
    operator
        .propose(
            game,
            seed,
            1,
            winners,
            B256::repeat_byte(0x77),
            "ipfs://transcript".to_owned(),
        )
        .await?;
    deployer
        .raw_request::<_, serde_json::Value>("evm_increaseTime".into(), (WINDOW + 1,))
        .await?;
    deployer
        .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
        .await?;
    let (finalized, _) = operator.finalize(game).await?;
    assert_eq!(finalized.status, Status::Settled);

    let pot = STAKE * u64::from(SEATS);
    for (seat, player) in players.iter().enumerate() {
        if seat < 2 {
            let (claimed, _) = player.claim(game).await?;
            assert_eq!(claimed.amount, U256::from(pot / 2));
            assert_eq!(balance(player).await?, U256::from(FUNDS - STAKE + pot / 2));
        } else {
            let account = player.address().ok_or("no signer")?;
            assert_eq!(operator.claimable(game, account).await?, U256::ZERO);
            assert_eq!(balance(player).await?, U256::from(FUNDS - STAKE));
        }
    }
    operator.claim(game).await?;
    assert_eq!(balance(&operator).await?, U256::from(FUNDS));
    Ok(())
}

#[tokio::test]
#[ignore = "needs anvil and forge build"]
async fn retried_send_lands_once() -> TestResult {
    let anvil = Anvil::new().try_spawn()?;
    let deployer = ProviderBuilder::new()
        .wallet(PrivateKeySigner::from(anvil.keys()[0].clone()))
        .connect_http(anvil.endpoint_url());
    let token = deploy(&deployer, "MockERC20", &[]).await?;
    let signer = PrivateKeySigner::from(anvil.keys()[1].clone());
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("key");
    std::fs::write(&path, hex::encode(signer.to_bytes()))?;
    // Any contract passes the connect check; this test only uses `send`.
    let mut config = ChainConfig::new(anvil.endpoint_url(), anvil.chain_id(), token, Some(path));
    config.receipt_timeout = Duration::from_millis(300);
    let client = ChainClient::connect(config).await?;

    // Blocks stop until 700 ms in, so the first attempts time out and get resent.
    deployer
        .raw_request::<_, serde_json::Value>("evm_setAutomine".into(), (false,))
        .await?;
    let miner = deployer.clone();
    let mined = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        miner
            .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
            .await
    });
    let mint = IERC20::new(token, &deployer)
        .mint(signer.address(), U256::from(5))
        .into_transaction_request();
    let sent = client.send("mint", mint, || async { Ok(false) }).await?;
    mined.await??;
    deployer
        .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
        .await?;

    assert!(matches!(sent, Sent::Mined(_)));
    assert_eq!(deployer.get_transaction_count(signer.address()).await?, 1);
    assert_eq!(
        client.token_balance(token, signer.address()).await?,
        U256::from(5)
    );
    Ok(())
}
