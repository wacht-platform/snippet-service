# Notification cursor contract

`GET /notifications` (alias `/notifications/replay`) retains existing authentication.

Time mode query: `since_created_at=<nonnegative Unix seconds>&since_event_id=<0..i64::MAX>&limit=<1..500>`.
`since_event_id` defaults to 0; `limit` defaults to 100. The exclusive lower bound is `(since_created_at, since_event_id)`, ordered lexicographically by server timestamp then event ID. Do not combine with a nonzero legacy `since`. An event-ID-only query without `since_created_at` is invalid.

Response: `{"events":[...],"next_cursor":{"created_at":1700000000,"event_id":123},"has_more":true}`.
The cursor is the last scanned row, including corrupt, expired, and policy-filtered rows. If no row was scanned it equals the request tuple. Each request scans at most 500 rows plus one lookahead; empty events with `has_more=true` must continue. Follow-up pages use both fields of `next_cursor` unchanged. Commit ingested events and the scan cursor atomically; deduplicate by durable event ID/notification ID.

WebSocket `/events` envelopes remain `{"kind":"notification","notification":{...}}`. The saved notification includes the same integer `created_at` and `event_id` as polling. Maintain an observed high-water tuple as the lexicographic maximum of successfully ingested WS event tuples and polling `next_cursor`. The server persists a nondecreasing timestamp watermark across restarts and journal pruning; event IDs break timestamp ties. Server timestamps may be ahead of wall time after clock rollback; clients must not replace them with local time.

On a new recovery run, start at `since_created_at=max(0, observed_cursor.created_at-10800)&since_event_id=0` (three-hour overlap). Do not subtract three hours on each page. Keep the active recovery scan cursor separate from the observed high-water tuple: WS updates must not move an in-flight scan forward or skip earlier missing events. Exhaust `has_more`, even on empty pages. Concurrent WS arrivals can advance observed high-water while overlap polling deduplicates them. With no cursor, use a chosen initial timestamp (e.g. current Unix seconds minus three hours) or 0 to scan all retained history.

Retention remains 24 hours; overlap does not extend retention. Replay applies current notification policy and expiry. Pending idle candidates remain eligible under the existing policy; no idle suppression was introduced.

Compatibility: requests without `since_created_at` retain legacy `since=<event_id>` paging and numeric `next_cursor`. Time-mode responses alone return the tuple. Existing WS payload shape is unchanged.
