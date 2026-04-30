use anyhow::{Context, Result, bail};
use minik2::*;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::signal;
use tokio::time::{Duration, sleep};

const CONFIG_FILE_NAME: &str = "ever-elect.json";
const DEFAULT_ENDPOINT: &str = "https://rpc-testnet.tychoprotocol.com";
const DEFAULT_CONFIG_FOLDER: &str = "~/.tycho";
const ONE_TOKEN: u128 = 1_000_000_000;
const MASTERCHAIN: i8 = -1;
const BASECHAIN: i8 = 0;
const DEPOOL_ROUND_STEP_WAITING_VALIDATOR_REQUEST: u8 = 2;

#[tokio::main]
async fn main() -> Result<()> {
    match CliCommand::parse()? {
        CliCommand::Run { config_path } => run(config_path).await,
        CliCommand::Init {
            config_path,
            explicit_path,
        } => init(config_path, explicit_path),
        CliCommand::Help => {
            print_help();
            Ok(())
        }
    }
}

async fn run(config_path: PathBuf) -> Result<()> {
    let app = AppConfig::load(config_path)?;
    let node_keys_file = NodeKeysFile::load(&app.node_keys_path)?;
    let node_keys =
        KeyPair::from_secret_hex(&node_keys_file.secret).context("invalid node secret")?;

    if node_keys.public_key_hex() != node_keys_file.public {
        bail!("node public key does not match node secret");
    }

    let transport = Transport::jrpc(&app.endpoint)?;
    let mut runtime = RuntimeState::new(&transport, &app)?;

    log_info("started validation loop");
    log_info("transport=jrpc");
    log_info(format!("endpoint={}", app.endpoint));
    log_info(format!("node_keys={}", app.node_keys_path.display()));
    log_info(format!("validation={}", runtime.validation.name()));
    log_info(format!("wallet={}", runtime.wallet.address()));
    log_info(format!("validator_public={}", node_keys.public_key_hex()));
    log_info(format!("send_enabled={}", app.send));

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                log_info("shutdown signal received");
                break;
            }
            wait = run_once(&transport, &mut runtime, &node_keys, &app) => {
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
    Run {
        config_path: PathBuf,
    },
    Init {
        config_path: PathBuf,
        explicit_path: bool,
    },
    Help,
}

