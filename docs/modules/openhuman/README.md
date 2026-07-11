# OpenHuman Module

The openhuman module owns launcher and integration seams for the vendored
OpenHuman checkout at `vendor/openhuman`. It intentionally shells out through
Cargo for now, because OpenHuman is a full application crate and should not be a
heavy default dependency of the OpenCompany library.

Use `opencompany open-human --dry-run` to inspect the generated command before
running the vendored checkout.

## JSON-RPC backing (Phase 3)

Beyond the launcher, this module adapts OpenHuman's JSON-RPC surface to kernel
ports. `rpc.rs` defines the `OpenHumanRpc` transport trait (methods
`openhuman.<namespace>_<function>`, `GET /health`) with an in-memory
`MockOpenHumanRpc` so the providers are testable offline; `http_client.rs`
holds the real `reqwest` client behind the optional `openhuman-rpc` feature.
`tools.rs` (`OpenHumanToolProvider`) filters the RPC tool catalog by the
company's manifest grants and rejects ungranted calls before any side effect;
`channel.rs` (`OpenHumanChannelAdapter`) sends over the channels domain. All of
it degrades to built-in tools and the operator channel with a boot warning when
OpenHuman is unreachable — never a boot failure.
