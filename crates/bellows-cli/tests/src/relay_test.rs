use super::*;

#[test]
fn order_waits_only_for_work_begun_before_the_horizon() {
    let (changed, _) = watch::channel(0);
    let order = Order::new(changed);
    let first = order.begin();
    let horizon = order.horizon();
    let second = order.begin();
    assert!(!order.passed(horizon));
    order.finish(second);
    assert!(!order.passed(horizon), "an earlier item is still open");
    order.finish(first);
    assert!(order.passed(horizon));
    assert!(order.passed(order.horizon()));
}

#[test]
fn relay_rejects_a_wrong_session_token() {
    let shared = Shared {
        client: reqwest::Client::new(),
        upstream: "http://127.0.0.1:1".into(),
        upstream_token: None,
        local_token: "abc123".into(),
        spool_dir: PathBuf::new(),
        spool_limit: 0,
        spooled: AtomicU64::new(0),
        spool_files: AtomicU64::new(0),
        uploads: Semaphore::new(1),
        blobs: Mutex::new(HashMap::new()),
        blob_order: Order::new(watch::channel(0).0),
        record_order: Order::new(watch::channel(0).0),
        tracker: TaskTracker::new(),
        stats: Stats::default(),
    };
    let mut headers = HeaderMap::new();
    assert!(!shared.authorized(&headers));
    headers.insert(header::AUTHORIZATION, "Bearer abc124".parse().unwrap());
    assert!(!shared.authorized(&headers));
    headers.insert(header::AUTHORIZATION, "Bearer abc12".parse().unwrap());
    assert!(!shared.authorized(&headers));
    headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
    assert!(shared.authorized(&headers));
}
