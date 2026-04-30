use anyhow::{Context, Result, bail};
use minik2::*;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::signal;
use tokio::time::{Duration, sleep};

const DEFAULT_CONFIG_PATH: &str = "ever-elect.json";
const ONE_TOKEN: u128 = 1_000_000_000;
const MASTERCHAIN: i8 = -1;

#[tokio::main]
async fn main() -> Result<()> {
    match CliCommand::parse()? {
        CliCommand::Run { config_path } => run(config_path).await,
        CliCommand::Init { config_path } => init(config_path),
        CliCommand::Help => {
            print_help();
            Ok(())
        }
    }
}

async fn run(config_path: PathBuf) -> Result<()> {
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

enum CliCommand {
    Run { config_path: PathBuf },
    Init { config_path: PathBuf },
    Help,
}

impl CliCommand {
    fn parse() -> Result<Self> {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        match args.as_slice() {
            [] => Ok(Self::Run {
                config_path: PathBuf::from(DEFAULT_CONFIG_PATH),
            }),
            [cmd] if cmd == "run" => Ok(Self::Run {
                config_path: PathBuf::from(DEFAULT_CONFIG_PATH),
            }),
            [cmd, path] if cmd == "run" => Ok(Self::Run {
                config_path: PathBuf::from(path),
            }),
            [cmd] if cmd == "init" => Ok(Self::Init {
                config_path: PathBuf::from(DEFAULT_CONFIG_PATH),
            }),
            [cmd, path] if cmd == "init" => Ok(Self::Init {
                config_path: PathBuf::from(path),
            }),
            [cmd] if cmd == "help" || cmd == "--help" || cmd == "-h" => Ok(Self::Help),
            [path] => Ok(Self::Run {
                config_path: PathBuf::from(path),
            }),
            _ => bail!("invalid arguments; use `ever-elect help`"),
        }
    }
}

fn print_help() {
    println!(
        "Usage:\n  ever-elect run [config]\n  ever-elect init [config]\n  ever-elect help\n\nNo command is the same as `ever-elect run ever-elect.json`."
    );
}

fn init(config_path: PathBuf) -> Result<()> {
    let config_path = absolute_path(&config_path)?;
    let created_config = write_default_config_if_missing(&config_path)?;
    if created_config {
        log_info(format!("created config {}", config_path.display()));
    } else {
        log_info(format!("config already exists {}", config_path.display()));
    }

    let exe = std::env::current_exe()
        .context("failed to get current executable path")?
        .canonicalize()
        .context("failed to canonicalize current executable path")?;
    let working_dir = std::env::current_dir()
        .context("failed to get current directory")?
        .canonicalize()
        .context("failed to canonicalize current directory")?;
    let service_path = user_service_path()?;

    if let Some(parent) = service_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    fs::write(
        &service_path,
        service_unit(&exe, &config_path, &working_dir),
    )
    .with_context(|| format!("failed to write {}", service_path.display()))?;
    log_info(format!("wrote user service {}", service_path.display()));

    reload_user_systemd();

    log_info("start with: systemctl --user start ever-elect.service");
    log_info("enable with: systemctl --user enable ever-elect.service");
    log_info("logs with: journalctl --user -u ever-elect.service -f");
    Ok(())
}

fn write_default_config_if_missing(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }

    fs::write(path, serde_json::to_string_pretty(&default_template())?)
        .with_context(|| format!("failed to write default config {}", path.display()))?;
    Ok(true)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    let path = expand_home(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("failed to get current directory")?
            .join(path))
    }
}

fn user_service_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user")
        .join("ever-elect.service"))
}

fn service_unit(exe: &Path, config_path: &Path, working_dir: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Ever Elect validator elections\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={}\n\
         ExecStart={} run {}\n\
         Restart=always\n\
         RestartSec=10\n\
         KillSignal=SIGTERM\n\
         TimeoutStopSec=30\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_quote(working_dir),
        systemd_quote(exe),
        systemd_quote(config_path)
    )
}

fn systemd_quote(path: &Path) -> String {
    let path = path.display().to_string();
    if !path.contains([' ', '\t', '"', '\\']) {
        return path;
    }

    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn reload_user_systemd() {
    match ProcessCommand::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
    {
        Ok(status) if status.success() => log_info("reloaded user systemd manager"),
        Ok(status) => log_error(format!(
            "systemctl --user daemon-reload exited with {status}"
        )),
        Err(e) => log_error(format!("failed to run systemctl --user daemon-reload: {e}")),
    }
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
            Ok(boundary_wait_secs(until_elections_start))
        }
        ElectionTimeline::AfterElections { until_round_end } => {
            log_info("waiting for the next validation round");
            Ok(boundary_wait_secs(until_round_end))
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
                return Ok(boundary_wait_secs(until_elections_end));
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

            for attempt in 1..=app.confirmation_attempts.max(1) {
                if attempt > 1 {
                    sleep(app.confirmation_interval()).await;
                }

                let updated = elector.get_data().await?;
                let Some(current) = updated.current_election() else {
                    log_info(format!(
                        "waiting for election confirmation attempt={} reason=no_current_election",
                        attempt
                    ));
                    continue;
                };

                if let Some(member) = current.member(&validator_key) {
                    if member.src_addr != wallet.address().address {
                        bail!("registered election source address does not match wallet");
                    }

                    log_info(format!(
                        "election request confirmed election_id={} registered_stake={}",
                        current.elect_at, member.msg_value
                    ));
                    return Ok(boundary_wait_secs(until_elections_end));
                }

                log_info(format!(
                    "waiting for election confirmation attempt={} election_id={}",
                    attempt, current.elect_at
                ));
            }

            bail!("validator key is not registered after participation confirmation timeout")
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

fn boundary_wait_secs(wait_secs: u32) -> Duration {
    Duration::from_secs(wait_secs as u64).max(Duration::from_secs(5))
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
    confirmation_attempts: usize,
    confirmation_interval_secs: u64,
    poll_interval_secs: u64,
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
            confirmation_attempts: 20,
            confirmation_interval_secs: 3,
            poll_interval_secs: 60,
            error_retry_interval_secs: 30,
        }
    }
}

impl AppConfig {
    fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            let default = Self::default();
            write_default_config_if_missing(path)?;
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

    fn error_retry_interval(&self) -> Duration {
        Duration::from_secs(self.error_retry_interval_secs)
    }

    fn confirmation_interval(&self) -> Duration {
        Duration::from_secs(self.confirmation_interval_secs)
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
    confirmation_attempts: usize,
    confirmation_interval_secs: u64,
    poll_interval_secs: u64,
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
        confirmation_attempts: 20,
        confirmation_interval_secs: 3,
        poll_interval_secs: 60,
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
