//! GET /goals and GET /goals/{id}: goals with progress, blockers and the plan,
//! for the app to render.

use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::coordination::goals::{self, attention, progress};
use crate::coordination::{Goal, GoalStatus, Task, TaskStatus};
use crate::store::Store;

use super::{unauthorized, Auth, Shared};

pub fn router() -> Router<Shared> {
    Router::new().route("/goals", get(list)).route("/goals/{id}", get(detail))
}

#[derive(Deserialize)]
struct ListQuery {
    token: Option<String>,
    status: Option<String>,
}

fn summary(goal: &Goal, tasks: &[Task]) -> Value {
    let (done, total) = progress(tasks);
    let in_force = goal.acceptance.iter().filter(|c| c.dropped.is_empty()).count();
    json!({
        "id": goal.id, "title": goal.title, "outcome": goal.outcome, "status": goal.status,
        "requester": goal.requester, "created_at": goal.created_at, "closed_at": goal.closed_at,
        "progress": {"done": done, "total": total},
        "evidence": {"have": in_force - goal.unevidenced().len(), "total": in_force},
        "attention": attention(goal, tasks).map(goals::Attention::as_str),
    })
}

/// A goal with its tasks (slim, in plan order), progress, what is blocking it
/// and the plan.
pub(super) fn detail_view(store: &Store, goal: &Goal) -> Value {
    let tasks = store.goal_tasks(&goal.id).unwrap_or_default();
    let mut out = summary(goal, &tasks);
    let slice = |t: &Task| goals::slice_number(&goal.plan, &t.id);
    let rows: Vec<Value> = tasks
        .iter()
        .map(|t| {
            let sub = store.board_sub_status(t);
            let agent = store
                .list_task_agents(&t.id)
                .unwrap_or_default()
                .into_iter()
                .find(|m| m.removed_at.is_none() && m.role == "implementer")
                .map(|m| m.agent_id);
            json!({
                "id": t.id, "slice": slice(t), "title": t.title, "status": t.status, "session_id": t.session_id,
                "agent": agent, "status_reason": sub.status_reason, "waiting_on": sub.waiting_on,
                "depends_on": store.blockers_of(&t.id).unwrap_or_default(), "updated_at": t.updated_at,
                "result": t.result.as_ref().map(|r| r.summary.clone()),
            })
        })
        .collect();
    let blockers: Vec<Value> = tasks
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Blocked | TaskStatus::InReview))
        .map(|t| {
            let sub = store.board_sub_status(t);
            json!({"task_id": t.id, "title": t.title, "status": t.status, "reason": sub.status_reason, "waiting_on": sub.waiting_on})
        })
        .collect();
    out["acceptance"] = json!(goal.acceptance);
    out["plan"] = json!(goal.plan["slices"].as_array().cloned().unwrap_or_default());
    out["tasks"] = json!(rows);
    out["blockers"] = json!(blockers);
    out
}

async fn list(State(d): State<Shared>, Query(q): Query<ListQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let status = match q.status.as_deref().map(str::trim) {
        None | Some("") | Some("all") => None,
        Some(s) => match GoalStatus::parse(s) {
            Some(parsed) => Some(parsed),
            None => return (StatusCode::BAD_REQUEST, "status must be open, closed, cancelled or all").into_response(),
        },
    };
    match d.store.list_goals(status) {
        Ok(found) => Json(
            found
                .iter()
                .map(|g| summary(g, &d.store.goal_tasks(&g.id).unwrap_or_default()))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

async fn detail(State(d): State<Shared>, Query(a): Query<Auth>, Path(id): Path<String>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match d.store.get_goal(&id) {
        Ok(Some(goal)) => Json(detail_view(&d.store, &goal)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such goal").into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::GoalCriterion;
    use crate::coordination::VerifyMethod;

    #[test]
    fn the_detail_shows_progress_blockers_plan_and_attention() {
        let store = Store::open_in_memory().unwrap();
        let mut goal = Goal::new(
            "g".into(), "Launch".into(), "Live".into(),
            vec![GoalCriterion { criterion: "loads".into(), method: VerifyMethod::Command, evidence: String::new(), dropped: String::new() }],
            "user".into(), "2026-01-01T00:00:00Z".into(),
        );
        goal.plan = json!({"slices": [{"n": 0, "task_id": "a", "title": "A", "depends_on": []}, {"n": 1, "task_id": "b", "title": "B", "depends_on": [0]}]});
        store.create_goal(&goal).unwrap();
        for (id, st) in [("a", TaskStatus::Done), ("b", TaskStatus::Failed), ("c", TaskStatus::Cancelled)] {
            let mut t = Task::filed_by_human(id.into(), id.to_uppercase(), "d".into(), "s".into(), 0, "2026-01-01T00:00:00Z".into());
            t.goal_id = Some("g".into());
            t.status = st;
            store.create_task(&t).unwrap();
        }
        let v = detail_view(&store, &goal);
        assert_eq!(v["progress"], json!({"done": 1, "total": 2}));
        assert_eq!(v["evidence"], json!({"have": 0, "total": 1}));
        assert_eq!(v["attention"], "stalled");
        assert_eq!(v["blockers"][0]["task_id"], "b");
        assert_eq!(v["tasks"][1]["slice"], 1);
        assert_eq!(v["plan"][1]["depends_on"], json!([0]));
        assert_eq!(v["acceptance"][0]["criterion"], "loads");
        assert_eq!(summary(&goal, &[])["progress"], json!({"done": 0, "total": 0}));
    }

    #[test]
    fn task_json_carries_the_goal() {
        let store = Store::open_in_memory().unwrap();
        store.create_goal(&Goal::new("g".into(), "Launch".into(), "Live".into(), vec![], "u".into(), "2026-01-01T00:00:00Z".into())).unwrap();
        let mut t = Task::filed_by_human("a".into(), "A".into(), "d".into(), "s".into(), 0, "2026-01-01T00:00:00Z".into());
        t.goal_id = Some("g".into());
        let fields = crate::task_state_tools::api_fields(&store, &t);
        assert_eq!((fields["goal_id"].as_str(), fields["goal_title"].as_str()), (Some("g"), Some("Launch")));
        t.goal_id = None;
        let fields = crate::task_state_tools::api_fields(&store, &t);
        assert!(fields["goal_id"].is_null() && fields["goal_title"].is_null());
    }
}
