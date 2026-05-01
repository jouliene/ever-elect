use crate::config::*;
use crate::util::*;
use anyhow::{Context, Result, bail};
use minik2::*;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::signal;
use tokio::time::{Duration, sleep};

pub(crate) async fn run(config_path: PathBuf) -> Result<()> {
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
    let time_diff = now_sec()
        .saturating_sub(gen_utime)
        .min(MAX_ELECTOR_TIME_DIFF_SECS);
    let timeline = config.elections_timeline()?;

    log_info(format!(
        "gen_utime={} time_diff={} timeline={timeline:?} wallet_balance={}",
        gen_utime,
        time_diff,
        runtime.wallet.balance()
    ));

    match &mut runtime.validation {
        RuntimeValidation::Depool { depool, config, .. } => {
            if !prepare_depool(transport, &mut runtime.wallet, depool.as_mut(), config, app).await?
            {
                return Ok(app.poll_interval());
            }
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
                    if let Some(wait) =
                        simple_stake_unfreeze_wait(&elector_data, elections_end, time_diff)
                    {
                        return Ok(wait);
                    }

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
                    missed_election_id,
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
                        missed_election_id,
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
    let price_factor = config.compute_price_factor(true)?;
    let elector_gas = apply_price_factor(ONE_TOKEN, price_factor);
    let transfer_reserve = simple_transfer_reserve(price_factor);

    if let Some(credit) = elector_data.credit_for(&wallet.address().address)
        && credit > 0
    {
        log_info(format!("recoverable_previous_stake={credit}"));
        let message = elector.recover_stake_message(price_factor)?;
        if !simple_wallet_can_transfer(wallet, message.value, transfer_reserve, "recover_stake")
            .await?
        {
            return Ok(app.poll_interval());
        }
        let receipt = send_elector_message(wallet, config, &message, app.retry).await?;
        log_info(format!("recover_message_hash={}", receipt.message_hash));
        wallet.update().await?;
    }

    if let Some(member) = current.member(&validator_key) {
        log_info(format!(
            "already participating election_id={} stake={} source={}",
            current.elect_at, member.msg_value, member.src_addr
        ));
        return Ok(boundary_wait_secs(until_elections_end));
    }

    let stake_overhead = elector_gas.saturating_add(transfer_reserve);
    let stake = stake_config.stake_nano(wallet.balance(), stake_overhead, elections_stake)?;
    config.check_stake(stake)?;
    let stake_factor = config.compute_stake_factor(app.stake_factor)?;

    log_info(format!(
        "prepared simple election request election_id={} elector_election_id={} until_end={} stake={} stake_factor={}",
        elections_end, current.elect_at, until_elections_end, stake, stake_factor
    ));

    let message = elector.participate_message(ParticipateParams {
        node_keys,
        wallet_address: &wallet.address().address,
        election_id: current.elect_at,
        stake,
        stake_factor,
        price_factor,
        signature_context: config.signature_context()?,
    })?;
    if !simple_wallet_can_transfer(wallet, message.value, transfer_reserve, "participate").await? {
        return Ok(app.poll_interval());
    }
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
    missed_election_id: &mut Option<u32>,
) -> Result<Duration> {
    let validator_key = HashBytes(node_keys.public_key_bytes());
    if missed_election_id.is_some_and(|election_id| election_id != current.elect_at) {
        *missed_election_id = None;
    }

    if let Some(member) = current.member(&validator_key)
        && depool_has_source(depool, &member.src_addr)
    {
        log_info(format!(
            "already participating via depool election_id={} stake={} source={}",
            current.elect_at, member.msg_value, member.src_addr
        ));
        return Ok(boundary_wait_secs(until_elections_end));
    }

    if *missed_election_id == Some(current.elect_at) {
        log_info(format!(
            "depool current election was already marked unavailable election_id={}; waiting for next election; rounds={}",
            current.elect_at,
            format_depool_rounds(depool)
        ));
        return Ok(app.poll_interval());
    }

    let ready_round = match update_depool_for_election(
        wallet,
        depool,
        depool_config,
        app,
        current.elect_at,
    )
    .await?
    {
        DePoolElectionUpdate::Ready(round) => round,
        DePoolElectionUpdate::NotReady => {
            log_info(format!(
                "depool is not ready for election_id={}; rounds={}",
                current.elect_at,
                format_depool_rounds(depool)
            ));
            return Ok(app.poll_interval());
        }
        DePoolElectionUpdate::MissedCurrentElection => {
            *missed_election_id = Some(current.elect_at);
            log_info(format!(
                "depool current election is unavailable for this pooling round election_id={}; waiting for next election; rounds={}",
                current.elect_at,
                format_depool_rounds(depool)
            ));
            return Ok(app.poll_interval());
        }
    };

    if ready_round.step != DEPOOL_ROUND_STEP_WAITING_VALIDATOR_REQUEST {
        log_info(format!(
            "depool target round is not waiting for validator request election_id={} round_id={} step={}",
            current.elect_at, ready_round.id, ready_round.step
        ));
        return Ok(app.poll_interval());
    }

    config.check_stake(ready_round.stake as u128)?;
    let participate_value = app.depool_participate_value_nano()?;

    let stake_factor = config.compute_stake_factor(app.stake_factor)?;
    let adnl_addr = validator_key;
    let data_to_sign = build_elections_data_to_sign(
        current.elect_at,
        stake_factor,
        &ready_round.proxy.address,
        &adnl_addr,
    );
    let data_to_sign = config.signature_context()?.apply(&data_to_sign);
    let signature = node_keys.sign(&data_to_sign).to_bytes();

    log_info(format!(
        "prepared depool election request election_id={} elector_election_id={} until_end={} depool={} round_id={} round_step={} round_supposed_elected_at={} proxy={} round_stake={} validator_stake={} participate_value={} stake_factor={}",
        elections_end,
        current.elect_at,
        until_elections_end,
        depool.address,
        ready_round.id,
        ready_round.step,
        ready_round.supposed_elected_at,
        ready_round.proxy,
        ready_round.stake,
        ready_round.validator_stake,
        participate_value,
        stake_factor
    ));

    let receipt = send_depool_participate_request(
        depool,
        wallet,
        app,
        participate_value,
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
    transport: &Transport,
    wallet: &mut EverWallet,
    depool: &mut DePool,
    config: &DepoolRuntimeConfig,
    app: &AppConfig,
) -> Result<bool> {
    depool.update().await?;

    if !depool.is_active() {
        let Some(new_depool) = config.new.as_ref() else {
            bail!("configured DePool {} is not active", depool.address);
        };

        log_info(format!("depool_not_active address={}", depool.address));

        if !ensure_depool_deploy_balance(wallet, depool, app).await? {
            return Ok(false);
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

    maintain_depool_balance(wallet, depool, app).await?;
    maintain_depool_proxy_balances(transport, wallet, depool, app).await?;

    if let Some(validator_wallet) = &depool.validator_wallet
        && validator_wallet != wallet.address()
    {
        bail!(
            "DePool validator wallet mismatch: depool has {}, configured wallet is {}",
            validator_wallet,
            wallet.address()
        );
    }

    ensure_depool_round_stake(wallet, depool, config, app).await?;

    Ok(true)
}

async fn ensure_depool_deploy_balance(
    wallet: &mut EverWallet,
    depool: &mut DePool,
    app: &AppConfig,
) -> Result<bool> {
    if depool.account_balance >= MIN_BALANCE_FOR_DEPLOY {
        return Ok(true);
    }

    let target_balance = DEPOOL_TARGET_BALANCE.max(MIN_BALANCE_FOR_DEPLOY + DEFAULT_DEPOOL_GAS);
    let topup = target_balance.saturating_sub(depool.account_balance);

    wallet.update().await?;
    if !wallet_can_spend(wallet, app, topup)? {
        log_info(format!(
            "depool_deploy_waiting_for_balance depool={} balance={} required={} missing={} wallet_balance={} wallet_reserve={}",
            depool.address,
            depool.account_balance,
            MIN_BALANCE_FOR_DEPLOY,
            MIN_BALANCE_FOR_DEPLOY.saturating_sub(depool.account_balance),
            wallet.balance(),
            wallet_operation_reserve(app)?
        ));
        log_validator_wallet_topup(wallet, app, topup, "depool_deploy_topup")?;
        return Ok(false);
    }

    log_info(format!(
        "depool_deploy_topup depool={} balance={} target={} topup={}",
        depool.address, depool.account_balance, target_balance, topup
    ));
    match wallet
        .send_transaction_safe_with_retry(&depool.address, topup, false, 3, None, app.retry)
        .await
    {
        Ok(receipt) => {
            log_info(format!(
                "depool_deploy_topup_success depool={} value={} hash={}",
                depool.address, topup, receipt.message_hash
            ));
        }
        Err(e) => {
            log_error(format!(
                "depool_deploy_topup_failed depool={} value={} error={e:#}",
                depool.address, topup
            ));
            return Err(e);
        }
    }

    depool.update().await?;
    Ok(depool.account_balance >= MIN_BALANCE_FOR_DEPLOY)
}

async fn maintain_depool_balance(
    wallet: &mut EverWallet,
    depool: &mut DePool,
    app: &AppConfig,
) -> Result<()> {
    if depool.own_balance >= DEPOOL_MIN_BALANCE as i128 {
        log_info(format!(
            "depool_balance_ready depool={} own_balance={} account_balance={}",
            depool.address, depool.own_balance, depool.account_balance
        ));
        return Ok(());
    }

    let topup = u128::try_from(DEPOOL_TARGET_BALANCE as i128 - depool.own_balance)
        .context("DePool own balance topup does not fit uint128")?;
    log_info(format!(
        "depool_balance_low depool={} own_balance={} account_balance={} target={} topup={}",
        depool.address, depool.own_balance, depool.account_balance, DEPOOL_TARGET_BALANCE, topup
    ));

    wallet.update().await?;
    if !wallet_can_spend(wallet, app, topup)? {
        log_info(format!(
            "wallet balance is too low for DePool balance topup balance={} topup={} reserve={}",
            wallet.balance(),
            topup,
            wallet_operation_reserve(app)?
        ));
        log_validator_wallet_topup(wallet, app, topup, "depool_balance_topup")?;
        return Ok(());
    }

    match depool.receive_funds(wallet, topup).await {
        Ok(receipt) => {
            log_info(format!(
                "depool_balance_topup_success depool={} value={} hash={}",
                depool.address, topup, receipt.message_hash
            ));
            depool.update().await?;
            Ok(())
        }
        Err(e) => {
            log_error(format!(
                "depool_balance_topup_failed depool={} value={} error={e:#}",
                depool.address, topup
            ));
            Err(e)
        }
    }
}

async fn maintain_depool_proxy_balances(
    transport: &Transport,
    wallet: &mut EverWallet,
    depool: &DePool,
    app: &AppConfig,
) -> Result<()> {
    if depool.proxies.is_empty() {
        log_info("depool has no proxies yet; skipping proxy balance check");
        return Ok(());
    }

    for proxy in &depool.proxies {
        let proxy_balance = account_balance(transport, proxy).await?;
        if proxy_balance >= DEPOOL_PROXY_MIN_BALANCE {
            log_info(format!(
                "depool_proxy_balance_ready depool={} proxy={} balance={}",
                depool.address, proxy, proxy_balance
            ));
            continue;
        }

        let topup = DEPOOL_PROXY_TARGET_BALANCE.saturating_sub(proxy_balance);
        log_info(format!(
            "depool_proxy_balance_low depool={} proxy={} balance={} target={} topup={}",
            depool.address, proxy, proxy_balance, DEPOOL_PROXY_TARGET_BALANCE, topup
        ));

        wallet.update().await?;
        if !wallet_can_spend(wallet, app, topup)? {
            log_info(format!(
                "wallet balance is too low for DePool proxy topup balance={} topup={} reserve={}",
                wallet.balance(),
                topup,
                wallet_operation_reserve(app)?
            ));
            log_validator_wallet_topup(wallet, app, topup, "depool_proxy_topup")?;
            continue;
        }

        match wallet
            .send_transaction_safe_with_retry(proxy, topup, false, 3, None, app.retry)
            .await
        {
            Ok(receipt) => {
                log_info(format!(
                    "depool_proxy_topup_success depool={} proxy={} value={} hash={}",
                    depool.address, proxy, topup, receipt.message_hash
                ));
            }
            Err(e) => {
                log_error(format!(
                    "depool_proxy_topup_failed depool={} proxy={} value={} error={e:#}",
                    depool.address, proxy, topup
                ));
                return Err(e);
            }
        }
    }

    Ok(())
}

fn wallet_can_spend(wallet: &EverWallet, app: &AppConfig, value: u128) -> Result<bool> {
    Ok(wallet.balance() >= wallet_required_balance(app, value)?)
}

fn wallet_required_balance(app: &AppConfig, value: u128) -> Result<u128> {
    Ok(value
        .saturating_add(wallet_operation_reserve(app)?)
        .saturating_add(1))
}

fn wallet_operation_reserve(app: &AppConfig) -> Result<u128> {
    Ok(app
        .depool_wallet_reserve_nano()?
        .saturating_add(app.depool_participate_value_nano()?)
        .saturating_add(DEFAULT_DEPOOL_GAS))
}

fn simple_transfer_reserve(price_factor: u64) -> u128 {
    apply_price_factor(ONE_TOKEN / 2, price_factor)
}

async fn simple_wallet_can_transfer(
    wallet: &mut EverWallet,
    value: u128,
    reserve: u128,
    operation: &str,
) -> Result<bool> {
    wallet.update().await?;
    let target_balance = value.saturating_add(reserve);
    if wallet.balance() >= target_balance {
        return Ok(true);
    }

    log_info(format!(
        "wallet balance is too low for simple {operation} balance={} target_balance={} value={} reserve={}",
        wallet.balance(),
        target_balance,
        value,
        reserve
    ));
    log_simple_validator_wallet_topup(wallet, value, reserve, operation);
    Ok(false)
}

fn log_validator_wallet_topup(
    wallet: &EverWallet,
    app: &AppConfig,
    operation_value: u128,
    reason: &str,
) -> Result<()> {
    let required_balance = wallet_required_balance(app, operation_value)?;
    log_info(format!(
        "topup_validator_wallet wallet={} with_at_least={} reason={} balance={} required_balance={} operation_value={} reserve={}",
        wallet.address(),
        required_balance.saturating_sub(wallet.balance()),
        reason,
        wallet.balance(),
        required_balance,
        operation_value,
        wallet_operation_reserve(app)?
    ));
    Ok(())
}

fn log_simple_validator_wallet_topup(
    wallet: &EverWallet,
    operation_value: u128,
    reserve: u128,
    reason: &str,
) {
    let required_balance = operation_value.saturating_add(reserve);
    log_info(format!(
        "topup_validator_wallet wallet={} with_at_least={} reason=simple_{} balance={} required_balance={} operation_value={} reserve={}",
        wallet.address(),
        required_balance.saturating_sub(wallet.balance()),
        reason,
        wallet.balance(),
        required_balance,
        operation_value,
        reserve
    ));
}

async fn update_depool_for_election(
    wallet: &mut EverWallet,
    depool: &mut DePool,
    config: &DepoolRuntimeConfig,
    app: &AppConfig,
    election_id: u32,
) -> Result<DePoolElectionUpdate> {
    let mut sent_ticktock = false;

    for attempt in 1..=DEPOOL_UPDATE_ATTEMPTS {
        depool.update().await?;
        ensure_depool_round_stake(wallet, depool, config, app).await?;
        depool.update().await?;

        let required_stake = required_depool_stake(depool, config)?;
        let Some(target_round) = select_target_depool_round(depool, required_stake, election_id)?
        else {
            log_info(format!(
                "depool target round is not available depool={} attempt={} required={} rounds={}",
                depool.address,
                attempt,
                required_stake,
                format_depool_rounds(depool)
            ));
            return Ok(DePoolElectionUpdate::NotReady);
        };

        log_info(format!(
            "depool_target_round depool={} attempt={} election_id={} round_id={} step={} supposed_elected_at={} stake={} validator_stake={} completion_reason={}",
            depool.address,
            attempt,
            election_id,
            target_round.id,
            target_round.step,
            target_round.supposed_elected_at,
            target_round.stake,
            target_round.validator_stake,
            target_round.completion_reason
        ));

        if target_round.supposed_elected_at == election_id {
            return Ok(DePoolElectionUpdate::Ready(target_round));
        }

        if sent_ticktock && target_round.completion_reason == DEPOOL_COMPLETION_REASON_FAKE_ROUND {
            log_info(format!(
                "depool target round is fake after ticktock depool={} round_id={}",
                depool.address, target_round.id
            ));
            return Ok(DePoolElectionUpdate::NotReady);
        }

        if attempt == DEPOOL_UPDATE_ATTEMPTS {
            if target_round.step == DEPOOL_ROUND_STEP_POOLING {
                log_info(format!(
                    "depool pooling round did not enter validator request after ticktock depool={} election_id={} round_id={} supposed_elected_at={} stake={} validator_stake={} rounds={}",
                    depool.address,
                    election_id,
                    target_round.id,
                    target_round.supposed_elected_at,
                    target_round.stake,
                    target_round.validator_stake,
                    format_depool_rounds(depool)
                ));
                return Ok(DePoolElectionUpdate::MissedCurrentElection);
            }

            log_info(format!(
                "failed to update DePool target round depool={} election_id={} rounds={}",
                depool.address,
                election_id,
                format_depool_rounds(depool)
            ));
            return Ok(DePoolElectionUpdate::NotReady);
        }

        wallet.update().await?;
        if !wallet_can_spend(wallet, app, DEPOOL_TICKTOCK_VALUE)? {
            log_info(format!(
                "wallet balance is too low for DePool ticktock balance={} value={} reserve={}",
                wallet.balance(),
                DEPOOL_TICKTOCK_VALUE,
                wallet_operation_reserve(app)?
            ));
            log_validator_wallet_topup(wallet, app, DEPOOL_TICKTOCK_VALUE, "depool_ticktock")?;
            return Ok(DePoolElectionUpdate::NotReady);
        }

        log_info(format!(
            "depool_ticktock depool={} value={} attempt={}",
            depool.address, DEPOOL_TICKTOCK_VALUE, attempt
        ));
        match send_depool_ticktock_request(depool, wallet, app, DEPOOL_TICKTOCK_VALUE).await {
            Ok(receipt) => {
                log_info(format!(
                    "depool_ticktock_success depool={} value={} hash={}",
                    depool.address, DEPOOL_TICKTOCK_VALUE, receipt.message_hash
                ));
            }
            Err(e) => {
                log_error(format!(
                    "depool_ticktock_failed depool={} value={} error={e:#}",
                    depool.address, DEPOOL_TICKTOCK_VALUE
                ));
                return Err(e);
            }
        }
        sent_ticktock = true;
        sleep(Duration::from_secs(DEPOOL_TICKTOCK_INTERVAL_SECS)).await;
    }

    Ok(DePoolElectionUpdate::NotReady)
}

async fn ensure_depool_round_stake(
    wallet: &mut EverWallet,
    depool: &mut DePool,
    config: &DepoolRuntimeConfig,
    app: &AppConfig,
) -> Result<()> {
    let participant = depool.get_participant_info(wallet.address())?;
    let required = required_depool_stake(depool, config)?;
    let round_stake = depool_pooling_round_stake(depool, participant);

    if round_stake.current >= required as u128 {
        log_info(format!(
            "depool_validator_stake_ready current={} required={} target_rounds={}",
            round_stake.current, required, round_stake.rounds_label
        ));
        return Ok(());
    }

    wallet.update().await?;
    let missing_stake = (required as u128).saturating_sub(round_stake.current);
    let stake_to_add = missing_stake.max(depool.min_stake as u128);
    let stake_message_value = stake_to_add.saturating_add(DEFAULT_DEPOOL_GAS);
    let Some(stake_budget) = depool_available_stake(wallet.balance(), app)? else {
        log_info(format!(
            "wallet balance is too low for DePool staking balance={} required={} wallet_reserve={} participate_reserve={} gas_reserve={}",
            wallet.balance(),
            required,
            app.depool_wallet_reserve_nano()?,
            app.depool_participate_value_nano()?,
            DEFAULT_DEPOOL_GAS
        ));
        log_validator_wallet_topup(wallet, app, stake_message_value, "depool_add_stake")?;
        return Ok(());
    };

    if stake_to_add > stake_budget.stake {
        log_info(format!(
            "wallet balance is too low to reach DePool assurance balance={} current={} required={} missing={} stake_to_add={} available_stake={} target_rounds={}",
            wallet.balance(),
            round_stake.current,
            required,
            missing_stake,
            stake_to_add,
            stake_budget.stake,
            round_stake.rounds_label
        ));
        log_validator_wallet_topup(wallet, app, stake_message_value, "depool_add_stake")?;
        return Ok(());
    }

    log_info(format!(
        "depool_validator_stake_missing current={} required={} missing={} stake_to_add={} available_stake={} target_rounds={} wallet_reserve={} participate_reserve={} gas_reserve={}",
        round_stake.current,
        required,
        missing_stake,
        stake_to_add,
        stake_budget.stake,
        round_stake.rounds_label,
        stake_budget.wallet_reserve,
        stake_budget.participate_reserve,
        stake_budget.gas_reserve
    ));

    log_info(format!(
        "depool_add_stake depool={} stake={} message_value={} target_rounds={}",
        depool.address,
        stake_to_add,
        stake_to_add + DEFAULT_DEPOOL_GAS,
        round_stake.rounds_label
    ));
    match depool.add_ordinary_stake(wallet, stake_to_add).await {
        Ok(receipt) => {
            log_info(format!(
                "depool_add_stake_success depool={} stake={} hash={}",
                depool.address, stake_to_add, receipt.message_hash
            ));
            depool.update().await?;
            Ok(())
        }
        Err(e) => {
            log_error(format!(
                "depool_add_stake_failed depool={} stake={} error={e:#}",
                depool.address, stake_to_add
            ));
            Err(e)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DePoolStakeBudget {
    stake: u128,
    wallet_reserve: u128,
    participate_reserve: u128,
    gas_reserve: u128,
}

fn depool_available_stake(balance: u128, app: &AppConfig) -> Result<Option<DePoolStakeBudget>> {
    let wallet_reserve = app.depool_wallet_reserve_nano()?;
    let participate_reserve = app.depool_participate_value_nano()?;
    let gas_reserve = DEFAULT_DEPOOL_GAS;
    let total_reserve = wallet_reserve
        .saturating_add(participate_reserve)
        .saturating_add(gas_reserve)
        .saturating_add(DEFAULT_DEPOOL_GAS);

    Ok((balance > total_reserve).then_some(DePoolStakeBudget {
        stake: balance - total_reserve,
        wallet_reserve,
        participate_reserve,
        gas_reserve,
    }))
}

fn required_depool_stake(depool: &DePool, config: &DepoolRuntimeConfig) -> Result<u64> {
    Ok(depool
        .validator_assurance
        .max(depool.min_stake)
        .max(config.new_validator_assurance_nano()?))
}

#[derive(Debug, Clone)]
struct DePoolRoundStake {
    current: u128,
    rounds_label: String,
}

fn depool_pooling_round_stake(
    depool: &DePool,
    participant: Option<&DePoolParticipant>,
) -> DePoolRoundStake {
    depool_pooling_round_stake_from_state(depool.get_rounds(), participant)
}

fn depool_pooling_round_stake_from_state(
    rounds: &[DePoolRound],
    participant: Option<&DePoolParticipant>,
) -> DePoolRoundStake {
    if rounds.len() >= 3 {
        let prev_round = &rounds[0];
        let pooling_round = rounds
            .iter()
            .find(|round| round.step == DEPOOL_ROUND_STEP_POOLING)
            .unwrap_or(&rounds[2]);
        let current = participant_round_stake(participant, pooling_round.id)
            + participant_round_stake(participant, prev_round.id);

        return DePoolRoundStake {
            current,
            rounds_label: format!("pooling:{}+prev:{}", pooling_round.id, prev_round.id),
        };
    }

    DePoolRoundStake {
        current: participant
            .map(|participant| participant.total_round_stake)
            .unwrap_or_default(),
        rounds_label: "all".to_owned(),
    }
}

fn participant_round_stake(participant: Option<&DePoolParticipant>, round_id: u64) -> u128 {
    participant
        .and_then(|participant| {
            participant
                .rounds
                .iter()
                .find(|round| round.round_id == round_id)
        })
        .map(|round| round.total as u128)
        .unwrap_or_default()
}

async fn account_balance(transport: &Transport, address: &StdAddr) -> Result<u128> {
    Ok(transport
        .get_account_state(address.to_string())
        .await?
        .account()
        .map(|account| account.balance.tokens.into())
        .unwrap_or_default())
}

#[allow(clippy::too_many_arguments)]
async fn send_depool_participate_request(
    depool: &DePool,
    wallet: &mut EverWallet,
    app: &AppConfig,
    value: u128,
    query_id: u64,
    validator_key: HashBytes,
    stake_at: u32,
    max_factor: u32,
    adnl_addr: HashBytes,
    signature: impl AsRef<[u8]>,
) -> Result<SendReceipt> {
    let payload = build_participate_in_elections_payload(
        query_id,
        validator_key,
        stake_at,
        max_factor,
        adnl_addr,
        signature,
    )?;

    wallet
        .send_transaction_safe_with_retry(
            &depool.address,
            value,
            true,
            3,
            Some(&payload),
            app.retry,
        )
        .await
}

async fn send_depool_ticktock_request(
    depool: &DePool,
    wallet: &mut EverWallet,
    app: &AppConfig,
    value: u128,
) -> Result<SendReceipt> {
    let payload = build_ticktock_payload()?;

    wallet
        .send_transaction_safe_with_retry(
            &depool.address,
            value,
            true,
            3,
            Some(&payload),
            app.retry,
        )
        .await
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

#[derive(Debug, Clone)]
struct ReadyDePoolRound {
    id: u64,
    step: u8,
    completion_reason: u8,
    supposed_elected_at: u32,
    proxy: StdAddr,
    stake: u64,
    validator_stake: u64,
}

#[derive(Debug, Clone)]
enum DePoolElectionUpdate {
    Ready(ReadyDePoolRound),
    NotReady,
    MissedCurrentElection,
}

fn select_target_depool_round(
    depool: &DePool,
    required_stake: u64,
    election_id: u32,
) -> Result<Option<ReadyDePoolRound>> {
    select_target_depool_round_from_state(
        depool.get_rounds(),
        &depool.proxies,
        required_stake,
        election_id,
    )
}

fn select_target_depool_round_from_state(
    rounds: &[DePoolRound],
    proxies: &[StdAddr],
    required_stake: u64,
    election_id: u32,
) -> Result<Option<ReadyDePoolRound>> {
    if proxies.is_empty() {
        bail!("DePool has no proxies");
    }

    let Some(round) = rounds
        .iter()
        .find(|round| round.supposed_elected_at == election_id && round.stake >= required_stake)
        .or_else(|| {
            rounds.iter().find(|round| {
                round.step == DEPOOL_ROUND_STEP_POOLING && round.stake >= required_stake
            })
        })
    else {
        return Ok(None);
    };

    let proxy = proxies[(round.id as usize) % proxies.len()].clone();
    Ok(Some(ReadyDePoolRound {
        id: round.id,
        step: round.step,
        completion_reason: round.completion_reason,
        supposed_elected_at: round.supposed_elected_at,
        proxy,
        stake: round.stake,
        validator_stake: round.validator_stake,
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

fn simple_stake_unfreeze_wait(
    elector_data: &ElectorData,
    elections_end: u32,
    time_diff: u32,
) -> Option<Duration> {
    compute_simple_stake_unfreeze_wait_secs(
        elector_data.nearest_unfreeze_at(elections_end),
        elections_end,
        now_sec(),
        time_diff,
    )
    .map(boundary_wait_secs)
}

fn compute_simple_stake_unfreeze_wait_secs(
    unfreeze_at: Option<u32>,
    elections_end: u32,
    now: u32,
    time_diff: u32,
) -> Option<u32> {
    let unfreeze_at = unfreeze_at?;
    let unfreeze_at = unfreeze_at.saturating_add(SIMPLE_STAKE_UNFREEZE_OFFSET_SECS);
    let elect_deadline = elections_end.saturating_sub(SIMPLE_ELECTIONS_END_OFFSET_SECS);

    if unfreeze_at > elect_deadline {
        log_info(format!(
            "stakes will unfreeze after the simple election deadline unfreeze_at={} elections_end={} end_offset={}",
            unfreeze_at, elections_end, SIMPLE_ELECTIONS_END_OFFSET_SECS
        ));
        return None;
    }

    let until_unfreeze = unfreeze_at.saturating_sub(now);
    if until_unfreeze == 0 {
        return None;
    }

    let wait = until_unfreeze.saturating_add(time_diff);
    log_info(format!(
        "waiting for simple stake unfreeze until_unfreeze={} time_diff={} wait={}",
        until_unfreeze, time_diff, wait
    ));
    Some(wait)
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
                        missed_election_id: None,
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
        missed_election_id: Option<u32>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy() -> StdAddr {
        parse_std_addr("0:1111111111111111111111111111111111111111111111111111111111111111")
            .expect("valid address")
    }

    fn depool_round(id: u64, step: u8, stake: u64, supposed_elected_at: u32) -> DePoolRound {
        DePoolRound {
            id,
            supposed_elected_at,
            unfreeze: 0,
            stake_held_for: 0,
            vset_hash_in_election_phase: 0,
            step,
            completion_reason: 0,
            stake,
            recovered_stake: 0,
            unused: 0,
            is_validator_stake_completed: false,
            participant_reward: 0,
            participant_qty: 0,
            validator_stake: 0,
            validator_remaining_stake: 0,
            handled_stakes_and_rewards: 0,
        }
    }

    fn participant_with_rounds(rounds: &[(u64, u64)]) -> DePoolParticipant {
        let address = proxy();
        DePoolParticipant {
            address: address.clone(),
            round_qty: rounds.len() as u8,
            reward: 0,
            vesting_parts: 0,
            lock_parts: 0,
            reinvest: false,
            withdraw_value: 0,
            vesting_donor: address.clone(),
            lock_donor: address,
            total_round_stake: rounds.iter().map(|(_, total)| *total as u128).sum(),
            rounds: rounds
                .iter()
                .map(|(round_id, total)| DePoolParticipantRound {
                    round_id: *round_id,
                    ordinary: *total,
                    vesting: 0,
                    lock: 0,
                    total: *total,
                })
                .collect(),
        }
    }

    #[test]
    fn selects_funded_pooling_round_as_ticktock_candidate() {
        let rounds = vec![
            depool_round(0, 9, 0, 0),
            depool_round(1, 6, 0, 0),
            depool_round(2, DEPOOL_ROUND_STEP_POOLING, 505_000_000_000_000, 0),
            depool_round(3, 0, 0, 0),
        ];

        let selected = select_target_depool_round_from_state(
            &rounds,
            &[proxy()],
            5_000_000_000_000,
            1_777_634_017,
        )
        .expect("select target round")
        .expect("pooling round should be selected");

        assert_eq!(selected.id, 2);
        assert_eq!(selected.step, DEPOOL_ROUND_STEP_POOLING);
    }

    #[test]
    fn prefers_current_election_round_over_pooling_candidate() {
        let election_id = 1_777_634_017;
        let rounds = vec![
            depool_round(2, DEPOOL_ROUND_STEP_POOLING, 505_000_000_000_000, 0),
            depool_round(
                4,
                DEPOOL_ROUND_STEP_WAITING_VALIDATOR_REQUEST,
                505_000_000_000_000,
                election_id,
            ),
        ];

        let selected = select_target_depool_round_from_state(
            &rounds,
            &[proxy()],
            5_000_000_000_000,
            election_id,
        )
        .expect("select target round")
        .expect("current election round should be selected");

        assert_eq!(selected.id, 4);
        assert_eq!(selected.step, DEPOOL_ROUND_STEP_WAITING_VALIDATOR_REQUEST);
    }

    #[test]
    fn staking_target_tracks_pooling_step_after_ticktock() {
        let election_id = 1_777_634_017;
        let rounds = vec![
            depool_round(0, 9, 0, 0),
            depool_round(1, 6, 0, 0),
            depool_round(
                2,
                DEPOOL_ROUND_STEP_WAITING_VALIDATOR_REQUEST,
                505_000_000_000_000,
                election_id,
            ),
            depool_round(3, DEPOOL_ROUND_STEP_POOLING, 0, 0),
        ];
        let participant = participant_with_rounds(&[(2, 5_000_000_000_000)]);

        let stake = depool_pooling_round_stake_from_state(&rounds, Some(&participant));

        assert_eq!(stake.current, 0);
        assert_eq!(stake.rounds_label, "pooling:3+prev:0");
    }

    #[test]
    fn simple_unfreeze_wait_includes_offset_and_time_diff() {
        let wait = compute_simple_stake_unfreeze_wait_secs(Some(1_000), 2_000, 1_100, 7)
            .expect("wait for unfreeze");

        assert_eq!(wait, 507);
    }

    #[test]
    fn simple_unfreeze_wait_skips_after_election_deadline() {
        let wait = compute_simple_stake_unfreeze_wait_secs(Some(1_500), 2_000, 1_100, 7);

        assert_eq!(wait, None);
    }
}
