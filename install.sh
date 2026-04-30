#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

cargo build --release

mkdir -p "$HOME/.cargo/bin"
install -m 0755 target/release/ever-elect "$HOME/.cargo/bin/ever-elect"

echo "installed $HOME/.cargo/bin/ever-elect"
echo "next: ever-elect init"
