//! Dropping files, folders and links into a company's memory.
//!
//! ```text
//! POST …/memory/ingest        multipart: one or many files (folder drops carry paths)
//! POST …/memory/ingest/links  JSON: URLs to fetch and remember
//! ```
//!
//! ## What a drop becomes
//!
//! Text, chunked, in the [`ContextStore`](crate::ports::ContextStore) the
//! agents recall from — the same store an operator fact is mirrored into, so a
//! dropped document reaches a teammate on its next turn with no further step.
//! The extraction and chunking rules live in [`crate::ingest`]; this module is
//! the transport and the reporting.
//!
//! The original bytes are **not** kept. See [`crate::ingest`] for why that is
//! the design and not an omission — in short, the workspace tree is where
//! files live, and a second copy of every upload there would make this page a
//! silently diverging file manager.
//!
//! ## One answer per file, never one for the batch
//!
//! A folder drop is dozens of files, and some of them are always going to be
//! `.DS_Store`, a PNG, or a scanned PDF with no text layer. Failing the whole
//! request over one of them would make the feature unusable on any real
//! folder, and silently skipping them would leave an operator believing their
//! folder is in memory when a third of it is not. So every file gets a row in
//! the response saying what happened to it, and the request itself succeeds as
//! long as it was well-formed.
//!
//! ## Links are fetched by the host, so the guard is the host's too
//!
//! `POST …/memory/ingest/links` makes this server issue an outbound request to
//! an operator-supplied URL — the shape of a server-side request forgery. The
//! request is therefore restricted to `http`/`https` and refused for any host
//! that resolves to a loopback, link-local, or private address, which is what
//! stops "remember this page" from being a read primitive against the
//! deployment's own network.

use axum::extract::Path;
use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, Multipart};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ingest::{Extracted, MAX_DOCUMENT_BYTES, chunk_document, extract};
use crate::ports::types::ContextChunk;
use crate::server::error::ApiError;
use crate::server::ops::scope::{ScopedCompany, scoped};

/// How much one ingest request may carry.
///
/// A folder drop is many files in one request, so this is a multiple of the
/// per-document cap rather than equal to it. The console still batches — this
/// is the ceiling, not the expected size.
const INGEST_BODY_LIMIT: usize = 8 * MAX_DOCUMENT_BYTES;

/// The largest page body a link ingest will read.
#[cfg(feature = "documents")]
const MAX_LINK_BYTES: usize = 4 * 1024 * 1024;

/// How long the host waits for a link to answer.
#[cfg(feature = "documents")]
const LINK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Builds the ingest route fragment.
pub fn router() -> Router<AppState> {
    scoped("/memory/ingest", post(ingest))
        .layer(DefaultBodyLimit::max(INGEST_BODY_LIMIT))
        .merge(scoped("/memory/ingest/links", post(ingest_links)))
        .merge(scoped("/memory/document/{source}", delete(forget_document)))
}

/// What happened to one dropped file or link.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct IngestedItem {
    /// The name the operator will recognise — a file name, a relative path
    /// inside a dropped folder, or a URL.
    source: String,
    /// `stored`, `empty`, `unsupported`, or `failed`.
    status: &'static str,
    /// How many memory chunks it became.
    chunks: usize,
    /// Why it is not `stored`, when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl IngestedItem {
    fn failed(source: String, detail: String) -> Self {
        Self {
            source,
            status: "failed",
            chunks: 0,
            detail: Some(detail),
        }
    }
}

/// The whole batch's outcome.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct IngestedDto {
    /// One row per file or link, in the order they were sent.
    items: Vec<IngestedItem>,
    /// Chunks written across the batch — what the Brain counter will move by.
    chunks: usize,
    /// How many sources actually landed in memory.
    stored: usize,
}

impl IngestedDto {
    fn of(items: Vec<IngestedItem>) -> Self {
        Self {
            chunks: items.iter().map(|i| i.chunks).sum(),
            stored: items.iter().filter(|i| i.status == "stored").count(),
            items,
        }
    }
}

/// Links to fetch and remember.
///
/// Deserialized even in a build without the feature — the refusing handler
/// still takes the body, so a console gets the honest "this build cannot" and
/// not a deserialization error about a shape it sent correctly.
#[derive(Debug, Deserialize)]
struct LinksRequest {
    #[cfg_attr(
        not(feature = "documents"),
        allow(dead_code, reason = "the refusing handler reads no field")
    )]
    urls: Vec<String>,
}

