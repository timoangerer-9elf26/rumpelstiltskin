use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Slack user id that `from = "owner"` means.
    pub owner: String,
    /// Let agents write to channels the bot is not a member of (needs `chat:write.public`).
    /// Off by default: `post`, `upload` and `api` refuse such channels.
    #[serde(default)]
    pub write_anywhere: bool,
    /// Where thread/channel → Paseo agent ids are kept across restarts.
    pub state_file: Option<PathBuf>,
    #[serde(default)]
    pub paseo: Paseo,
    #[serde(rename = "channel", default)]
    pub channels: Vec<Channel>,
}

/// Defaults for every agent the bot starts.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Paseo {
    /// Path to the paseo CLI.
    pub bin: String,
    /// Daemon endpoint (`--host`); the local daemon when unset.
    pub host: Option<String>,
    pub provider: String,
    pub mode: String,
}

impl Default for Paseo {
    fn default() -> Self {
        Paseo {
            bin: "paseo".into(),
            host: None,
            provider: "claude/sonnet".into(),
            mode: "bypassPermissions".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    /// Slack channel id, "dm" for every direct message to the bot, or "*" for every channel
    /// the bot is in that has no entry of its own.
    pub id: String,
    pub listen: Listen,
    pub from: From,
    /// Senders (user or bot ids) that never trigger, even when `from` allows them.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Repo the agent's new worktree workspace is created from.
    pub repo: Option<PathBuf>,
    #[serde(default)]
    pub session: Session,
    /// Send every trigger to this existing Paseo agent instead.
    pub agent: Option<String>,
    /// Overrides `[paseo] provider` for this channel.
    pub provider: Option<String>,
    /// Slack's "is working" indicator. It only exists inside a thread, so in DMs it opens one.
    #[serde(default = "yes")]
    pub loading: bool,
    /// Where new agents work: a fresh worktree of `repo`, or `repo` itself.
    #[serde(default)]
    pub workspace: Workspace,
    /// Explain `rumpel` in every prompt. Turn off when the repo's AGENTS.md does.
    #[serde(default = "yes")]
    pub hints: bool,
    /// Emoji name (without colons) that triggers on the message it is added to, e.g. "rumpel".
    /// `from` and `ignore` apply to whoever adds it.
    pub reaction: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Listen {
    /// Every message, top-level or in a thread.
    All,
    /// Only messages that @mention the bot.
    Mention,
}

/// Which Paseo agent a trigger goes to.
#[derive(Debug, Deserialize, PartialEq, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Session {
    /// One agent per Slack thread; follow-ups in the thread continue it.
    #[default]
    Thread,
    /// One agent for the whole channel.
    Channel,
    /// A fresh agent for every trigger.
    New,
}

#[derive(Debug, Deserialize, PartialEq, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Workspace {
    /// A new git worktree per agent: isolated, but its files vanish with the branch.
    #[default]
    Worktree,
    /// The checkout itself, so agents can commit memory and skills to its main branch.
    Local,
}

impl Workspace {
    pub fn as_str(self) -> &'static str {
        match self {
            Workspace::Worktree => "worktree",
            Workspace::Local => "local",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum From {
    /// "owner" or "everyone".
    Keyword(Keyword),
    /// Explicit Slack user or bot ids.
    Ids(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Keyword {
    Owner,
    Everyone,
}

/// The subset of a Slack `message` event the filter looks at.
#[derive(Debug, Deserialize)]
pub struct Message {
    pub channel: String,
    pub user: Option<String>,
    #[serde(default)]
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub subtype: Option<String>,
    pub bot_id: Option<String>,
    pub channel_type: Option<String>,
}

fn yes() -> bool {
    true
}

impl Message {
    pub fn is_dm(&self) -> bool {
        self.channel_type.as_deref() == Some("im")
    }

    /// User id for people, bot id for bots that post without a user.
    pub fn sender(&self) -> Option<&str> {
        self.user.as_deref().or(self.bot_id.as_deref())
    }
}

/// The bot's own identity, so it never answers itself.
pub struct Me {
    pub user: String,
    pub bot: String,
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        for ch in &cfg.channels {
            if ch.repo.is_none() && ch.agent.is_none() {
                return Err(format!("channel {} needs `repo` or `agent`", ch.id));
            }
        }
        Ok(cfg)
    }

    /// Returns the channel config when this message should start or continue an agent.
    pub fn should_trigger(&self, me: &Me, msg: &Message) -> Option<&Channel> {
        // Edits, deletes and joins never trigger; posts by other bots do.
        if msg.subtype.as_deref().is_some_and(|s| s != "file_share" && s != "bot_message") {
            return None;
        }
        let sender = msg.sender()?;
        if sender == me.user || msg.bot_id.as_deref() == Some(&me.bot) {
            return None;
        }
        let ch = self.channel(&msg.channel, msg.is_dm())?;
        if ch.listen == Listen::Mention && !msg.text.contains(&format!("<@{}>", me.user)) {
            return None;
        }
        self.allows(ch, sender, [msg.user.as_deref(), msg.bot_id.as_deref()]).then_some(ch)
    }

    /// The channel config when `reactor` adding `:emoji:` in `channel` should trigger.
    pub fn reaction_trigger(&self, me: &Me, channel: &str, reactor: &str, emoji: &str) -> Option<&Channel> {
        if reactor == me.user {
            return None;
        }
        // reaction_added carries no channel_type; DM channel ids start with D.
        let ch = self.channel(channel, channel.starts_with('D'))?;
        (ch.reaction.as_deref() == Some(emoji) && self.allows(ch, reactor, [Some(reactor), None])).then_some(ch)
    }

    /// Exact id first, then "dm" for direct messages or "*" for any other channel the bot is in.
    fn channel(&self, id: &str, dm: bool) -> Option<&Channel> {
        let fallback = if dm { "dm" } else { "*" };
        let find = |want: &str| self.channels.iter().find(|c| c.id == want);
        find(id).or_else(|| find(fallback))
    }

    /// `ignore` first, then `from`; `ids` are the sender's user and bot id.
    fn allows(&self, ch: &Channel, sender: &str, ids: [Option<&str>; 2]) -> bool {
        if ch.ignore.iter().any(|i| ids.contains(&Some(i.as_str()))) {
            return false;
        }
        match &ch.from {
            From::Keyword(Keyword::Everyone) => true,
            From::Keyword(Keyword::Owner) => sender == self.owner,
            From::Ids(list) => list.iter().any(|i| ids.contains(&Some(i.as_str()))),
        }
    }
}

/// Whether agents may write to a channel, given its `conversations.info` entry.
pub fn may_write(write_anywhere: bool, channel: &serde_json::Value) -> bool {
    write_anywhere || channel["is_member"] == true || channel["is_im"] == true || channel["is_mpim"] == true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Me {
        Me { user: "UBOT".into(), bot: "BBOT".into() }
    }

    fn cfg() -> Config {
        toml::from_str(
            r#"
            owner = "UTIMO"
            [[channel]]
            id = "CALL"
            listen = "all"
            from = "everyone"
            ignore = ["UNOISY", "BNOISY"]
            repo = "/r"
            [[channel]]
            id = "CMENTION"
            listen = "mention"
            from = "owner"
            reaction = "rumpel"
            agent = "a1"
            [[channel]]
            id = "CLIST"
            listen = "all"
            from = ["UA", "BCI"]
            repo = "/r"
            [[channel]]
            id = "dm"
            listen = "all"
            from = "owner"
            repo = "/r"
            session = "channel"
            loading = false
            workspace = "local"
            hints = false
            "#,
        )
        .unwrap()
    }

    fn msg(channel: &str, user: &str, text: &str) -> Message {
        Message {
            channel: channel.into(),
            user: Some(user.into()),
            text: text.into(),
            ts: "1.0".into(),
            thread_ts: None,
            subtype: None,
            bot_id: None,
            channel_type: None,
        }
    }

    fn bot_msg(channel: &str, bot: &str) -> Message {
        Message { user: None, bot_id: Some(bot.into()), subtype: Some("bot_message".into()), ..msg(channel, "", "hi") }
    }

    fn fires(c: &Config, m: &Message) -> bool {
        c.should_trigger(&me(), m).is_some()
    }

    #[test]
    fn filters() {
        let c = cfg();
        assert!(fires(&c, &msg("CALL", "UX", "hi")));
        assert!(!fires(&c, &msg("COTHER", "UTIMO", "hi")), "unconfigured channel");
        assert!(!fires(&c, &msg("CMENTION", "UTIMO", "hi")), "no mention");
        assert!(fires(&c, &msg("CMENTION", "UTIMO", "<@UBOT> hi")));
        assert!(!fires(&c, &msg("CMENTION", "UX", "<@UBOT> hi")), "not the owner");
        assert!(fires(&c, &msg("CLIST", "UA", "hi")));
        assert!(!fires(&c, &msg("CLIST", "UX", "hi")));
        assert!(!fires(&c, &msg("CALL", "UNOISY", "hi")), "ignored user");
        let edited = Message { subtype: Some("message_changed".into()), ..msg("CALL", "UX", "hi") };
        assert!(!fires(&c, &edited));
        let file = Message { subtype: Some("file_share".into()), ..msg("CALL", "UX", "f") };
        assert!(fires(&c, &file));
    }

    #[test]
    fn bots_count_as_senders_but_never_itself() {
        let c = cfg();
        assert!(fires(&c, &bot_msg("CALL", "BOTHER")), "other bots trigger");
        assert!(fires(&c, &bot_msg("CLIST", "BCI")), "bot id in from list");
        assert!(!fires(&c, &bot_msg("CLIST", "BOTHER")));
        assert!(!fires(&c, &bot_msg("CALL", "BNOISY")), "ignored bot");
        assert!(!fires(&c, &bot_msg("CALL", "BBOT")), "own bot id");
        assert!(!fires(&c, &msg("CALL", "UBOT", "hi")), "own bot user");
        let app_post = Message { bot_id: Some("BOTHER".into()), ..msg("CALL", "UAPPBOT", "hi") };
        assert!(fires(&c, &app_post), "app bot posting with a user id");
    }

    #[test]
    fn wildcard_covers_channels_without_their_own_entry() {
        let c: Config = toml::from_str(
            r#"
            owner = "UTIMO"
            [[channel]]
            id = "*"
            listen = "mention"
            from = "owner"
            repo = "/r"
            [[channel]]
            id = "CALL"
            listen = "all"
            from = "everyone"
            repo = "/r"
            "#,
        )
        .unwrap();
        assert!(c.should_trigger(&me(), &msg("CNEW", "UTIMO", "<@UBOT> hi")).is_some(), "any channel");
        assert!(c.should_trigger(&me(), &msg("CNEW", "UTIMO", "hi")).is_none(), "wildcard's listen");
        assert!(c.should_trigger(&me(), &msg("CALL", "UX", "hi")).is_some(), "own entry wins");
        let dm = Message { channel_type: Some("im".into()), ..msg("D1", "UTIMO", "<@UBOT> hi") };
        assert!(c.should_trigger(&me(), &dm).is_none(), "DMs need the dm entry");
    }

    #[test]
    fn reactions() {
        let c = cfg();
        let fires = |ch: &str, who: &str, emoji: &str| c.reaction_trigger(&me(), ch, who, emoji).is_some();
        assert!(fires("CMENTION", "UTIMO", "rumpel"), "no mention needed");
        assert!(!fires("CMENTION", "UTIMO", "eyes"), "other emoji");
        assert!(!fires("CMENTION", "UX", "rumpel"), "from = owner applies to the reactor");
        assert!(!fires("CMENTION", "UBOT", "rumpel"), "own reactions");
        assert!(!fires("CALL", "UX", "rumpel"), "channel without reaction");
        assert!(!fires("COTHER", "UTIMO", "rumpel"), "unconfigured channel");
    }

    #[test]
    fn dms() {
        let c = cfg();
        let dm = |user: &str| Message { channel_type: Some("im".into()), ..msg("D123", user, "hi") };
        assert!(fires(&c, &dm("UTIMO")));
        assert!(!fires(&c, &dm("UX")), "dm from = owner");
        assert_eq!(c.should_trigger(&me(), &dm("UTIMO")).unwrap().session, Session::Channel);
        assert!(!c.should_trigger(&me(), &dm("UTIMO")).unwrap().loading, "loading = false");
        assert!(c.should_trigger(&me(), &msg("CALL", "UX", "hi")).unwrap().loading, "loading defaults on");
        let call = c.should_trigger(&me(), &msg("CALL", "UX", "hi")).unwrap();
        assert_eq!((call.workspace, call.hints), (Workspace::Worktree, true), "defaults");
        let d = c.should_trigger(&me(), &dm("UTIMO")).unwrap();
        assert_eq!((d.workspace, d.hints), (Workspace::Local, false));
        assert!(!fires(&c, &msg("D123", "UTIMO", "hi")), "a D… id without channel_type im is not a DM");
        let no_dm: Config = toml::from_str("owner = \"UTIMO\"").unwrap();
        assert!(!fires(&no_dm, &dm("UTIMO")), "DMs ignored without a dm entry");
    }

    #[test]
    fn write_scope() {
        use serde_json::json;
        assert!(may_write(false, &json!({"is_member": true})));
        assert!(!may_write(false, &json!({"is_member": false})));
        assert!(!may_write(false, &json!({})), "unknown membership fails closed");
        assert!(may_write(false, &json!({"is_im": true})));
        assert!(may_write(true, &json!({"is_member": false})));
        assert!(!cfg().write_anywhere, "default is member channels only");
    }

    #[test]
    fn channel_needs_repo_or_agent() {
        let dir = std::env::temp_dir().join("rumpel-test.toml");
        std::fs::write(&dir, "owner = \"U\"\n[[channel]]\nid = \"C\"\nlisten = \"all\"\nfrom = \"owner\"\n").unwrap();
        assert!(Config::load(&dir).unwrap_err().contains("repo"));
    }

    #[test]
    fn example_config_parses() {
        toml::from_str::<Config>(include_str!("../config.example.toml")).unwrap();
        toml::from_str::<Config>(include_str!("../docs/examples/rumpel-config.toml")).unwrap();
    }
}
