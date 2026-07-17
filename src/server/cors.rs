//! Cross-origin support for the console, credentials included.
//!
//! Only needed for one shape: a Vite dev server on `:5173` talking to a host on
//! `:8080`. Same-origin deployments — the normal case, including hosted mode —
//! never exercise this. Use the Vite proxy and you will not need it at all.
//!
//! ## Why an allowlist, and not a wildcard
//!
//! The session is a cookie, so the browser only sends it cross-origin when the
//! response carries `Access-Control-Allow-Credentials: true`. The Fetch
//! standard forbids pairing that with `Access-Control-Allow-Origin: *` — a
//! wildcard is rejected outright by the browser. So the origin must be echoed
//! back explicitly, which means we must know which origins are allowed.
//!
//! That is not a formality. Echoing back whatever `Origin` arrives, with
//! credentials on, hands every site on the internet the ability to make
//! authenticated requests as the signed-in user and read the responses. This
//! module therefore echoes an origin **only** when it appears in a
//! deliberately configured list, and is off entirely by default.
//!
//! Hand-rolled rather than pulling `tower-http`: this is a handful of response
//! headers, and the crate has no tower middleware layer to hang one on.

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;

use crate::error::OpenCompanyError;

/// The origins permitted to make credentialed cross-origin requests.
///
/// Empty means CORS is off, which is the default and the right answer for every
/// same-origin deployment.
#[derive(Clone, Debug, Default)]
pub struct CorsConfig {
    /// Exact origins (scheme + host + port), e.g. `http://localhost:5173`.
    pub allowed_origins: Vec<String>,
}

impl CorsConfig {
    /// Reads `OPENCOMPANY_CORS_ORIGINS`: a comma-separated list of exact
    /// origins. Unset or empty disables CORS.
    ///
    /// Rejects `*` explicitly rather than letting it through as a literal
    /// origin that would never match: someone who writes it means "allow
    /// everything", and with credentials that is precisely what must not
    /// happen. Failing tells them; silently never matching would not.
    pub fn from_env() -> Result<Self, OpenCompanyError> {
        let raw = match std::env::var("OPENCOMPANY_CORS_ORIGINS") {
            Ok(raw) if !raw.trim().is_empty() => raw,
            _ => return Ok(Self::default()),
        };
        let mut allowed_origins = Vec::new();
        for origin in raw.split(',').map(str::trim).filter(|o| !o.is_empty()) {
            if origin == "*" {
                return Err(OpenCompanyError::Config(
                    "OPENCOMPANY_CORS_ORIGINS cannot be '*': the session is a cookie, and a \
                     wildcard origin is forbidden with credentials. List exact origins, e.g. \
                     http://localhost:5173"
                        .to_string(),
                ));
            }
            if !origin.starts_with("http://") && !origin.starts_with("https://") {
                return Err(OpenCompanyError::Config(format!(
                    "OPENCOMPANY_CORS_ORIGINS entry {origin:?} is not an origin; it needs a \
                     scheme, e.g. http://localhost:5173"
                )));
            }
            // An origin is scheme+host+port only. A trailing path never matches
            // what a browser sends, so it is a typo worth naming.
            if origin.matches('/').count() > 2 {
                return Err(OpenCompanyError::Config(format!(
                    "OPENCOMPANY_CORS_ORIGINS entry {origin:?} has a path; an origin is just \
                     scheme://host:port"
                )));
            }
            allowed_origins.push(origin.to_string());
        }
        Ok(Self { allowed_origins })
    }

    /// Whether any origin is allowed at all.
    pub fn is_enabled(&self) -> bool {
        !self.allowed_origins.is_empty()
    }

