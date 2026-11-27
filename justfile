# Install or update both binaries from this checkout using Cargo.lock.
# A running service keeps executing the old binary, so restart it.
install:
    cargo install --locked --path crates/otter-cli --force
    cargo install --locked --path crates/agentbox --force
    if otter status | grep '^Service: running' >/dev/null; then otter service restart; fi
