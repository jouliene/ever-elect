# ever-elect

`ever-elect` is a small Tycho validator election helper built with
[`minik2`](https://github.com/jouliene/minik2).

It uses JRPC transport and participates in validator elections from a simple
wallet.

## Install

```bash
git clone https://github.com/jouliene/ever-elect.git
cd ever-elect
./install.sh
ever-elect init
```

The installer builds the release binary and copies it to:

```text
~/.cargo/bin/ever-elect
```

Make sure `~/.cargo/bin` is in `PATH`.

## Init

```bash
ever-elect init
```

This creates:

```text
~/.tycho/ever-elect.json
~/.config/systemd/user/ever-elect.service
```

Default config:

```json
{
  "endpoint": "https://rpc-testnet.tychoprotocol.com",
  "node_keys_path": "~/.tycho/node_keys.json",
  "elections_path": "~/.tycho/elections.json",
  "send": false
}
```

Set `send` to `true` only after checking the endpoint, wallet address, node key,
and stake in `~/.tycho/elections.json`.

## Run

Manual run:

```bash
ever-elect run
```

User service:

```bash
systemctl --user start ever-elect.service
journalctl --user -u ever-elect.service -f
```

Enable service autostart:

```bash
systemctl --user enable ever-elect.service
```
