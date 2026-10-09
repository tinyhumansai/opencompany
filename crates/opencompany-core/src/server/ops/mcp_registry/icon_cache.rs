//! Directory icons, fetched once per address and inlined as data URIs.
//!
//! A directory page waits for its icons, so a slow icon host must not hold the
//! page: each request waits at most [`ICON_BUDGET`], the fetch keeps running in
//! the background and fills the cache for the next page, concurrent requests
//! for one address share one fetch, and a failed fetch is remembered for
//! [`ICON_NEGATIVE_TTL`] instead of being retried on every page.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};

/// How long a page waits for one icon.
pub(in crate::server::ops) const ICON_BUDGET: Duration = Duration::from_millis(1_500);

/// How long a failed icon address is left alone.
pub(in crate::server::ops) const ICON_NEGATIVE_TTL: Duration = Duration::from_secs(10 * 60);

/// Entries kept before the cache starts over.
const ICON_CACHE_LIMIT: usize = 512;

type PendingIcon = Shared<BoxFuture<'static, Option<String>>>;

enum Slot {
    Ready(String),
    Failed(Instant),
    Pending(PendingIcon),
}

/// The per-process icon cache.
pub(in crate::server::ops) struct IconCache {
    slots: Mutex<HashMap<String, Slot>>,
    budget: Duration,
    negative_ttl: Duration,
}

impl IconCache {
    /// A cache with the given wait budget and failure memory.
    pub(in crate::server::ops) fn new(budget: Duration, negative_ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::new(HashMap::new()),
            budget,
            negative_ttl,
        })
    }

    /// The icon for `url` if it is cached or arrives within the budget.
    /// Must run inside a Tokio runtime: the fetch is spawned so it can outlive
    /// this request.
    pub(in crate::server::ops) async fn get<F, Fut>(
        self: &Arc<Self>,
        url: String,
        fetch: F,
    ) -> Option<String>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Option<String>> + Send + 'static,
    {
        let pending = {
            let mut slots = self.slots.lock().ok()?;
            match slots.get(&url) {
                Some(Slot::Ready(icon)) => return Some(icon.clone()),
                Some(Slot::Failed(at)) if at.elapsed() < self.negative_ttl => return None,
                Some(Slot::Pending(pending)) => pending.clone(),
                _ => {
                    if slots.len() >= ICON_CACHE_LIMIT {
                        slots.retain(|_, slot| matches!(slot, Slot::Pending(_)));
                    }
                    let pending = self.spawn(url.clone(), fetch(url.clone()));
                    slots.insert(url.clone(), Slot::Pending(pending.clone()));
                    pending
                }
            }
        };
        match tokio::time::timeout(self.budget, pending).await {
            Ok(icon) => icon,
            Err(_) => {
                tracing::debug!(
                    "[mcp-registry] icon {url} still loading after {:?}",
                    self.budget
                );
                None
            }
        }
    }

    fn spawn<Fut>(self: &Arc<Self>, url: String, fetch: Fut) -> PendingIcon
    where
        Fut: Future<Output = Option<String>> + Send + 'static,
    {
        let cache = Arc::clone(self);
        let task = tokio::spawn(async move {
            let icon = fetch.await;
            if let Ok(mut slots) = cache.slots.lock() {
                let slot = match &icon {
                    Some(icon) => Slot::Ready(icon.clone()),
                    None => Slot::Failed(Instant::now()),
                };
                slots.insert(url, slot);
            }
            icon
        });
        task.map(|joined| joined.ok().flatten()).boxed().shared()
    }
}

#[cfg(test)]
#[path = "icon_cache_tests.rs"]
mod tests;
