//! `POST …/skills/upload` — store skills an operator dropped on the console.
//!
//! Admin-only, like every other skill write: an uploaded document joins every
//! agent's effective prompt company-wide.
//!
//! ## One outcome per file, not one per request
//!
//! An operator drops several files at once and one of them is malformed. A
//! whole-request refusal would throw away the good ones and give no clue which
//! file was at fault, so each file gets its own row — stored, or refused with
//! the reason. The request itself only fails for something true of all of it:
//! a body over the limit, or no file parts at all.
//!
//! ## Nothing is persisted before both gates run
//!
//! [`read_upload`](crate::company::skill_upload::read_upload) decides what the
//! file is, the write plane's own size ceiling bounds the assembled document,
//! and [`vet_skill`](super::vet_skill) runs the shared validator and the
//! content scan. Only then does the delta reach the
//! [`SkillStateStore`](crate::ports::SkillStateStore). A `block` verdict
//! returns the report and writes nothing, and the same per-request `force`
//! override the install path carries applies here — there is deliberately no
//! setting that silences a class of finding for a whole host.

use axum::extract::{DefaultBodyLimit, Multipart, multipart::MultipartError};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

use crate::AppState;
use crate::company::skill_upload::read_upload;
use crate::error::OpenCompanyError;
use crate::ports::skills_state::{SkillSource, SkillState};
use crate::server::error::ApiError;
use crate::server::ops::{AdminScopedCompany, scoped};

use super::{InstalledSkill, ScanSummary, check_skill_doc_size, vet_skill, write_lock};

/// How much one upload request may carry.
///
/// A drop is several files at once, so this is a multiple of what a single
/// archive may expand to rather than equal to it. It is the ceiling, not the
/// expected size: a hand-authored skill is a few kilobytes.
const MAX_UPLOAD_BODY: usize = 8 * 1024 * 1024;

/// How many files one upload request may carry.
///
/// The dialog uploads a selection, not a directory tree. A ceiling here is what
/// keeps the per-file loop — which validates and scans each document — bounded
/// by something other than the body limit divided by the smallest valid file.
const MAX_UPLOAD_FILES: usize = 16;

/// Builds the upload route fragment.
pub(super) fn router() -> Router<AppState> {
    scoped("/skills/upload", post(upload)).layer(DefaultBodyLimit::max(MAX_UPLOAD_BODY))
}

/// What happened to one uploaded file.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadRow {
    /// The file name as the operator's own file manager shows it, so a row can
    /// be matched to what they dropped.
    file: String,
    /// Whether this file was stored.
    ok: bool,
    /// The stored skill, with the report of the write that stored it.
    #[serde(skip_serializing_if = "Option::is_none")]
    skill: Option<InstalledSkill>,
    /// Why this file was not stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Whether that refusal was a blocking scan verdict — the one refusal
    /// resending with `force` overrides. Stated as a field so the console can
    /// offer that without matching on the wording of `error`.
    scan_blocked: bool,
}

impl UploadRow {
    fn stored(file: String, skill: InstalledSkill) -> Self {
        Self {
            file,
            ok: true,
            skill: Some(skill),
            error: None,
            scan_blocked: false,
        }
    }

    fn refused(file: String, refusal: Refusal) -> Self {
        Self {
            file,
            ok: false,
            skill: None,
            error: Some(refusal.message),
            scan_blocked: refusal.scan_blocked,
        }
    }
}

/// The answer: one row per file, in the order they were sent.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadedDto {
    results: Vec<UploadRow>,
}

/// One file as it came off the wire.
struct Part {
    filename: String,
    bytes: Vec<u8>,
}

