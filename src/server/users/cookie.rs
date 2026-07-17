//! Session cookie naming, parsing, and rendering.
//!
//! Hand-rolled rather than pulled from a crate, for the same reason
//! [`bearer`](crate::server::platform_auth) is: it is a small parse of a header
//! we control both ends of. The value alphabet is base64url by construction
//! (see [`token`](super::token)), so percent-encoding, quoted values, and
//! `Expires` date parsing — the genuinely hard parts of RFC 6265 — are all dead
//! code here. Adding `axum-extra`'s cookie feature would pull a dependency;
//! `tower-cookies` would introduce the first tower middleware layer in a
//! codebase that has none.
//!
//! ## Why the cookie is named per company
//!
//! In hosted mode one container serves one company, so a fixed name would do.
//! But in local development one process serves many companies from one origin,
//! and a fixed name would mean logging into company B silently destroys your
//! session for company A — same origin, same cookie name, last write wins.
//! Naming the cookie `oc_session_<company>` keeps them independent.
//!
//! It also gives the GraphQL handler a way to find the company: the resolver's
//! company argument lives in the request *body*, which is unavailable when
//! extractors run, but the cookie name carries it.
//!
//! ## Why the name is validated
//!
//! [`CompanyId::new`](crate::ports::types::CompanyId) performs no validation —
//! any string is a company id. A company whose id contained `;` or `=` could
//! otherwise inject attributes into the `Set-Cookie` header we render
//! (`oc_session_evil; Path=/; HttpOnly=...`). [`session_cookie_name`] returns
//! `None` for such an id rather than emitting a forgeable header.

use std::collections::HashMap;

use axum::http::HeaderMap;
use axum::http::header::COOKIE;

use crate::ports::types::CompanyId;

/// The prefix every session cookie name carries.
const SESSION_COOKIE_PREFIX: &str = "oc_session_";

/// The session cookie name for `company`, or `None` when the id cannot safely
/// name a cookie.
///
/// Restricted to `[A-Za-z0-9_-]`: a superset-safe subset of RFC 6265's token
/// characters, and enough for every id the runtime mints
/// (`{millis:012x}-{counter:012x}`) or a manifest slug produces. A company that
/// cannot name a cookie cannot mint a session; the login route refuses rather
/// than rendering a header an attacker chose.
pub fn session_cookie_name(company: &CompanyId) -> Option<String> {
    let id = company.as_ref();
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(format!("{SESSION_COOKIE_PREFIX}{id}"))
}

/// The company id embedded in a session cookie name, if it is one.
pub fn company_from_cookie_name(name: &str) -> Option<&str> {
    name.strip_prefix(SESSION_COOKIE_PREFIX)
        .filter(|id| !id.is_empty())
}

/// Parses a `Cookie` request header into name → value pairs.
///
/// Values are taken verbatim: we only ever set base64url values, so there is
/// nothing to decode. Browsers never send cookie *attributes*, so there are
/// none to skip. A later duplicate of a name wins, matching the fact that a
/// browser sends the most specific cookie last.
pub fn parse_cookies(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    // A client may legitimately send more than one Cookie header.
    for header in headers.get_all(COOKIE) {
        let Ok(raw) = header.to_str() else {
            continue;
        };
        for pair in raw.split(';') {
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            // `split_once` keeps any '=' inside the value, which base64url
            // padding would produce if we ever stopped stripping it.
            out.insert(name.to_string(), value.trim().to_string());
        }
    }
    out
}

/// Reads one cookie by name.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    parse_cookies(headers).remove(name)
}

/// Renders a `Set-Cookie` value for a freshly minted session.
///
/// - `HttpOnly`: JavaScript must not be able to read a session token, so an XSS
///   cannot exfiltrate it.
/// - `SameSite=Lax`: `Strict` would drop the cookie on the *first* landing from
///   the magic link, which is a cross-site top-level navigation out of a mail
///   client. `Lax` allows exactly that (top-level GET) while still withholding
///   the cookie from cross-site POSTs — and since every state-changing route
///   here is a POST, that is also the CSRF defense.
/// - `Secure` unless `insecure`, which is set only for plain-http loopback dev.
pub fn set_cookie(name: &str, value: &str, max_age_secs: u64, insecure: bool) -> String {
    let mut out = format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}");
    if !insecure {
        out.push_str("; Secure");
    }
    out
}

/// Renders the `Set-Cookie` value that deletes a session cookie.
///
/// Attributes must match the ones it was set with, or the browser treats it as
/// a different cookie and keeps the original.
pub fn clear_cookie(name: &str, insecure: bool) -> String {
    let mut out = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if !insecure {
        out.push_str("; Secure");
    }
    out
}

#[cfg(test)]
mod test {
    use super::*;

    fn headers(pairs: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for p in pairs {
            h.append(COOKIE, p.parse().unwrap());
        }
        h
    }

