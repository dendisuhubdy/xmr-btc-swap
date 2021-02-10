mod bitcoind;
mod electrs;

use crate::testutils;
use anyhow::{Context, Result};
use bitcoin_harness::{BitcoindRpcApi, Client};
use futures::Future;
use get_port::get_port;
use libp2p::{core::Multiaddr, PeerId};
use monero_harness::{image, Monero};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use swap::{
    bitcoin,
    bitcoin::Timelock,
    execution_params,
    execution_params::{ExecutionParams, GetExecutionParams},
    monero,
    protocol::{alice, alice::AliceState, bob, bob::BobState, SwapAmounts},
    seed::Seed,
};
use tempfile::tempdir;
use testcontainers::{clients::Cli, Container, Docker, RunArgs};
use tokio::{task::JoinHandle, time::sleep};
use tracing_core::dispatcher::DefaultGuard;
use tracing_log::LogTracer;
use url::Url;
use uuid::Uuid;

const TEST_WALLET_NAME: &str = "testwallet";

#[derive(Debug, Clone)]
pub struct StartingBalances {
    pub xmr: monero::Amount,
    pub btc: bitcoin::Amount,
}

struct AliceParams {
    seed: Seed,
    execution_params: ExecutionParams,
    swap_id: Uuid,
    bitcoin_wallet: Arc<bitcoin::Wallet>,
    monero_wallet: Arc<monero::Wallet>,
    db_path: PathBuf,
    listen_address: Multiaddr,
}

impl AliceParams {
    pub fn builder(&self) -> alice::Builder {
        alice::Builder::new(
            self.seed,
            self.execution_params,
            self.swap_id,
            self.bitcoin_wallet.clone(),
            self.monero_wallet.clone(),
            self.db_path.clone(),
            self.listen_address.clone(),
        )
    }

    fn peer_id(&self) -> PeerId {
        self.builder().peer_id()
    }
}

#[derive(Clone)]
struct BobParams {
    seed: Seed,
    db_path: PathBuf,
    swap_id: Uuid,
    bitcoin_wallet: Arc<bitcoin::Wallet>,
    monero_wallet: Arc<monero::Wallet>,
    alice_address: Multiaddr,
    alice_peer_id: PeerId,
    execution_params: ExecutionParams,
}

impl BobParams {
    pub fn builder(&self) -> bob::Builder {
        bob::Builder::new(
            self.seed,
            self.db_path.clone(),
            self.swap_id,
            self.bitcoin_wallet.clone(),
            self.monero_wallet.clone(),
            self.alice_address.clone(),
            self.alice_peer_id,
            self.execution_params,
        )
    }
}

pub struct BobEventLoopJoinHandle(JoinHandle<()>);

pub struct AliceEventLoopJoinHandle(JoinHandle<()>);

pub struct TestContext {
    swap_amounts: SwapAmounts,

    alice_params: AliceParams,
    alice_starting_balances: StartingBalances,
    alice_bitcoin_wallet: Arc<bitcoin::Wallet>,
    alice_monero_wallet: Arc<monero::Wallet>,

    bob_params: BobParams,
    bob_starting_balances: StartingBalances,
    bob_bitcoin_wallet: Arc<bitcoin::Wallet>,
    bob_monero_wallet: Arc<monero::Wallet>,
}

impl TestContext {
    pub async fn new_swap_as_alice(&mut self) -> (alice::Swap, AliceEventLoopJoinHandle) {
        let (swap, mut event_loop) = self
            .alice_params
            .builder()
            .with_init_params(self.swap_amounts)
            .build()
            .await
            .unwrap();

        let join_handle = tokio::spawn(async move { event_loop.run().await });

        (swap, AliceEventLoopJoinHandle(join_handle))
    }

    pub async fn new_swap_as_bob(&mut self) -> (bob::Swap, BobEventLoopJoinHandle) {
        let (swap, event_loop) = self
            .bob_params
            .builder()
            .with_init_params(self.swap_amounts)
            .build()
            .await
            .unwrap();

        let join_handle = tokio::spawn(async move { event_loop.run().await });

        (swap, BobEventLoopJoinHandle(join_handle))
    }