/// `POST …/skills/upload` — multipart skill drop.
///
/// Parts named `file` are the skills; a `force` part carrying `true` overrides
/// a blocking scan verdict for this request, matching the install path's flag.
/// Every part is read before any is stored, because `force` may arrive after
/// the files it applies to and a stream position must not decide whether an
/// override was honoured.
async fn upload(
    company: AdminScopedCompany,
    mut multipart: Multipart,
) -> Result<Json<UploadedDto>, ApiError> {
    let mut parts: Vec<Part> = Vec::new();
    let mut force = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| multipart_error(error, "malformed skill upload"))?
    {
        match field.name() {
            Some("force") => {
                let value = field
                    .text()
                    .await
                    .map_err(|error| multipart_error(error, "unreadable `force` field"))?;
                force = value.trim().eq_ignore_ascii_case("true");
            }
            Some("file") => {
                let filename = field
                    .file_name()
                    .map(str::to_string)
                    .unwrap_or_else(|| "(unnamed)".to_string());
                if parts.len() >= MAX_UPLOAD_FILES {
                    return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                        "that is more than {MAX_UPLOAD_FILES} files in one upload. Nothing was \
                         stored — drop them in smaller batches."
                    ))));
                }
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|error| multipart_error(error, "unreadable uploaded file"))?;
                parts.push(Part {
                    filename,
                    bytes: bytes.to_vec(),
                });
            }
            // Ignored rather than refused: a browser's `FormData` carries
            // fields this route has no opinion about.
            _ => {}
        }
    }

    if parts.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "that upload carried no files.".to_string(),
        )));
    }

    let lock = write_lock(company.id());
    let _guard = lock.lock().await;
    let mut results = Vec::with_capacity(parts.len());
    for part in parts {
        match store(&company, &part, force).await {
            Ok(skill) => results.push(UploadRow::stored(part.filename, skill)),
            Err(problem) => results.push(UploadRow::refused(part.filename, problem)),
        }
    }
    Ok(Json(UploadedDto { results }))
}

/// Reads, vets and stores one uploaded file.
///
/// Returns the refusal as a sentence rather than an [`ApiError`] because it
/// lands on one row of a multi-file answer; a status code cannot say which of
/// five files was the bad one.
async fn store(
    company: &AdminScopedCompany,
    part: &Part,
    force: bool,
) -> Result<InstalledSkill, Refusal> {
    let read = read_upload(&part.filename, &part.bytes).map_err(Refusal::plain)?;
    check_skill_doc_size(&read.doc)
        .map_err(problem_text)
        .map_err(Refusal::plain)?;
    let scan: ScanSummary = vet_skill(&read.slug, &read.doc, force).map_err(Refusal::from)?;
    let delta = SkillState {
        slug: read.slug,
        enabled: true,
        source: SkillSource::Custom,
        custom_doc: Some(read.doc),
    };
    company
        .runtime
        .skills()
        .set(company.id(), &delta)
        .await
        .map_err(|error| Refusal::plain(error.to_string()))?;
    Ok(InstalledSkill::from_state(&delta).with_scan(scan))
}

/// One file's refusal: the sentence an operator reads, and whether resending
/// with `force` would store it after all.
struct Refusal {
    message: String,
    scan_blocked: bool,
}

impl Refusal {
    /// A refusal `force` cannot help with — a file that is not a skill, a
    /// document over the size limit, a store that would not take it.
    fn plain(message: String) -> Self {
        Self {
            message,
            scan_blocked: false,
        }
    }
}

impl From<super::VetRefusal> for Refusal {
    fn from(refusal: super::VetRefusal) -> Self {
        Self {
            scan_blocked: refusal.is_scan_block(),
            message: refusal.message().to_string(),
        }
    }
}

/// The operator-facing sentence inside a refusal, for a per-file row.
fn problem_text(error: ApiError) -> String {
    error.0.to_string()
}

/// Classifies a multipart failure the way the ingest route does: a body that
/// overran the limit is a 413, anything else a malformed request.
fn multipart_error(error: MultipartError, context: &str) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError(OpenCompanyError::WorkspaceQuota(format!(
            "that upload is larger than the {} MiB one request may carry, so it was cut off \
             before anything could be read. Nothing was stored.",
            MAX_UPLOAD_BODY / (1024 * 1024)
        )));
    }
    ApiError(OpenCompanyError::InvalidRequest(format!(
        "{context}: {error}"
    )))
}

#[cfg(test)]
#[path = "upload_tests.rs"]
mod tests;
