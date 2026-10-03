//! Secret redaction for everything an agent's turn writes into memory.
//!
//! The memory loop (`memory_loop::outcome_chunk`) stores the operator's message
//! and the agent's answer verbatim, so a one-time-secret URL or a bearer token
//! pasted into chat would otherwise be recalled into every later turn. This is
//! the single scrubbing point for that write path.
//!
//! Host-side for now: TinyMemory's `safety` scrubber covers long bearer tokens
//! and PII but not `/secret/<key>` one-time URLs or short bearer values, and
//! its policy is not extensible. Moving these two rules upstream is tracked in
//! the core-pushdown plan; until then this stays the stricter of the two.

/// The one-time-secret URL path. Exact match: those URLs are lowercase.
const SECRET_URL_MARKER: &str = "/secret/";
/// The HTTP `Authorization` scheme, matched ASCII-case-insensitively (RFC
/// 9110's auth-scheme ABNF is case-insensitive, so `bearer sk-...` and
/// `BEARER sk-...` are credentials too).
const BEARER_MARKER: &str = "bearer ";

/// Byte offset of the next [`BEARER_MARKER`] occurrence, case-insensitively.
/// The marker is pure ASCII, so a byte scan is safe and allocation-free.
fn find_bearer_marker(text: &str) -> Option<usize> {
    text.as_bytes()
        .windows(BEARER_MARKER.len())
        .position(|w| w.eq_ignore_ascii_case(BEARER_MARKER.as_bytes()))
}

/// The earliest marker in `rest`, as `(byte offset, marker)`. The marker
/// string is only used for its byte length; the output always carries the
/// caller's original casing.
fn next_marker(rest: &str) -> Option<(usize, &'static str)> {
    rest.find(SECRET_URL_MARKER)
        .map(|p| (p, SECRET_URL_MARKER))
        .into_iter()
        .chain(find_bearer_marker(rest).map(|p| (p, BEARER_MARKER)))
        .min_by_key(|(p, _)| *p)
}

