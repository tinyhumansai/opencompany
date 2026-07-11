//! Example harness entrypoint.
//!
//! Boots the company declared in `agents.toml` on the shared `opencompany`
//! host crate. See `README.md` for what this harness does and `agents.toml`
//! for the full roster.

fn main() -> opencompany::Result<()> {
    opencompany::run_company(concat!(env!("CARGO_MANIFEST_DIR"), "/agents.toml"))
}
