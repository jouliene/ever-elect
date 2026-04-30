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
```

The installer installs `rustup` when needed, updates the Rust stable toolchain,
refreshes `minik2`, builds the release binary, and installs it to
`~/.cargo/bin/ever-elect`.

## Init

Initialize the Tycho node first if `~/.tycho/node_keys.json` does not exist:

```bash
tycho node init
```

Then create the ever-elect config:

```bash
ever-elect init
```

Follow the prompts. This writes `~/.tycho/ever-elect.json` and
`~/.config/systemd/user/ever-elect.service`.

## Run

```bash
ever-elect run
```

## Service

```bash
systemctl --user start ever-elect.service
journalctl --user -u ever-elect.service -f
```

Enable autostart:

```bash
systemctl --user enable ever-elect.service
```

If user systemd is not available on the server:

```bash
sudo loginctl enable-linger "$USER"
systemctl --user daemon-reload
```