/// Redacts secrets from text on its way into memory.
///
/// Measured in a live deployment: chat messages of the form "here is the link
/// with the password" carried a full one-time-secret URL
/// (`.../secret/<key>`), and because every operator message is remembered
/// verbatim, `recall` could pull the still-unopened secret back into a later
/// turn's context — leaving it readable in the store.
///
/// Deliberately *replaces* rather than *rejects*: memory should still record
/// that a link was shared, since that is the context an agent needs to
/// understand what happened. Only the secret value itself is removed.
///
/// openhuman's [`redact_text`](oh::agent::experience::redact_text) does the same
/// for its own experience records; this is the opencompany-side of that rule.
/// `pub(crate)` because the outcome-chunk store path
/// ([`memory_loop::outcome_chunk`](crate::harness::built_in::memory_loop::outcome_chunk))
/// writes operator text directly to the [`ContextStore`], bypassing
/// [`Memory::store`], and must apply the same redaction.
pub(crate) fn redact_secrets(text: &str) -> std::borrow::Cow<'_, str> {
    // Two unambiguous shapes. A generic "anything that looks like a token"
    // regex would mangle ordinary prose.
    if !text.contains(SECRET_URL_MARKER) && find_bearer_marker(text).is_none() {
        return std::borrow::Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((pos, marker)) = next_marker(rest) {
        out.push_str(&rest[..pos + marker.len()]);
        // The HTTP scheme marker is matched case-insensitively, but its casing
        // changes how aggressive the redaction may be: the capital "Bearer "
        // form is the unambiguous auth-header spelling, so a plain digit-free
        // word after it like `secret` is still a credential; the lower-case
        // form is also ordinary English prose ("ring bearer", "standard bearer
        // candidate"), so a plain word there is only redacted once it is long
        // enough that prose is implausible.
        let aggressive = marker == SECRET_URL_MARKER || rest.as_bytes()[pos].is_ascii_uppercase();
        let tail = &rest[pos + marker.len()..];
        // The MCP config trims the value after the marker, so extra whitespace
        // (`Bearer   sk-...`) is legal. Keep it in the output but skip it here —
        // the value scan would otherwise stop at the first space and store the
        // credential verbatim.
        let value_start = tail.len() - tail.trim_start().len();
        out.push_str(&tail[..value_start]);
        let mut value = &tail[value_start..];
        // Formatted chat can wrap a credential in Markdown backticks or quotes
        // (`Bearer `sk-...``); skip one leading wrapper so the scan reaches the
        // credential instead of stopping at length zero. The wrapper is kept in
        // the output, and its closing mate stays in `rest`.
        let wrapper_len = usize::from(matches!(
            value.as_bytes().first(),
            Some(b'`' | b'\'' | b'"')
        ));
        out.push_str(&value[..wrapper_len]);
        value = &value[wrapper_len..];
        // The value runs up to the first character that cannot be part of a token.
        let end = value
            .find(|c: char| !is_token_char(c))
            .unwrap_or(value.len());
        // Long values are always a secret. A short value is still redacted when
        // it is token-shaped — it contains a digit, which prose after a marker
        // does not — so a valid short bearer credential like `s3cret` (the MCP
        // config accepts any non-empty bearer value) does not slip through. A
        // digit-free short value is redacted once it reaches six characters
        // when the marker is the unambiguous capital form, or when the value
        // is not a plain word (`sk-longsecret`, a JWT): "or" and "token" stay
        // shorter, while a genuine if weak digit-free credential like `secret`
        // does not. A plain word after a lower-case "bearer " is ordinary
        // prose too, so it is only redacted past twelve characters.
        let has_digit = value[..end].chars().any(|c| c.is_ascii_digit());
        let plain_word = !value[..end].chars().any(|c| !c.is_ascii_alphanumeric());
        let threshold = if has_digit {
            4
        } else if aggressive || !plain_word {
            6
        } else {
            12
        };
        let mut consumed = end;
        if end >= threshold {
            out.push_str("[REDACTED]");
            // The MCP config keeps the whole trimmed remainder as the bearer
            // value, so a credential can be several space-separated fragments
            // (`Bearer firstpart secondpart`). Once the first fragment looked
            // credential-like, keep redacting the run — but only fragments
            // that themselves clear a floor, so trailing prose ("please", "the
            // token was rotated") survives. The digit-free floor is higher for
            // continuation so a short prose word cannot enter the run. Only a
            // space stop triggers this: a wrapper or punctuation stop means the
            // credential was a single token.
            if value.as_bytes().get(end) == Some(&b' ') {
                let mut cursor = end;
                while let Some(rel) = value[cursor..].find(' ') {
                    // Probe the fragment after this run of spaces without
                    // consuming the space yet: if it clears the floor it is
                    // part of the credential run, otherwise it is prose and
                    // the space must survive into `rest`.
                    let token_start = cursor + rel + 1;
                    let frag_end = value[token_start..]
                        .find(|c: char| !is_token_char(c))
                        .unwrap_or(value.len() - token_start);
                    if frag_end == 0 {
                        cursor = token_start; // repeated spaces; keep probing
                        continue;
                    }
                    let frag = &value[token_start..token_start + frag_end];
                    let floor = if frag.chars().any(|c| c.is_ascii_digit()) {
                        4
                    } else {
                        8
                    };
                    if frag.chars().count() >= floor {
                        out.push(' ');
                        out.push_str("[REDACTED]");
                        cursor = token_start + frag_end;
                    } else {
                        break; // cursor stays at the space before the prose
                    }
                }
                consumed = cursor;
            }
        } else {
            // Too short to be a secret: leave it, or this function would mangle
            // ordinary text like "Bearer or not".
            out.push_str(&value[..end]);
        }
        rest = &tail[consumed + value_start + wrapper_len..];
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// A character that can appear inside a credential value. Beyond base64url's
/// `-` and `_`, a JWT joins its `header.payload.signature` segments with `.`,
/// and opaque keys use the base64 punctuation `+`, `/`, `~`, `=`. Stopping at
/// any of these would leak the un-redacted remainder of the credential into
/// memory, so the value runs until a character that cannot be part of a token.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '~' | '=')
}

#[cfg(test)]
#[path = "redact_tests.rs"]
mod tests;