    pub async fn stop_and_resume_alice_from_db(
        &mut self,
        join_handle: AliceEventLoopJoinHandle,
    ) -> alice::Swap {
        join_handle.0.abort();

        let (swap, mut event_loop) = self.alice_params.builder().build().await.unwrap();

        tokio::spawn(async move { event_loop.run().await });

        swap
    }

    pub async fn stop_and_resume_bob_from_db(
        &mut self,
        join_handle: BobEventLoopJoinHandle,
    ) -> (bob::Swap, BobEventLoopJoinHandle) {
        join_handle.0.abort();

        let (swap, event_loop) = self.bob_params.builder().build().await.unwrap();

        let join_handle = tokio::spawn(async move { event_loop.run().await });

        (swap, BobEventLoopJoinHandle(join_handle))
    }

    pub async fn assert_alice_redeemed(&self, state: AliceState) {
        assert!(matches!(state, AliceState::BtcRedeemed));

        self.alice_bitcoin_wallet
            .sync_wallet()
            .await
            .expect("Could not sync wallet");

        let btc_balance_after_swap = self.alice_bitcoin_wallet.as_ref().balance().await.unwrap();
        assert_eq!(
            btc_balance_after_swap,
            self.alice_starting_balances.btc + self.swap_amounts.btc
                - bitcoin::Amount::from_sat(bitcoin::TX_FEE)
        );

        let xmr_balance_after_swap = self
            .alice_monero_wallet
            .as_ref()
            .get_balance()
            .await
            .unwrap();
        assert!(xmr_balance_after_swap <= self.alice_starting_balances.xmr - self.swap_amounts.xmr);
    }

    pub async fn assert_alice_refunded(&self, state: AliceState) {
        assert!(matches!(state, AliceState::XmrRefunded));

        self.alice_bitcoin_wallet
            .sync_wallet()
            .await
            .expect("Could not sync wallet");

        let btc_balance_after_swap = self.alice_bitcoin_wallet.as_ref().balance().await.unwrap();
        assert_eq!(btc_balance_after_swap, self.alice_starting_balances.btc);

        // Ensure that Alice's balance is refreshed as we use a newly created wallet
        self.alice_monero_wallet
            .as_ref()
            .inner
            .refresh()
            .await
            .unwrap();
        let xmr_balance_after_swap = self
            .alice_monero_wallet
            .as_ref()
            .get_balance()
            .await
            .unwrap();
        assert_eq!(xmr_balance_after_swap, self.swap_amounts.xmr);
    }

    pub async fn assert_alice_punished(&self, state: AliceState) {
        assert!(matches!(state, AliceState::BtcPunished));

        self.alice_bitcoin_wallet
            .sync_wallet()
            .await
            .expect("Could not sync wallet");

        let btc_balance_after_swap = self.alice_bitcoin_wallet.as_ref().balance().await.unwrap();
        assert_eq!(
            btc_balance_after_swap,
            self.alice_starting_balances.btc + self.swap_amounts.btc
                - bitcoin::Amount::from_sat(2 * bitcoin::TX_FEE)
        );

        let xmr_balance_after_swap = self
            .alice_monero_wallet
            .as_ref()
            .get_balance()
            .await
            .unwrap();
        assert!(xmr_balance_after_swap <= self.alice_starting_balances.xmr - self.swap_amounts.xmr);
    }

