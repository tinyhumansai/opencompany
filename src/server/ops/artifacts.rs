//! Versioned task artifacts (#187) — the Task Detail **Artifacts** tab.
//!
//! Routes, under both scope forms:
//!
//! | Route | Purpose |
//! | ----- | ------- |
//! | `GET …/tasks/{task_id}/artifacts` | one task's artifacts, newest first |
//! | `GET …/artifacts/{artifact_id}` | one artifact + its full version history |
//! | `POST …/artifacts` | open an artifact with its first version |
//! | `POST …/artifacts/{artifact_id}/versions` | append a version — the human edit |
//! | `GET …/artifacts/{artifact_id}/diff` | a diff between two versions |
//!
//! The append route is the interesting one: an operator's pre-approval edit is
//! recorded as a **new version by a different author**, never as a mutation of
//! the agent's. That is what makes "the agent wrote X, the operator shipped Y"
//! answerable afterwards — and it is why `PATCH`-a-version deliberately does
//! not exist. Editing a stored version in place would destroy the one datum
//! this whole port is for.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::artifacts::{
    ArtifactAuthor, ArtifactDiff, ArtifactKind, ArtifactRecord, ArtifactVersion,
};
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// The note stamped on a version created through the append route when the
/// caller supplies none. Kept as a constant so the console can recognise it.
const OPERATOR_EDIT_NOTE: &str = "operator edit before approval";

/// Builds the artifact route fragment.
pub fn router() -> Router<AppState> {
    scoped("/tasks/{task_id}/artifacts", get(list_for_task))
        .merge(scoped("/artifacts", post(create_artifact)))
        // Registered before the dynamic `{artifact_id}` routes for the same
        // reason `/tasks/inflight` is: a static segment must not be shadowed by
        // an id. Axum prefers the static segment, and no artifact id collides
        // with these words in practice, but the ordering is kept explicit.
        .merge(scoped(
            "/artifacts/{artifact_id}",
            get(get_artifact).delete(delete_artifact),
        ))
        .merge(scoped(
            "/artifacts/{artifact_id}/versions",
            post(append_version),
        ))
        .merge(scoped("/artifacts/{artifact_id}/diff", get(diff_artifact)))
}

/// The `{task_id}` path segment.
#[derive(Debug, Deserialize)]
struct TaskPath {
    task_id: String,
}

/// The `{artifact_id}` path segment.
#[derive(Debug, Deserialize)]
struct ArtifactPath {
    artifact_id: String,
}

/// The create body. `kind` defaults to `text`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateArtifact {
    task_id: String,
    title: String,
    body: String,
    #[serde(default)]
    kind: Option<ArtifactKind>,
    /// The agent that produced it. Defaults to `"agent"` when unattributed.
    #[serde(default)]
    author_id: Option<String>,
}

/// The append-a-version body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppendVersion {
    body: String,
    /// Who wrote it. Defaults to `operator` — the append route exists for the
    /// human edit, so an unattributed append is a human one.
    #[serde(default)]
    author: Option<ArtifactAuthor>,
    #[serde(default)]
    author_id: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// `?from=&to=` on the diff route. Both optional — see [`diff_artifact`].
#[derive(Debug, Deserialize)]
struct DiffQuery {
    #[serde(default)]
    from: Option<u32>,
    #[serde(default)]
    to: Option<u32>,
}

/// An artifact as the console renders it: the record plus the derived
/// human-edit diff, so the Artifacts tab needs one call rather than two.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactView {
    #[serde(flatten)]
    artifact: ArtifactRecord,
    /// The agent→operator diff, when a human has edited. Omitted otherwise, so
    /// an unedited artifact carries no empty scaffolding.
    #[serde(skip_serializing_if = "Option::is_none")]
    human_edit_diff: Option<ArtifactDiff>,
}

impl From<ArtifactRecord> for ArtifactView {
    fn from(artifact: ArtifactRecord) -> Self {
        Self {
            human_edit_diff: artifact.human_edit_diff(),
            artifact,
        }
    }
}

/// `GET …/tasks/{task_id}/artifacts` — one task's artifacts, newest first.
async fn list_for_task(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
) -> Result<Json<Vec<ArtifactView>>, ApiError> {
    let rows = company
        .runtime
        .artifacts()
        .list(company.id(), Some(&task_id))
        .await?;
    Ok(Json(rows.into_iter().map(ArtifactView::from).collect()))
}

/// `GET …/artifacts/{artifact_id}` — one artifact with its full history.
async fn get_artifact(
    company: ScopedCompany,
    Path(ArtifactPath { artifact_id }): Path<ArtifactPath>,
) -> Result<Json<ArtifactView>, ApiError> {
    Ok(Json(load(&company, &artifact_id).await?.into()))
}

