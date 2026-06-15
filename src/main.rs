//! ccoptimizer CLI entry point. See `docs/specs/cli.md`.

fn main() -> anyhow::Result<()> {
    ccoptimizer::cli::run()
}
