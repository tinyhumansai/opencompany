//! BYOK Composio: the company's **own** Composio account, reached directly.
//!
//! [`composio`](crate::harness::composio) is the managed route — every call
//! goes to the OpenHuman/TinyHumans backend, which holds the Composio API key,
//! enforces its own toolkit allowlist, bills the margin, and derives the
//! Composio entity from the bearer it is handed. That is the default and needs
//! no configuration.
//!
//! This module is the other route. A company that has its own Composio account
//! stores its API key ([`API_KEY_KEY`](crate::company::composio::API_KEY_KEY))
//! and every Composio call is then made against `backend.composio.dev` with
//! that key in `x-api-key` — no proxy, no platform identity, no platform bill.
//! It mirrors OpenHuman's own `backend` / `direct` split (see
//! `vendor/openhuman/src/openhuman/integrations/composio/client.rs::create_composio_client`),
//! and reuses OpenHuman's direct client
//! ([`oh::tools::ComposioTool`]) and its response reshapers wherever they are
//! reachable, so a BYOK result is the same envelope a managed one is and the
//! callers in [`composio`](crate::harness::composio) never branch on shape.
//!
//! # What is reused, and what is written here
//!
//! * `authorize`, `execute` and `list connections` are the vendored client's —
//!   [`oh::tools::ComposioTool::get_connection_url`],
//!   [`direct_execute`] and [`direct_list_connections`]. Nothing about the
//!   Composio contract is restated here.
//! * `list tools` and `list toolkits` are **not** reachable: the vendored
//!   reshapers for both are `pub(crate)` inside OpenHuman, and `vendor/openhuman`
//!   is a submodule this repo consumes rather than edits. They are re-stated
//!   below against the same two documented v3 endpoints, in the same envelopes,
//!   and marked so — if OpenHuman ever widens their visibility, both should be
//!   deleted in favour of the upstream ones.
//!
//! # What BYOK does not carry
//!
//! Per-toolkit `extra_params` at authorize time: the v3 link call takes no such
//! field, and a BYOK operator sets those on the auth config in their own
//! Composio dashboard. Everything else the managed route offers — including
//! revoking a connected account — has a direct equivalent here.
//!
//! # The credential still never reaches an agent
//!
//! The API key is handled exactly as the managed bearer is: it is the scrub
//! vector for every result, it is absent from every tracing line, and no type
//! here derives [`Debug`] over it. And the egress spine is the same — a BYOK
//! execute discloses (and under `LocalOnly` refuses) the transfer before the
//! round-trip, so choosing BYOK is not a way around the gate.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use openhuman_core::openhuman as oh;

use oh::integrations::composio::client::{direct_execute, direct_list_connections};
use oh::integrations::composio::types::{
    ComposioAuthorizeResponse, ComposioConnectionsResponse, ComposioDeleteResponse,
    ComposioExecuteResponse, ComposioToolFunction, ComposioToolSchema, ComposioToolkitCatalogEntry,
    ComposioToolkitsResponse, ComposioToolsResponse,
};

use crate::company::composio::{DIRECT_BASE_URL, DIRECT_ENTITY_ID};

/// Composio's v3 API root, derived from the non-secret base the console reports
/// so the two cannot name different hosts.
fn v3_base() -> String {
    format!("{DIRECT_BASE_URL}/api/v3")
}

/// How many rows one `/tools` page pulls.
///
/// Was `200`, described here as "Composio's own maximum for these endpoints" —
/// a phrase that was wrong twice over: it is not the maximum, and "these
/// endpoints" is what let one number govern two that answer to different
/// limits (see [`TOOLKITS_PAGE_LIMIT`]).
/// That is not what the API documents — `/tools` accepts `limit` up to **1000**
/// — and `tinyhumansai/backend` has been paging the same endpoint at 1000 since
/// before this client existed (`REST_PAGE_LIMIT` in
/// `controllers/agentIntegrations/composio/listTools.ts`). The old value made
/// the three-page budget a 600-row ceiling, which GitHub's 893-action catalogue
/// overflows: an agent asking for repo-scoped issue actions was handed 600 rows
/// that did not contain them and concluded, reasonably and wrongly, that the
/// capability did not exist.
///
/// At 1000 the same three pages reach 3000 rows, GitHub fits in a single
/// request, and the round-trip count drops with it.
const TOOLS_PAGE_LIMIT: &str = "1000";

/// How many rows one `/toolkits` page pulls.
///
/// **Deliberately not [`TOOLS_PAGE_LIMIT`].** The two endpoints shared one
/// constant until review caught it, and the justification above is about
/// `/tools` alone — the 1000 is what the tools endpoint documents and what
/// `listTools.ts` has always paged it at. Nothing in that reasoning transfers
/// to the provider catalogue, and applying it there was an unevidenced
/// widening: the two would keep moving together for a reason that only holds
/// for one of them.
///
/// 500 is what `tinyhumansai/backend` fetches the catalogue at
/// (`CATALOG_FETCH_LIMIT` in `services/composio/catalog.ts`), against the same
/// API, in production. The directory is ~1501 entries, so this still pages —
/// it is a page size, not a ceiling on the listing.
const TOOLKITS_PAGE_LIMIT: &str = "500";

/// How many pages one listing will follow before it stops.
///
/// Composio's v3 listings are cursor-paginated and **large**: the toolkit
/// directory is 1501 entries over 8 pages, and an unscoped `/tools` query is
/// 52,268 over 262. Following either to the end is not an option on a request
/// path — one full toolkit sweep measured 8.4s against a 5s budget on both the
/// host (`composio_toolkits::FETCH_TIMEOUT`) and the console
/// (`COMPOSIO_PROBE_TIMEOUT_MS`), so it would not merely be slow, it would
/// always time out and degrade to the fallback list.
///
/// Three pages is what fits that budget with margin (~3.2s measured). It is a
/// **bound, not a belief that 600 is enough** — [`Paged::dropped`] carries what
/// was left behind so no caller can mistake a truncated listing for a complete
/// one, which is the whole reason this is a named constant with a number
/// attached rather than a bare `limit=200` and silence.
const MAX_PAGES: usize = 3;

/// One listing, and what it did not reach.
pub(crate) struct Paged<T> {
    /// The rows fetched.
    pub(crate) items: Vec<T>,
    /// How many rows Composio said exist beyond the ones fetched, when it said.
    /// `0` means the listing is complete.
    pub(crate) dropped: usize,
}

