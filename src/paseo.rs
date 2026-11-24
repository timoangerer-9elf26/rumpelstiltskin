//! Thin wrapper over the `paseo` CLI: the bot never talks to the daemon directly.

use crate::config::Paseo;
use serde_json::Value;
use std::path::Path;

type Res<T> = Result<T, String>;

async fn paseo(p: &Paseo, args: &[&str]) -> Res<Value> {
    let mut cmd = tokio::process::Command::new(&p.bin);
    if let Some(host) = &p.host {
        cmd.args(["--host", host]);
    }
    // The agent inherits nothing Slack-related from the bot.
    cmd.args(args).env_remove("SLACK_BOT_TOKEN").env_remove("SLACK_APP_TOKEN");
    let out = cmd.output().await.map_err(|e| format!("{}: {e}", p.bin))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Human hints ("Created workspace …") precede the JSON on stdout.
    let json = stdout.find('{').map(|i| &stdout[i..]).unwrap_or("");
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("paseo {}: {}{}", args[0], err.trim(), json.trim()));
    }
    serde_json::from_str(json).map_err(|e| format!("paseo {}: {e}", args[0]))
}

/// Starts an agent in a new worktree workspace of `repo`; returns its id without waiting.
pub async fn create(
    p: &Paseo,
    provider: &str,
    workspace: &str,
    repo: &Path,
    title: &str,
    env: &[(&str, &str)],
    prompt: &str,
) -> Res<String> {
    let repo = repo.to_string_lossy();
    let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut args = vec!["run", "--no-wait", "--json", "--provider", provider, "--mode", &p.mode];
    args.extend(["--new-workspace", workspace, "--cwd", &repo, "--title", title]);
    for e in &env {
        args.extend(["--env", e]);
    }
    args.push(prompt);
    let r = paseo(p, &args).await?;
    r["agentId"].as_str().map(String::from).ok_or_else(|| format!("paseo run: no agentId in {r}"))
}

/// Queues a follow-up; steers a running turn instead of interrupting it.
pub async fn send(p: &Paseo, agent: &str, prompt: &str) -> Res<()> {
    paseo(p, &["send", "--no-wait", "--json", "--behavior", "steer", agent, prompt]).await.map(|_| ())
}

/// Blocks until the agent stops working; returns its status (idle, permission, error, …).
/// A failed wait says nothing about the agent: a busy daemon misses its liveness check
/// (WAIT_FAILED) during long runs while the agent keeps working. So it waits again and
/// gives up only after the daemon stayed unreachable for about five minutes.
pub async fn wait(p: &Paseo, agent: &str) -> Res<String> {
    const RETRIES: u32 = 20;
    let mut failures = 0;
    loop {
        match paseo(p, &["wait", "--json", agent]).await {
            Ok(r) => return Ok(r["status"].as_str().unwrap_or("unknown").to_string()),
            Err(e) if failures < RETRIES => {
                failures += 1;
                eprintln!("rumpel: wait for {agent} failed ({failures}/{RETRIES}), retrying: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn wait_survives_liveness_timeouts() {
        let dir = std::env::temp_dir().join(format!("rumpel-wait-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("paseo");
        // Fails twice like the busy daemon did, then reports the agent idle.
        let script = format!(
            "#!/bin/sh\nn=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}\n\
             if [ $n -le 2 ]; then echo '{{\"error\":{{\"code\":\"WAIT_FAILED\"}}}}'; exit 1; fi\n\
             echo '{{\"status\":\"idle\"}}'\n",
            c = dir.join("count").display()
        );
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let p = Paseo { bin: bin.display().to_string(), ..Paseo::default() };
        assert_eq!(wait(&p, "a1").await.unwrap(), "idle");
        assert_eq!(std::fs::read_to_string(dir.join("count")).unwrap().trim(), "3");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Real daemon round trip: `cargo test -- --ignored live_round_trip` (costs a few cents of Haiku).
    #[tokio::test]
    #[ignore]
    async fn live_round_trip() {
        let p = Paseo { mode: "bypassPermissions".into(), ..Paseo::default() };
        let repo = std::env::var("RUMPEL_TEST_REPO").expect("RUMPEL_TEST_REPO=<git checkout>");
        let env = [("SLACK_CHANNEL", "CTEST")];
        let id = create(&p, "claude/haiku", "worktree", Path::new(&repo), "[slack] live test", &env, "Reply only with the value of $SLACK_CHANNEL.")
            .await
            .unwrap();
        assert_eq!(wait(&p, &id).await.unwrap(), "idle");
        send(&p, &id, "Reply only with: two").await.unwrap();
        assert_eq!(wait(&p, &id).await.unwrap(), "idle");
        eprintln!("agent {id} ok; archive it with `paseo archive {id}`");
    }
}
