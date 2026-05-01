use crate::util::{
    BASECHAIN, DEFAULT_DEPOOL_PARTICIPATE_VALUE, DEFAULT_DEPOOL_WALLET_RESERVE, DEFAULT_ENDPOINT,
    MASTERCHAIN, default_elections_path, ensure_workchain, expand_home, parse_tokens_to_nano,
};
use anyhow::{Context, Result, bail};
use minik2::*;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::time::Duration;

pub(crate) struct LoadedWallet {
    pub(crate) keys: KeyPair,
    pub(crate) address: String,
    pub(crate) elections_stake: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct AppConfig {
    pub(crate) endpoint: String,
    pub(crate) node_keys_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) elections_path: Option<PathBuf>,
    pub(crate) send: bool,
    #[serde(skip_serializing)]
    pub(crate) once: bool,
    #[serde(skip_serializing)]
    pub(crate) retry: usize,
    #[serde(skip_serializing)]
    pub(crate) stake_factor: Option<u32>,
    #[serde(skip_serializing)]
    pub(crate) depool_participate_value: String,
    #[serde(skip_serializing)]
    pub(crate) depool_wallet_reserve: String,
    #[serde(skip_serializing)]
    pub(crate) confirmation_attempts: usize,
    #[serde(skip_serializing)]
    pub(crate) confirmation_interval_secs: u64,
    #[serde(skip_serializing)]
    pub(crate) poll_interval_secs: u64,
    #[serde(skip_serializing)]
    pub(crate) error_retry_interval_secs: u64,
    pub(crate) validation: ValidationConfig,
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
            depool_participate_value: DEFAULT_DEPOOL_PARTICIPATE_VALUE.to_owned(),
            depool_wallet_reserve: DEFAULT_DEPOOL_WALLET_RESERVE.to_owned(),
            confirmation_attempts: 20,
            confirmation_interval_secs: 3,
            poll_interval_secs: 60,
            error_retry_interval_secs: 30,
            validation: ValidationConfig::default(),
        }
    }
}

impl AppConfig {
    pub(crate) fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
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

    pub(crate) fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    pub(crate) fn error_retry_interval(&self) -> Duration {
        Duration::from_secs(self.error_retry_interval_secs)
    }

    pub(crate) fn confirmation_interval(&self) -> Duration {
        Duration::from_secs(self.confirmation_interval_secs)
    }

    pub(crate) fn depool_participate_value_nano(&self) -> Result<u128> {
        let value = parse_tokens_to_nano(&self.depool_participate_value)?;
        if value == 0 {
            bail!("depool_participate_value must be greater than zero");
        }
        Ok(value)
    }

    pub(crate) fn depool_wallet_reserve_nano(&self) -> Result<u128> {
        let value = parse_tokens_to_nano(&self.depool_wallet_reserve)?;
        if value == 0 {
            bail!("depool_wallet_reserve must be greater than zero");
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ValidationConfig {
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
pub(crate) struct SimpleValidationConfig {
    pub(crate) wallet: SimpleWalletConfig,
    pub(crate) stake: StakeConfig,
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
pub(crate) enum SimpleWalletConfig {
    ElectionsJson { path: Option<PathBuf> },
    Stored { wallet: StoredWalletConfig },
}

impl Default for SimpleWalletConfig {
    fn default() -> Self {
        Self::ElectionsJson { path: None }
    }
}

impl SimpleWalletConfig {
    pub(crate) fn load(&self, legacy_path: Option<&PathBuf>) -> Result<LoadedWallet> {
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
pub(crate) enum StakeConfig {
    FromElectionsJson,
    Fixed { amount: String },
    Float { keep_wallet_balance: String },
}

impl StakeConfig {
    pub(crate) fn stake_nano(
        &self,
        wallet_balance: u128,
        election_overhead: u128,
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
                    .checked_sub(keep.saturating_add(election_overhead))
                    .context("wallet balance is too low for floating stake")
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct DepoolValidationConfig {
    pub(crate) validator_wallet: StoredWalletConfig,
    pub(crate) depool: DepoolConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum DepoolConfig {
    New(NewDepoolConfig),
    Existing { address: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct NewDepoolConfig {
    pub(crate) address: String,
    pub(crate) seed: Option<String>,
    pub(crate) public: String,
    pub(crate) secret: String,
    pub(crate) min_stake: String,
    pub(crate) validator_assurance: String,
    pub(crate) participant_reward_fraction: u8,
}

impl NewDepoolConfig {
    pub(crate) fn min_stake_nano(&self) -> Result<u128> {
        parse_tokens_to_nano(&self.min_stake)
    }

    pub(crate) fn validator_assurance_nano(&self) -> Result<u128> {
        parse_tokens_to_nano(&self.validator_assurance)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DepoolRuntimeConfig {
    pub(crate) address: String,
    pub(crate) new: Option<NewDepoolConfig>,
}

impl DepoolRuntimeConfig {
    pub(crate) fn from_config(config: &DepoolConfig) -> Result<Self> {
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

    pub(crate) fn new_validator_assurance_nano(&self) -> Result<u64> {
        let Some(new) = &self.new else {
            return Ok(0);
        };
        let assurance = new.validator_assurance_nano()?;
        u64::try_from(assurance).context("validator assurance does not fit uint64")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct StoredWalletConfig {
    pub(crate) address: String,
    pub(crate) seed: Option<String>,
    pub(crate) public: String,
    pub(crate) secret: String,
}

impl StoredWalletConfig {
    pub(crate) fn from_seed(seed: &str, workchain: i8) -> Result<Self> {
        let keys = KeyPair::from_seed(seed)?;
        let address = EverWallet::compute_address(workchain, keys.public_key())?.to_string();

        Ok(Self {
            address,
            seed: Some(seed.to_owned()),
            public: keys.public_key_hex(),
            secret: keys.secret_key_hex(),
        })
    }

    pub(crate) fn load(&self, workchain: i8) -> Result<LoadedStoredWallet> {
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

pub(crate) struct LoadedStoredWallet {
    pub(crate) keys: KeyPair,
    pub(crate) address: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct NodeKeysFile {
    pub(crate) secret: String,
    pub(crate) public: String,
}

impl NodeKeysFile {
    pub(crate) fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_config_serialization_hides_internal_settings() {
        let value = serde_json::to_value(AppConfig::default()).expect("serialize config");
        let object = value.as_object().expect("config object");

        for key in [
            "once",
            "retry",
            "stake_factor",
            "depool_participate_value",
            "depool_wallet_reserve",
            "confirmation_attempts",
            "confirmation_interval_secs",
            "poll_interval_secs",
            "error_retry_interval_secs",
        ] {
            assert!(!object.contains_key(key), "{key} should be internal");
        }

        assert!(object.contains_key("endpoint"));
        assert!(object.contains_key("node_keys_path"));
        assert!(object.contains_key("send"));
        assert!(object.contains_key("validation"));
    }
}
