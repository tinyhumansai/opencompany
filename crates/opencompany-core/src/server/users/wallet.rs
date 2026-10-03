//! Wallet sign-in: proving control of an Ed25519 key instead of a mailbox.
//!
//! The company's [`AuthMode::Wallet`](crate::app::config::AuthMode::Wallet)
//! answer to "who are you". Everything downstream of identification is
//! unchanged — the same `eligibility` → `upsert_from_eligibility` →
//! `mint_session` path a magic link takes, the same session, the same cookie,
//! the same roster. Only the proof differs.
//!
//! ## The flow
//!
//! 1. `POST …/auth/wallet/challenge {address}` mints a 256-bit nonce, stores its
//!    hash against the address, and returns the exact message to sign.
//! 2. The wallet signs those bytes.
//! 3. `POST …/auth/wallet/verify {nonce, signature}` redeems the nonce exactly
//!    once (enforced in the store), checks the signature, and mints a session.
//!
//! ## What the signature covers
//!
//! ```text
//! opencompany-wallet-login-v1\n
//! <company id>\n
//! <base58 address>\n
//! <nonce>\n
//! <issued at, epoch millis>
//! ```
//!
//! Isolated in [`challenge_message`] and versioned by its first line, so the
//! layout can be changed without a signature minted under the old one silently
//! verifying under the new.
//!
//! Every field is bound for a reason, and the reasons are the attacks:
//!
//! - **the domain tag** — a wallet signs for many protocols with one key, and
//!   without it a signature gathered anywhere else could be replayed as a login.
//! - **the company id** — otherwise a signature for company A logs the holder
//!   into company B on a host serving both.
//! - **the address** — binds the proof to the key it claims to come from.
//! - **the nonce** — makes each signature good exactly once.
//! - **the issue time** — makes a captured message legible as stale.
//!
//! ## Verification reads the record, never the request
//!
//! [`verify_challenge`] takes the address and the issue time from the **stored**
//! challenge, not from the request body. The caller supplies only the nonce and
//! the signature. Rebuilding the message from anything the caller sent would let
//! them choose what they are proving, which is the whole game: a caller who can
//! name the address gets to pick which wallet to check against.
//!
//! ## No mailbox anywhere
//!
//! A wallet company sends no login mail, no invite mail, and has no password
//! path: there is no address to send to. The roster still works — an admin
//! invites a base58 address exactly as they would invite an email — but the
//! invitee learns of it out of band. See `docs/spec/runtime/users.md`.

use serde::{Deserialize, Serialize};

use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;
use crate::ports::{
    LoginCodeRecord, LoginIdentity, decode_wallet_address, generate_id, normalize_wallet,
};
use crate::server::users::token::{self, TokenSource};

/// How long a wallet challenge stays redeemable.
///
/// Much shorter than a magic link's fifteen minutes, and the difference is not
/// caution — it is that the two are used differently. A link waits in a mailbox
/// for a human to notice it; a challenge is answered by software the moment it
/// arrives. Anything past a few minutes is a stalled flow, not a slow one.
pub const CHALLENGE_TTL_MILLIS: u64 = 5 * 60 * 1000;

/// How soon a wallet may be issued a *replacement* challenge.
///
/// A wallet address is public once it is on the roster, so without this an
/// anonymous caller could hit `/auth/wallet/challenge` in a loop and
/// invalidate the legitimate owner's pending challenge before they finish
/// signing it — the owner's own signature would then fail as `invalid_login`,
/// indistinguishable from a wrong one. Mirrors the magic-link path's
/// `RESEND_INTERVAL_MILLIS`, shorter because this flow is answered by
/// software in seconds rather than a human noticing a mailbox.
pub const CHALLENGE_RESEND_INTERVAL_MILLIS: u64 = 30 * 1000;

/// The domain-separation tag pinning the signed layout's version.
pub const CHALLENGE_DOMAIN: &str = "opencompany-wallet-login-v1";

