//! Avatar uploads: the custom-image half of choosing a face.
//!
//! ```text
//! POST …/avatars    upload an image and get back the reference to store
//! ```
//!
//! A face is stored on a teammate or a person as a short reference
//! (`docs/spec/runtime/avatars.md`) — either a `tiny:<flavour>` mascot, which
//! needs no upload at all, or a `blob:<nodeId>` naming bytes this host holds.
//! This route is how those bytes get here: it answers with the reference, which
//! the caller then `PATCH`es onto the teammate or onto themselves.
//!
//! ## Why this is not just `POST …/workspace/upload`
//!
//! The generic upload accepts anything and stores it wherever the console was
//! standing. That is right for the Files tab and wrong for a face, which needs
//! three things the generic route deliberately does not do:
//!
//! * **The type is sniffed, not believed.** A stored avatar is served back to
//!   every member of the company from this origin for as long as the teammate
//!   exists, so what it is served as has to be a fact about the bytes. See
//!   [`sniff_image`](crate::company::avatar::sniff_image).
//! * **The ceiling is an avatar's, not a file's.** The workspace cap is tens of
//!   megabytes because it is for documents; a face is bounded by
//!   [`MAX_AVATAR_BYTES`](crate::company::avatar::MAX_AVATAR_BYTES).
//! * **One folder.** Avatars land in a single `avatars/` folder rather than
//!   scattering through the tree, so an operator can see what the company is
//!   holding — and delete one — without hunting.
//!
//! ## Reading them back
//!
//! There is no `GET` here. The bytes are an ordinary binary workspace node, so
//! `GET …/workspace/blob/{nodeId}` already serves them — with the `nosniff` and
//! inline-renderable rules that route argues for. A second read path would be a
//! second place for those rules to be got wrong.

use axum::extract::{DefaultBodyLimit, Multipart};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

use crate::AppState;
use crate::company::avatar::{
    AVATARS_FOLDER, MAX_AVATAR_BYTES, check_image_dimensions, sniff_image,
};
use crate::company::workspace_names::kebab_name_or;
use crate::error::OpenCompanyError;
use crate::ports::generate_id;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the avatar route fragment.
pub fn router() -> Router<AppState> {
    scoped("/avatars", post(upload)).layer(
        // Sized against the avatar ceiling with a little headroom for the
        // multipart framing, rather than against the workspace upload limit:
        // this route's whole premise is that a face is small, and a body limit
        // is the cheapest place to enforce it — nothing over it is ever
        // buffered. The handler still checks the payload itself, because the
        // limit bounds the *request* and the cap is about the *image*.
        DefaultBodyLimit::max(MAX_AVATAR_BYTES + 64 * 1024),
    )
}

/// What an upload answers with.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadedAvatar {
    /// The reference to store — `blob:<nodeId>`. The caller `PATCH`es this onto
    /// a teammate or onto themselves; nothing has been *worn* yet.
    avatar: String,
    /// The node holding the bytes, for a caller that wants to read them back
    /// through `GET …/workspace/blob/{nodeId}` without re-parsing `avatar`.
    node_id: String,
    /// The media type the bytes were **sniffed** as, not the one declared.
    mime: String,
    /// The stored payload's exact length, as the store computed it.
    size: u64,
}

/// `POST …/avatars` — upload an image and get back the reference to store.
async fn upload(
    company: ScopedCompany,
    mut multipart: Multipart,
) -> Result<Json<UploadedAvatar>, ApiError> {
    let mut file: Option<(String, Vec<u8>)> = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        // A body that stops mid-part is indistinguishable from a malformed one,
        // which is why the limit above has headroom over the cap: an image over
        // the ceiling is meant to be refused by name and size below, not to
        // arrive here as a parse error.
        ApiError(OpenCompanyError::InvalidRequest(format!(
            "malformed avatar upload: {e}"
        )))
    })? {
        if field.name() == Some("file") {
            let name = field
                .file_name()
                .map(str::to_string)
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| "avatar".to_string());
            let bytes = field.bytes().await.map_err(|e| {
                ApiError(OpenCompanyError::InvalidRequest(format!(
                    "unreadable avatar upload: {e}"
                )))
            })?;
            file = Some((name, bytes.to_vec()));
        }
        // Every other field is ignored rather than refused: a browser's
        // `FormData` may carry parts this route has no use for.
    }

    let Some((name, bytes)) = file else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "the upload carried no `file` part".to_string(),
        )));
    };
    if bytes.len() > MAX_AVATAR_BYTES {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "that image is {:.1} MB — an avatar has to be under {} MB.",
            bytes.len() as f64 / 1_048_576.0,
            MAX_AVATAR_BYTES / 1_048_576
        ))));
    }
    let Some(mime) = sniff_image(&bytes) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "that file isn't a PNG, JPEG, GIF or WebP image. (An SVG can carry \
             script, so it can't be an avatar.)"
                .to_string(),
        )));
    };
    // Sniffing proves the bytes *name* an image; this proves the image they
    // name is not a decompression bomb. An avatar is drawn at a handful of
    // pixels, so a header claiming 65535×65535 in a 4 MiB payload has nothing
    // legitimate behind it — refuse it before it is stored and served to every
    // member who views the roster.
    check_image_dimensions(&bytes).map_err(ApiError)?;

    // One folder for every avatar this company holds. Adopted rather than
    // created-or-failed, so two people picking a face at the same moment do not
    // race a second `avatars/` folder into the tree.
    let folder = company
        .runtime
        .workspace()
        .adopt_or_create_folder(
            company.id(),
            None,
            AVATARS_FOLDER,
            WorkspaceOrigin::Operator,
        )
        .await?;

    // The last path segment only — a browser may send a whole path as the
    // filename — kebab-named like every other workspace node, and suffixed with
    // the id so two people uploading `avatar.png` do not collide in one folder.
    let stem = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&name)
        .trim()
        .to_string();
    let id = generate_id();
    let node = WorkspaceNode {
        name: format!("{}-{}", kebab_name_or(&stem, "avatar"), id),
        id: id.clone(),
        kind: NodeKind::File,
        parent_id: Some(folder.id().to_string()),
        updated_at_millis: crate::ports::now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        // Sniffed, never declared — see the module docs.
        mime: Some(mime.to_string()),
        // Both recomputed by the store from the bytes; a caller-supplied digest
        // would be an unverified claim about the payload.
        size: None,
        sha256: None,
        adopted: false,
    };
    let stored = company
        .runtime
        .workspace()
        .create_binary(company.id(), &node, &bytes)
        .await?;

    Ok(Json(UploadedAvatar {
        avatar: format!("blob:{}", stored.id),
        node_id: stored.id,
        mime: mime.to_string(),
        // The store's own count, not `bytes.len()`: the response should report
        // what was stored rather than what was sent.
        size: stored.size.unwrap_or(bytes.len() as u64),
    }))
}
