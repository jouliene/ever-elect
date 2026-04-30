# ever-elect

`ever-elect` is a small Tycho validator election helper built with
[`minik2`](https://github.com/jouliene/minik2).

It currently uses JRPC transport only. Control socket support will be added
after it is available in `minik2`.

## Build

```bash
cargo build --release
```

## Config

The default config file is `ever-elect.json`:

```json
{
  "endpoint": "https://rpc-testnet.tychoprotocol.com",
  "node_keys_path": "~/.tycho/node_keys.json",
  "elections_path": "~/.tycho/elections.json",
  "send": false,
  "once": false,
  "retry": 3,
  "stake_factor": null,
  "confirmation_attempts": 20,
  "confirmation_interval_secs": 3,
  "poll_interval_secs": 60,
  "max_sleep_interval_secs": 300,
  "error_retry_interval_secs": 30
}
```

Keep `send` set to `false` for dry runs. Set it to `true` only when the node
keys, wallet keys, wallet address, stake amount, and endpoint are confirmed.

## Run

```bash
cargo run --release
```

Or with a custom config path:

```bash
cargo run --release -- /path/to/ever-elect.json
```