    #[test]
    fn cookie_name_carries_the_company() {
        let name = session_cookie_name(&CompanyId::new("acme")).unwrap();
        assert_eq!(name, "oc_session_acme");
        assert_eq!(company_from_cookie_name(&name), Some("acme"));
        assert_eq!(company_from_cookie_name("unrelated"), None);
        assert_eq!(company_from_cookie_name("oc_session_"), None);
    }

    #[test]
    fn minted_company_ids_can_name_a_cookie() {
        // The runtime's own id shape must never be rejected.
        let id = CompanyId::new(crate::ports::generate_id());
        assert!(session_cookie_name(&id).is_some(), "{id:?} was rejected");
    }

    #[test]
    fn a_company_id_that_could_forge_a_header_gets_no_cookie() {
        // CompanyId::new validates nothing, so this is reachable. Emitting
        // `oc_session_evil; Path=/; HttpOnly=x=y` would let the id choose the
        // cookie's attributes.
        for hostile in [
            "evil;Path=/",
            "evil=x",
            "evil name",
            "evil\nSet-Cookie: a=b",
            "",
            "evil;Secure",
        ] {
            assert!(
                session_cookie_name(&CompanyId::new(hostile)).is_none(),
                "{hostile:?} must not be able to name a cookie"
            );
        }
    }

    #[test]
    fn parses_multiple_cookies_from_one_header() {
        let h = headers(&["a=1; oc_session_acme=tok; b=2"]);
        assert_eq!(cookie(&h, "oc_session_acme").as_deref(), Some("tok"));
        assert_eq!(cookie(&h, "a").as_deref(), Some("1"));
        assert_eq!(cookie(&h, "missing"), None);
    }

    #[test]
    fn parses_across_separate_cookie_headers() {
        // A client may split cookies across headers; missing one would drop a
        // session that was actually presented.
        let h = headers(&["a=1", "oc_session_acme=tok"]);
        assert_eq!(cookie(&h, "oc_session_acme").as_deref(), Some("tok"));
    }

    #[test]
    fn tolerates_whitespace_and_odd_pairs() {
        let h = headers(&["  a = 1 ;;  oc_session_acme =  tok  ; junk ; =novalue"]);
        assert_eq!(cookie(&h, "oc_session_acme").as_deref(), Some("tok"));
        assert_eq!(cookie(&h, "a").as_deref(), Some("1"));
        // A pair with no '=' and one with an empty name are skipped, not fatal.
        assert_eq!(cookie(&h, "junk"), None);
    }

    #[test]
    fn keeps_equals_signs_inside_a_value() {
        let h = headers(&["t=aa==bb"]);
        assert_eq!(cookie(&h, "t").as_deref(), Some("aa==bb"));
    }

    #[test]
    fn later_duplicate_wins() {
        let h = headers(&["t=first; t=second"]);
        assert_eq!(cookie(&h, "t").as_deref(), Some("second"));
    }

    #[test]
    fn no_cookie_header_is_not_an_error() {
        assert!(parse_cookies(&HeaderMap::new()).is_empty());
        assert_eq!(cookie(&HeaderMap::new(), "t"), None);
    }

    #[test]
    fn set_cookie_carries_the_defensive_attributes() {
        let rendered = set_cookie("oc_session_acme", "tok", 3600, false);
        assert!(rendered.starts_with("oc_session_acme=tok;"));
        assert!(rendered.contains("HttpOnly"), "{rendered}");
        assert!(rendered.contains("SameSite=Lax"), "{rendered}");
        assert!(rendered.contains("Path=/"), "{rendered}");
        assert!(rendered.contains("Max-Age=3600"), "{rendered}");
        assert!(rendered.contains("Secure"), "{rendered}");
    }

    #[test]
    fn secure_is_dropped_only_for_insecure_dev() {
        let rendered = set_cookie("t", "v", 60, true);
        assert!(
            !rendered.contains("Secure"),
            "http loopback dev cannot set Secure: {rendered}"
        );
        // Everything else still applies.
        assert!(rendered.contains("HttpOnly"));
    }

    #[test]
    fn clear_cookie_expires_immediately_and_matches_set_attributes() {
        let rendered = clear_cookie("oc_session_acme", false);
        assert!(rendered.contains("Max-Age=0"), "{rendered}");
        // A browser only replaces a cookie when name/path match.
        assert!(rendered.contains("Path=/"), "{rendered}");
        assert!(rendered.contains("HttpOnly"), "{rendered}");
        assert!(rendered.contains("Secure"), "{rendered}");
    }

    #[test]
    fn a_rendered_cookie_parses_back() {
        let token = super::super::token::mint_session_token(&super::super::token::OsTokens);
        let rendered = set_cookie("oc_session_acme", &token, 60, false);
        // Simulate the browser echoing just the name=value pair back.
        let pair = rendered.split(';').next().unwrap();
        let h = headers(&[pair]);
        assert_eq!(
            cookie(&h, "oc_session_acme").as_deref(),
            Some(token.as_str())
        );
    }
}