/// A company's own Composio account.
///
/// Holds the vendored direct client for the operations it covers and the raw
/// key for the two this module states itself. No [`Debug`]: the key is a
/// credential, and this type exists on the request path.
#[derive(Clone)]
pub(crate) struct DirectComposio {
    tool: Arc<oh::tools::ComposioTool>,
    api_key: String,
    /// The v3 root the two listings below are addressed to.
    ///
    /// A field rather than the const inline so the tests can point them at a
    /// local mock. It is `#[cfg(test)]`-settable only: production has exactly
    /// one constructor, and it always pins Composio's HTTPS host — an
    /// injectable base reachable from a shipped build would be a way to send
    /// the `x-api-key` header somewhere else.
    v3_base: String,
}

impl DirectComposio {
    /// A client over this company's API key.
    ///
    /// The vendored [`oh::tools::ComposioTool`] takes a [`SecurityPolicy`] for
    /// its own `Tool::execute` gating; nothing here goes through that surface —
    /// the harness's own approval policy and grant gate are what admit a
    /// Composio call in this repo — so the default policy is what it is handed,
    /// exactly as OpenHuman's own factory does.
    ///
    /// [`SecurityPolicy`]: oh::security::SecurityPolicy
    pub(crate) fn new(api_key: &str) -> Self {
        let api_key = api_key.trim().to_string();
        let tool = oh::tools::ComposioTool::new(
            &api_key,
            Some(DIRECT_ENTITY_ID),
            Arc::new(oh::security::SecurityPolicy::default()),
        );
        Self {
            tool: Arc::new(tool),
            api_key,
            v3_base: v3_base(),
        }
    }

    /// Test-only seam: the same client with its v3 listings pointed at `base`,
    /// so the response parsing can be exercised against a local mock instead of
    /// `backend.composio.dev`.
    #[cfg(test)]
    pub(crate) fn with_v3_base_for_test(mut self, base: impl Into<String>) -> Self {
        self.v3_base = base.into();
        self
    }

    /// The connected accounts this company holds, in the managed route's own
    /// envelope. Straight through to the vendored reshaper, which also carries
    /// the invalid-key backoff gate — an `ak_` that Composio has revoked stops
    /// being re-presented on every poll.
    pub(crate) async fn list_connections(&self) -> Result<ComposioConnectionsResponse> {
        direct_list_connections(&self.tool).await
    }

    /// Begin an OAuth handoff and return Composio's hosted connect URL.
    ///
    /// The v3 link response carries no stable connection id — the row is
    /// created when the operator finishes OAuth on Composio's page — so the id
    /// is reported empty and the console's existing connection poll is what
    /// surfaces the result. Same answer OpenHuman's `direct_authorize` gives;
    /// it is `pub(super)` upstream, so the four lines are re-stated rather than
    /// called.
    pub(crate) async fn authorize(&self, toolkit: &str) -> Result<ComposioAuthorizeResponse> {
        let toolkit = toolkit.trim();
        if toolkit.is_empty() {
            anyhow::bail!("composio authorize: toolkit must not be empty");
        }
        let connect_url = self
            .tool
            .get_connection_url(Some(toolkit), None, DIRECT_ENTITY_ID)
            .await?;
        Ok(ComposioAuthorizeResponse {
            connect_url,
            connection_id: String::new(),
        })
    }

    /// Run one Composio action as this company's own account.
    ///
    /// The egress spine runs first, exactly as the managed pinned path does: a
    /// BYOK call ships the same arguments to the same third party, so it
    /// discloses the transfer — and under `LocalOnly` is refused — before the
    /// round-trip rather than after it.
    pub(crate) async fn execute(
        &self,
        tool: &str,
        arguments: Option<Value>,
        connection_id: Option<&str>,
    ) -> Result<ComposioExecuteResponse> {
        use oh::security::egress::{EgressDescriptor, emit_external_transfer, enforce_egress};

        let egress = EgressDescriptor::composio(tool);
        enforce_egress(&egress)?;
        emit_external_transfer(egress);

        direct_execute(&self.tool, tool, arguments, DIRECT_ENTITY_ID, connection_id).await
    }

