# crypto

Offline cryptographic primitives shared across the crate, so every caller that
checks a signature checks it the same way.

| File | What it is |
|---|---|
| `mod.rs` | Module root. |
| `ed25519.rs` | `verify_b58`: Ed25519 verification over base58 keys and signatures, used by wallet sign-in (`server/users/wallet.rs`) and the runner handshake (`runner/attest.rs`). Also the test-only `LocalSigner` that produces signatures for those tests. |
| `ed25519_tests.rs` | Sibling tests: round trip, wrong key, tampered message, malformed input. |

Nothing here generates or stores key material: the shipped binary only
verifies. The `ed25519-dalek` and `bs58` dependencies this uses are
unconditional because wallet sign-in is configuration, not a build option.
