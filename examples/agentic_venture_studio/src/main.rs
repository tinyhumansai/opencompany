//! Example harness entrypoint.
//!
//! Boots the company roster declared in `agents.toml` on top of the shared
//! `opencompany` host crate. See `README.md` for what this harness does and
//! `agents.toml` for the full agent roster.

/// The agent roster manifest for this harness, embedded at build time.
const MANIFEST: &str = include_str!("../agents.toml");

fn main() {
    println!(
        "OpenCompany v{} — launching the `{}` harness",
        opencompany::VERSION,
        env!("CARGO_PKG_NAME"),
    );
    println!("\n--- agents.toml ---\n{MANIFEST}");
}