    /// Composio v3 `GET /tools`, in the managed route's [`ComposioToolsResponse`]
    /// envelope — **with** each action's input schema.
    ///
    /// Schemas are the point. `oh::tools::ComposioTool::list_actions` is public
    /// and hits the same endpoint, but flattens to a name/description shape that
    /// drops `input_parameters`; an agent handed that has no way to learn an
    /// action's arguments and starts guessing them. OpenHuman's own
    /// schema-preserving reshaper (`direct_list_tools`) is `pub(crate)`, so this
    /// restates it — same endpoint, same `toolkit_versions=latest` pin (without
    /// it v3 answers from a snapshot that lists nothing for any toolkit
    /// published since launch), same envelope.
    pub(crate) async fn list_tools(
        &self,
        toolkits: &[String],
        // Full-text narrowing, applied by Composio over each action's name,
        // slug and description (`search`), and its declared tags (`tags`).
        //
        // Both were absent before, and their absence is what made discovery
        // fail rather than merely be coarse: the tool surface has taken a
        // `search` argument all along, but applied it CLIENT-SIDE to whatever
        // survived the page budget. Searching "issue" among 600 rows cannot
        // return an action sitting in the 293 that were dropped, so a narrowing
        // the caller asked for silently narrowed nothing. `tinyhumansai/backend`
        // threads `tags` server-side for the same reason.
        //
        // A `None` for either sends no parameter at all, which leaves the query
        // exactly as it was — the widening is opt-in, so no existing caller
        // changes behaviour.
        search: Option<&str>,
        tags: Option<&[String]>,
    ) -> Result<ComposioToolsResponse> {
        let mut params: Vec<(&str, String)> = vec![
            ("limit", TOOLS_PAGE_LIMIT.to_string()),
            ("toolkit_versions", "latest".to_string()),
        ];
        if let Some(term) = search.map(str::trim).filter(|term| !term.is_empty()) {
            params.push(("search", term.to_string()));
        }
        let tag_values: Vec<&str> = tags
            .unwrap_or(&[])
            .iter()
            .map(|tag| tag.trim())
            .filter(|tag| !tag.is_empty())
            .collect();
        // Curated preview when the caller has narrowed nothing (the shape
        // `tinyhumansai/backend` documents: "when `important` is omitted,
        // server-side defaults the filter to curated-only — returning ~50
        // 'featured' tools per toolkit").
        //
        // That default does NOT hold on this raw REST path — omitting the
        // parameter here returns the whole catalogue, which is how a bare
        // `toolkits: ["github"]` came back as 893 actions and overflowed the
        // page budget. So the curation is requested explicitly.
        //
        // Gated on having no `search` and no `tags`, and that is the whole
        // design: an unnarrowed call is a **browse**, and ~50 featured actions
        // is a far better answer to "what can GitHub do" than 893 rows the
        // reader cannot skim and the budget cannot carry. A call that names
        // either one is a **search**, and a search must be able to reach the
        // long tail — `GITHUB_LIST_REPOSITORY_ISSUES` is not featured, and it is
        // exactly what the last agent went looking for and reported as absent.
        //
        // So: browse is curated and small, search is complete. Neither is
        // truncated silently — `render_header` states `available`, `matched` and
        // `showing` on every listing.
        let narrowed = search.is_some_and(|term| !term.trim().is_empty()) || !tag_values.is_empty();
        if !narrowed {
            params.push(("important", "true".to_string()));
        }
        if !tag_values.is_empty() {
            // Comma-separated in one parameter, matching `toolkit_slug`'s
            // handling directly below — repeating a parameter is what returned
            // an empty body there, and there is no reason to assume this
            // endpoint treats a second one differently.
            params.push(("tags", tag_values.join(",")));
        }
        let slugs: Vec<&str> = toolkits
            .iter()
            .map(|slug| slug.trim())
            .filter(|slug| !slug.is_empty())
            .collect();
        if !slugs.is_empty() {
            // `toolkit_slug`, NOT `toolkits`. Composio v3 silently **ignores**
            // an unknown query parameter rather than refusing it, so
            // `toolkits=gmail` does not narrow anything — it returns the first
            // page of the entire 52,268-tool catalogue, alphabetically, which
            // is how an agent asking for Gmail came back holding
            // `0CODEKIT_CALCULATE_BMI`. Verified against the live API:
            // `toolkits=gmail` → 52,268 items, `toolkit_slug=gmail` → 63.
            //
            // Multiple slugs go as ONE comma-separated value; repeating the
            // parameter returns an empty body instead of a union.
            params.push(("toolkit_slug", slugs.join(",")));
        }
        let paged: Paged<V3Tool> = self
            .get_paged("/tools", &params)
            .await
            .context("Composio v3 /tools")?;
        if paged.dropped > 0 {
            tracing::warn!(
                toolkits = ?slugs,
                fetched = paged.items.len(),
                dropped = paged.dropped,
                search,
                tags = ?tag_values,
                "[composio-byok] list_tools: stopped at the page budget; narrow with `search` or \
                 `tags`, or name fewer toolkits, to see the rest"
            );
        }
        Ok(ComposioToolsResponse {
            tools: paged
                .items
                .into_iter()
                .filter_map(|item| {
                    let name = item.slug.or(item.name.clone())?;
                    let name = name.trim().to_string();
                    (!name.is_empty()).then(|| ComposioToolSchema {
                        kind: "function".to_string(),
                        function: ComposioToolFunction {
                            name,
                            description: item.description,
                            parameters: item.input_parameters,
                            output_parameters: item.output_parameters,
                        },
                    })
                })
                .collect(),
        })
    }

