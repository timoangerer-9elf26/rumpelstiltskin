mod config;
mod paseo;
mod setup;
mod slack;

use clap::{Parser, Subcommand};
use config::{Config, Me, Message, Session};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use slack::{Res, Slack};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as Ws;

/// Slack bot for Paseo agents: `run` hands matching messages to agents; the other
/// commands let agents read and write Slack as the bot.
///
/// Tokens: SLACK_BOT_TOKEN (xoxb-) and, for `run`, SLACK_APP_TOKEN (xapp-), from the
/// environment or ~/.config/rumpel/bot-token and app-token.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Listen via Socket Mode and start or continue a Paseo agent per trigger.
    Run {
        /// Default: ~/.config/rumpel/config.toml
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Validate the config and both tokens.
    Check {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Post a message (Markdown) as the bot; prints its ts.
    Post {
        channel: String,
        /// Message text; read from stdin when omitted.
        text: Option<String>,
        /// Reply in this thread (its parent's ts).
        #[arg(long)]
        thread: Option<String>,
    },
    /// Print a channel's latest messages or a whole thread as "ts [sender] text" lines.
    Read {
        channel: String,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Upload a file into a channel or thread.
    Upload {
        channel: String,
        file: PathBuf,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        comment: Option<String>,
    },
    /// Call any Slack Web API method with JSON arguments (argument or stdin); prints the response.
    /// Examples: chat.update, reactions.add, chat.delete, users.info, conversations.list.
    Api { method: String, json: Option<String> },
    /// Print the Slack app manifest for a bot with this name; paste it at api.slack.com/apps.
    Manifest {
        #[arg(long)]
        name: String,
    },
    /// Interactive first-time setup: tokens, config.toml, home repo, service. Safe to rerun.
    Setup,
    /// Create an agent home repo from the built-in template (AGENTS.md, CLAUDE.md, memory/,
    /// .agents/skills/ with .claude/skills linked to it), then customize its AGENTS.md.
    /// Existing files are left alone.
    Init {
        dir: PathBuf,
        /// The agent's name, e.g. Rumpel.
        #[arg(long)]
        name: String,
    },
}

/// The home repo template, embedded so `init` works without this source tree.
const TEMPLATE: [(&str, &str); 4] = [
    ("AGENTS.md", include_str!("../template/AGENTS.md")),
    ("CLAUDE.md", include_str!("../template/CLAUDE.md")),
    ("memory/MEMORY.md", include_str!("../template/memory/MEMORY.md")),
    (".agents/skills/README.md", include_str!("../template/.agents/skills/README.md")),
];

fn init(dir: &std::path::Path, name: &str) -> Res<()> {
    for (rel, content) in TEMPLATE {
        let path = dir.join(rel);
        if path.exists() {
            println!("kept    {}", path.display());
            continue;
        }
        std::fs::create_dir_all(path.parent().expect("has parent")).map_err(|e| e.to_string())?;
        std::fs::write(&path, content.replace("{{name}}", name)).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("created {}", path.display());
    }
    // One skills folder for every agent: Codex reads .agents/skills, Claude Code .claude/skills.
    let link = dir.join(".claude/skills");
    if link.symlink_metadata().is_err() {
        std::fs::create_dir_all(dir.join(".claude")).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink("../.agents/skills", &link).map_err(|e| format!("{}: {e}", link.display()))?;
        println!("linked  {} -> ../.agents/skills", link.display());
    }
    if !dir.join(".git").exists() {
        let ok = std::process::Command::new("git").arg("init").arg("-q").arg(dir).status().map_err(|e| e.to_string())?;
        if !ok.success() {
            return Err("git init failed".into());
        }
    }
    println!(
        "\nNext: fill in the Context section of AGENTS.md, commit, push, and point a channel at it:\n  \
         repo = \"{}\"\n  workspace = \"local\"\n  hints = false",
        dir.display()
    );
    Ok(())
}

#[tokio::main]
async fn main() {
    let result = match Cli::parse().cmd {
        Cmd::Run { config } => run(config_path(config)).await,
        Cmd::Check { config } => check(config_path(config)).await,
        Cmd::Post { channel, text, thread } => async {
            guard_write(&channel).await?;
            let ts = Slack::bot()?.post(&channel, &arg_or_stdin(text)?, thread.as_deref()).await?;
            println!("{ts}");
            Ok(())
        }
        .await,
        Cmd::Read { channel, thread, limit } => async {
            println!("{}", Slack::bot()?.read(&channel, thread.as_deref(), limit).await?);
            Ok(())
        }
        .await,
        Cmd::Upload { channel, file, thread, comment } => async {
            guard_write(&channel).await?;
            Slack::bot()?.upload(&channel, &file, thread.as_deref(), comment.as_deref()).await
        }
        .await,
        Cmd::Init { dir, name } => init(&dir, &name),
        Cmd::Manifest { name } => {
            print!("{}", include_str!("../manifest.yaml").replace("{{name}}", &name));
            Ok(())
        }
        Cmd::Setup => setup::setup().await,
        Cmd::Api { method, json } => async {
            let body: Value = serde_json::from_str(&arg_or_stdin(json)?).map_err(|e| format!("json: {e}"))?;
            // Writes name their target as `channel` (chat.*, reactions.*) or `channel_id` (files).
            let writes = !method.starts_with("conversations.") && !method.starts_with("users.");
            if let (true, Some(ch)) = (writes, body["channel"].as_str().or(body["channel_id"].as_str())) {
                guard_write(ch).await?;
            }
            let r = Slack::bot()?.call_form(&method, &body).await?;
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
            Ok(())
        }
        .await,
    };
    if let Err(e) = result {
        eprintln!("rumpel: {e}");
        std::process::exit(1);
    }
}

/// Enforces `write_anywhere = false` (the default) for agent writes.
/// ponytail: a policy for cooperative agents, not a sandbox; the token itself can post wherever its scopes allow.
async fn guard_write(channel: &str) -> Res<()> {
    let path = config_path(None);
    let anywhere = path.exists() && Config::load(&path)?.write_anywhere;
    // DMs (D…) and user ids (U…/W…, which chat.postMessage turns into the bot's DM with that
    // person) are not channels the bot is "added to"; conversations.info on them needs im:read.
    if anywhere || channel.starts_with(['D', 'U', 'W']) {
        return Ok(());
    }
    let info = Slack::bot()?.get("conversations.info", &[("channel", channel)]).await?;
    if config::may_write(false, &info["channel"]) {
        Ok(())
    } else {
        Err(format!("the bot is not a member of {channel}; invite it or set write_anywhere = true"))
    }
}

fn arg_or_stdin(arg: Option<String>) -> Res<String> {
    match arg {
        Some(a) => Ok(a),
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
            Ok(s)
        }
    }
}

fn config_path(arg: Option<PathBuf>) -> PathBuf {
    arg.unwrap_or_else(|| slack::config_dir().join("config.toml"))
}

async fn check(path: PathBuf) -> Res<()> {
    let cfg = Config::load(&path)?;
    println!("config ok: {} channel(s), owner {}", cfg.channels.len(), cfg.owner);
    println!("bot token ok: bot user {}", Slack::bot()?.me().await?.user);
    let app = Slack::new(slack::token("SLACK_APP_TOKEN", "app-token")?);
    app.call("apps.connections.open", &json!({})).await?;
    println!("app token ok: Socket Mode available");
    Ok(())
}

struct Bot {
    slack: Slack,
    cfg: Config,
    me: Me,
    /// Session key ("channel" or "channel thread_ts") → Paseo agent id.
    sessions: Mutex<HashMap<String, String>>,
    state_file: PathBuf,
}

async fn run(path: PathBuf) -> Res<()> {
    let cfg = Config::load(&path)?;
    let app = Slack::new(slack::token("SLACK_APP_TOKEN", "app-token")?);
    let slack = Slack::bot()?;
    let me = slack.me().await?;
    let state_file = cfg.state_file.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state/rumpel/sessions.json")
    });
    let sessions = match std::fs::read_to_string(&state_file) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", state_file.display()))?,
        Err(_) => HashMap::new(),
    };
    eprintln!("rumpel: running as {}, {} channel(s)", me.user, cfg.channels.len());
    let bot = Arc::new(Bot { slack, cfg, me, sessions: Mutex::new(sessions), state_file });
    // ponytail: in-memory dedupe of the last 1000 triggers; lost on restart.
    let mut seen = (HashSet::new(), VecDeque::new());
    loop {
        if let Err(e) = listen(&bot, &app, &mut seen).await {
            // A bad or revoked token will not heal by retrying; let the supervisor see the failure.
            const FATAL: [&str; 5] = ["invalid_auth", "not_authed", "token_revoked", "account_inactive", "not_allowed_token_type"];
            if FATAL.iter().any(|f| e.contains(f)) {
                return Err(e);
            }
            eprintln!("rumpel: connection lost: {e}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

type Seen = (HashSet<String>, VecDeque<String>);

/// Records a trigger key; false when it was already seen.
fn first_time(seen: &mut Seen, key: String) -> bool {
    if !seen.0.insert(key.clone()) {
        return false;
    }
    seen.1.push_back(key);
    if seen.1.len() > 1000 {
        let old = seen.1.pop_front().expect("non-empty");
        seen.0.remove(&old);
    }
    true
}

/// One Socket Mode connection; returns when Slack closes or asks us to reconnect.
async fn listen(bot: &Arc<Bot>, app: &Slack, seen: &mut Seen) -> Res<()> {
    let open = app.call("apps.connections.open", &json!({})).await?;
    let url = open["url"].as_str().ok_or("no socket url")?;
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.map_err(|e| e.to_string())?;
    eprintln!("rumpel: connected");
    while let Some(frame) = ws.next().await {
        let text = match frame.map_err(|e| e.to_string())? {
            Ws::Text(t) => t,
            Ws::Ping(p) => {
                ws.send(Ws::Pong(p)).await.map_err(|e| e.to_string())?;
                continue;
            }
            Ws::Close(_) => return Ok(()),
            _ => continue,
        };
        let env: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        // Ack first: Slack redelivers unacked envelopes after 3 s, and agent runs take longer.
        if let Some(id) = env["envelope_id"].as_str() {
            let ack = json!({ "envelope_id": id }).to_string();
            ws.send(Ws::Text(ack.into())).await.map_err(|e| e.to_string())?;
        }
        match env["type"].as_str() {
            Some("disconnect") => return Ok(()),
            Some("events_api") => {}
            _ => continue,
        }
        let event = &env["payload"]["event"];
        let bot = bot.clone();
        match event["type"].as_str() {
            Some("message") => {
                let Ok(msg) = serde_json::from_value::<Message>(event.clone()) else { continue };
                if bot.cfg.should_trigger(&bot.me, &msg).is_none() || !first_time(seen, format!("{} {}", msg.channel, msg.ts)) {
                    continue;
                }
                tokio::spawn(async move {
                    let ch = bot.cfg.should_trigger(&bot.me, &msg).expect("checked above");
                    run_trigger(&bot, ch, msg).await
                });
            }
            Some("reaction_added") if event["item"]["type"] == "message" => {
                let field = |v: &Value| v.as_str().unwrap_or_default().to_string();
                let (channel, ts) = (field(&event["item"]["channel"]), field(&event["item"]["ts"]));
                let (reactor, emoji) = (field(&event["user"]), field(&event["reaction"]));
                if bot.cfg.reaction_trigger(&bot.me, &channel, &reactor, &emoji).is_none()
                    || !first_time(seen, format!("{channel} {ts} :{emoji}: {reactor}"))
                {
                    continue;
                }
                tokio::spawn(async move {
                    let ch = bot.cfg.reaction_trigger(&bot.me, &channel, &reactor, &emoji).expect("checked above");
                    match reacted_message(&bot.slack, &channel, &ts, &reactor, &emoji).await {
                        Ok(msg) => run_trigger(&bot, ch, msg).await,
                        Err(e) => eprintln!("rumpel: reaction {channel} {ts}: {e}"),
                    }
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// A reaction trigger as a message from the reactor: the reacted message, quoted, in its own place.
async fn reacted_message(slack: &Slack, channel: &str, ts: &str, reactor: &str, emoji: &str) -> Res<Message> {
    let r = slack.get("conversations.replies", &[("channel", channel), ("ts", ts), ("limit", "1")]).await?;
    let m = r["messages"].as_array().and_then(|a| a.first()).ok_or("reacted message not found")?;
    let author = m["user"].as_str().or(m["bot_id"].as_str()).unwrap_or("?");
    let quoted: String = m["text"].as_str().unwrap_or_default().lines().map(|l| format!("> {l}\n")).collect();
    Ok(Message {
        channel: channel.into(),
        user: Some(reactor.into()),
        text: format!("added :{emoji}: to this message by <@{author}>:\n{quoted}"),
        ts: ts.into(),
        // A thread parent's thread_ts is its own ts; only replies sit in another message's thread.
        thread_ts: m["thread_ts"].as_str().filter(|t| *t != ts).map(String::from),
        subtype: None,
        bot_id: None,
        channel_type: channel.starts_with('D').then(|| "im".into()),
    })
}

/// :eyes: while the agent works, a warning in Slack if starting or running it fails.
async fn run_trigger(bot: &Bot, ch: &config::Channel, msg: Message) {
    let (channel, ts) = (msg.channel.as_str(), msg.ts.as_str());
    let eyes = json!({ "channel": channel, "timestamp": ts, "name": "eyes" });
    bot.slack.try_call("reactions.add", eyes.clone()).await;
    if let Err(e) = handle(bot, ch, &msg).await {
        eprintln!("rumpel: {channel} {ts}: {e}");
        // A top-level DM gets a top-level warning; everything else stays in its thread.
        let thread = msg.thread_ts.as_deref().or((!msg.is_dm()).then_some(ts));
        if let Err(e) = bot.slack.post(channel, &format!(":warning: {e}"), thread).await {
            eprintln!("rumpel: warning post failed: {e}");
        }
    }
    bot.slack.try_call("reactions.remove", eyes).await;
}

/// What the agent reads before every Slack message: where it came from and, with `hints`,
/// how to answer. Without hints the repo's AGENTS.md explains `rumpel`.
fn prompt(msg: &Message, thread: &str, hints: bool) -> String {
    let ch = &msg.channel;
    // Naming a thread for a top-level message made agents reply in one.
    let place = match (&msg.thread_ts, msg.is_dm()) {
        (Some(t), _) => format!("{ch}, in thread {t}"),
        (None, true) => format!("DM {ch}, directly in the chat (no thread; message {})", msg.ts),
        (None, false) => format!("{ch}, directly in the channel (message {})", msg.ts),
    };
    let head = format!("Slack message from <@{}> in {place}:\n\n{}", msg.sender().unwrap_or("?"), msg.text);
    if !hints {
        return format!("{head}\n\n---\nReply with `rumpel` as described in AGENTS.md.");
    }
    format!("{head}\n\n---\n{}", tool_hints(msg, thread))
}

fn tool_hints(msg: &Message, thread: &str) -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "rumpel".into());
    let ch = &msg.channel;
    let in_thread = format!("`{exe} post {ch} --thread {thread} <<'EOF'` … `EOF`");
    let top_level = format!("`{exe} post {ch} <<'EOF'` … `EOF`");
    // Agents copy the first ready-made command, so the default reply form goes first.
    let replies = if msg.is_dm() && msg.thread_ts.is_none() {
        format!(
            "- Reply (default in DMs, a new message): {top_level}\n\
             - Reply in a thread when it helps (long details, several progress posts): {in_thread}"
        )
    } else {
        format!("- Reply in the thread: {in_thread}\n- As a new message in the channel: {top_level}")
    };
    format!(
        "The bot posts nothing for you. You decide whether, when, how often and where you reply, \
         also proactively in other channels. Tool: `{exe}` (see `{exe} --help`), text is Markdown, `post` prints the ts:\n\
         {replies}\n\
         - Another channel: its id instead of {ch}; a DM to a person: their user id\n\
         - Read the thread/channel: `{exe} read {ch} --thread {thread}`\n\
         - File: `{exe} upload {ch} <file> --thread {thread}`\n\
         - Anything else (chat.update, reactions.add, …): `{exe} api <method> '<json>'`"
    )
}

/// Hands one trigger to its Paseo agent and shows Slack's loading indicator until the turn ends.
async fn handle(bot: &Bot, ch: &config::Channel, msg: &Message) -> Res<()> {
    let thread = msg.thread_ts.as_deref().unwrap_or(&msg.ts);
    eprintln!("rumpel: trigger {} {} from {:?}", msg.channel, msg.ts, msg.sender());
    let prompt = prompt(msg, thread, ch.hints);
    let p = &bot.cfg.paseo;
    let agent = if let Some(fixed) = &ch.agent {
        paseo::send(p, fixed, &prompt).await?;
        fixed.clone()
    } else {
        let key = match ch.session {
            Session::Thread => Some(format!("{} {thread}", msg.channel)),
            Session::Channel => Some(msg.channel.clone()),
            Session::New => None,
        };
        // Held across lookup and creation so two quick triggers in one thread share one agent.
        let mut sessions = bot.sessions.lock().await;
        let existing = key.as_ref().and_then(|k| sessions.get(k)).cloned();
        let continued = match existing {
            Some(id) => match paseo::send(p, &id, &prompt).await {
                Ok(()) => Some(id),
                Err(e) => {
                    eprintln!("rumpel: agent {id} gone ({e}), starting a new one");
                    None
                }
            },
            None => None,
        };
        match continued {
            Some(id) => id,
            None => {
                // A new agent mid-thread gets the thread so far as context.
                let context = match msg.thread_ts.as_deref() {
                    Some(t) => bot.slack.read(&msg.channel, Some(t), 200).await.unwrap_or_default(),
                    None => String::new(),
                };
                let full = if context.is_empty() { prompt } else { format!("Thread so far:\n{context}\n\n{prompt}") };
                let repo = ch.repo.as_ref().expect("validated: repo or agent");
                let provider = ch.provider.as_deref().unwrap_or(&p.provider);
                let title = format!("[slack] {}", msg.text.chars().take(50).collect::<String>());
                let env = [("SLACK_CHANNEL", msg.channel.as_str()), ("SLACK_THREAD_TS", thread)];
                let id = paseo::create(p, provider, ch.workspace.as_str(), repo, &title, &env, &full).await?;
                if let Some(k) = key {
                    sessions.insert(k, id.clone());
                    save(&bot.state_file, &sessions)?;
                }
                id
            }
        }
    };
    let result = if ch.loading {
        // Slack drops `processing` after an hour, so it is renewed until the agent stops.
        let status = |s: &str| json!({ "channel_id": msg.channel, "thread_ts": thread, "status": s });
        let wait = paseo::wait(p, &agent);
        tokio::pin!(wait);
        let result = loop {
            bot.slack.try_call("agents.sessions.setStatus", status("processing")).await;
            tokio::select! {
                r = &mut wait => break r,
                _ = tokio::time::sleep(Duration::from_secs(50 * 60)) => {}
            }
        };
        bot.slack.try_call("agents.sessions.setStatus", status("active")).await;
        result
    } else {
        paseo::wait(p, &agent).await
    };
    match result?.as_str() {
        "idle" | "completed" => Ok(()),
        "permission" => Err(format!("Agent `{agent}` is waiting for a permission in Paseo.")),
        other => Err(format!("Agent `{agent}` ended with status `{other}`.")),
    }
}

fn save(path: &std::path::Path, sessions: &HashMap<String, String>) -> Res<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let raw = serde_json::to_string_pretty(sessions).map_err(|e| e.to_string())?;
    std::fs::write(path, raw).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(channel_type: Option<&str>, thread_ts: Option<&str>) -> Message {
        Message {
            channel: "D1".into(),
            user: Some("U1".into()),
            text: "hi".into(),
            ts: "2.0".into(),
            thread_ts: thread_ts.map(String::from),
            subtype: None,
            bot_id: None,
            channel_type: channel_type.map(String::from),
        }
    }

    #[test]
    fn init_fills_name_and_keeps_existing_files() {
        let dir = std::env::temp_dir().join(format!("rumpel-init-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        std::fs::write(dir.join("memory/MEMORY.md"), "mine").unwrap();
        init(&dir, "Rumpel").unwrap();
        let agents = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert!(agents.starts_with("# Rumpel") && !agents.contains("{{name}}"));
        assert_eq!(std::fs::read_to_string(dir.join("memory/MEMORY.md")).unwrap(), "mine");
        assert!(dir.join(".git").exists() && dir.join(".agents/skills/README.md").exists());
        assert!(dir.join(".claude/skills").symlink_metadata().unwrap().file_type().is_symlink());
        assert!(dir.join(".claude/skills/README.md").exists(), "symlink resolves");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The first ready-made `post` command is the one agents use.
    fn first_post(p: &str) -> &str {
        p.lines().find(|l| l.contains(" post ")).unwrap()
    }

    #[test]
    fn default_reply_form() {
        let dm = prompt(&msg(Some("im"), None), "2.0", true);
        assert!(!first_post(&dm).contains("--thread"), "top-level DM");
        assert!(dm.lines().next().unwrap().contains("no thread"), "header must not call it a thread");
        assert!(first_post(&prompt(&msg(Some("im"), Some("1.0")), "1.0", true)).contains("--thread"), "DM thread");
        assert!(first_post(&prompt(&msg(Some("channel"), None), "2.0", true)).contains("--thread"), "channel");
        let bare = prompt(&msg(Some("im"), None), "2.0", false);
        assert!(bare.contains("no thread") && !bare.contains(" post "), "hints off keeps only the header");
    }
}
