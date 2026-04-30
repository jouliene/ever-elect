use anyhow::{Context, Result, bail};
use minik2::*;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::signal;
use tokio::time::{Duration, sleep};

const DEFAULT_CONFIG_PATH: &str = "ever-elect.json";
const ONE_TOKEN: u128 = 1_000_000_000;
const MASTERCHAIN: i8 = -1;

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_owned());
    let app = AppConfig::load(config_path)?;
    let node_keys_file = NodeKeysFile::load(&app.node_keys_path)?;
    let elections = ElectionsFile::load(&app.elections_path)?;

    let node_keys =
        KeyPair::from_secret_hex(&node_keys_file.secret).context("invalid node secret")?;
    let wallet_keys =
        KeyPair::from_secret_hex(&elections.wallet_secret).context("invalid wallet secret")?;

    if node_keys.public_key_hex() != node_keys_file.public {
        bail!("node public key does not match node secret");
    }
    if wallet_keys.public_key_hex() != elections.wallet_public {
        bail!("wallet public key does not match wallet secret");
    }

    let transport = Transport::jrpc(&app.endpoint)?;
    let mut wallet = EverWallet::with_workchain(&transport, wallet_keys, MASTERCHAIN)?;

    if wallet.address().to_string() != elections.wallet_address {
        bail!(
            "wallet address mismatch: config has {}, derived {}",
            elections.wallet_address,
            wallet.address()
        );
    }

    log_info("started validation loop");
    log_info("transport=jrpc");
    log_info(format!("endpoint={}", app.endpoint));
    log_info(format!("wallet={}", wallet.address()));
    log_info(format!("validator_public={}", node_keys.public_key_hex()));
    log_info(format!("stake={}", elections.stake_nano()?));
    log_info(format!("send_enabled={}", app.send));

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                log_info("shutdown signal received");
                break;
            }
            wait = run_once(&transport, &mut wallet, &node_keys, &elections, &app) => {
                let wait = match wait {
                    Ok(wait) => wait,
                    Err(e) => {
                        log_error(format!("election loop iteration failed: {e:#}"));
                        app.error_retry_interval()
                    }
                };

                if app.once {
                    break;
                }

                log_info(format!("sleeping seconds={}", wait.as_secs()));
                tokio::select! {
                    _ = &mut shutdown => {
                        log_info("shutdown signal received");
                        break;
                    }
                    _ = sleep(wait) => {}
                }
            }
        }
    }

    log_info("stopped validation loop");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            log_error(format!("failed to listen for ctrl-c: {e}"));
        }
    };

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let terminate = async {
            match signal(SignalKind::terminate()) {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(e) => {
                    log_error(format!("failed to listen for sigterm: {e}"));
                    std::future::pending::<()>().await;
                }
            }
        };

        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}

async fn run_once(
    transport: &Transport,
    wallet: &mut EverWallet,
    node_keys: &KeyPair,
    elections: &ElectionsFile,
    app: &AppConfig,
) -> Result<Duration> {
    let config = Config::fetch(transport).await?;
    let elector = Elector::from_config(transport, &config)?;
    let elector_data = elector.get_data().await?;
    let elector_state = transport
        .get_account_state(elector.address().to_string())
        .await?;

    wallet.update().await?;

    let gen_utime = elector_state
        .timings()
        .map(|timings| timings.gen_utime)
        .unwrap_or_default();
    let timeline = config.elections_timeline()?;

    log_info(format!(
        "gen_utime={} time_diff={} timeline={timeline:?} wallet_balance={}",
        gen_utime,
        now_sec().saturating_sub(gen_utime),
        wallet.balance()
    ));

    match timeline {
        ElectionTimeline::BeforeElections {
            until_elections_start,
        } => {
            log_info("waiting for the elections to start");
            Ok(clamp_wait_secs(
                until_elections_start,
                app.max_sleep_interval(),
            ))
        }
        ElectionTimeline::AfterElections { until_round_end } => {
            log_info("waiting for the next validation round");
            Ok(clamp_wait_secs(until_round_end, app.max_sleep_interval()))
        }
        ElectionTimeline::Elections {
            until_elections_end,
            elections_end,
            ..
        } => {
            let Some(current) = elector_data.current_election() else {
                log_info("elections are open in config, but elector has no current election");
                return Ok(app.poll_interval());
            };

            let validator_key = HashBytes(node_keys.public_key_bytes());
            if let Some(member) = current.member(&validator_key) {
                log_info(format!(
                    "already participating election_id={} stake={} source={}",
                    current.elect_at, member.msg_value, member.src_addr
                ));
                return Ok(clamp_wait_secs(
                    until_elections_end,
                    app.max_sleep_interval(),
                ));
            }

            if let Some(credit) = elector_data.credit_for(&wallet.address().address) {
                if credit > 0 {
                    log_info(format!("recoverable_previous_stake={credit}"));
                    if app.send {
                        let message =
                            elector.recover_stake_message(config.compute_price_factor(true)?)?;
                        let receipt =
                            send_elector_message(wallet, &config, &message, app.retry).await?;
                        log_info(format!("recover_message_hash={}", receipt.message_hash));
                    }
                }
            }

            let stake = elections.stake_nano()?;
            config.check_stake(stake)?;
            let stake_factor = config.compute_stake_factor(app.stake_factor)?;

            log_info(format!(
                "prepared election request election_id={} elector_election_id={} until_end={} stake={} stake_factor={}",
                elections_end, current.elect_at, until_elections_end, stake, stake_factor
            ));

            if !app.send {
                log_info("dry run enabled; set send=true in ever-elect config to submit stake");
                return Ok(app.poll_interval());
            }

            let message = elector.participate_message(ParticipateParams {
                node_keys,
                wallet_address: &wallet.address().address,
                election_id: current.elect_at,
                stake,
                stake_factor,
                price_factor: config.compute_price_factor(true)?,
                signature_context: config.signature_context()?,
            })?;
            let receipt = send_elector_message(wallet, &config, &message, app.retry).await?;
            log_info(format!("participate_message_hash={}", receipt.message_hash));

            let updated = elector.get_data().await?;
            let current = updated
                .current_election()
                .context("elector has no current election after participation")?;
            let member = current
                .member(&validator_key)
                .context("validator key is not registered after participation")?;
            if member.src_addr != wallet.address().address {
                bail!("registered election source address does not match wallet");
            }

            log_info(format!(
                "election request confirmed election_id={} registered_stake={}",
                current.elect_at, member.msg_value
            ));
            Ok(clamp_wait_secs(
                until_elections_end,
                app.max_sleep_interval(),
            ))
        }
    }
}