/// `POST …/artifacts` — open an artifact with its first, agent-authored version.
async fn create_artifact(
    company: ScopedCompany,
    Json(body): Json<CreateArtifact>,
) -> Result<(StatusCode, Json<ArtifactView>), ApiError> {
    let record = ArtifactRecord::new(
        generate_id(),
        body.task_id,
        body.title,
        body.kind.unwrap_or(ArtifactKind::Text),
        body.body,
        body.author_id.unwrap_or_else(|| "agent".to_string()),
        now_millis(),
    );
    company
        .runtime
        .artifacts()
        .upsert(company.id(), &record)
        .await?;
    Ok((StatusCode::CREATED, Json(record.into())))
}

/// `POST …/artifacts/{artifact_id}/versions` — append a revision.
///
/// This is how a human edit is captured. The prior version is left untouched,
/// so the diff between them stays computable forever; there is deliberately no
/// route that rewrites a stored version.
async fn append_version(
    company: ScopedCompany,
    Path(ArtifactPath { artifact_id }): Path<ArtifactPath>,
    Json(body): Json<AppendVersion>,
) -> Result<Json<ArtifactView>, ApiError> {
    let mut record = load(&company, &artifact_id).await?;
    let author = body.author.unwrap_or(ArtifactAuthor::Operator);
    let author_id = body.author_id.unwrap_or_else(|| match author {
        ArtifactAuthor::Operator => "operator".to_string(),
        ArtifactAuthor::Agent => "agent".to_string(),
    });
    let note = body.note.or_else(|| match author {
        ArtifactAuthor::Operator => Some(OPERATOR_EDIT_NOTE.to_string()),
        ArtifactAuthor::Agent => None,
    });
    record.push_version(body.body, author, author_id, now_millis(), note);
    company
        .runtime
        .artifacts()
        .upsert(company.id(), &record)
        .await?;
    Ok(Json(record.into()))
}

/// `GET …/artifacts/{artifact_id}/diff?from=&to=` — diff two revisions.
///
/// With neither bound supplied this answers the question the tab actually asks
/// — *what did the human change?* — by returning the agent→operator diff. A
/// caller that wants a specific pair passes both; passing one is a 400 rather
/// than a guess, because "diff v2 against something I picked for you" is not a
/// question anyone asked.
async fn diff_artifact(
    company: ScopedCompany,
    Path(ArtifactPath { artifact_id }): Path<ArtifactPath>,
    Query(q): Query<DiffQuery>,
) -> Result<Json<ArtifactDiff>, ApiError> {
    let record = load(&company, &artifact_id).await?;
    match (q.from, q.to) {
        (None, None) => record.human_edit_diff().map(Json).ok_or_else(|| {
            ApiError(OpenCompanyError::InvalidRequest(format!(
                "artifact {artifact_id} has no operator edit to diff; pass ?from=&to="
            )))
        }),
        (Some(from), Some(to)) => {
            let from_v = version(&record, from, &artifact_id)?;
            let to_v = version(&record, to, &artifact_id)?;
            Ok(Json(ArtifactDiff::between(from_v, to_v)))
        }
        _ => Err(ApiError(OpenCompanyError::InvalidRequest(
            "pass both `from` and `to`, or neither for the human-edit diff".to_string(),
        ))),
    }
}

/// `DELETE …/artifacts/{artifact_id}`.
async fn delete_artifact(
    company: ScopedCompany,
    Path(ArtifactPath { artifact_id }): Path<ArtifactPath>,
) -> Result<StatusCode, ApiError> {
    if company
        .runtime
        .artifacts()
        .delete(company.id(), &artifact_id)
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(OpenCompanyError::NotFound(format!(
            "artifact {artifact_id}"
        ))))
    }
}

/// Loads an artifact or 404s, so every handler reports a missing id the same way.
async fn load(company: &ScopedCompany, artifact_id: &str) -> Result<ArtifactRecord, ApiError> {
    company
        .runtime
        .artifacts()
        .get(company.id(), artifact_id)
        .await?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::NotFound(format!(
                "artifact {artifact_id}"
            )))
        })
}

/// Resolves a version number, or 400s naming the one that does not exist.
fn version<'a>(
    record: &'a ArtifactRecord,
    version: u32,
    artifact_id: &str,
) -> Result<&'a ArtifactVersion, ApiError> {
    record.version(version).ok_or_else(|| {
        ApiError(OpenCompanyError::InvalidRequest(format!(
            "artifact {artifact_id} has no version {version}"
        )))
    })
}