    /// Revoke one connected account: Composio v3
    /// `DELETE /connected_accounts/{id}`.
    ///
    /// The vendored [`oh::tools::ComposioTool`] has no delete, and OpenHuman's
    /// own direct mode routes its revoke through the backend — but neither is a
    /// statement about Composio, which exposes the route perfectly well. A
    /// no-auth probe answers `401` on it and `404` on a route that does not
    /// exist, so the endpoint is real; the vendored client simply never grew a
    /// method for it.
    ///
    /// A 2xx is the revoke. Composio's delete responses carry no body worth
    /// mapping, so success is reported as `deleted: true` and the
    /// memory-chunk count the backend-proxied shape carries stays zero — that
    /// field counts what the *OpenHuman backend* cleaned up alongside the
    /// revoke, and on this route there is no backend to have done so.
    pub(crate) async fn delete_connection(
        &self,
        connection_id: &str,
    ) -> Result<ComposioDeleteResponse> {
        let connection_id = connection_id.trim();
        if connection_id.is_empty() {
            anyhow::bail!("composio delete_connection: a connection id is required");
        }
        // A Composio connection id is an opaque nanoid (`ca_…`). Rather than
        // pull in an escaper for it, refuse anything that is not one: a value
        // carrying a slash or a query character is not an id this company holds,
        // and interpolating it into a path would let it address a different
        // route entirely.
        if !connection_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            anyhow::bail!("composio delete_connection: `{connection_id}` is not a connection id");
        }
        let url = format!("{}/connected_accounts/{connection_id}", self.v3_base);
        let resp = oh::config::build_runtime_proxy_client_with_timeouts("composio.byok", 60, 10)
            .delete(&url)
            .header("x-api-key", &self.api_key)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                anyhow::bail!(
                    "Composio rejected this company's API key — check it at app.composio.dev, \
                     or clear it to go back to OpenHuman-managed Composio"
                );
            }
            anyhow::bail!("Composio answered {status}");
        }
        Ok(ComposioDeleteResponse {
            deleted: true,
            memory_chunks_deleted: 0,
        })
    }

    /// Composio v3 `GET /toolkits`, in the managed route's
    /// [`ComposioToolkitsResponse`] envelope.
    ///
    /// A BYOK company has **no server-enforced allowlist** — its Composio
    /// dashboard is the boundary — so what this returns is the catalog of what
    /// that account can connect, and every entry is reported `enabled`. That is
    /// the honest answer for this route and it is what makes the console's
    /// provider grid work in BYOK mode; OpenHuman's direct branch returns an
    /// empty list here, which would leave the grid blank with nothing saying
    /// why.
    ///
    /// Every field but `slug` is best-effort. Composio nests the display
    /// metadata under `meta`, and its category entries are objects with a name;
    /// both are parsed tolerantly, because a catalog row that will not fully
    /// deserialize should still be connectable.
    pub(crate) async fn list_toolkits(&self) -> Result<ComposioToolkitsResponse> {
        let paged: Paged<V3Toolkit> = self
            .get_paged("/toolkits", &[("limit", TOOLKITS_PAGE_LIMIT.to_string())])
            .await
            .context("Composio v3 /toolkits")?;
        if paged.dropped > 0 {
            // Composio publishes 1501 toolkits; the budget above reaches 600 of
            // them. Said out loud because a console grid showing 600 providers
            // looks exactly like a complete one.
            tracing::warn!(
                fetched = paged.items.len(),
                dropped = paged.dropped,
                "[composio-byok] list_toolkits: stopped at the page budget — the provider grid is \
                 showing part of this account's catalogue"
            );
        }
        let catalog: Vec<ComposioToolkitCatalogEntry> = paged
            .items
            .into_iter()
            .filter_map(|item| {
                let slug = item.slug.trim().to_ascii_lowercase();
                if slug.is_empty() {
                    return None;
                }
                let meta = item.meta.unwrap_or_default();
                Some(ComposioToolkitCatalogEntry {
                    name: item.name.trim().to_string(),
                    logo: meta
                        .logo
                        .map(|logo| logo.trim().to_string())
                        .filter(|logo| !logo.is_empty()),
                    description: meta
                        .description
                        .map(|text| text.trim().to_string())
                        .filter(|text| !text.is_empty()),
                    categories: meta
                        .categories
                        .into_iter()
                        .filter_map(V3Category::name)
                        .collect(),
                    // No gate stands between a BYOK company and its own
                    // account, so every listed toolkit is connectable.
                    enabled: Some(true),
                    slug,
                })
            })
            .collect();
        Ok(ComposioToolkitsResponse {
            toolkits: catalog.iter().map(|entry| entry.slug.clone()).collect(),
            catalog,
        })
    }

    /// Follow Composio's cursor pagination for up to [`MAX_PAGES`], returning
    /// what was fetched and what was left behind.
    ///
    /// Every v3 listing answers `{items, next_cursor, total_items, …}`, and the
    /// cursor is strictly sequential — page N+1's cursor only exists once page N
    /// has been read — so this cannot be parallelised into the request budget.
    /// It stops at whichever comes first: the cursor running out (a complete
    /// listing, `dropped: 0`) or the page budget.
    ///
    /// `dropped` is computed from Composio's own `total_items` rather than
    /// guessed, so a truncated listing can say how much it is missing instead of
    /// merely admitting that it might be. When `total_items` never arrived on
    /// any page — an older/degraded response shape — the exact count is
    /// unknowable, but reaching this line at all is only possible via the page
    /// budget running out with a live cursor still in hand (the loop's only
    /// other exit returns early with `dropped: 0`), so *some* truncation is a
    /// certainty even without a number for it. `.max(1)` reports that
    /// certainty instead of letting an absent count read as a complete list.
    async fn get_paged<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Paged<T>> {
        let mut items: Vec<T> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut total: Option<usize> = None;

        for _ in 0..MAX_PAGES {
            let mut page_params: Vec<(&str, String)> = params.to_vec();
            if let Some(ref cursor) = cursor {
                page_params.push(("cursor", cursor.clone()));
            }
            let page: V3Page<T> = self.get(path, &page_params).await?;
            total = total.or(page.total_items);
            items.extend(page.items);
            match page.next_cursor.filter(|c| !c.trim().is_empty()) {
                // No cursor left: the listing is complete, whatever `total_items`
                // claimed. Trusting the count over the cursor here would invent a
                // `dropped` for a listing that had already ended.
                None => return Ok(Paged { items, dropped: 0 }),
                Some(next) => cursor = Some(next),
            }
        }

        // Reaching here means the budget ran out with a cursor still live —
        // see the doc comment above. `.max(1)` is a floor, not a substitute for
        // the real count: when `total` is present, its subtraction already
        // exceeds it (there is more data than what was fetched, by
        // definition), so the floor only ever engages when `total` was absent.
        let dropped = total.unwrap_or(0).saturating_sub(items.len()).max(1);
        Ok(Paged { items, dropped })
    }

    /// One authenticated v3 GET, decoded into `T`.
    ///
    /// Built through OpenHuman's own runtime HTTP factory so a BYOK call
    /// inherits the same proxy configuration and timeouts every other Composio
    /// call in this process has — the alternative, a bare `reqwest::Client`,
    /// would quietly ignore a proxy the operator configured for everything else.
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T> {
        let url = format!("{}{path}", self.v3_base);
        let resp = oh::config::build_runtime_proxy_client_with_timeouts("composio.byok", 60, 10)
            .get(&url)
            .header("x-api-key", &self.api_key)
            .query(params)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            // The body may echo the request; it is not rendered here at all —
            // the caller scrubs, but a status line cannot carry a key and is
            // enough to tell a bad key from an outage.
            //
            // 401 is called by name because it is the one failure an operator
            // can act on, and by far the likeliest: a key that was mistyped,
            // revoked, or belongs to a different Composio account. "Composio
            // answered 401 Unauthorized" is a fact; "Composio rejected this
            // company's API key" is the same fact plus what to do about it.
            if status == reqwest::StatusCode::UNAUTHORIZED {
                anyhow::bail!(
                    "Composio rejected this company's API key — check it at app.composio.dev, \
                     or clear it to go back to OpenHuman-managed Composio"
                );
            }
            anyhow::bail!("Composio answered {status}");
        }
        resp.json::<T>().await.context("decoding the response")
    }
}

// ── v3 wire shapes ──────────────────────────────────────────────────
//
// Deliberately private and deliberately tolerant: every field is optional, so
// a Composio response that grows or renames something degrades to a thinner
// row rather than failing the whole listing.

/// One page of any v3 listing.
///
/// Generic over the row because `/tools` and `/toolkits` differ only in what
/// `items` holds — the pagination envelope around them is identical, and two
/// copies of it would be two places to get the cursor handling wrong.
#[derive(Debug, Deserialize)]
struct V3Page<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
    /// Absent or blank on the last page.
    #[serde(default)]
    next_cursor: Option<String>,
    /// How many rows exist across every page, when Composio says.
    #[serde(default)]
    total_items: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct V3Tool {
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// v3 names the input schema `input_parameters`; older payloads say
    /// `parameters`. Both land here, matching the vendored client's own alias.
    #[serde(default, alias = "parameters")]
    input_parameters: Option<Value>,
    #[serde(default)]
    output_parameters: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct V3Toolkit {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    meta: Option<V3ToolkitMeta>,
}

#[derive(Debug, Default, Deserialize)]
struct V3ToolkitMeta {
    #[serde(default)]
    logo: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    categories: Vec<V3Category>,
}

/// A category, which Composio publishes as an object but which older payloads
/// (and the backend-proxied path) carry as a bare string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum V3Category {
    Name(String),
    Object {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        slug: Option<String>,
    },
}