/// The prefix a challenge nonce is hashed under before it is stored.
///
/// Challenges share [`LoginCodeStore`](crate::ports::LoginCodeStore) with magic
/// links and device-pairing codes. They are kept apart by hashing under distinct
/// domains rather than by a discriminant field, for the same reason as device
/// pairing: three keyspaces cannot collide, whereas one keyspace plus a check is
/// only as good as every caller remembering the check.
const CHALLENGE_PREFIX: &str = "wallet-challenge:";

/// What the console asks for: a challenge for one wallet address.
#[derive(Debug, Deserialize)]
pub(crate) struct ChallengeRequest {
    /// The base58 Ed25519 public key the caller claims.
    pub(crate) address: String,
}

/// The challenge to sign.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChallengeResult {
    /// The one-time nonce, echoed back on verify so the record can be found.
    pub(crate) nonce: String,
    /// The exact bytes to sign, as UTF-8. The client must sign this verbatim —
    /// it must not rebuild the layout, which would make every future change to
    /// the layout a breaking change for every client.
    pub(crate) message: String,
    /// When the challenge stops being redeemable, epoch millis.
    pub(crate) expires_at_millis: u64,
}

/// The answer to a challenge.
#[derive(Debug, Deserialize)]
pub(crate) struct VerifyRequest {
    /// The nonce from the challenge, which is how the record is found.
    pub(crate) nonce: String,
    /// The base58-encoded Ed25519 signature over the challenge message.
    pub(crate) signature: String,
}

/// Hashes a nonce into its stored form, under this module's domain.
pub(crate) fn challenge_hash(nonce: &str) -> String {
    token::sha256_hex(&format!("{CHALLENGE_PREFIX}{nonce}"))
}

/// Builds the canonical bytes a wallet signature covers. See the module docs for
/// the layout and for why each field is bound.
pub fn challenge_message(
    company: &CompanyId,
    address: &str,
    nonce: &str,
    issued_at_millis: u64,
) -> String {
    format!(
        "{CHALLENGE_DOMAIN}\n{}\n{address}\n{nonce}\n{issued_at_millis}",
        company.as_ref()
    )
}

/// Parses a caller-supplied wallet address into its canonical form.
///
/// Returns `None` for anything that is not a 32-byte base58 key. The caller
/// answers that with the same generic failure as every other login refusal — a
/// malformed address must not be distinguishable from an unknown one, or the
/// route says which addresses this company has heard of.
pub(crate) fn parse_wallet_address(raw: &str) -> Option<String> {
    let address = normalize_wallet(raw);
    decode_wallet_address(&address).ok().map(|_| address)
}

/// Mints a challenge for `address` and persists its hash.
///
/// The record's identity key is the wallet's [`LoginIdentity`] key, so a
/// challenge lives in the same keyspace as the user it will authenticate and
/// [`crate::server::users::routes`] can treat both alike.
pub(crate) async fn issue_challenge(
    runtime: &CompanyRuntime,
    src: &dyn TokenSource,
    address: &str,
    now: u64,
) -> Result<ChallengeResult, OpenCompanyError> {
    let identity = LoginIdentity::Wallet(address.to_string()).key();
    // Throttled the same way `request_code` throttles a resend: checked
    // before touching the store, and answered with a response indistinguishable
    // from a fresh one, so the throttle itself cannot become a membership
    // oracle. Unlike a magic link, there is no out-of-band copy to silently
    // withhold — the caller needs a challenge payload back to sign — so the
    // decoy path already built for an ineligible address serves the identical
    // purpose here: a real nonce over the real layout that never redeems,
    // leaving the pending challenge (and the owner's chance to answer it)
    // untouched.
    if let Some(previous) = runtime
        .login_codes()
        .latest_for_email(runtime.id(), &identity)
        .await?
        && previous.is_redeemable(now)
        && now.saturating_sub(previous.created_at_millis) < CHALLENGE_RESEND_INTERVAL_MILLIS
    {
        return Ok(unpersisted_challenge(runtime.id(), src, address, now));
    }
    let nonce = token::mint_login_code(src);
    let record = LoginCodeRecord {
        id: generate_id(),
        // Only the hash is persisted, exactly as for a magic link. The nonce is
        // not a secret in the same way — it is handed to whoever asked — but a
        // dump of this table must still not let anyone answer a live challenge
        // for a wallet whose owner is midway through signing one.
        code_hash: challenge_hash(&nonce),
        email: identity.clone(),
        created_at_millis: now,
        expires_at_millis: now + CHALLENGE_TTL_MILLIS,
        consumed_at_millis: None,
    };
    // One live challenge per wallet, exactly as the magic link keeps one live
    // code per address: issuing a new one invalidates the last, so a repeated
    // `challenge` request past the throttle window cannot accumulate unbounded
    // rows for a wallet an attacker keeps naming.
    runtime
        .login_codes()
        .delete_for_email(runtime.id(), &identity)
        .await?;
    runtime.login_codes().create(runtime.id(), &record).await?;
    Ok(ChallengeResult {
        message: challenge_message(runtime.id(), address, &nonce, now),
        nonce,
        expires_at_millis: record.expires_at_millis,
    })
}