/// Extracts one source's bytes and writes its chunks, returning its row.
///
/// Per-source rather than per-batch failure handling: see the module doc.
async fn store_source(
    company: &ScopedCompany,
    source: String,
    declared: Option<&str>,
    bytes: &[u8],
) -> IngestedItem {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return IngestedItem::failed(
            source,
            format!(
                "larger than the {} MiB this page reads from one document",
                MAX_DOCUMENT_BYTES / (1024 * 1024)
            ),
        );
    }
    let text = match extract(&source, declared, bytes) {
        Extracted::Text(text) => text,
        Extracted::Empty => {
            return IngestedItem {
                source,
                status: "empty",
                chunks: 0,
                detail: Some(
                    "no text in it — a scanned document needs OCR before memory can hold it"
                        .to_string(),
                ),
            };
        }
        Extracted::Unsupported(reason) => {
            return IngestedItem {
                source,
                status: "unsupported",
                chunks: 0,
                detail: Some(reason),
            };
        }
    };

    let chunks = chunk_document(&source, &text);
    let mut written = 0;
    for chunk in chunks {
        let put = company
            .runtime
            .context
            .put(
                company.id(),
                ContextChunk {
                    label: chunk.label,
                    body: chunk.body,
                },
            )
            .await;
        match put {
            Ok(_) => written += 1,
            // Reported as a partial store rather than a silent success: some
            // of the document is in memory and the operator has to know which
            // state they are in before deciding to re-drop it.
            Err(error) => {
                tracing::warn!(company = %company.id(), %source, %error, "memory ingest chunk failed");
                return IngestedItem {
                    source,
                    status: "failed",
                    chunks: written,
                    detail: Some(format!(
                        "stored {written} chunks, then memory refused the next one: {error}"
                    )),
                };
            }
        }
    }
    IngestedItem {
        source,
        status: "stored",
        chunks: written,
        detail: None,
    }
}

/// The document whose chunks are to be forgotten, by its label slug.
#[derive(Debug, Deserialize)]
struct DocumentPath {
    /// The `document/{slug}` label's slug half, as
    /// [`label_for`](crate::ingest::label_for) derived it.
    source: String,
}

/// `DELETE …/memory/document/{source}` — forget one dropped document.
///
/// The counterpart the drop zone needs to be usable: dropping the wrong folder
/// is a mistake an operator makes once, and without this the only remedy is a
/// company's whole memory. Scoped to `document/` labels alone — nothing here
/// can reach an agent's own memory or a task outcome, which are records of
/// what happened rather than material an operator supplied.
async fn forget_document(
    company: ScopedCompany,
    Path(DocumentPath { source }): Path<DocumentPath>,
) -> Result<Json<ForgottenDto>, ApiError> {
    let prefix = format!("{}/{source}/", crate::ingest::DOCUMENT_LABEL_PREFIX);
    let context = company.runtime.context.as_ref();
    let all = context.list(company.id(), "").await?;
    let mut forgotten = 0;
    for meta in all.iter().filter(|m| m.label.starts_with(&prefix)) {
        // Chunks are content-addressed, so one address can carry several
        // labels: two identical paragraphs in one folder drop, or a document
        // re-dropped under a new name. The delete is label-scoped (#1300):
        // exactly this document's claim goes, any other label keeps the body,
        // and the body is reaped with its last claim atomically inside the
        // port — the same shape the fact mirror's reap uses, minus the old
        // snapshot guard that skipped shared chunks entirely (which left a
        // deleted document's claims listed forever).
        if context
            .delete_label(company.id(), &meta.addr, &meta.label)
            .await?
        {
            forgotten += 1;
        }
    }
    if forgotten == 0 {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "no document memory under `{source}`"
        ))));
    }
    Ok(Json(ForgottenDto { forgotten }))
}

/// How much of a document was forgotten.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ForgottenDto {
    /// Chunks removed.
    forgotten: usize,
}

/// Classifies a multipart failure the way the workspace upload does: a body
/// that overran the limit is a 413, anything else a malformed request.
fn multipart_error(error: MultipartError, context: &str) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError(OpenCompanyError::InvalidRequest(format!(
            "this drop is larger than the {} MiB one request may carry, so it was cut off before \
             anything could be read. Nothing was stored — drop it in smaller batches.",
            INGEST_BODY_LIMIT / (1024 * 1024)
        )));
    }
    ApiError(OpenCompanyError::InvalidRequest(format!(
        "{context}: {error}"
    )))
}

/// `POST …/memory/ingest` — multipart file drop.
///
/// A folder drop sends each file with its **relative path** as the part's
/// filename (`Contracts/2026/acme.pdf`). That path is kept as the source name
/// rather than reduced to a basename, because it is what the operator sees in
/// their own file manager and the only thing distinguishing the four
/// `README.md`s a repository drop contains.
async fn ingest(
    company: ScopedCompany,
    mut multipart: Multipart,
) -> Result<Json<IngestedDto>, ApiError> {
    let mut items = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| multipart_error(e, "malformed multipart drop"))?
    {
        if field.name() != Some("file") {
            // Ignored rather than refused: a browser's `FormData` carries
            // fields this route has no use for, and failing the drop over one
            // would be a puzzle to debug from the console side.
            continue;
        }
        let name = field
            .file_name()
            .map(str::to_string)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "untitled".to_string());
        let declared = field.content_type().map(str::to_string);
        // The read is the one place a body-limit overrun surfaces, and it
        // aborts the whole request: a truncated part is indistinguishable from
        // a malformed one, so there is nothing honest to report per file.
        let bytes = field
            .bytes()
            .await
            .map_err(|e| multipart_error(e, "unreadable file part"))?;
        items.push(store_source(&company, name, declared.as_deref(), &bytes).await);
    }

    if items.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "the drop carried no files".to_string(),
        )));
    }
    Ok(Json(IngestedDto::of(items)))
}

