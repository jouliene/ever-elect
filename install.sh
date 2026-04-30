#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

ensure_cargo() {
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi

    if command -v cargo >/dev/null 2>&1; then
        return
    fi

    if ! command -v curl >/dev/null 2>&1; then
        echo "cargo is not installed and curl is required to install Rust with rustup" >&2
        echo "install curl first, then rerun ./install.sh" >&2
        exit 1
    fi

    echo "cargo not found; installing Rust with rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal

    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
}

ensure_cargo

cargo build --release

mkdir -p "$HOME/.cargo/bin"
install -m 0755 target/release/ever-elect "$HOME/.cargo/bin/ever-elect"

echo "installed $HOME/.cargo/bin/ever-elect"
if [[ ":$PATH:" != *":$HOME/.cargo/bin:"* ]]; then
    echo "add cargo bin to this shell with: source \"$HOME/.cargo/env\""
fi
echo "next: ever-elect init"