    pub async fn assert_bob_redeemed(&self, state: BobState) {
        self.bob_bitcoin_wallet
            .sync_wallet()
            .await
            .expect("Could not sync wallet");

        let lock_tx_id = if let BobState::XmrRedeemed { tx_lock_id } = state {
            tx_lock_id
        } else {
            panic!("Bob in not in xmr redeemed state: {:?}", state);
        };

        let lock_tx_bitcoin_fee = self
            .bob_bitcoin_wallet
            .transaction_fee(lock_tx_id)
            .await
            .unwrap();

        let btc_balance_after_swap = self.bob_bitcoin_wallet.as_ref().balance().await.unwrap();
        assert_eq!(
            btc_balance_after_swap,
            self.bob_starting_balances.btc - self.swap_amounts.btc - lock_tx_bitcoin_fee
        );

        // Ensure that Bob's balance is refreshed as we use a newly created wallet
        self.bob_monero_wallet
            .as_ref()
            .inner
            .refresh()
            .await
            .unwrap();
        let xmr_balance_after_swap = self.bob_monero_wallet.as_ref().get_balance().await.unwrap();
        assert_eq!(
            xmr_balance_after_swap,
            self.bob_starting_balances.xmr + self.swap_amounts.xmr
        );
    }

    pub async fn assert_bob_refunded(&self, state: BobState) {
        self.bob_bitcoin_wallet
            .sync_wallet()
            .await
            .expect("Could not sync wallet");

        let lock_tx_id = if let BobState::BtcRefunded(state4) = state {
            state4.tx_lock_id()
        } else {
            panic!("Bob in not in btc refunded state: {:?}", state);
        };
        let lock_tx_bitcoin_fee = self
            .bob_bitcoin_wallet
            .transaction_fee(lock_tx_id)
            .await
            .unwrap();

        let btc_balance_after_swap = self.bob_bitcoin_wallet.as_ref().balance().await.unwrap();

        let alice_submitted_cancel = btc_balance_after_swap
            == self.bob_starting_balances.btc
                - lock_tx_bitcoin_fee
                - bitcoin::Amount::from_sat(bitcoin::TX_FEE);

        let bob_submitted_cancel = btc_balance_after_swap
            == self.bob_starting_balances.btc
                - lock_tx_bitcoin_fee
                - bitcoin::Amount::from_sat(2 * bitcoin::TX_FEE);

        // The cancel tx can be submitted by both Alice and Bob.
        // Since we cannot be sure who submitted it we have to assert accordingly
        assert!(alice_submitted_cancel || bob_submitted_cancel);

        let xmr_balance_after_swap = self.bob_monero_wallet.as_ref().get_balance().await.unwrap();
        assert_eq!(xmr_balance_after_swap, self.bob_starting_balances.xmr);
    }

    pub async fn assert_bob_punished(&self, state: BobState) {
        self.bob_bitcoin_wallet
            .sync_wallet()
            .await
            .expect("Could not sync wallet");

        let lock_tx_id = if let BobState::BtcPunished { tx_lock_id } = state {
            tx_lock_id
        } else {
            panic!("Bob in not in btc punished state: {:?}", state);
        };

        let lock_tx_bitcoin_fee = self
            .bob_bitcoin_wallet
            .transaction_fee(lock_tx_id)
            .await
            .unwrap();

        let btc_balance_after_swap = self.bob_bitcoin_wallet.as_ref().balance().await.unwrap();
        assert_eq!(
            btc_balance_after_swap,
            self.bob_starting_balances.btc - self.swap_amounts.btc - lock_tx_bitcoin_fee
        );

        let xmr_balance_after_swap = self.bob_monero_wallet.as_ref().get_balance().await.unwrap();
        assert_eq!(xmr_balance_after_swap, self.bob_starting_balances.xmr);
    }
}