impl V3Category {
    /// The display name, preferring the published name over the slug. `None`
    /// when the row carries neither.
    fn name(self) -> Option<String> {
        let raw = match self {
            Self::Name(name) => Some(name),
            Self::Object { name, slug } => name.or(slug),
        }?;
        let trimmed = raw.trim().to_string();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

// ── Checking a DRAFT key, before anything is stored ──────────────────
//
// The console's "paste an API key" flow validates the key it was handed rather
// than the key the company is already on, and it does so BEFORE the write. That
// ordering is a deliberate departure from the inference connect flow
// (`docs/modules/inference/connect-flow.md`), which writes the credential
// first: its probe resolves a key by provider slug out of the store, so the
// only way to exercise a draft there is to store it and roll back on a
// destructive failure — which buys a rollback path and an orphaned-secret
// failure mode along with it. Composio's probe takes the key **directly**, as
// an argument, so there is nothing to roll back: a key that Composio rejects is
// simply never written, and no failure of this function can leave the company
// holding a credential it did not choose. That doc's own "Testing a draft"
// section is where the shape comes from.
//
// No SSRF guard, on purpose. The connect-flow doc spends a section on SSRF
// because its probe dials an endpoint the OPERATOR typed. This one dials
// `DIRECT_BASE_URL` — a compile-time constant, the same one `v3_base` pins for
// every other call in this module — and takes no endpoint from any caller.
// There is no attacker-controlled destination here to guard, and adding a guard
// would suggest, wrongly, that there is a way to point this somewhere else.

/// How long a draft-key check may take before it is reported as a timeout.
///
/// Shorter than the 60s the listing calls allow: this one sits in front of an
/// operator watching a modal, the answer is advisory, and a check that has not
/// come back in ten seconds has already failed at being a check.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Ask Composio whether it recognises `api_key`, without storing it anywhere.
///
/// `Ok(())` means Composio answered the call. `Err` carries a raw reason for
/// [`classify`](crate::company::composio_probe::classify) — **for
/// classification and a debug log only**: the caller puts
/// [`describe`](crate::company::composio_probe::describe)'s fixed copy on the
/// wire, never this string.
///
/// The cheapest authenticated read Composio v3 has: one page of one toolkit.
/// The response body is discarded — only whether the call was accepted matters,
/// and a body that echoed the request is not something to carry back.
pub(crate) async fn probe_api_key(api_key: &str) -> Result<(), String> {
    probe_at(&v3_base(), api_key).await
}

/// [`probe_api_key`] against an explicit base.
///
/// Private, and reachable from a shipped build only through [`probe_api_key`],
/// which always pins Composio's own host — the same rule `v3_base` follows for
/// [`DirectComposio`], and for the same reason: a base a caller could choose
/// would be a way to send the `x-api-key` header somewhere else.
async fn probe_at(base: &str, api_key: &str) -> Result<(), String> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        // Not reachable from the route (it never probes a clear), but a blank
        // key would otherwise be sent to Composio and come back 401 —
        // classified `auth`, which is the destructive class. Refuse it here in
        // the non-destructive class instead.
        return Err("no Composio API key was given, so nothing could be checked".to_string());
    }
    let url = format!("{base}/toolkits");
    let request = oh::config::build_runtime_proxy_client_with_timeouts("composio.probe", 10, 5)
        .get(&url)
        .header("x-api-key", api_key)
        .query(&[("limit", "1")])
        .send();
    let resp = match tokio::time::timeout(PROBE_TIMEOUT, request).await {
        Err(_) => {
            return Err(format!(
                "Composio timed out after {}s",
                PROBE_TIMEOUT.as_secs()
            ));
        }
        // `reqwest`'s own rendering names the URL (a constant here) and the
        // transport fault; it never carries a header, so it cannot carry the
        // key. It is still only ever classified and debug-logged.
        Ok(Err(err)) => return Err(err.to_string()),
        Ok(Ok(resp)) => resp,
    };
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    // The status line and nothing else. A Composio error body can echo the
    // request, and a proxy's error body is an HTML page — neither is something
    // to carry back from a function whose output is classified by substring.
    Err(format!("Composio answered {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_v3_tool_keeps_the_schema_an_agent_needs_to_call_it() {
        // The whole reason this module restates the listing: `list_actions`
        // would drop `input_parameters`, and an agent without an action's
        // schema invents arguments.
        let body: V3Page<V3Tool> = serde_json::from_str(
            r#"{"items":[{"slug":"GMAIL_SEND_EMAIL","description":"Send",
                 "input_parameters":{"type":"object"},"output_parameters":{"type":"object"}}]}"#,
        )
        .unwrap();
        assert_eq!(body.items.len(), 1);
        assert!(body.items[0].input_parameters.is_some());
        assert!(body.items[0].output_parameters.is_some());
    }

    #[test]
    fn an_older_payload_spelling_parameters_still_carries_its_schema() {
        let body: V3Page<V3Tool> =
            serde_json::from_str(r#"{"items":[{"slug":"X","parameters":{"type":"object"}}]}"#)
                .unwrap();
        assert!(body.items[0].input_parameters.is_some());
    }

    #[test]
    fn categories_parse_whether_composio_sends_objects_or_strings() {
        let meta: V3ToolkitMeta = serde_json::from_str(
            r#"{"categories":[{"name":"Productivity","slug":"productivity"},"email",{"slug":"crm"}]}"#,
        )
        .unwrap();
        let names: Vec<String> = meta
            .categories
            .into_iter()
            .filter_map(V3Category::name)
            .collect();
        assert_eq!(names, vec!["Productivity", "email", "crm"]);
    }

    #[test]
    fn a_toolkit_row_missing_everything_but_a_slug_is_still_connectable() {
        // A thin catalog row must not drop the provider — the console renders
        // its own typography for slug-only entries, and a missing row is a
        // provider the operator simply cannot connect.
        let body: V3Page<V3Toolkit> =
            serde_json::from_str(r#"{"items":[{"slug":"GMAIL"},{"slug":"  "}]}"#).unwrap();
        assert_eq!(body.items.len(), 2);
        let kept: Vec<String> = body
            .items
            .into_iter()
            .map(|item| item.slug.trim().to_ascii_lowercase())
            .filter(|slug| !slug.is_empty())
            .collect();
        assert_eq!(
            kept,
            vec!["gmail"],
            "a blank slug is dropped, a bare one is kept"
        );
    }

    /// A mock Composio v3 that answers the two listings this module states
    /// itself, asserting the `x-api-key` header on the way through: the whole
    /// point of BYOK is that the *company's* key, and no other credential,
    /// reaches Composio.
    /// The two endpoints answer to different limits, and shared one constant
    /// until review caught it. The 1000 is justified for `/tools` only; the
    /// catalogue is paged at what the backend proves against the same API.
    #[test]
    fn the_two_endpoints_do_not_share_a_page_limit() {
        assert_eq!(TOOLS_PAGE_LIMIT, "1000");
        assert_eq!(TOOLKITS_PAGE_LIMIT, "500");
        assert_ne!(
            TOOLS_PAGE_LIMIT, TOOLKITS_PAGE_LIMIT,
            "a tools-only justification must not govern the provider catalogue"
        );
    }

    /// A `/tools` server that records the query it was asked with.
    ///
    /// The existing fixture asserts on counts, which cannot see a query
    /// parameter at all — and `important=true` changes what the *server*
    /// returns, so a count-based assertion would pass whether it was sent or
    /// not (tinysweeper on tinyhumansai/opencompany#2153).
    async fn spawn_query_recorder() -> (String, Arc<Mutex<Vec<HashMap<String, String>>>>) {
        use axum::extract::Query;
        use axum::routing::get;
        use axum::{Json, Router};

        let seen: Arc<Mutex<Vec<HashMap<String, String>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        // `/tools`, not `/api/v3/tools`: `with_v3_base_for_test` already carries
        // the API path, exactly as the fixtures below mount it.
        let app = Router::new().route(
            "/tools",
            get(move |Query(params): Query<HashMap<String, String>>| {
                let sink = sink.clone();
                async move {
                    sink.lock().expect("query sink").push(params);
                    Json(serde_json::json!({ "items": [] }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, seen)
    }

    /// An unnarrowed browse asks for the curated set; a search must reach the
    /// long tail, so it must not.
    #[tokio::test]
    async fn important_is_sent_only_for_an_unnarrowed_browse() {
        let (base, seen) = spawn_query_recorder().await;
        let direct = DirectComposio::new("ak_live").with_v3_base_for_test(base);

        direct
            .list_tools(&["github".to_string()], None, None)
            .await
            .expect("browse");
        direct
            .list_tools(&["github".to_string()], Some("issue"), None)
            .await
            .expect("search");
        direct
            .list_tools(
                &["github".to_string()],
                None,
                Some(&["important".to_string()]),
            )
            .await
            .expect("tag-narrowed");

        let seen = seen.lock().expect("query sink");
        assert_eq!(seen.len(), 3, "three calls were made");
        assert_eq!(
            seen[0].get("important").map(String::as_str),
            Some("true"),
            "an unnarrowed browse asks for the curated set: {:?}",
            seen[0]
        );
        assert!(
            !seen[1].contains_key("important"),
            "a search must reach past the featured actions: {:?}",
            seen[1]
        );
        assert!(
            !seen[2].contains_key("important"),
            "a tag filter is a search too: {:?}",
            seen[2]
        );
        assert_eq!(
            seen[2].get("tags").map(String::as_str),
            Some("important"),
            "the tag itself still travels: {:?}",
            seen[2]
        );
        assert_eq!(
            seen[1].get("search").map(String::as_str),
            Some("issue"),
            "the search term travels server-side: {:?}",
            seen[1]
        );
    }

    async fn spawn_composio_v3() -> String {
        use axum::extract::Query;
        use axum::http::HeaderMap;
        use axum::routing::get;
        use axum::{Json, Router};
        use std::collections::HashMap;

        async fn toolkits(headers: HeaderMap) -> Json<serde_json::Value> {
            assert_eq!(
                headers.get("x-api-key").and_then(|v| v.to_str().ok()),
                Some("ak_live"),
                "a BYOK call must present the company's own key"
            );
            Json(serde_json::json!({"items": [
                {"slug": "GMAIL", "name": "Gmail",
                 "meta": {"logo": "https://cdn/gmail.png", "description": " Email ",
                          "categories": [{"name": "Productivity"}, "email"]}},
                {"slug": "slack", "name": "Slack"}
            ]}))
        }

        async fn tools(
            headers: HeaderMap,
            Query(params): Query<HashMap<String, String>>,
        ) -> Json<serde_json::Value> {
            assert_eq!(
                headers.get("x-api-key").and_then(|v| v.to_str().ok()),
                Some("ak_live")
            );
            assert_eq!(
                params.get("toolkit_versions").map(String::as_str),
                Some("latest"),
                "without the pin, v3 answers from a snapshot listing nothing recent"
            );
            // `toolkits` is the parameter Composio v3 silently ignores; if it
            // ever comes back the listing stops being scoped at all.
            assert!(
                !params.contains_key("toolkits"),
                "`toolkits` scopes nothing on Composio v3 — `toolkit_slug` is the one it honours"
            );
            let scoped = params.get("toolkit_slug").cloned().unwrap_or_default();
            Json(serde_json::json!({"items": [
                {"slug": "GMAIL_SEND_EMAIL", "description": scoped,
                 "input_parameters": {"type": "object"}}
            ]}))
        }

        let app = Router::new()
            .route("/toolkits", get(toolkits))
            .route("/tools", get(tools));
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn the_catalog_is_the_company_s_own_and_every_row_is_connectable() {
        // OpenHuman's direct branch answers this with an empty list, which
        // would leave the console's provider grid blank with nothing saying
        // why. A BYOK company has no gate above its own account, so what it is
        // offered is what that account lists.
        let base = spawn_composio_v3().await;
        let direct = DirectComposio::new("ak_live").with_v3_base_for_test(base);

        let resp = direct.list_toolkits().await.expect("catalog");
        assert_eq!(
            resp.toolkits,
            vec!["gmail", "slack"],
            "slugs are normalized"
        );
        assert!(
            resp.catalog.iter().all(|e| e.enabled == Some(true)),
            "nothing stands between a company and its own account"
        );
        let gmail = &resp.catalog[0];
        assert_eq!(gmail.name, "Gmail");
        assert_eq!(gmail.description.as_deref(), Some("Email"));
        assert_eq!(gmail.logo.as_deref(), Some("https://cdn/gmail.png"));
        assert_eq!(gmail.categories, vec!["Productivity", "email"]);
        // A row with no `meta` at all is still offered, described by its slug.
        assert_eq!(resp.catalog[1].slug, "slack");
        assert!(resp.catalog[1].description.is_none());
    }

    #[tokio::test]
    async fn a_tool_listing_carries_the_schema_and_is_scoped_to_the_toolkits_asked_for() {
        let base = spawn_composio_v3().await;
        let direct = DirectComposio::new("ak_live").with_v3_base_for_test(base);

        let resp = direct
            .list_tools(
                &["gmail".to_string(), "  ".to_string(), "slack".to_string()],
                None,
                None,
            )
            .await
            .expect("tools");
        assert_eq!(resp.tools.len(), 1);
        let function = &resp.tools[0].function;
        assert_eq!(function.name, "GMAIL_SEND_EMAIL");
        assert!(
            function.parameters.is_some(),
            "an agent without the schema invents arguments"
        );
        assert_eq!(
            function.description.as_deref(),
            Some("gmail,slack"),
            "blank slugs are dropped, the rest are sent as one csv"
        );
    }

    /// Composio's listings are cursor-paginated and far larger than one page:
    /// 1501 toolkits, and 52,268 tools unscoped. A single `limit=200` request
    /// returns the first 200 and says nothing, which is indistinguishable from a
    /// complete answer — so the listing follows the cursor, and reports what it
    /// could not reach.
    #[tokio::test]
    async fn a_listing_follows_the_cursor_and_says_what_it_could_not_reach() {
        use axum::extract::Query;
        use axum::routing::get;
        use axum::{Json, Router};
        use std::collections::HashMap;

        // Five pages of one row each, against a three-page budget.
        async fn toolkits(
            Query(params): Query<HashMap<String, String>>,
        ) -> Json<serde_json::Value> {
            let page: usize = params
                .get("cursor")
                .and_then(|c| c.parse().ok())
                .unwrap_or(1);
            Json(serde_json::json!({
                "items": [{ "slug": format!("tk{page}"), "name": format!("Toolkit {page}") }],
                "next_cursor": (page < 5).then(|| (page + 1).to_string()),
                "total_items": 5,
            }))
        }

        let app = Router::new().route("/toolkits", get(toolkits));
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let direct = DirectComposio::new("ak_live").with_v3_base_for_test(format!("http://{addr}"));
        let resp = direct.list_toolkits().await.expect("catalog");
        assert_eq!(
            resp.toolkits,
            vec!["tk1", "tk2", "tk3"],
            "three pages are followed, not one"
        );
    }

    /// A listing that hits the page budget with no `total_items` on any page —
    /// an older or degraded response shape — still must not report `dropped: 0`.
    /// Reaching the end of the budget with a live cursor in hand is itself proof
    /// that more exists; the exact count is merely unknown, not zero.
    #[tokio::test]
    async fn a_truncated_listing_with_no_total_still_reports_dropped() {
        use axum::extract::Query;
        use axum::routing::get;
        use axum::{Json, Router};
        use std::collections::HashMap;

        async fn toolkits(
            Query(params): Query<HashMap<String, String>>,
        ) -> Json<serde_json::Value> {
            let page: usize = params
                .get("cursor")
                .and_then(|c| c.parse().ok())
                .unwrap_or(1);
            // No `total_items` field at all on any page.
            Json(serde_json::json!({
                "items": [{ "slug": format!("tk{page}"), "name": format!("Toolkit {page}") }],
                "next_cursor": (page + 1).to_string(),
            }))
        }

        let app = Router::new().route("/toolkits", get(toolkits));
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let direct = DirectComposio::new("ak_live").with_v3_base_for_test(format!("http://{addr}"));
        let paged: Paged<V3Toolkit> = direct
            .get_paged("/toolkits", &[("limit", "200".to_string())])
            .await
            .expect("page");
        assert_eq!(
            paged.items.len(),
            3,
            "the page budget, not the never-ending cursor"
        );
        assert!(
            paged.dropped >= 1,
            "no total_items must not read as a complete listing: dropped={}",
            paged.dropped
        );
    }

    /// A listing that ends inside the budget is complete, and must not claim to
    /// have dropped anything — the cursor running out is the authority, not the
    /// `total_items` count beside it.
    #[tokio::test]
    async fn a_listing_that_fits_reports_nothing_dropped() {
        use axum::routing::get;
        use axum::{Json, Router};

        let app = Router::new().route(
            "/toolkits",
            get(|| async {
                Json(serde_json::json!({
                    "items": [{ "slug": "gmail", "name": "Gmail" }],
                    "next_cursor": null,
                    // Deliberately inconsistent with `items`: an exhausted cursor
                    // ends the listing whatever the count says.
                    "total_items": 99,
                }))
            }),
        );
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let direct = DirectComposio::new("ak_live").with_v3_base_for_test(format!("http://{addr}"));
        let paged: Paged<V3Toolkit> = direct
            .get_paged("/toolkits", &[("limit", "200".to_string())])
            .await
            .expect("page");
        assert_eq!(paged.items.len(), 1);
        assert_eq!(paged.dropped, 0, "an exhausted cursor means complete");
    }

    /// Revoking works on this route. It is worth a test precisely because it was
    /// once assumed not to: the vendored client has no delete method, and that
    /// was mistaken for Composio not having the endpoint. It has one — a no-auth
    /// probe answers 401 on it and 404 on a route that does not exist.
    #[tokio::test]
    async fn an_account_can_be_revoked_on_this_route() {
        use axum::extract::Path;
        use axum::http::HeaderMap;
        use axum::routing::delete;
        use axum::{Json, Router};
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().route(
            "/connected_accounts/{id}",
            delete(move |Path(id): Path<String>, headers: HeaderMap| {
                let sink = Arc::clone(&sink);
                async move {
                    assert_eq!(
                        headers.get("x-api-key").and_then(|v| v.to_str().ok()),
                        Some("ak_live"),
                        "a revoke presents the company's own key, like every other BYOK call"
                    );
                    sink.lock().unwrap().push(id);
                    Json(serde_json::json!({}))
                }
            }),
        );
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let direct = DirectComposio::new("ak_live").with_v3_base_for_test(format!("http://{addr}"));
        let resp = direct
            .delete_connection("ca_abc123")
            .await
            .expect("revoked");
        assert!(resp.deleted);
        assert_eq!(seen.lock().unwrap().as_slice(), ["ca_abc123"]);
    }

    /// An id that is not one cannot be interpolated into the path — a value
    /// carrying a slash would address a different route entirely.
    #[tokio::test]
    async fn a_value_that_is_not_a_connection_id_is_refused_before_any_request() {
        let direct = DirectComposio::new("ak_live").with_v3_base_for_test("http://127.0.0.1:1");
        for bad in ["", "  ", "ca_1/../../toolkits", "ca_1?x=y"] {
            assert!(
                direct.delete_connection(bad).await.is_err(),
                "`{bad}` must not reach the wire"
            );
        }
    }

    /// Live end-to-end against a real Composio account, driving the exact calls
    /// the agent's `composio_list_tools` and `composio_execute` make.
    ///
    /// `#[ignore]`d: it needs a real `ak_…` and a live connected GitHub account,
    /// so it cannot run in CI. Run it by hand against a rig:
    ///
    /// ```text
    /// COMPOSIO_LIVE_KEY=$(cat <data-dir>/companies/<id>/secrets/%k-composio%2Fapi%5Fkey) \
    ///   cargo test --features composio --lib live_byok -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs a real Composio key and a live GitHub connection"]
    async fn live_byok_lists_and_executes_against_real_composio() {
        let key = std::env::var("COMPOSIO_LIVE_KEY").expect("COMPOSIO_LIVE_KEY");
        let owner = std::env::var("LIVE_OWNER").unwrap_or_else(|_| "tinyhumansai".into());
        let repo = std::env::var("LIVE_REPO").unwrap_or_else(|_| "opencompany".into());
        let direct = DirectComposio::new(&key);

        // 1. The listing the agent discovers actions through.
        let tools = direct
            .list_tools(&["github".to_string()], None, None)
            .await
            .expect("list_tools");
        let target = tools
            .tools
            .iter()
            .find(|t| t.function.name == "GITHUB_LIST_PULL_REQUESTS")
            .expect("GITHUB_LIST_PULL_REQUESTS is in the listing");
        assert!(
            target.function.parameters.is_some(),
            "the agent needs the schema to build arguments"
        );
        println!("listed {} github tools", tools.tools.len());

        // 2. The execute the agent then makes.
        let resp = direct
            .execute(
                "GITHUB_LIST_PULL_REQUESTS",
                Some(serde_json::json!({
                    "owner": owner, "repo": repo,
                    "state": "open", "sort": "created", "per_page": 5
                })),
                None,
            )
            .await
            .expect("execute");
        println!("successful={} error={:?}", resp.successful, resp.error);
        let items = resp
            .data
            .get("details")
            .or_else(|| resp.data.get("data"))
            .cloned()
            .unwrap_or(resp.data.clone());
        println!(
            "{}",
            serde_json::to_string_pretty(&items).unwrap_or_default()
        );
        assert!(resp.successful, "execute failed: {:?}", resp.error);
    }

    #[tokio::test]
    async fn a_refused_key_is_reported_without_echoing_it() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::get;

        let app = Router::new().route(
            "/toolkits",
            get(|| async { (StatusCode::UNAUTHORIZED, "Invalid API key: ak_live") }),
        );
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let direct = DirectComposio::new("ak_live").with_v3_base_for_test(format!("http://{addr}"));
        let err = format!("{:#}", direct.list_toolkits().await.expect_err("401"));
        assert!(
            err.contains("rejected this company's API key"),
            "a rejected key is named as such, not left as a bare status: {err}"
        );
        assert!(
            !err.contains("ak_live"),
            "a body that echoes the key must never reach the caller: {err}"
        );
    }

    // ── The draft-key probe ──────────────────────────────────────────

    /// A mock Composio v3 `/toolkits` that answers `status`, recording the
    /// `x-api-key` it was handed. Returns the base and the recorder.
    async fn spawn_probe_backend(
        status: axum::http::StatusCode,
        body: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        use axum::extract::State;
        use axum::http::HeaderMap;

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route(
                "/toolkits",
                axum::routing::get(
                    move |State(seen): State<Arc<Mutex<Vec<String>>>>, headers: HeaderMap| async move {
                        if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
                            seen.lock().unwrap().push(key.to_string());
                        }
                        (status, body)
                    },
                ),
            )
            .with_state(seen.clone());
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    /// A key Composio accepts probes clean, and the key really is what was
    /// presented — a probe that authenticated as something else would report on
    /// a credential the operator is not about to store.
    #[tokio::test]
    async fn a_key_composio_accepts_probes_clean_and_is_the_key_presented() {
        let (base, seen) = spawn_probe_backend(axum::http::StatusCode::OK, r#"{"items":[]}"#).await;
        probe_at(&base, "ak_not_a_real_key_0123456789")
            .await
            .expect("a 200 is a clean probe");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["ak_not_a_real_key_0123456789"]
        );
    }

    /// A rejected key comes back as a status line that classifies `auth` — and
    /// the raw reason never carries the key, even when the upstream body does.
    #[tokio::test]
    async fn a_rejected_key_classifies_auth_without_echoing_itself() {
        use crate::company::composio_probe::{ComposioProbeClass, classify};

        let (base, _) = spawn_probe_backend(
            axum::http::StatusCode::UNAUTHORIZED,
            r#"{"error":"invalid key ak_not_a_real_key_0123456789"}"#,
        )
        .await;
        let err = probe_at(&base, "ak_not_a_real_key_0123456789")
            .await
            .expect_err("a 401 is not a clean probe");
        assert_eq!(classify(&err), ComposioProbeClass::Auth, "{err}");
        assert!(
            !err.contains("ak_not_a_real_key"),
            "the probe must not carry the key back, even when the body echoes it: {err}"
        );
    }

    /// A gateway in front of Composio is NOT a statement about the key: the
    /// classifier has to see a non-destructive class, or a corporate proxy
    /// deletes a working credential.
    #[tokio::test]
    async fn a_gateway_failure_is_never_read_as_a_bad_key() {
        use crate::company::composio_probe::{ComposioProbeClass, classify};

        let (base, _) =
            spawn_probe_backend(axum::http::StatusCode::BAD_GATEWAY, "<html>proxy</html>").await;
        let err = probe_at(&base, "ak_not_a_real_key_0123456789")
            .await
            .expect_err("a 502 is not a clean probe");
        assert_eq!(classify(&err), ComposioProbeClass::Unknown, "{err}");
        assert!(!classify(&err).is_destructive());
    }

    /// Nothing listening classifies as a connection problem, not a credential
    /// one. Port 0 in a URL is never bound, so this needs no server at all.
    #[tokio::test]
    async fn an_unreachable_host_is_a_connection_problem_not_a_credential_one() {
        use crate::company::composio_probe::classify;

        let err = probe_at("http://127.0.0.1:1/api/v3", "ak_not_a_real_key_0123456789")
            .await
            .expect_err("nothing is listening there");
        assert!(
            !classify(&err).is_destructive(),
            "an unreachable host must never take a key away: {err} -> {}",
            classify(&err)
        );
    }
}
