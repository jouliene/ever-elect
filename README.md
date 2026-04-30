# ever-elect

`ever-elect` is a small Tycho validator election helper built with
[`minik2`](https://github.com/jouliene/minik2).

It uses JRPC transport and can participate in validator elections either
directly from a masterchain Ever Wallet or through a workchain DePool.

## Install

```bash
git clone https://github.com/jouliene/ever-elect.git
cd ever-elect
./install.sh
source "$HOME/.cargo/env"
ever-elect init
```

The installer installs Rust with `rustup` when `cargo` is missing, then builds
the release binary and copies it to:

```text
~/.cargo/bin/ever-elect
```

Make sure `~/.cargo/bin` is in `PATH`.

## Init

```bash
ever-elect init
```

This asks for:

- endpoint, defaulting to `https://rpc-testnet.tychoprotocol.com`
- Tycho config folder, defaulting to `~/.tycho`
- validation mode: simple or DePool
- wallet creation/restoration details
- stake policy for simple validation
- DePool deployment or existing DePool details for DePool validation

The config folder must already contain `node_keys.json`; initialize the node
first with `tycho node init`.

`init` creates:

```text
~/.tycho/ever-elect.json
~/.config/systemd/user/ever-elect.service
```

Simple validation config shape:

```json
{
  "endpoint": "https://rpc-testnet.tychoprotocol.com",
  "node_keys_path": "~/.tycho/node_keys.json",
  "send": false,
  "validation": {
    "type": "simple",
    "wallet": {
      "source": "elections_json",
      "path": "~/.tycho/elections.json"
    },
    "stake": {
      "type": "fixed",
      "amount": "500000"
    }
  }
}
```

DePool validation config uses a workchain `0` validator wallet and either an
existing workchain `0` DePool address or a stored DePool deployment plan. Set
`send` to `true` only after checking the endpoint, node key, wallet, DePool, and
stake/deployment settings.

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
