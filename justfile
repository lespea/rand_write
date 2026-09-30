linux := "x86_64-unknown-linux-gnu"
mac := "aarch64-apple-darwin"
windows := "x86_64-pc-windows-msvc"

# Cross builds need: rustup target add aarch64-apple-darwin x86_64-pc-windows-msvc
#                    cargo install --locked cargo-zigbuild cargo-xwin  (plus zig and wine)

# Lint, test and build for every platform
default: clippy test build

# Lint every platform
clippy:
    cargo clippy --target {{linux}} --all-targets -- -D warnings
    cargo clippy --target {{mac}} --all-targets -- -D warnings
    cargo clippy --target {{windows}} --all-targets -- -D warnings

# Run tests on Linux and on Windows (under wine); macOS tests can only be compiled here
test:
    cargo test --target {{linux}}
    cargo xwin test --target {{windows}}
    cargo zigbuild --tests --target {{mac}}

# Release build for every platform
build:
    cargo build --release --target {{linux}}
    cargo zigbuild --release --target {{mac}}
    cargo xwin build --release --target {{windows}}