async fn send_elector_message(
    wallet: &EverWallet,
    config: &Config,
    message: &ElectorMessage,
    retry: usize,
) -> Result<SendReceipt> {
    let prepared = wallet
        .prepare_transaction_with_signature_context(
            &message.to,
            message.value,
            message.bounce,
            message.flags,
            Some(&message.payload),
            config.signature_context()?,
        )
        .await?;
    let attempts = retry.max(1);
    let mut last_error = None;

    for attempt in 0..attempts {
        match wallet.send_prepared(&prepared).await {
            Ok(receipt) => return Ok(receipt),
            Err(e) => {
                last_error = Some(e);
                if attempt + 1 < attempts {
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    Err(last_error.expect("attempts is never zero"))
}

fn clamp_wait_secs(wait_secs: u32, max: Duration) -> Duration {
    Duration::from_secs(wait_secs as u64)
        .max(Duration::from_secs(5))
        .min(max)
}

fn now_sec() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before unix epoch")
        .as_secs() as u32
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct AppConfig {
    endpoint: String,
    node_keys_path: PathBuf,
    elections_path: PathBuf,
    send: bool,
    once: bool,
    retry: usize,
    stake_factor: Option<u32>,
    poll_interval_secs: u64,
    max_sleep_interval_secs: u64,
    error_retry_interval_secs: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
        Self {
            endpoint: "https://rpc-testnet.tychoprotocol.com".to_owned(),
            node_keys_path: PathBuf::from(format!("{home}/.tycho/node_keys.json")),
            elections_path: PathBuf::from(format!("{home}/.tycho/elections.json")),
            send: false,
            once: false,
            retry: 3,
            stake_factor: None,
            poll_interval_secs: 60,
            max_sleep_interval_secs: 300,
            error_retry_interval_secs: 30,
        }
    }
}

impl AppConfig {
    fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            let default = Self::default();
            fs::write(path, serde_json::to_string_pretty(&default_template())?)
                .with_context(|| format!("failed to write default config {}", path.display()))?;
            log_info(format!("created default config {}", path.display()));
            return Ok(default);
        }

        let data = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    fn max_sleep_interval(&self) -> Duration {
        Duration::from_secs(self.max_sleep_interval_secs)
    }

    fn error_retry_interval(&self) -> Duration {
        Duration::from_secs(self.error_retry_interval_secs)
    }
}

#[derive(serde::Serialize)]
struct AppConfigTemplate<'a> {
    endpoint: &'a str,
    node_keys_path: &'a str,
    elections_path: &'a str,
    send: bool,
    once: bool,
    retry: usize,
    stake_factor: Option<u32>,
    poll_interval_secs: u64,
    max_sleep_interval_secs: u64,
    error_retry_interval_secs: u64,
}

fn default_template() -> AppConfigTemplate<'static> {
    AppConfigTemplate {
        endpoint: "https://rpc-testnet.tychoprotocol.com",
        node_keys_path: "~/.tycho/node_keys.json",
        elections_path: "~/.tycho/elections.json",
        send: false,
        once: false,
        retry: 3,
        stake_factor: None,
        poll_interval_secs: 60,
        max_sleep_interval_secs: 300,
        error_retry_interval_secs: 30,
    }
}

#[derive(Debug, Deserialize)]
struct NodeKeysFile {
    secret: String,
    public: String,
}

impl NodeKeysFile {
    fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = expand_home(path.as_ref());
        let data = fs::read_to_string(&path)
            .with_context(|| format!("failed to read node keys {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
    }
}

#[derive(Debug, Deserialize)]
struct ElectionsFile {
    #[serde(rename = "ty")]
    ty: String,
    wallet_secret: String,
    wallet_public: String,
    wallet_address: String,
    stake: String,
}

impl ElectionsFile {
    fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = expand_home(path.as_ref());
        let data = fs::read_to_string(&path)
            .with_context(|| format!("failed to read elections config {}", path.display()))?;
        let mut this: Self = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if this.ty != "Simple" {
            bail!("only Simple elections config is supported");
        }
        this.stake = this.stake.trim().to_owned();
        Ok(this)
    }

    fn stake_nano(&self) -> Result<u128> {
        self.stake
            .parse::<u128>()
            .context("elections stake must be an integer token amount")?
            .checked_mul(ONE_TOKEN)
            .context("elections stake is too large")
    }
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(path) = path.to_str() else {
        return path.to_owned();
    };

    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    PathBuf::from(path)
}

fn log_info(message: impl AsRef<str>) {
    log("INFO", message.as_ref());
}

fn log_error(message: impl AsRef<str>) {
    log("ERROR", message.as_ref());
}

fn log(level: &str, message: &str) {
    let ts = chrono::Local::now().format("%b %d %H:%M:%S");
    println!("{ts} ever-elect[{level}]: {message}");
}
