#!/bin/sh
set -eu

# Locate the installed extension so updates do not leave a versioned path behind.
extension_dir="$(code --locate-extension rust-lang.rust-analyzer)"
exec "$extension_dir/server/rust-analyzer" "$@"
