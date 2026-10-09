use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

const URL: &str = "https://icons.example.com/a.png";

fn counted(
    calls: &Arc<AtomicUsize>,
    delay: Duration,
    icon: Option<&'static str>,
) -> impl FnOnce(String) -> BoxFuture<'static, Option<String>> {
    let calls = Arc::clone(calls);
    move |_url| {
        calls.fetch_add(1, Ordering::SeqCst);
        async move {
            tokio::time::sleep(delay).await;
            icon.map(str::to_string)
        }
        .boxed()
    }
}

#[tokio::test(start_paused = true)]
async fn a_fetched_icon_is_served_from_memory_afterwards() {
    let cache = IconCache::new(ICON_BUDGET, ICON_NEGATIVE_TTL);
    let calls = Arc::new(AtomicUsize::new(0));
    let first = cache
        .get(
            URL.into(),
            counted(&calls, Duration::ZERO, Some("data:image/png;base64,AA")),
        )
        .await;
    let second = cache
        .get(URL.into(), counted(&calls, Duration::ZERO, Some("other")))
        .await;
    assert_eq!(first.as_deref(), Some("data:image/png;base64,AA"));
    assert_eq!(second, first);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_slow_icon_does_not_hold_the_page_and_lands_for_the_next_one() {
    let cache = IconCache::new(ICON_BUDGET, ICON_NEGATIVE_TTL);
    let calls = Arc::new(AtomicUsize::new(0));
    let started = tokio::time::Instant::now();
    let first = cache
        .get(
            URL.into(),
            counted(&calls, Duration::from_secs(5), Some("late")),
        )
        .await;
    assert_eq!(first, None);
    assert!(started.elapsed() <= ICON_BUDGET + Duration::from_millis(10));

    tokio::time::sleep(Duration::from_secs(5)).await;
    let second = cache
        .get(URL.into(), counted(&calls, Duration::ZERO, Some("again")))
        .await;
    assert_eq!(second.as_deref(), Some("late"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn concurrent_requests_share_one_fetch() {
    let cache = IconCache::new(ICON_BUDGET, ICON_NEGATIVE_TTL);
    let calls = Arc::new(AtomicUsize::new(0));
    let (a, b) = tokio::join!(
        cache.get(
            URL.into(),
            counted(&calls, Duration::from_millis(100), Some("x"))
        ),
        cache.get(
            URL.into(),
            counted(&calls, Duration::from_millis(100), Some("y"))
        ),
    );
    assert_eq!(a.as_deref(), Some("x"));
    assert_eq!(b.as_deref(), Some("x"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_failed_icon_is_not_retried_until_its_ttl_passes() {
    let cache = IconCache::new(ICON_BUDGET, ICON_NEGATIVE_TTL);
    let calls = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        cache
            .get(URL.into(), counted(&calls, Duration::ZERO, None))
            .await,
        None
    );
    assert_eq!(
        cache
            .get(URL.into(), counted(&calls, Duration::ZERO, Some("x")))
            .await,
        None
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    tokio::time::advance(ICON_NEGATIVE_TTL + Duration::from_secs(1)).await;
    assert_eq!(
        cache
            .get(URL.into(), counted(&calls, Duration::ZERO, Some("x")))
            .await
            .as_deref(),
        Some("x")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