    /// The request's `Origin`, if this config permits it.
    fn permitted<'a>(&self, headers: &'a HeaderMap) -> Option<&'a str> {
        let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == origin)
            .then_some(origin)
    }

    /// The CORS headers to attach to a response, if any.
    ///
    /// Returns nothing for an origin that is not allowed, which the browser
    /// then blocks — the request may still have reached the handler, so this is
    /// not authorization. Authorization is the session; this only decides who
    /// may *read* the answer.
    pub fn headers_for(
        &self,
        request_headers: &HeaderMap,
    ) -> Vec<(header::HeaderName, HeaderValue)> {
        let Some(origin) = self.permitted(request_headers) else {
            return Vec::new();
        };
        let Ok(origin) = HeaderValue::from_str(origin) else {
            return Vec::new();
        };
        vec![
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin),
            (
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            ),
            // The origin is echoed per-request, so caches must key on it or one
            // origin's response could be served to another.
            (header::VARY, HeaderValue::from_static("Origin")),
        ]
    }

    /// The response to a preflight `OPTIONS`, if the origin is allowed.
    pub fn preflight(&self, request_headers: &HeaderMap) -> Option<Response> {
        use axum::response::IntoResponse;

        let mut response = StatusCode::NO_CONTENT.into_response();
        let cors = self.headers_for(request_headers);
        if cors.is_empty() {
            return None;
        }
        let headers = response.headers_mut();
        for (name, value) in cors {
            headers.insert(name, value);
        }
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
        );
        // Echo what was asked for rather than guessing a list: the console sends
        // `content-type`, and this stays correct if that ever changes.
        let requested = request_headers
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("content-type"));
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
        Some(response)
    }
}

/// Whether a method is a CORS preflight.
pub fn is_preflight(method: &Method) -> bool {
    method == Method::OPTIONS
}

#[cfg(test)]
mod test {
    use super::*;

    fn cfg(origins: &[&str]) -> CorsConfig {
        CorsConfig {
            allowed_origins: origins.iter().map(|o| o.to_string()).collect(),
        }
    }

    fn with_origin(origin: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::ORIGIN, origin.parse().unwrap());
        h
    }

    #[test]
    fn disabled_by_default() {
        assert!(!CorsConfig::default().is_enabled());
        assert!(
            CorsConfig::default()
                .headers_for(&with_origin("http://localhost:5173"))
                .is_empty()
        );
    }

    #[test]
    fn an_allowed_origin_is_echoed_with_credentials() {
        let headers =
            cfg(&["http://localhost:5173"]).headers_for(&with_origin("http://localhost:5173"));
        let map: std::collections::HashMap<_, _> = headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.to_str().unwrap()))
            .collect();
        assert_eq!(
            map.get("access-control-allow-origin"),
            Some(&"http://localhost:5173")
        );
        assert_eq!(map.get("access-control-allow-credentials"), Some(&"true"));
        // Or a cache could hand one origin's response to another.
        assert_eq!(map.get("vary"), Some(&"Origin"));
    }

    #[test]
    fn the_origin_is_never_a_wildcard() {
        // A wildcard with credentials is rejected by the browser, so emitting
        // one would silently break the console rather than loosen it — but the
        // deeper point is that we must never echo an origin we were not told
        // about.
        let headers =
            cfg(&["http://localhost:5173"]).headers_for(&with_origin("https://evil.test"));
        assert!(
            headers.is_empty(),
            "an unlisted origin must get no CORS headers at all"
        );
    }

    #[test]
    fn matching_is_exact() {
        let c = cfg(&["http://localhost:5173"]);
        // Substring and suffix tricks are how CORS allowlists usually break.
        for hostile in [
            "http://localhost:5173.evil.test",
            "https://localhost:5173",
            "http://localhost:51730",
            "http://evil.test?http://localhost:5173",
            "null",
        ] {
            assert!(
                c.headers_for(&with_origin(hostile)).is_empty(),
                "{hostile:?} must not match"
            );
        }
    }

    #[test]
    fn no_origin_header_means_no_cors() {
        assert!(
            cfg(&["http://localhost:5173"])
                .headers_for(&HeaderMap::new())
                .is_empty()
        );
    }

    #[test]
    fn preflight_answers_only_for_an_allowed_origin() {
        let c = cfg(&["http://localhost:5173"]);
        assert!(c.preflight(&with_origin("https://evil.test")).is_none());

        let response = c
            .preflight(&with_origin("http://localhost:5173"))
            .expect("an allowed origin gets a preflight response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
        assert!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("PATCH")
        );
    }

    #[test]
    fn preflight_echoes_the_requested_headers() {
        let mut h = with_origin("http://localhost:5173");
        h.insert(
            header::ACCESS_CONTROL_REQUEST_HEADERS,
            "content-type, x-custom".parse().unwrap(),
        );
        let response = cfg(&["http://localhost:5173"]).preflight(&h).unwrap();
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
                .unwrap(),
            "content-type, x-custom"
        );
    }
}