impl CliCommand {
    fn parse() -> Result<Self> {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        match args.as_slice() {
            [] => Ok(Self::Run {
                config_path: default_config_path(),
            }),
            [cmd] if cmd == "run" => Ok(Self::Run {
                config_path: default_config_path(),
            }),
            [cmd, path] if cmd == "run" => Ok(Self::Run {
                config_path: PathBuf::from(path),
            }),
            [cmd] if cmd == "init" => Ok(Self::Init {
                config_path: default_config_path(),
                explicit_path: false,
            }),
            [cmd, path] if cmd == "init" => Ok(Self::Init {
                config_path: PathBuf::from(path),
                explicit_path: true,
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
        "Usage:\n  ever-elect run [config]\n  ever-elect init [config]\n  ever-elect help\n\nDefault config: ~/.tycho/ever-elect.json"
    );
}

fn init(config_path: PathBuf, explicit_path: bool) -> Result<()> {
    let config_path = absolute_path(&config_path)?;
    let default_folder = if explicit_path {
        config_path
            .parent()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| DEFAULT_CONFIG_FOLDER.to_owned())
    } else {
        DEFAULT_CONFIG_FOLDER.to_owned()
    };

    println!("ever-elect init");
    let endpoint = prompt_text("Endpoint", DEFAULT_ENDPOINT)?;
    let config_folder = prompt_text("Config folder", &default_folder)?;
    let node_keys_path = join_user_path(&config_folder, "node_keys.json");

    if !expand_home(&node_keys_path).exists() {
        bail!(
            "node keys not found at {}; initialize the node first with `tycho node init`",
            node_keys_path.display()
        );
    }

    let validation = prompt_validation(&config_folder)?;
    let config = AppConfig {
        endpoint,
        node_keys_path,
        elections_path: None,
        validation,
        ..AppConfig::default()
    };

    let write_path = if explicit_path {
        config_path
    } else {
        expand_home(&join_user_path(&config_folder, CONFIG_FILE_NAME))
    };

    if write_path.exists()
        && !prompt_confirm(
            &format!("Overwrite existing {}?", write_path.display()),
            false,
        )?
    {
        log_info(format!("left config unchanged {}", write_path.display()));
    } else {
        write_config(&write_path, &config)?;
        log_info(format!("wrote config {}", write_path.display()));
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

    fs::write(&service_path, service_unit(&exe, &write_path, &working_dir))
        .with_context(|| format!("failed to write {}", service_path.display()))?;
    log_info(format!("wrote user service {}", service_path.display()));

    if reload_user_systemd() {
        log_info("start with: systemctl --user start ever-elect.service");
        log_info("enable with: systemctl --user enable ever-elect.service");
        log_info("logs with: journalctl --user -u ever-elect.service -f");
    } else {
        log_info(format!(
            "manual run: ever-elect run {}",
            write_path.display()
        ));
        log_info("user systemd is not available in this session");
        log_info("to use the service, enable lingering with: sudo loginctl enable-linger $USER");
        log_info(
            "then log out/in or start a proper user session and rerun: systemctl --user daemon-reload",
        );
    }
    Ok(())
}

fn prompt_validation(config_folder: &str) -> Result<ValidationConfig> {
    match prompt_choice("Type of validation", &["Simple", "Depool"], 1)? {
        1 => {
            let wallet = match prompt_choice(
                "Validator's wallet (must be in masterchain: -1:...)",
                &["Use from elections.json", "Create new", "Restore from seed"],
                1,
            )? {
                1 => SimpleWalletConfig::ElectionsJson {
                    path: Some(join_user_path(config_folder, "elections.json")),
                },
                2 => SimpleWalletConfig::Stored {
                    wallet: create_wallet_config(MASTERCHAIN)?,
                },
                3 => SimpleWalletConfig::Stored {
                    wallet: restore_wallet_config(MASTERCHAIN)?,
                },
                _ => unreachable!(),
            };

            let stake = prompt_simple_stake()?;

            Ok(ValidationConfig::Simple(SimpleValidationConfig {
                wallet,
                stake,
            }))
        }
        2 => {
            let validator_wallet = match prompt_choice(
                "Validator's wallet (must be in workchain: 0:...)",
                &["Create new", "Restore from seed"],
                1,
            )? {
                1 => create_wallet_config(BASECHAIN)?,
                2 => restore_wallet_config(BASECHAIN)?,
                _ => unreachable!(),
            };

            let depool = match prompt_choice("Depool", &["Create new", "Use existing"], 1)? {
                1 => DepoolConfig::New(prompt_new_depool_config(&validator_wallet)?),
                2 => {
                    let address = prompt_text("Existing DePool address (workchain 0)", "")?;
                    ensure_workchain(&address, BASECHAIN)?;
                    DepoolConfig::Existing { address }
                }
                _ => unreachable!(),
            };

            Ok(ValidationConfig::Depool(DepoolValidationConfig {
                validator_wallet,
                depool,
            }))
        }
        _ => unreachable!(),
    }
}

fn prompt_simple_stake() -> Result<StakeConfig> {
    match prompt_choice(
        "Stake size for simple validation",
        &[
            "Fixed amount",
            "Float: use wallet balance except reserved amount",
        ],
        1,
    )? {
        1 => {
            let amount = prompt_token_amount("Fixed stake amount", "500000")?;
            Ok(StakeConfig::Fixed { amount })
        }
        2 => {
            let keep_wallet_balance = prompt_token_amount("Keep on wallet before staking", "100")?;
            Ok(StakeConfig::Float {
                keep_wallet_balance,
            })
        }
        _ => unreachable!(),
    }
}

fn create_wallet_config(workchain: i8) -> Result<StoredWalletConfig> {
    let seed = Seed::generate()?;
    let wallet = StoredWalletConfig::from_seed(seed.as_str(), workchain)?;

    println!("Generated wallet:");
    println!("  address: {}", wallet.address);
    println!("  public:  {}", wallet.public);
    println!("  seed:    {}", seed.as_str());
    println!("Back up this seed. It will also be stored in ever-elect.json.");

    Ok(wallet)
}

fn restore_wallet_config(workchain: i8) -> Result<StoredWalletConfig> {
    let seed = prompt_text("Seed phrase", "")?;
    let wallet = StoredWalletConfig::from_seed(&seed, workchain)?;

    println!("Restored wallet:");
    println!("  address: {}", wallet.address);
    println!("  public:  {}", wallet.public);

    Ok(wallet)
}

fn prompt_new_depool_config(validator_wallet: &StoredWalletConfig) -> Result<NewDepoolConfig> {
    let seed = Seed::generate()?;
    let keys = KeyPair::from_seed(seed.as_str())?;
    let address = DePool::compute_address(BASECHAIN, &keys)?.to_string();
    let min_stake = prompt_token_amount("DePool min stake", "100")?;
    let validator_assurance = prompt_token_amount("Validator assurance", "500")?;
    let participant_reward_fraction = prompt_u8("Participant reward fraction", 95)?;

    println!("Generated DePool:");
    println!("  address:          {address}");
    println!("  public:           {}", keys.public_key_hex());
    println!("  seed:             {}", seed.as_str());
    println!("  validator wallet: {}", validator_wallet.address);

    Ok(NewDepoolConfig {
        address,
        seed: Some(seed.to_string()),
        public: keys.public_key_hex(),
        secret: keys.secret_key_hex(),
        min_stake,
        validator_assurance,
        participant_reward_fraction,
    })
}

fn prompt_text(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    io::stdout().flush().context("failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read stdin")?;
    let input = input.trim();

    if input.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(input.to_owned())
    }
}

fn prompt_choice(label: &str, options: &[&str], default: usize) -> Result<usize> {
    println!("{label}:");
    for (idx, option) in options.iter().enumerate() {
        println!("{}. {}", idx + 1, option);
    }

    loop {
        let input = prompt_text("Select", &default.to_string())?;
        let choice = input
            .parse::<usize>()
            .with_context(|| format!("invalid choice `{input}`"))?;
        if (1..=options.len()).contains(&choice) {
            return Ok(choice);
        }
        println!("Choose a number from 1 to {}", options.len());
    }
}

fn prompt_confirm(label: &str, default: bool) -> Result<bool> {
    let default_text = if default { "Y/n" } else { "y/N" };
    loop {
        let input = prompt_text(label, default_text)?;
        match input.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" | "Y/n" | "y/n" => return Ok(true),
            "n" | "no" | "N/y" | "n/y" | "y/N" | "Y/N" => return Ok(false),
            _ => println!("Please answer y or n"),
        }
    }
}

fn prompt_token_amount(label: &str, default: &str) -> Result<String> {
    let value = prompt_text(label, default)?;
    parse_tokens_to_nano(&value).with_context(|| format!("invalid token amount `{value}`"))?;
    Ok(value)
}

fn prompt_u8(label: &str, default: u8) -> Result<u8> {
    let value = prompt_text(label, &default.to_string())?;
    value
        .parse::<u8>()
        .with_context(|| format!("invalid u8 value `{value}`"))
}

fn write_config(path: &Path, config: &AppConfig) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let config = format!("{}\n", serde_json::to_string_pretty(config)?);
    fs::write(path, config).with_context(|| format!("failed to write {}", path.display()))
}

fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".tycho").join(CONFIG_FILE_NAME)
}

fn default_elections_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".tycho").join("elections.json")
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

fn join_user_path(folder: &str, file_name: &str) -> PathBuf {
    PathBuf::from(folder).join(file_name)
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

fn reload_user_systemd() -> bool {
    match ProcessCommand::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output()
    {
        Ok(output) if output.status.success() => {
            log_info("reloaded user systemd manager");
            true
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            if stderr.is_empty() {
                log_error(format!(
                    "systemctl --user daemon-reload exited with {}",
                    output.status
                ));
            } else {
                log_error(format!(
                    "systemctl --user daemon-reload exited with {}: {}",
                    output.status, stderr
                ));
            }
            false
        }
        Err(e) => {
            log_error(format!("failed to run systemctl --user daemon-reload: {e}"));
            false
        }
    }
}

async fn run_once(
    transport: &Transport,
    runtime: &mut RuntimeState,
    node_keys: &KeyPair,
    app: &AppConfig,
) -> Result<Duration> {
    let config = Config::fetch(transport).await?;
    let elector = Elector::from_config(transport, &config)?;
    let elector_data = elector.get_data().await?;
    let elector_state = transport
        .get_account_state(elector.address().to_string())
        .await?;

    runtime.wallet.update().await?;

    let gen_utime = elector_state
        .timings()
        .map(|timings| timings.gen_utime)
        .unwrap_or_default();
    let timeline = config.elections_timeline()?;

    log_info(format!(
        "gen_utime={} time_diff={} timeline={timeline:?} wallet_balance={}",
        gen_utime,
        now_sec().saturating_sub(gen_utime),
        runtime.wallet.balance()
    ));

    match &mut runtime.validation {
        RuntimeValidation::Depool { depool, config } => {
            prepare_depool(&mut runtime.wallet, depool.as_mut(), config, app).await?;
        }
        RuntimeValidation::Simple { .. } => {}
    }

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

            match &mut runtime.validation {
                RuntimeValidation::Simple {
                    stake,
                    elections_stake,
                } => {
                    run_simple_election(
                        &config,
                        &elector,
                        current,
                        &elector_data,
                        &mut runtime.wallet,
                        node_keys,
                        stake,
                        elections_stake.as_deref(),
                        app,
                        elections_end,
                        until_elections_end,
                    )
                    .await
                }
                RuntimeValidation::Depool {
                    depool,
                    config: depool_config,
                } => {
                    run_depool_election(
                        &config,
                        &elector,
                        current,
                        depool.as_mut(),
                        depool_config,
                        &mut runtime.wallet,
                        node_keys,
                        app,
                        elections_end,
                        until_elections_end,
                    )
                    .await
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_simple_election(
    config: &Config,
    elector: &Elector,
    current: &CurrentElectionData,
    elector_data: &ElectorData,
    wallet: &mut EverWallet,
    node_keys: &KeyPair,
    stake_config: &StakeConfig,
    elections_stake: Option<&str>,
    app: &AppConfig,
    elections_end: u32,
    until_elections_end: u32,
) -> Result<Duration> {
    let validator_key = HashBytes(node_keys.public_key_bytes());
    if let Some(member) = current.member(&validator_key) {
        log_info(format!(
            "already participating election_id={} stake={} source={}",
            current.elect_at, member.msg_value, member.src_addr
        ));
        return Ok(boundary_wait_secs(until_elections_end));
    }

    if let Some(credit) = elector_data.credit_for(&wallet.address().address)
        && credit > 0
    {
        log_info(format!("recoverable_previous_stake={credit}"));
        if app.send {
            let message = elector.recover_stake_message(config.compute_price_factor(true)?)?;
            let receipt = send_elector_message(wallet, config, &message, app.retry).await?;
            log_info(format!("recover_message_hash={}", receipt.message_hash));
        }
    }

    let elector_gas = apply_price_factor(ONE_TOKEN, config.compute_price_factor(true)?);
    let stake = stake_config.stake_nano(wallet.balance(), elector_gas, elections_stake)?;
    config.check_stake(stake)?;
    let stake_factor = config.compute_stake_factor(app.stake_factor)?;

    log_info(format!(
        "prepared simple election request election_id={} elector_election_id={} until_end={} stake={} stake_factor={}",
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
    let receipt = send_elector_message(wallet, config, &message, app.retry).await?;
    log_info(format!("participate_message_hash={}", receipt.message_hash));

    confirm_simple_participation(
        elector,
        &validator_key,
        wallet.address(),
        app,
        until_elections_end,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_depool_election(
    config: &Config,
    elector: &Elector,
    current: &CurrentElectionData,
    depool: &mut DePool,
    depool_config: &DepoolRuntimeConfig,
    wallet: &mut EverWallet,
    node_keys: &KeyPair,
    app: &AppConfig,
    elections_end: u32,
    until_elections_end: u32,
) -> Result<Duration> {
    let validator_key = HashBytes(node_keys.public_key_bytes());
    if let Some(member) = current.member(&validator_key)
        && depool_has_source(depool, &member.src_addr)
    {
        log_info(format!(
            "already participating via depool election_id={} stake={} source={}",
            current.elect_at, member.msg_value, member.src_addr
        ));
        return Ok(boundary_wait_secs(until_elections_end));
    }

    depool.update().await?;
    if app.send {
        let receipt = depool.ticktock(wallet).await?;
        log_info(format!("depool_ticktock_hash={}", receipt.message_hash));
        depool.update().await?;
    }

    let Some((round_id, proxy)) = select_ready_depool_round(depool, current.elect_at)? else {
        log_info(format!(
            "depool is not ready for election_id={}; waiting for a round in WaitingValidatorRequest; rounds={}",
            current.elect_at,
            format_depool_rounds(depool)
        ));
        return Ok(app.poll_interval());
    };

    let stake_factor = config.compute_stake_factor(app.stake_factor)?;
    let adnl_addr = validator_key;
    let data_to_sign =
        build_elections_data_to_sign(current.elect_at, stake_factor, &proxy.address, &adnl_addr);
    let data_to_sign = config.signature_context()?.apply(&data_to_sign);
    let signature = node_keys.sign(&data_to_sign).to_bytes();

    log_info(format!(
        "prepared depool election request election_id={} elector_election_id={} until_end={} depool={} round_id={} proxy={} stake_factor={}",
        elections_end,
        current.elect_at,
        until_elections_end,
        depool.address,
        round_id,
        proxy,
        stake_factor
    ));

    if !app.send {
        log_info("dry run enabled; set send=true in ever-elect config to submit DePool request");
        return Ok(app.poll_interval());
    }

    let receipt = depool
        .participate_in_elections(
            wallet,
            now_millis()?,
            validator_key,
            current.elect_at,
            stake_factor,
            adnl_addr,
            signature,
        )
        .await?;
    log_info(format!(
        "depool_participate_message_hash={}",
        receipt.message_hash
    ));

    confirm_depool_participation(elector, depool, &validator_key, app, until_elections_end)
        .await
        .with_context(|| format!("depool={}", depool_config.address))
}

async fn prepare_depool(
    wallet: &mut EverWallet,
    depool: &mut DePool,
    config: &DepoolRuntimeConfig,
    app: &AppConfig,
) -> Result<()> {
    depool.update().await?;

    if !depool.is_active() {
        let Some(new_depool) = config.new.as_ref() else {
            bail!("configured DePool {} is not active", depool.address);
        };

        log_info(format!("depool_not_active address={}", depool.address));
        if !app.send {
            log_info("dry run enabled; skipping DePool topup/deploy");
            return Ok(());
        }

        let target_balance = MIN_BALANCE_FOR_DEPLOY + DEFAULT_DEPOOL_GAS;
        if depool.account_balance < target_balance {
            let topup = target_balance - depool.account_balance;
            log_info(format!(
                "topping_up_depool address={} value={topup}",
                depool.address
            ));
            let receipt = wallet
                .send_transaction_safe_with_retry(&depool.address, topup, false, 3, None, app.retry)
                .await?;
            log_info(format!("depool_topup_hash={}", receipt.message_hash));
            depool.update().await?;
        }

        let depool_keys = KeyPair::from_secret_hex(&new_depool.secret)?;
        let receipt = depool
            .deploy(
                &depool_keys,
                new_depool.min_stake_nano()?,
                new_depool.validator_assurance_nano()?,
                wallet.address(),
                new_depool.participant_reward_fraction,
            )
            .await?;
        log_info(format!("depool_deploy_hash={}", receipt.message_hash));
        depool.update().await?;
    }

    if let Some(validator_wallet) = &depool.validator_wallet
        && validator_wallet != wallet.address()
    {
        bail!(
            "DePool validator wallet mismatch: depool has {}, configured wallet is {}",
            validator_wallet,
            wallet.address()
        );
    }

    let participant_stake = depool
        .get_participant_info(wallet.address())?
        .map(|participant| participant.total_round_stake)
        .unwrap_or_default();
    let required = depool
        .validator_assurance
        .max(depool.min_stake)
        .max(config.new_validator_assurance_nano()?);

    if participant_stake >= required as u128 {
        log_info(format!(
            "depool_validator_stake_ready current={} required={}",
            participant_stake, required
        ));
        return Ok(());
    }

    wallet.update().await?;
    let Some(stake) = depool_available_stake(wallet.balance()) else {
        log_info(format!(
            "wallet balance is too low for DePool staking balance={} required={}",
            wallet.balance(),
            required
        ));
        return Ok(());
    };

    log_info(format!(
        "depool_validator_stake_missing current={} required={} available_stake={}",
        participant_stake, required, stake
    ));

    if !app.send {
        log_info("dry run enabled; skipping DePool addOrdinaryStake");
        return Ok(());
    }

    let receipt = depool.add_ordinary_stake(wallet, stake).await?;
    log_info(format!("depool_add_stake_hash={}", receipt.message_hash));
    depool.update().await?;
    Ok(())
}

fn depool_available_stake(balance: u128) -> Option<u128> {
    let reserve = DEFAULT_DEPOOL_GAS.saturating_mul(2);
    (balance > reserve).then_some(balance - reserve)
}

async fn confirm_simple_participation(
    elector: &Elector,
    validator_key: &HashBytes,
    wallet_address: &StdAddr,
    app: &AppConfig,
    until_elections_end: u32,
) -> Result<Duration> {
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

        if let Some(member) = current.member(validator_key) {
            if member.src_addr != wallet_address.address {
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

async fn confirm_depool_participation(
    elector: &Elector,
    depool: &mut DePool,
    validator_key: &HashBytes,
    app: &AppConfig,
    until_elections_end: u32,
) -> Result<Duration> {
    for attempt in 1..=app.confirmation_attempts.max(1) {
        if attempt > 1 {
            sleep(app.confirmation_interval()).await;
        }

        depool.update().await?;
        let updated = elector.get_data().await?;
        let Some(current) = updated.current_election() else {
            log_info(format!(
                "waiting for depool election confirmation attempt={} reason=no_current_election",
                attempt
            ));
            continue;
        };

        if let Some(member) = current.member(validator_key) {
            if !depool_has_source(depool, &member.src_addr) {
                bail!("registered election source address does not match a DePool proxy");
            }

            log_info(format!(
                "depool election request confirmed election_id={} registered_stake={} source={}",
                current.elect_at, member.msg_value, member.src_addr
            ));
            return Ok(boundary_wait_secs(until_elections_end));
        }

        log_info(format!(
            "waiting for depool election confirmation attempt={} election_id={}",
            attempt, current.elect_at
        ));
    }

    bail!("validator key is not registered after DePool confirmation timeout")
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

fn select_ready_depool_round(depool: &DePool, election_id: u32) -> Result<Option<(u64, StdAddr)>> {
    if depool.proxies.is_empty() {
        bail!("DePool has no proxies");
    }

    Ok(depool
        .get_rounds()
        .iter()
        .find(|round| {
            round.supposed_elected_at == election_id
                && round.step == DEPOOL_ROUND_STEP_WAITING_VALIDATOR_REQUEST
        })
        .map(|round| {
            (
                round.id,
                depool.proxies[(round.id as usize) % depool.proxies.len()].clone(),
            )
        }))
}

fn format_depool_rounds(depool: &DePool) -> String {
    let rounds = depool
        .get_rounds()
        .iter()
        .map(|round| {
            format!(
                "{{id={}, supposed_elected_at={}, step={}, stake={}, validator_stake={}}}",
                round.id, round.supposed_elected_at, round.step, round.stake, round.validator_stake
            )
        })
        .collect::<Vec<_>>();

    if rounds.is_empty() {
        "none".to_owned()
    } else {
        rounds.join(", ")
    }
}

fn depool_has_source(depool: &DePool, source: &HashBytes) -> bool {
    depool.proxies.iter().any(|proxy| &proxy.address == source)
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

fn now_millis() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_millis() as u64)
}

struct RuntimeState {
    wallet: EverWallet,
    validation: RuntimeValidation,
}

impl RuntimeState {
    fn new(transport: &Transport, app: &AppConfig) -> Result<Self> {
        match &app.validation {
            ValidationConfig::Simple(simple) => {
                let loaded = simple.wallet.load(app.elections_path.as_ref())?;
                let wallet = EverWallet::with_workchain(transport, loaded.keys, MASTERCHAIN)?;
                if wallet.address().to_string() != loaded.address {
                    bail!(
                        "wallet address mismatch: config has {}, derived {}",
                        loaded.address,
                        wallet.address()
                    );
                }

                Ok(Self {
                    wallet,
                    validation: RuntimeValidation::Simple {
                        stake: simple.stake.clone(),
                        elections_stake: loaded.elections_stake,
                    },
                })
            }
            ValidationConfig::Depool(depool_config) => {
                let loaded = depool_config.validator_wallet.load(BASECHAIN)?;
                let wallet = EverWallet::with_workchain(transport, loaded.keys, BASECHAIN)?;
                if wallet.address().to_string() != loaded.address {
                    bail!(
                        "validator wallet address mismatch: config has {}, derived {}",
                        loaded.address,
                        wallet.address()
                    );
                }

                let runtime_config = DepoolRuntimeConfig::from_config(&depool_config.depool)?;
                let depool = DePool::new(transport, runtime_config.address.clone())?;

                Ok(Self {
                    wallet,
                    validation: RuntimeValidation::Depool {
                        depool: Box::new(depool),
                        config: runtime_config,
                    },
                })
            }
        }
    }
}

enum RuntimeValidation {
    Simple {
        stake: StakeConfig,
        elections_stake: Option<String>,
    },
    Depool {
        depool: Box<DePool>,
        config: DepoolRuntimeConfig,
    },
}

impl RuntimeValidation {
    fn name(&self) -> &'static str {
        match self {
            Self::Simple { .. } => "simple",
            Self::Depool { .. } => "depool",
        }
    }
}

struct LoadedWallet {
    keys: KeyPair,
    address: String,
    elections_stake: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
struct AppConfig {
    endpoint: String,
    node_keys_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    elections_path: Option<PathBuf>,
    send: bool,
    once: bool,
    retry: usize,
    stake_factor: Option<u32>,
    confirmation_attempts: usize,
    confirmation_interval_secs: u64,
    poll_interval_secs: u64,
    error_retry_interval_secs: u64,
    validation: ValidationConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
        Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            node_keys_path: PathBuf::from(format!("{home}/.tycho/node_keys.json")),
            elections_path: None,
            send: false,
            once: false,
            retry: 3,
            stake_factor: None,
            confirmation_attempts: 20,
            confirmation_interval_secs: 3,
            poll_interval_secs: 60,
            error_retry_interval_secs: 30,
            validation: ValidationConfig::default(),
        }
    }
}

impl AppConfig {
    fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            bail!(
                "config {} does not exist; run `ever-elect init` first",
                path.display()
            );
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ValidationConfig {
    Simple(SimpleValidationConfig),
    Depool(DepoolValidationConfig),
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self::Simple(SimpleValidationConfig::default())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
struct SimpleValidationConfig {
    wallet: SimpleWalletConfig,
    stake: StakeConfig,
}

impl Default for SimpleValidationConfig {
    fn default() -> Self {
        Self {
            wallet: SimpleWalletConfig::default(),
            stake: StakeConfig::FromElectionsJson,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum SimpleWalletConfig {
    ElectionsJson { path: Option<PathBuf> },
    Stored { wallet: StoredWalletConfig },
}

impl Default for SimpleWalletConfig {
    fn default() -> Self {
        Self::ElectionsJson { path: None }
    }
}

impl SimpleWalletConfig {
    fn load(&self, legacy_path: Option<&PathBuf>) -> Result<LoadedWallet> {
        match self {
            Self::ElectionsJson { path } => {
                let path = path
                    .as_ref()
                    .or(legacy_path)
                    .cloned()
                    .unwrap_or_else(default_elections_path);
                let elections = ElectionsFile::load(path)?;
                let keys = KeyPair::from_secret_hex(&elections.wallet_secret)
                    .context("invalid wallet secret")?;

                if keys.public_key_hex() != elections.wallet_public {
                    bail!("wallet public key does not match wallet secret");
                }

                ensure_workchain(&elections.wallet_address, MASTERCHAIN)?;

                Ok(LoadedWallet {
                    keys,
                    address: elections.wallet_address,
                    elections_stake: Some(elections.stake),
                })
            }
            Self::Stored { wallet } => {
                let loaded = wallet.load(MASTERCHAIN)?;
                Ok(LoadedWallet {
                    keys: loaded.keys,
                    address: loaded.address,
                    elections_stake: None,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StakeConfig {
    FromElectionsJson,
    Fixed { amount: String },
    Float { keep_wallet_balance: String },
}

impl StakeConfig {
    fn stake_nano(
        &self,
        wallet_balance: u128,
        elector_gas: u128,
        elections_stake: Option<&str>,
    ) -> Result<u128> {
        match self {
            Self::FromElectionsJson => {
                let stake =
                    elections_stake.context("stake is not available from elections.json")?;
                parse_tokens_to_nano(stake)
            }
            Self::Fixed { amount } => parse_tokens_to_nano(amount),
            Self::Float {
                keep_wallet_balance,
            } => {
                let keep = parse_tokens_to_nano(keep_wallet_balance)?;
                wallet_balance
                    .checked_sub(keep.saturating_add(elector_gas))
                    .context("wallet balance is too low for floating stake")
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DepoolValidationConfig {
    validator_wallet: StoredWalletConfig,
    depool: DepoolConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum DepoolConfig {
    New(NewDepoolConfig),
    Existing { address: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct NewDepoolConfig {
    address: String,
    seed: Option<String>,
    public: String,
    secret: String,
    min_stake: String,
    validator_assurance: String,
    participant_reward_fraction: u8,
}

impl NewDepoolConfig {
    fn min_stake_nano(&self) -> Result<u128> {
        parse_tokens_to_nano(&self.min_stake)
    }

    fn validator_assurance_nano(&self) -> Result<u128> {
        parse_tokens_to_nano(&self.validator_assurance)
    }
}

#[derive(Debug, Clone)]
struct DepoolRuntimeConfig {
    address: String,
    new: Option<NewDepoolConfig>,
}

impl DepoolRuntimeConfig {
    fn from_config(config: &DepoolConfig) -> Result<Self> {
        match config {
            DepoolConfig::New(new) => {
                ensure_workchain(&new.address, BASECHAIN)?;
                let keys = KeyPair::from_secret_hex(&new.secret)
                    .context("invalid DePool deployment secret")?;
                if keys.public_key_hex() != new.public {
                    bail!("DePool public key does not match secret");
                }
                let expected = DePool::compute_address(BASECHAIN, &keys)?;
                if expected.to_string() != new.address {
                    bail!(
                        "DePool address mismatch: config has {}, derived {}",
                        new.address,
                        expected
                    );
                }

                Ok(Self {
                    address: new.address.clone(),
                    new: Some(new.clone()),
                })
            }
            DepoolConfig::Existing { address } => {
                ensure_workchain(address, BASECHAIN)?;
                Ok(Self {
                    address: address.clone(),
                    new: None,
                })
            }
        }
    }

    fn new_validator_assurance_nano(&self) -> Result<u64> {
        let Some(new) = &self.new else {
            return Ok(0);
        };
        let assurance = new.validator_assurance_nano()?;
        u64::try_from(assurance).context("validator assurance does not fit uint64")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct StoredWalletConfig {
    address: String,
    seed: Option<String>,
    public: String,
    secret: String,
}

impl StoredWalletConfig {
    fn from_seed(seed: &str, workchain: i8) -> Result<Self> {
        let keys = KeyPair::from_seed(seed)?;
        let address = EverWallet::compute_address(workchain, keys.public_key())?.to_string();

        Ok(Self {
            address,
            seed: Some(seed.to_owned()),
            public: keys.public_key_hex(),
            secret: keys.secret_key_hex(),
        })
    }

    fn load(&self, workchain: i8) -> Result<LoadedStoredWallet> {
        ensure_workchain(&self.address, workchain)?;
        let keys = KeyPair::from_secret_hex(&self.secret).context("invalid wallet secret")?;
        if keys.public_key_hex() != self.public {
            bail!("wallet public key does not match wallet secret");
        }
        let expected = EverWallet::compute_address(workchain, keys.public_key())?;
        if expected.to_string() != self.address {
            bail!(
                "wallet address mismatch: config has {}, derived {}",
                self.address,
                expected
            );
        }

        Ok(LoadedStoredWallet {
            keys,
            address: self.address.clone(),
        })
    }
}

struct LoadedStoredWallet {
    keys: KeyPair,
    address: String,
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
}

fn ensure_workchain(address: &str, workchain: i8) -> Result<StdAddr> {
    let address = parse_std_addr(address)?;
    if address.workchain != workchain {
        bail!(
            "address {} must be in workchain {}, got {}",
            address,
            workchain,
            address.workchain
        );
    }
    Ok(address)
}

fn parse_tokens_to_nano(value: &str) -> Result<u128> {
    let value = value.trim();
    if value.is_empty() {
        bail!("amount is empty");
    }

    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    let whole = whole
        .parse::<u128>()
        .with_context(|| format!("invalid whole token amount `{whole}`"))?;
    if frac.len() > 9 {
        bail!("token amount has more than 9 decimal places");
    }
    let mut frac_padded = frac.to_owned();
    while frac_padded.len() < 9 {
        frac_padded.push('0');
    }
    let frac = if frac_padded.is_empty() {
        0
    } else {
        frac_padded
            .parse::<u128>()
            .with_context(|| format!("invalid fractional token amount `{frac}`"))?
    };

    whole
        .checked_mul(ONE_TOKEN)
        .and_then(|whole| whole.checked_add(frac))
        .context("token amount is too large")
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(path) = path.to_str() else {
        return path.to_owned();
    };

    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
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
    println!("{level} ever-elect: {message}");
}
