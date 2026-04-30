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

The runtime config file is `ever-elect.json`. It is intentionally ignored by
git because it is machine-local. `ever-elect.example.json` shows the default
shape:

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
  "error_retry_interval_secs": 30
}
```

Keep `send` set to `false` for dry runs. Set it to `true` only when the node
keys, wallet keys, wallet address, stake amount, and endpoint are confirmed.

## Run

```bash
./target/release/ever-elect run
```

Or with a custom config path:

```bash
./target/release/ever-elect run /path/to/ever-elect.json
```

No command is treated as `run`, so this still works:

```bash
./target/release/ever-elect
```

The app sleeps until known election boundaries when it has nothing useful to do,
and it wakes immediately on Ctrl-C or `systemctl --user stop`.

## Init User Service

Run init from the directory that should own `ever-elect.json`:

```bash
./target/release/ever-elect init
```

This creates `ever-elect.json` if it does not exist, writes:

```text
~/.config/systemd/user/ever-elect.service
```

and reloads the user systemd manager.

Start it:

```bash
systemctl --user start ever-elect.service
```

Enable it for future user sessions:

```bash
systemctl --user enable ever-elect.service
```

Watch logs:

```bash
journalctl --user -u ever-elect.service -f
```