pub async fn setup_test<T, F, C>(_config: C, testfn: T)
where
    T: Fn(TestContext) -> F,
    F: Future<Output = ()>,
    C: GetExecutionParams,
{
    let cli = Cli::default();

    let _guard = init_tracing();

    let execution_params = C::get_execution_params();

    let (monero, containers) = testutils::init_containers(&cli).await;

    let swap_amounts = SwapAmounts {
        btc: bitcoin::Amount::from_sat(1_000_000),
        xmr: monero::Amount::from_piconero(1_000_000_000_000),
    };

    let alice_starting_balances = StartingBalances {
        xmr: swap_amounts.xmr * 10,
        btc: bitcoin::Amount::ZERO,
    };

    let port = get_port().expect("Failed to find a free port");

    let listen_address: Multiaddr = format!("/ip4/127.0.0.1/tcp/{}", port)
        .parse()
        .expect("failed to parse Alice's address");

    let electrs_rpc_port = containers
        .electrs
        .get_host_port(testutils::electrs::RPC_PORT)
        .expect("Could not map electrs rpc port");
    let electrs_http_port = containers
        .electrs
        .get_host_port(testutils::electrs::HTTP_PORT)
        .expect("Could not map electrs http port");

    let (alice_bitcoin_wallet, alice_monero_wallet) = init_test_wallets(
        "alice",
        containers.bitcoind_url.clone(),
        &monero,
        alice_starting_balances.clone(),
        tempdir().unwrap().path(),
        electrs_rpc_port,
        electrs_http_port,
    )
    .await;

    let alice_params = AliceParams {
        seed: Seed::random().unwrap(),
        execution_params,
        swap_id: Uuid::new_v4(),
        bitcoin_wallet: alice_bitcoin_wallet.clone(),
        monero_wallet: alice_monero_wallet.clone(),
        db_path: tempdir().unwrap().path().to_path_buf(),
        listen_address,
    };

    let bob_starting_balances = StartingBalances {
        xmr: monero::Amount::ZERO,
        btc: swap_amounts.btc * 10,
    };

    let (bob_bitcoin_wallet, bob_monero_wallet) = init_test_wallets(
        "bob",
        containers.bitcoind_url,
        &monero,
        bob_starting_balances.clone(),
        tempdir().unwrap().path(),
        electrs_rpc_port,
        electrs_http_port,
    )
    .await;

    let bob_params = BobParams {
        seed: Seed::random().unwrap(),
        db_path: tempdir().unwrap().path().to_path_buf(),
        swap_id: Uuid::new_v4(),
        bitcoin_wallet: bob_bitcoin_wallet.clone(),
        monero_wallet: bob_monero_wallet.clone(),
        alice_address: alice_params.listen_address.clone(),
        alice_peer_id: alice_params.peer_id(),
        execution_params,
    };

    let test = TestContext {
        swap_amounts,
        alice_params,
        alice_starting_balances,
        alice_bitcoin_wallet,
        alice_monero_wallet,
        bob_params,
        bob_starting_balances,
        bob_bitcoin_wallet,
        bob_monero_wallet,
    };

    testfn(test).await
}

fn random_prefix() -> String {
    use rand::{distributions::Alphanumeric, thread_rng, Rng};
    use std::iter;
    const LEN: usize = 8;
    let mut rng = thread_rng();
    let chars: String = iter::repeat(())
        .map(|()| rng.sample(Alphanumeric))
        .map(char::from)
        .take(LEN)
        .collect();
    chars
}

async fn init_containers(cli: &Cli) -> (Monero, Containers<'_>) {
    let prefix = random_prefix();
    let bitcoind_name = format!("{}_{}", prefix, "bitcoind");
    let (bitcoind, bitcoind_url) =
        init_bitcoind_container(&cli, prefix.clone(), bitcoind_name.clone(), prefix.clone())
            .await
            .expect("could not init bitcoind");
    let electrs = init_electrs_container(&cli, prefix.clone(), bitcoind_name, prefix)
        .await
        .expect("could not init electrs");
    let (monero, monerods) = init_monero_container(&cli).await;
    (monero, Containers {
        bitcoind_url,
        bitcoind,
        monerods,
        electrs,
    })
}

async fn init_bitcoind_container(
    cli: &Cli,
    volume: String,
    name: String,
    network: String,
) -> Result<(Container<'_, Cli, bitcoind::Bitcoind>, Url)> {
    let image = bitcoind::Bitcoind::default()
        .with_volume(volume)
        .with_tag("0.19.1");

    let run_args = RunArgs::default().with_name(name).with_network(network);

    let docker = cli.run_with_args(image, run_args);
    let a = docker
        .get_host_port(testutils::bitcoind::RPC_PORT)
        .context("Could not map bitcoind rpc port")?;

    let bitcoind_url = {
        let input = format!(
            "http://{}:{}@localhost:{}",
            bitcoind::RPC_USER,
            bitcoind::RPC_PASSWORD,
            a
        );
        Url::parse(&input).unwrap()
    };

    init_bitcoind(bitcoind_url.clone(), 5).await?;

    Ok((docker, bitcoind_url.clone()))
}

