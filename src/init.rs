use crate::config::*;
use crate::util::*;
use anyhow::{Context, Result, bail};
use minik2::*;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

pub(crate) fn init(config_path: PathBuf, explicit_path: bool) -> Result<()> {
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

            let depool = match prompt_choice(
                "Depool",
                &["Create new", "Restore from seed", "Use existing by address"],
                1,
            )? {
                1 => DepoolConfig::New(prompt_new_depool_config(&validator_wallet)?),
                2 => prompt_restore_depool_config()?,
                3 => {
                    let address = prompt_text("Existing DePool address (workchain 0)", "")?;
                    ensure_workchain(&address, BASECHAIN)?;
                    DepoolConfig::Existing(ExistingDepoolConfig::address(address))
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
    prompt_depool_config_from_keys(
        "Generated DePool",
        Some(seed.to_string()),
        keys,
        validator_wallet,
    )
}

fn prompt_restore_depool_config() -> Result<DepoolConfig> {
    let seed = prompt_text("DePool seed phrase", "")?;
    let depool = ExistingDepoolConfig::from_seed(seed)?;

    println!("Restored DePool:");
    println!("  address: {}", depool.address);
    if let Some(public) = &depool.public {
        println!("  public:  {public}");
    }

    Ok(DepoolConfig::Existing(depool))
}

fn prompt_depool_config_from_keys(
    title: &str,
    seed: Option<String>,
    keys: KeyPair,
    validator_wallet: &StoredWalletConfig,
) -> Result<NewDepoolConfig> {
    let address = DePool::compute_address(BASECHAIN, &keys)?.to_string();
    let min_stake = prompt_token_amount("DePool min stake", "100")?;
    let validator_assurance = prompt_token_amount("Validator assurance", "500")?;
    let participant_reward_fraction = prompt_u8("Participant reward fraction", 95)?;

    println!("{title}:");
    println!("  address:          {address}");
    println!("  public:           {}", keys.public_key_hex());
    if let Some(seed) = &seed {
        println!("  seed:             {seed}");
    }
    println!("  validator wallet: {}", validator_wallet.address);

    Ok(NewDepoolConfig {
        address,
        seed,
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
