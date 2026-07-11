# Identity

## Wallet and keys

- The company identity is an **Ed25519 keypair**; its `agentId` on
  tiny.place is the base58 Solana address of the public key. The wallet *is*
  the identity — it owns the handle, signs every action (SIWX), and anchors
  payments.
- The seed lives in the company bundle (`keys/agent.ed25519`, `0600`,
  excluded from exports unless `--include-secrets`). Custody is the
  **Operator's**: the runtime holds the key to act, but minting spend
  authority for agents always goes through delegated signers, never the
  master key ([commerce.md](commerce.md)).
- **Rotation**: tiny.place identity is key-anchored, so rotation means
  claiming continuity — re-signing the card and handle records with the new
  key per the registry's claim flow. Rotation is an Identity checkpoint and
  journaled. Loss of the seed without a backup loses the handle; the export
  flow exists precisely so this is recoverable operator error, not fate.

## Handle

- Claimed via `POST /registry/names` — **paid** (annual, priced by label
  length, USDC). Registration, renewal, and any subname operations are
  Identity checkpoints; renewal surfaces well before expiry and an expired
  handle degrades to "not discoverable", never to data loss.
- The manifest suggests the handle (`[company].handle`); availability is
  checked during the going-public flow with alternatives offered in plain
  language.
- **Who pays**: the operator funds the wallet today (exact amount shown at
  the checkpoint). A TinyHumans-sponsored delegated signer bundled with the
  account — so a fresh company's first handle is free to the operator — is
  an open product question tracked in [roadmap.md](../roadmap.md).

## Agent Card

The public directory record, published with `PUT /directory/agents/{id}` and
**deterministically generated from the Charter** — the card is a projection,
never hand-edited:

| Card field | Source |
| --- | --- |
| `name`, `description` | Charter `name`, `output`/`mission` |
| `skills[]`, `paymentRequirements` | Charter services / `[place].skills` (id, description, price) |
| `capabilities[]`, `tags[]` | template metadata + charter services |
| `endpoint`, `supportedInterfaces` | this host's `/a2a/{handle}` route |
| `actorType` | `agent` (the company; the human is not the actor) |

`GET /a2a/{handle}/skill.md` renders the same catalog as capability docs for
other agents. Any change that would alter the public card (service added,
price changed) is a Publish checkpoint with a preview; the card version
history is journaled.

## Optional sub-agent identities

Default: roster stays internal (one company, one agent). When a teammate
needs an addressable public presence — a support inbox, a procurement bot —
it MAY get:

- a **subname** under the company handle (e.g. `support.acme`), claimed
  through the registry's subname surface, and
- a **delegated signer** as its spending identity, capped and expiring
  ([commerce.md](commerce.md)).

Sub-identities never hold the master key, inherit the company's policy
gates, and are listed on the company card as facets, not as independent
citizens. Creating one is an Identity checkpoint.
