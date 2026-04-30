use anyhow::{Context, Result, bail};
use minik2::*;
use std::path::{Path, PathBuf};

pub(crate) const CONFIG_FILE_NAME: &str = "ever-elect.json";
pub(crate) const DEFAULT_ENDPOINT: &str = "https://rpc-testnet.tychoprotocol.com";
pub(crate) const DEFAULT_CONFIG_FOLDER: &str = "~/.tycho";
pub(crate) const ONE_TOKEN: u128 = 1_000_000_000;
pub(crate) const MASTERCHAIN: i8 = -1;
pub(crate) const BASECHAIN: i8 = 0;
pub(crate) const DEFAULT_DEPOOL_PARTICIPATE_VALUE: &str = "5";
pub(crate) const DEFAULT_DEPOOL_WALLET_RESERVE: &str = "20";
pub(crate) const DEPOOL_ROUND_STEP_WAITING_VALIDATOR_REQUEST: u8 = 2;
pub(crate) const DEPOOL_COMPLETION_REASON_FAKE_ROUND: u8 = 2;
pub(crate) const DEPOOL_MIN_BALANCE: u128 = 20 * ONE_TOKEN;
pub(crate) const DEPOOL_TARGET_BALANCE: u128 = 30 * ONE_TOKEN;
pub(crate) const DEPOOL_PROXY_MIN_BALANCE: u128 = 3 * ONE_TOKEN;
pub(crate) const DEPOOL_PROXY_TARGET_BALANCE: u128 = 5 * ONE_TOKEN;
pub(crate) const DEPOOL_UPDATE_ATTEMPTS: usize = 4;
pub(crate) const DEPOOL_TICKTOCK_VALUE: u128 = ONE_TOKEN;
pub(crate) const DEPOOL_TICKTOCK_INTERVAL_SECS: u64 = 60;

pub(crate) fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".tycho").join(CONFIG_FILE_NAME)
}

pub(crate) fn default_elections_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".tycho").join("elections.json")
}

pub(crate) fn absolute_path(path: &Path) -> Result<PathBuf> {
    let path = expand_home(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("failed to get current directory")?
            .join(path))
    }
}

pub(crate) fn join_user_path(folder: &str, file_name: &str) -> PathBuf {
    PathBuf::from(folder).join(file_name)
}

pub(crate) fn ensure_workchain(address: &str, workchain: i8) -> Result<StdAddr> {
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

pub(crate) fn parse_tokens_to_nano(value: &str) -> Result<u128> {
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

pub(crate) fn expand_home(path: &Path) -> PathBuf {
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

pub(crate) fn log_info(message: impl AsRef<str>) {
    log("INFO", message.as_ref());
}

pub(crate) fn log_error(message: impl AsRef<str>) {
    log("ERROR", message.as_ref());
}

fn log(level: &str, message: &str) {
    println!("{level}: {message}");
}
