# show available recipes
default:
    @just --list --unsorted

# cargo build --release
build-linux:
    cargo build --target x86_64-unknown-linux-gnu --release

# cargo build windows exec (requires: rustup target add x86_64-pc-windows-gnu + mingw-w64)
build-windows:
    cargo build --target x86_64-pc-windows-gnu --release

# cargo fmt
fmt:
    cargo fmt

# cargo fmt --check
fmt-check:
    cargo fmt --check

# cargo clippy --all-targets -- -D warnings
lint:
    cargo clippy --all-targets -- -D warnings

# cargo test
test:
    cargo test

# cargo test --lib
test-lib:
    cargo test --lib

# fmt-check + lint + test
ci: fmt-check lint test

# remove build artifacts
clean:
    rm -rf target/
