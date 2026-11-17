#!/bin/bash
# Start memory-box dengan konfigurasi permanen (auth aktif, bind 0.0.0.0).
set -a
source "$HOME/.memory-box/auth.env"
set +a
cd "$HOME/memory-box" || exit 1
exec "$HOME/memory-box/target/release/mb-server"