pub async fn init_electrs_container(
    cli: &Cli,
    volume: String,
    bitcoind_container_name: String,
    network: String,
) -> Result<Container<'_, Cli, electrs::Electrs>> {
    let bitcoind_rpc_addr = format!(
        "{}:{}",
        bitcoind_container_name,
        testutils::bitcoind::RPC_PORT
    );
    let image = electrs::Electrs::default()
        .with_volume(volume)
        .with_daemon_rpc_addr(bitcoind_rpc_addr)
        .with_tag("latest");

    let run_args = RunArgs::default().with_network(network);

    let docker = cli.run_with_args(image, run_args);

    Ok(docker)
}

async fn mine(bitcoind_client: Client, reward_address: bitcoin::Address) -> Result<()> {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        bitcoind_client
            .generatetoaddress(1, reward_address.clone(), None)
            .await?;
    }
}

async fn init_bitcoind(node_url: Url, spendable_quantity: u32) -> Result<Client> {
    let bitcoind_client = Client::new(node_url.clone());

    bitcoind_client
        .createwallet(TEST_WALLET_NAME, None, None, None, None)
        .await?;

    let reward_address = bitcoind_client
        .with_wallet(TEST_WALLET_NAME)?
        .getnewaddress(None, None)
        .await?;

    bitcoind_client
        .generatetoaddress(101 + spendable_quantity, reward_address.clone(), None)
        .await?;
    let _ = tokio::spawn(mine(bitcoind_client.clone(), reward_address));
    Ok(bitcoind_client)
}

/// Send Bitcoin to the specified address, limited to the spendable bitcoin
/// quantity.
pub async fn mint(node_url: Url, address: bitcoin::Address, amount: bitcoin::Amount) -> Result<()> {
    let bitcoind_client = Client::new(node_url.clone());

    bitcoind_client
        .send_to_address(TEST_WALLET_NAME, address.clone(), amount)
        .await?;

    // Confirm the transaction
    let reward_address = bitcoind_client
        .with_wallet(TEST_WALLET_NAME)?
        .getnewaddress(None, None)
        .await?;
    bitcoind_client
        .generatetoaddress(1, reward_address, None)
        .await?;

    Ok(())
}

async fn init_monero_container(
    cli: &Cli,
) -> (
    Monero,
    Vec<Container<'_, Cli, monero_harness::image::Monero>>,
) {
    let (monero, monerods) = Monero::new(&cli, None, vec!["alice".to_string(), "bob".to_string()])
        .await
        .unwrap();

    (monero, monerods)
}

async fn init_test_wallets(
    name: &str,
    bitcoind_url: Url,
    monero: &Monero,
    starting_balances: StartingBalances,
    datadir: &Path,
    electrum_rpc_port: u16,
    electrum_http_port: u16,
) -> (Arc<bitcoin::Wallet>, Arc<monero::Wallet>) {
    monero
        .init(vec![(name, starting_balances.xmr.as_piconero())])
        .await
        .unwrap();

    let xmr_wallet = swap::monero::Wallet {
        inner: monero.wallet(name).unwrap().client(),
        network: monero::Network::default(),
    };

    let electrum_rpc_url = {
        let input = format!("tcp://@localhost:{}", electrum_rpc_port);
        Url::parse(&input).unwrap()
    };
    let electrum_http_url = {
        let input = format!("http://@localhost:{}", electrum_http_port);
        Url::parse(&input).unwrap()
    };

    let btc_wallet = swap::bitcoin::Wallet::new(
        electrum_rpc_url,
        electrum_http_url,
        bitcoin::Network::Regtest,
        datadir,
    )
    .await
    .expect("could not init btc wallet");

    if starting_balances.btc != bitcoin::Amount::ZERO {
        mint(
            bitcoind_url,
            btc_wallet.new_address().await.unwrap(),
            starting_balances.btc,
        )
        .await
        .expect("could not mint btc starting balance");
    }

    sleep(Duration::from_secs(5)).await;

    btc_wallet
        .sync_wallet()
        .await
        .expect("Could not sync btc wallet");

    (Arc::new(btc_wallet), Arc::new(xmr_wallet))
}

