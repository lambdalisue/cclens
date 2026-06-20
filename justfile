# cclens task runner. Run inside the Nix dev shell so rustc / clippy / cargo
# resolve to the pinned versions:  nix develop -c just <recipe>
# (or `nix develop` once, then `just <recipe>`).

# default: list the recipes
default:
    @just --list

# lint + format check (the CI gate): rustfmt --check, then clippy with warnings denied
check:
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings

# run the test suite
test:
    cargo test

# format all Rust in place
fmt:
    cargo fmt

# build the release binary
build:
    cargo build --release
