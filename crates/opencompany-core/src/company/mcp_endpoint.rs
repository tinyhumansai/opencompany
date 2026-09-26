//! The identity an MCP endpoint is compared on, and nothing else.
//!
//! Two surfaces ask "are these two records the same server?": the console's
//! server list, which folds directory installs into the declared rows, and the
//! agent's system prompt, which tells a model which of its two MCP dispatch
//! tools reaches a given server. Both answers have to agree — a server the
//! console shows as one row must not be described to the model as two, and a
//! server the model is told to reach one way must be the row the operator
//! configured. A second copy of this rule is how those two drift apart, so it
//! lives here rather than beside either caller.

/// Normalises an MCP endpoint to the identity two lists are compared on:
/// lowercased scheme and host, default port dropped, query and fragment
/// stripped, trailing slash dropped.
///
/// **The query string must go.** A List A server can carry its credential as a
/// query parameter (the BrowserBase style — see
/// [`AuthMaterial::QueryParam`](crate::company::mcp::AuthMaterial::QueryParam)),
/// so `…/mcp?token=abc` and `…/mcp` are the same server reached two ways. A
/// comparison that kept the query would never match them and the operator would
/// get the duplicate row this whole rule exists to prevent — with two
/// credentials and two health badges disagreeing about one server.
///
/// Returns `None` for a blank endpoint, which is what a stdio install has: no
/// address means nothing to reconcile *on*, not "reconciles with everything".
pub(crate) fn normalize_endpoint(endpoint: &str) -> Option<String> {
    let raw = endpoint.trim();
    if raw.is_empty() {
        return None;
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        // Not a URL we can decompose. Compare it case-insensitively as a whole
        // rather than guessing at a shape — a wrong split would merge two
        // unrelated rows, which is worse than leaving a duplicate.
        None => return Some(raw.to_ascii_lowercase()),
    };
    // `?` and `#` cannot legally appear in an authority, so cutting them off the
    // whole remainder first is safe and handles `https://host?q` too.
    let cut = rest.find(['?', '#']).unwrap_or(rest.len());
    let rest = &rest[..cut];
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let mut authority = authority.to_ascii_lowercase();
    for (default_scheme, port) in [("http", ":80"), ("https", ":443")] {
        if scheme == default_scheme
            && let Some(host) = authority.strip_suffix(port)
        {
            authority = host.to_string();
            break;
        }
    }
    let path = path.trim_end_matches('/');
    if authority.is_empty() && path.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}{path}"))
}
