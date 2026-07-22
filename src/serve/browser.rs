//! Connected browser-extension registry and request/response routing.
//!
//! Extensions connect outbound to `/browser/ws`. The daemon keeps the socket
//! private and exposes an authenticated HTTP surface for the future
//! `snippet browser` CLI to list browsers and issue commands.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::ws::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::tools::BrowserSummaryProvider;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DEVICE_NAME_CHARS: usize = 64;

pub fn validate_device_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("device name must not be empty".to_string());
    }
    if name.chars().count() > MAX_DEVICE_NAME_CHARS {
        return Err(format!(
            "device name must be at most {MAX_DEVICE_NAME_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("device name must not contain control characters or newlines".to_string());
    }
    Ok(name.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterMessage {
    #[serde(default)]
    pub browser: String,
    #[serde(default, alias = "deviceName")]
    pub device_name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BrowserInfo {
    #[serde(skip_serializing)]
    pub browser_id: String,
    pub browser: String,
    pub device_name: String,
    pub capabilities: Vec<String>,
    pub connected_at: String,
    pub last_seen: String,
}

struct BrowserConnection {
    info: BrowserInfo,
    outbound: mpsc::UnboundedSender<Message>,
}

type CommandResult = Result<Value, String>;
type Pending = HashMap<String, (String, oneshot::Sender<CommandResult>)>;

#[derive(Default)]
pub struct BrowserManager {
    connections: Mutex<HashMap<String, BrowserConnection>>,
    pending: Mutex<Pending>,
    snapshot: Arc<RwLock<Vec<BrowserInfo>>>,
}

fn render_browser_summary(browsers: &[BrowserInfo]) -> String {
    if browsers.is_empty() {
        return "[browsers]\nconnected = 0\n".to_string();
    }
    let mut out = format!("[browsers]\nconnected = {}\n", browsers.len());
    for browser in browsers.iter().take(5) {
        let device = browser
            .device_name
            .chars()
            .filter(|c| !c.is_control())
            .take(40)
            .collect::<String>();
        let name = browser
            .browser
            .chars()
            .filter(|c| !c.is_control())
            .take(20)
            .collect::<String>();
        out.push_str(&format!("- device = \"{device}\"; browser = \"{name}\"\n"));
    }
    if browsers.len() > 5 {
        out.push_str(&format!(
            "more = \"{} more connected; use `snippet browser list --json` for all\"\n",
            browsers.len() - 5
        ));
    }
    out
}

impl BrowserManager {
    async fn refresh_snapshot(&self) {
        let mut browsers: Vec<_> = self
            .connections
            .lock()
            .await
            .values()
            .map(|connection| connection.info.clone())
            .collect();
        browsers.sort_by(|a, b| a.device_name.cmp(&b.device_name));
        if let Ok(mut snapshot) = self.snapshot.write() {
            *snapshot = browsers;
        }
    }

    pub fn summary_provider(&self) -> BrowserSummaryProvider {
        let snapshot = Arc::clone(&self.snapshot);
        Arc::new(move || {
            let Ok(browsers) = snapshot.read() else {
                return String::new();
            };
            render_browser_summary(&browsers)
        })
    }

    pub async fn register(
        &self,
        registration: RegisterMessage,
        outbound: mpsc::UnboundedSender<Message>,
    ) -> Result<BrowserInfo, String> {
        let device_name = validate_device_name(&registration.device_name)?;
        let now = chrono::Utc::now().to_rfc3339();
        let browser_id = format!("browser-{}", uuid::Uuid::new_v4().simple());
        let mut connections = self.connections.lock().await;
        if connections
            .values()
            .any(|connection| connection.info.device_name == device_name)
        {
            return Err(format!("device name `{device_name}` is already connected"));
        }
        let info = BrowserInfo {
            browser_id: browser_id.clone(),
            browser: if registration.browser.is_empty() {
                "unknown".to_string()
            } else {
                registration.browser
            },
            device_name,
            capabilities: registration.capabilities,
            connected_at: now.clone(),
            last_seen: now,
        };
        connections.insert(
            browser_id,
            BrowserConnection {
                info: info.clone(),
                outbound,
            },
        );
        drop(connections);
        self.refresh_snapshot().await;
        Ok(info)
    }

    pub async fn unregister(&self, browser_id: &str) {
        self.connections.lock().await.remove(browser_id);
        self.refresh_snapshot().await;
        let mut pending = self.pending.lock().await;
        let ids: Vec<String> = pending
            .iter()
            .filter(|(_, (owner, _))| owner == browser_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some((_, waiter)) = pending.remove(&id) {
                let _ = waiter.send(Err(format!("browser `{browser_id}` disconnected")));
            }
        }
    }

    pub async fn touch(&self, browser_id: &str) {
        if let Some(connection) = self.connections.lock().await.get_mut(browser_id) {
            connection.info.last_seen = chrono::Utc::now().to_rfc3339();
        }
        self.refresh_snapshot().await;
    }

    pub async fn list(&self) -> Vec<BrowserInfo> {
        let mut browsers: Vec<_> = self
            .connections
            .lock()
            .await
            .values()
            .map(|connection| connection.info.clone())
            .collect();
        browsers.sort_by(|a, b| a.device_name.cmp(&b.device_name));
        browsers
    }

    pub async fn send_message(&self, browser_id: &str, message: Message) -> bool {
        let outbound = self
            .connections
            .lock()
            .await
            .get(browser_id)
            .map(|connection| connection.outbound.clone());
        outbound.is_some_and(|tx| tx.send(message).is_ok())
    }

    pub async fn send_command_for_device_name(
        &self,
        raw_device_name: &str,
        method: &str,
        args: Value,
    ) -> CommandResult {
        let device_name = validate_device_name(raw_device_name)?;
        let browser_id = {
            let connections = self.connections.lock().await;
            connections
                .values()
                .find(|connection| connection.info.device_name == device_name)
                .map(|connection| connection.info.browser_id.clone())
        };
        let Some(browser_id) = browser_id else {
            return Err(format!("no connected browser named `{device_name}`"));
        };
        self.send_command_internal(&browser_id, &device_name, method, args)
            .await
    }

    async fn send_command_internal(
        &self,
        browser_id: &str,
        display_name: &str,
        method: &str,
        args: Value,
    ) -> CommandResult {
        let (outbound, request_id) = {
            let connections = self.connections.lock().await;
            let Some(connection) = connections.get(browser_id) else {
                return Err(format!("browser `{display_name}` is no longer connected"));
            };
            (
                connection.outbound.clone(),
                uuid::Uuid::new_v4().simple().to_string(),
            )
        };
        let (waiter, receiver) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(request_id.clone(), (browser_id.to_string(), waiter));
        let message = Message::Text(
            json!({
                "type": "command",
                "id": request_id,
                "method": method,
                "args": args,
            })
            .to_string()
            .into(),
        );
        if outbound.send(message).is_err() {
            self.pending.lock().await.remove(&request_id);
            return Err(format!("browser `{display_name}` disconnected"));
        }
        match tokio::time::timeout(COMMAND_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("browser `{display_name}` command waiter dropped")),
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                Err(format!("browser `{display_name}` command timed out"))
            }
        }
    }

    pub async fn complete(
        &self,
        request_id: &str,
        ok: bool,
        result: Option<Value>,
        error: Option<String>,
    ) {
        let waiter = self.pending.lock().await.remove(request_id);
        let Some((_, waiter)) = waiter else { return };
        let value = if ok {
            Ok(result.unwrap_or(Value::Null))
        } else {
            Err(error.unwrap_or_else(|| "browser command failed".to_string()))
        };
        let _ = waiter.send(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn validates_device_names_at_registration_boundary() {
        assert_eq!(validate_device_name("  MacBook  ").unwrap(), "MacBook");
        assert!(validate_device_name("").is_err());
        assert!(validate_device_name(" \t ").is_err());
        assert!(validate_device_name("line\nname").is_err());
        assert!(validate_device_name(&"x".repeat(65)).is_err());
        assert_eq!(
            validate_device_name("日本語ブラウザ").unwrap(),
            "日本語ブラウザ"
        );
    }

    #[tokio::test]
    async fn rejects_duplicate_device_names_before_admission() {
        let manager = BrowserManager::default();
        let (first_tx, _) = mpsc::unbounded_channel();
        manager
            .register(
                RegisterMessage {
                    browser: "chrome".to_string(),
                    device_name: "same device".to_string(),
                    capabilities: Vec::new(),
                },
                first_tx,
            )
            .await
            .expect("first registration");
        let (second_tx, _) = mpsc::unbounded_channel();
        let error = manager
            .register(
                RegisterMessage {
                    browser: "firefox".to_string(),
                    device_name: " same device ".to_string(),
                    capabilities: Vec::new(),
                },
                second_tx,
            )
            .await
            .expect_err("duplicate registration");
        assert!(error.contains("already connected"));
        assert_eq!(manager.list().await.len(), 1);
    }

    #[tokio::test]
    async fn resolves_commands_by_device_name() {
        let manager = Arc::new(BrowserManager::default());
        let (outbound, mut received) = mpsc::unbounded_channel();
        manager
            .register(
                RegisterMessage {
                    browser: "firefox".to_string(),
                    device_name: "named device".to_string(),
                    capabilities: Vec::new(),
                },
                outbound,
            )
            .await
            .expect("registration");
        let manager_for_command = Arc::clone(&manager);
        let command = tokio::spawn(async move {
            manager_for_command
                .send_command_for_device_name(" named device ", "tabs.query", json!({}))
                .await
        });
        let message = received.recv().await.expect("command message");
        let value: Value = match message {
            Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
            other => panic!("unexpected message: {other:?}"),
        };
        let request_id = value["id"].as_str().expect("request id");
        manager
            .complete(request_id, true, Some(json!([])), None)
            .await;
        assert_eq!(command.await.unwrap().unwrap(), json!([]));
    }
    #[test]
    fn live_summary_limits_entries_and_reports_remaining_count() {
        let browsers: Vec<BrowserInfo> = (0..7)
            .map(|index| BrowserInfo {
                browser_id: format!("browser-{index}"),
                browser: "firefox".to_string(),
                device_name: format!("device-{index}"),
                capabilities: Vec::new(),
                connected_at: "now".to_string(),
                last_seen: "now".to_string(),
            })
            .collect();
        let summary = render_browser_summary(&browsers);

        assert_eq!(summary.matches("- device =").count(), 5);
        assert!(summary.contains("connected = 7"));
        assert!(summary.contains("2 more connected; use `snippet browser list --json` for all"));
        assert!(!summary.contains("device-5"));
    }

    #[tokio::test]
    async fn routes_command_result_to_the_waiting_request() {
        let manager = Arc::new(BrowserManager::default());
        let (outbound, mut received) = mpsc::unbounded_channel();
        let info = manager
            .register(
                RegisterMessage {
                    browser: "chrome".to_string(),
                    device_name: "test browser".to_string(),
                    capabilities: vec!["tabs.query".to_string()],
                },
                outbound,
            )
            .await
            .expect("registration");

        let manager_for_command = Arc::clone(&manager);
        let browser_id = info.browser_id.clone();
        let command = tokio::spawn(async move {
            manager_for_command
                .send_command_internal(
                    &browser_id,
                    "test browser",
                    "tabs.query",
                    json!({"active": true}),
                )
                .await
        });
        let message = received.recv().await.expect("command message");
        let value: Value = match message {
            Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
            other => panic!("unexpected message: {other:?}"),
        };
        assert_eq!(value["type"], "command");
        assert_eq!(value["method"], "tabs.query");
        let request_id = value["id"].as_str().unwrap();
        manager
            .complete(request_id, true, Some(json!([{"id": 7}])), None)
            .await;
        assert_eq!(command.await.unwrap().unwrap(), json!([{"id": 7}]));

        assert_eq!(manager.list().await.len(), 1);
        manager.unregister(&info.browser_id).await;
        assert!(manager.list().await.is_empty());
    }
}
