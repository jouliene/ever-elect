#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

ensure_rust() {
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi

    if ! command -v rustup >/dev/null 2>&1; then
        if ! command -v curl >/dev/null 2>&1; then
            echo "rustup is not installed and curl is required to install/update Rust" >&2
            echo "install curl first, then rerun ./install.sh" >&2
            exit 1
        fi

        echo "rustup not found; installing Rust stable with rustup..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --default-toolchain stable

        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi

    echo "updating Rust stable toolchain..."
    rustup update stable
    rustup default stable

    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi

    if ! command -v cargo >/dev/null 2>&1; then
        echo "cargo is not available after installing/updating Rust" >&2
        exit 1
    fi
}

ensure_rust

cargo update -p minik2
cargo build --release

mkdir -p "$HOME/.cargo/bin"
install -m 0755 target/release/ever-elect "$HOME/.cargo/bin/ever-elect"

echo "installed $HOME/.cargo/bin/ever-elect"
if [[ ":$PATH:" != *":$HOME/.cargo/bin:"* ]]; then
    echo "add cargo bin to this shell with: source \"$HOME/.cargo/env\""
fi
echo "next: ever-elect init"