// This is just to keep the containers alive
#[allow(dead_code)]
struct Containers<'a> {
    bitcoind_url: Url,
    bitcoind: Container<'a, Cli, bitcoind::Bitcoind>,
    monerods: Vec<Container<'a, Cli, image::Monero>>,
    electrs: Container<'a, Cli, electrs::Electrs>,
}

/// Utility function to initialize logging in the test environment.
/// Note that you have to keep the `_guard` in scope after calling in test:
///
/// ```rust
/// let _guard = init_tracing();
/// ```
pub fn init_tracing() -> DefaultGuard {
    // converts all log records into tracing events
    // Note: Make sure to initialize without unwrapping, otherwise this causes
    // trouble when running multiple tests.
    let _ = LogTracer::init();

    let global_filter = tracing::Level::WARN;
    let swap_filter = tracing::Level::DEBUG;
    let xmr_btc_filter = tracing::Level::DEBUG;
    let monero_harness_filter = tracing::Level::INFO;
    let bitcoin_harness_filter = tracing::Level::INFO;
    let testcontainers_filter = tracing::Level::DEBUG;

    use tracing_subscriber::util::SubscriberInitExt as _;
    tracing_subscriber::fmt()
        .with_env_filter(format!(
            "{},swap={},xmr_btc={},monero_harness={},bitcoin_harness={},testcontainers={}",
            global_filter,
            swap_filter,
            xmr_btc_filter,
            monero_harness_filter,
            bitcoin_harness_filter,
            testcontainers_filter
        ))
        .set_default()
}

pub mod alice_run_until {
    use swap::protocol::alice::AliceState;

    pub fn is_xmr_locked(state: &AliceState) -> bool {
        matches!(state, AliceState::XmrLocked { .. })
    }

    pub fn is_encsig_learned(state: &AliceState) -> bool {
        matches!(state, AliceState::EncSigLearned { .. })
    }
}

pub mod bob_run_until {
    use swap::protocol::bob::BobState;

    pub fn is_btc_locked(state: &BobState) -> bool {
        matches!(state, BobState::BtcLocked(..))
    }

    pub fn is_lock_proof_received(state: &BobState) -> bool {
        matches!(state, BobState::XmrLockProofReceived { .. })
    }

    pub fn is_xmr_locked(state: &BobState) -> bool {
        matches!(state, BobState::XmrLocked(..))
    }

    pub fn is_encsig_sent(state: &BobState) -> bool {
        matches!(state, BobState::EncSigSent(..))
    }
}

pub struct SlowCancelConfig;

impl GetExecutionParams for SlowCancelConfig {
    fn get_execution_params() -> ExecutionParams {
        ExecutionParams {
            bitcoin_cancel_timelock: Timelock::new(180),
            ..execution_params::Regtest::get_execution_params()
        }
    }
}

pub struct FastCancelConfig;

impl GetExecutionParams for FastCancelConfig {
    fn get_execution_params() -> ExecutionParams {
        ExecutionParams {
            bitcoin_cancel_timelock: Timelock::new(1),
            ..execution_params::Regtest::get_execution_params()
        }
    }
}

pub struct FastPunishConfig;

impl GetExecutionParams for FastPunishConfig {
    fn get_execution_params() -> ExecutionParams {
        ExecutionParams {
            bitcoin_cancel_timelock: Timelock::new(1),
            bitcoin_punish_timelock: Timelock::new(1),
            ..execution_params::Regtest::get_execution_params()
        }
    }
}