/// Mints a challenge that is **not** stored, for an address this company will
/// not sign in.
///
/// It is a real nonce over the real layout, indistinguishable from a live
/// challenge, and answering it fails exactly as a wrong signature does — because
/// there is no record to consume. Two things depend on that:
///
/// - the route cannot be used to ask whether a wallet is on the roster; and
/// - an anonymous caller cannot fill the login-code table by naming addresses,
///   which they otherwise could, since a wallet address is public and costs
///   nothing to invent.
pub(crate) fn unpersisted_challenge(
    company: &CompanyId,
    src: &dyn TokenSource,
    address: &str,
    now: u64,
) -> ChallengeResult {
    let nonce = token::mint_login_code(src);
    ChallengeResult {
        message: challenge_message(company, address, &nonce, now),
        nonce,
        expires_at_millis: now + CHALLENGE_TTL_MILLIS,
    }
}

/// Redeems a challenge and checks the signature, returning the proven address.
///
/// `None` for every failure — unknown nonce, expired, already spent, a record
/// that is not a wallet challenge, a signature that does not verify. The caller
/// answers all of them with one `401 invalid_login`, for the same reason the
/// magic-link routes do: any distinction is a membership oracle for the
/// company's wallet roster.
pub(crate) async fn verify_challenge(
    runtime: &CompanyRuntime,
    body: &VerifyRequest,
    now: u64,
) -> Option<String> {
    // Redemption is atomic in the store, so a nonce cannot be spent twice even
    // by two requests racing. This must happen *before* the signature check: a
    // caller who could burn attempts against a live nonce without consuming it
    // would have unlimited tries at forging one signature.
    let record = runtime
        .login_codes()
        .consume(runtime.id(), &challenge_hash(&body.nonce), now)
        .await
        .ok()??;

    // The address comes from the record. A challenge issued for one wallet must
    // never be answerable by another, and taking the address from the request
    // would be exactly that.
    let LoginIdentity::Wallet(address) = LoginIdentity::parse(&record.email) else {
        return None;
    };

    let message = challenge_message(
        runtime.id(),
        &address,
        &body.nonce,
        record.created_at_millis,
    );
    crate::crypto::ed25519::verify_b58(&address, message.as_bytes(), &body.signature).ok()?;
    Some(address)
}

/// The normalized wallet addresses the manifest bootstraps as admins.
///
/// The wallet counterpart of
/// [`manifest_admins`](crate::server::users::routes::manifest_admins), returning
/// [`LoginIdentity`] keys so the caller compares like with like. There is
/// deliberately no environment counterpart to `OPENCOMPANY_ADMIN_EMAIL`: that
/// variable exists because a *platform-provisioned* company's creator is known
/// only to the control plane, and the control plane records an email address,
/// not a wallet.
pub(crate) async fn manifest_wallets(
    runtime: &CompanyRuntime,
) -> Result<Vec<String>, OpenCompanyError> {
    Ok(crate::server::users::routes::load_manifest(runtime)
        .await?
        .map(|m| {
            m.users
                .wallets
                .iter()
                .map(|w| LoginIdentity::Wallet(normalize_wallet(w)).key())
                .collect()
        })
        .unwrap_or_default())
}