/// `POST …/memory/ingest/links`.
#[cfg(feature = "documents")]
async fn ingest_links(
    company: ScopedCompany,
    Json(request): Json<LinksRequest>,
) -> Result<Json<IngestedDto>, ApiError> {
    if request.urls.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "no links were sent".to_string(),
        )));
    }
    let client = reqwest::Client::builder()
        .timeout(LINK_TIMEOUT)
        // No redirect chasing: a permitted URL that redirects to `localhost`
        // would walk straight past the guard below, and following it is not
        // worth re-implementing the check per hop.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ApiError(OpenCompanyError::Config(format!("no HTTP client: {e}"))))?;

    let mut items = Vec::new();
    for url in request.urls {
        let url = url.trim().to_string();
        if let Err(refusal) = guard_link(&url) {
            items.push(IngestedItem::failed(url, refusal));
            continue;
        }
        items.push(fetch_link(&company, &client, url).await);
    }
    Ok(Json(IngestedDto::of(items)))
}

/// Fetches one link and stores what it said.
#[cfg(feature = "documents")]
async fn fetch_link(
    company: &ScopedCompany,
    client: &reqwest::Client,
    url: String,
) -> IngestedItem {
    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(error) => return IngestedItem::failed(url, format!("could not be fetched: {error}")),
    };
    if !response.status().is_success() {
        return IngestedItem::failed(url, format!("answered {}", response.status()));
    }
    let declared = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_string());
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => return IngestedItem::failed(url, format!("could not be read: {error}")),
    };
    if bytes.len() > MAX_LINK_BYTES {
        return IngestedItem::failed(
            url,
            format!(
                "the page is larger than the {} MiB this reads from one link",
                MAX_LINK_BYTES / (1024 * 1024)
            ),
        );
    }
    // A URL has no extension to dispatch on, so the declared content type
    // decides — with `text/html` the overwhelming case, and the extractor
    // handling `application/pdf` and friends from the same signal.
    let name = match declared.as_deref() {
        Some("text/html") | Some("application/xhtml+xml") | None => format!("{url}#html"),
        _ => url.clone(),
    };
    let mut item = store_source(company, name, declared.as_deref(), &bytes).await;
    // Report the URL the operator typed, not the extension-bearing name the
    // dispatch needed.
    item.source = url;
    item
}

/// Refuses a URL this host must not fetch on an operator's behalf.
///
/// Scheme and host only: DNS is resolved by the client at request time and
/// re-resolving here to check the address would be a different lookup than the
/// one that happens (a TOCTOU no host-level check closes). What this stops is
/// the direct form — `http://localhost:8080/admin`, `http://169.254.169.254/`
/// — which is the shape of every accidental and most deliberate attempts.
#[cfg(feature = "documents")]
fn guard_link(url: &str) -> Result<(), String> {
    let parsed = url
        .parse::<axum::http::Uri>()
        .map_err(|_| "not a URL".to_string())?;
    match parsed.scheme_str() {
        Some("http") | Some("https") => {}
        _ => return Err("only http:// and https:// links can be fetched".to_string()),
    }
    let host = parsed
        .host()
        .ok_or_else(|| "no host in the URL".to_string())?;
    let lowered = host.to_ascii_lowercase();
    if lowered == "localhost" || lowered.ends_with(".localhost") || lowered.ends_with(".internal") {
        return Err("that host is internal to this deployment".to_string());
    }
    if let Ok(address) = lowered.parse::<std::net::IpAddr>() {
        let private = match address {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unspecified() || v6.segments()[0] & 0xfe00 == 0xfc00
            }
        };
        if private {
            return Err("that address is inside this deployment's own network".to_string());
        }
    }
    Ok(())
}

/// Without the feature there is no HTTP client to fetch with, so the route
/// exists and refuses rather than 404ing — a missing route reads to the
/// console as an older host, which is a different thing to tell the operator.
#[cfg(not(feature = "documents"))]
async fn ingest_links(
    _company: ScopedCompany,
    Json(_request): Json<LinksRequest>,
) -> Result<Json<IngestedDto>, ApiError> {
    Err(ApiError(OpenCompanyError::InvalidRequest(
        "remembering a link needs the `documents` feature, which this build was compiled without"
            .to_string(),
    )))
}

#[cfg(test)]
mod test;
