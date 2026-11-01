use std::collections::HashMap;
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;

const WINDOW: Duration = Duration::from_secs(60);

static STATS: Mutex<Option<HashMap<String, (u64, Duration)>>> = Mutex::new(None);
static REPORTER: Once = Once::new();

pub async fn track(request: Request, next: Next) -> Response {
    REPORTER.call_once(|| {
        tokio::spawn(async {
            let mut tick = tokio::time::interval(WINDOW);
            tick.tick().await;
            loop {
                tick.tick().await;
                report();
            }
        });
    });
    let key = format!(
        "{} {}",
        request.method(),
        request
            .extensions()
            .get::<MatchedPath>()
            .map_or(request.uri().path(), |m| m.as_str())
    );
    let start = Instant::now();
    let response = next.run(request).await;
    record(key, start.elapsed());
    response
}

fn record(key: String, took: Duration) {
    let mut guard = STATS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = guard.get_or_insert_with(HashMap::new).entry(key).or_default();
    entry.0 += 1;
    entry.1 += took;
}

fn report() {
    let taken = STATS.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(map) = taken else { return };
    let total: u64 = map.values().map(|v| v.0).sum();
    let mut rows: Vec<_> = map.into_iter().collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(v.1));
    let top: Vec<String> = rows
        .iter()
        .take(6)
        .map(|(k, (n, t))| format!("{k} n={n} {}ms", t.as_millis()))
        .collect();
    eprintln!("[req-stats] 60s total={total} :: {}", top.join(" | "));
}
