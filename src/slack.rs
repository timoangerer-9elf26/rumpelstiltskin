//! Slack Web API client, shared by the listener and the agent-facing subcommands.

use crate::config::Me;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub type Res<T> = Result<T, String>;

/// `$NAME` from the environment, else `~/.config/rumpel/<file>` (agents read the same file).
pub fn token(name: &str, file: &str) -> Res<String> {
    if let Ok(t) = std::env::var(name) {
        return Ok(t);
    }
    let path = config_dir().join(file);
    std::fs::read_to_string(&path)
        .map(|t| t.trim().to_string())
        .map_err(|_| format!("{name} is not set and {} is missing", path.display()))
}

pub fn config_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/rumpel")
}

pub struct Slack {
    http: reqwest::Client,
    token: String,
}

impl Slack {
    pub fn new(token: String) -> Self {
        Slack { http: reqwest::Client::new(), token }
    }

    pub fn bot() -> Res<Self> {
        Ok(Slack::new(token("SLACK_BOT_TOKEN", "bot-token")?))
    }

    /// Any Web API method with a JSON body; errors when Slack answers `ok: false`.
    pub async fn call(&self, method: &str, body: &Value) -> Res<Value> {
        let req = self.http.post(format!("https://slack.com/api/{method}")).bearer_auth(&self.token).json(body);
        check(method, req).await
    }

    /// Form-encoded call: works for every method, reads included (they ignore JSON bodies).
    /// Non-string values (blocks, files, …) are sent as JSON strings, as Slack expects.
    pub async fn call_form(&self, method: &str, args: &Value) -> Res<Value> {
        let form: Vec<(String, String)> = args
            .as_object()
            .ok_or("arguments must be a JSON object")?
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string())))
            .collect();
        let req = self.http.post(format!("https://slack.com/api/{method}")).bearer_auth(&self.token).form(&form);
        check(method, req).await
    }

    /// Read methods (`conversations.*`, `files.getUploadURLExternal`) take form/query args, not JSON.
    pub async fn get(&self, method: &str, query: &[(&str, &str)]) -> Res<Value> {
        let req = self.http.get(format!("https://slack.com/api/{method}")).bearer_auth(&self.token).query(query);
        check(method, req).await
    }

    pub async fn me(&self) -> Res<Me> {
        let r = self.call("auth.test", &json!({})).await?;
        let field = |k: &str| r[k].as_str().unwrap_or_default().to_string();
        Ok(Me { user: field("user_id"), bot: field("bot_id") })
    }

    /// Returns the new message's ts so callers can thread under or update it.
    pub async fn post(&self, channel: &str, text: &str, thread: Option<&str>) -> Res<String> {
        // markdown_text renders standard Markdown (what agents write) and caps at 12k chars.
        // ponytail: hard cut; split into several posts if agents talk that much.
        let mut text: String = text.chars().take(11_900).collect();
        if text.trim().is_empty() {
            return Err("empty message".into());
        }
        if text.chars().count() == 11_900 {
            text.push_str("\n\n_(truncated)_");
        }
        let mut body = json!({ "channel": channel, "markdown_text": text });
        if let Some(ts) = thread {
            body["thread_ts"] = json!(ts);
        }
        let r = self.call("chat.postMessage", &body).await?;
        Ok(r["ts"].as_str().unwrap_or_default().to_string())
    }

    /// Best effort: cosmetic calls (reactions, loading status) only log failures.
    pub async fn try_call(&self, method: &str, body: Value) {
        if let Err(e) = self.call(method, &body).await {
            eprintln!("rumpel: {e}");
        }
    }

    /// A channel's latest messages or a whole thread, oldest first, as "ts [sender] text" lines.
    pub async fn read(&self, channel: &str, thread: Option<&str>, limit: usize) -> Res<String> {
        let limit = limit.to_string();
        let mut r = match thread {
            Some(ts) => self.get("conversations.replies", &[("channel", channel), ("ts", ts), ("limit", &limit)]).await?,
            None => self.get("conversations.history", &[("channel", channel), ("limit", &limit)]).await?,
        };
        let msgs = r["messages"].as_array_mut().map(std::mem::take).unwrap_or_default();
        let mut lines: Vec<String> = msgs
            .iter()
            .map(|m| {
                let who = m["user"].as_str().or(m["bot_id"].as_str()).unwrap_or("?");
                format!("{} [{who}] {}", m["ts"].as_str().unwrap_or("?"), m["text"].as_str().unwrap_or_default())
            })
            .collect();
        if thread.is_none() {
            lines.reverse(); // history comes newest first
        }
        Ok(lines.join("\n"))
    }

    /// Slack's two-step external upload, shared into `channel` (and thread).
    pub async fn upload(&self, channel: &str, file: &Path, thread: Option<&str>, comment: Option<&str>) -> Res<()> {
        // chat.postMessage turns a user id into the DM; file uploads need the DM's own D… id.
        if channel.starts_with(['U', 'W']) {
            return Err(format!("upload needs a channel or DM id (C…/G…/D…), not the user id {channel}"));
        }
        let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let len = bytes.len().to_string();
        let r = self.get("files.getUploadURLExternal", &[("filename", name), ("length", &len)]).await?;
        let (url, id) = (r["upload_url"].as_str().ok_or("no upload_url")?, r["file_id"].as_str().ok_or("no file_id")?);
        let resp = self.http.post(url).body(bytes).send().await.map_err(|e| format!("upload: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("upload: HTTP {}", resp.status()));
        }
        let mut body = json!({ "files": [{ "id": id, "title": name }], "channel_id": channel });
        if let Some(ts) = thread {
            body["thread_ts"] = json!(ts);
        }
        if let Some(c) = comment {
            body["initial_comment"] = json!(c);
        }
        self.call("files.completeUploadExternal", &body).await.map(|_| ())
    }
}

async fn check(method: &str, req: reqwest::RequestBuilder) -> Res<Value> {
    let resp: Value = req
        .send()
        .await
        .map_err(|e| format!("{method}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("{method}: {e}"))?;
    if resp["ok"] != true {
        return Err(format!("{method}: {}", resp["error"]));
    }
    Ok(resp)
}
