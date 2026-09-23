//! Reads the Codex CLI's ChatGPT-subscription credentials from `~/.codex/auth.json`
//! and refreshes them when the access token is close to expiry.
//!
//! Protocol lifted from openai/codex (`codex-rs/login`): the OAuth client id, the
//! refresh endpoint, and the shape of `auth.json`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Refresh when the access token has less than this many seconds left.
const REFRESH_SLACK_SECS: i64 = 120;
/// Give up on a refresh that has not answered in this long, rather than holding the
/// turn open on a socket that connected and then went quiet.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// One refresh at a time in this process: a child agent and the namer both load
/// credentials within milliseconds of the first turn, and spending the refresh token
/// twice would leave whichever write lands second holding a token the server has rotated.
static REFRESHING: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct Auth {
    pub access_token: String,
    pub account_id: Option<String>,
}

pub fn codex_home() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME")
        && !dir.trim().is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn auth_path() -> Result<PathBuf> {
    Ok(codex_home()?.join("auth.json"))
}

/// Load credentials, refreshing them first if the access token is about to expire.
pub async fn load(http: &reqwest::Client) -> Result<Auth> {
    let path = auth_path()?;
    let doc = read_doc(&path)?;

    let access = str_at(&doc, &["tokens", "access_token"]).ok_or_else(|| {
        anyhow!(
            "no ChatGPT tokens in {}. Run `codex login`.",
            path.display()
        )
    })?;

    let doc = match expires_within(&access, REFRESH_SLACK_SECS) {
        true => refresh(http, &path).await?,
        false => doc,
    };

    let access_token = str_at(&doc, &["tokens", "access_token"])
        .ok_or_else(|| anyhow!("no access_token after refresh"))?;
    let account_id = str_at(&doc, &["tokens", "account_id"])
        .or_else(|| str_at(&doc, &["tokens", "id_token"]).and_then(|jwt| account_id_from(&jwt)));

    Ok(Auth {
        access_token,
        account_id,
    })
}

fn read_doc(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "could not read {}. Run `codex login` first.",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| format!("{} is not valid JSON", path.display()))
}

/// Refresh the tokens in `path` and return the file as it now stands. Held under
/// [`REFRESHING`], and the file is re-read inside the lock so a caller that queued
/// behind another's refresh takes its result instead of spending the token again.
async fn refresh(http: &reqwest::Client, path: &Path) -> Result<Value> {
    let _held = REFRESHING
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let mut doc = read_doc(path)?;
    let access = str_at(&doc, &["tokens", "access_token"]).unwrap_or_default();
    if !expires_within(&access, REFRESH_SLACK_SECS) {
        return Ok(doc);
    }
    let refresh_token = str_at(&doc, &["tokens", "refresh_token"])
        .ok_or_else(|| anyhow!("access token expired and no refresh_token is present"))?;
    refresh_tokens(http, &refresh_token, &mut doc).await?;
    write_atomically(path, &serde_json::to_string_pretty(&doc)?)
        .with_context(|| format!("could not write refreshed tokens to {}", path.display()))?;
    Ok(doc)
}

/// Write through a neighbouring temp file and rename, so a concurrent reader sees either
/// the old credentials or the new ones and never half of a file.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("auth");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn refresh_tokens(
    http: &reqwest::Client,
    refresh_token: &str,
    doc: &mut Value,
) -> Result<()> {
    let sent = http
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send();
    let resp = tokio::time::timeout(REFRESH_TIMEOUT, sent)
        .await
        .map_err(|_| anyhow!("token refresh timed out"))?
        .context("token refresh request failed")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token refresh failed ({status}): {body}. Try `codex login` again.");
    }
    let refreshed: Value =
        serde_json::from_str(&body).context("token refresh returned non-JSON")?;

    let tokens = doc
        .get_mut("tokens")
        .ok_or_else(|| anyhow!("auth.json has no `tokens` object"))?;
    for key in ["id_token", "access_token", "refresh_token"] {
        if let Some(v) = refreshed.get(key).and_then(Value::as_str) {
            tokens[key] = json!(v);
        }
    }
    doc["last_refresh"] = json!(chrono::Utc::now().to_rfc3339());
    Ok(())
}

fn str_at(doc: &Value, path: &[&str]) -> Option<String> {
    let mut cur = doc;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::to_string)
}

/// The workspace/account id lives in the `https://api.openai.com/auth` claim of the id token.
fn account_id_from(id_token: &str) -> Option<String> {
    let claims = jwt_claims(id_token)?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

fn expires_within(jwt: &str, slack_secs: i64) -> bool {
    let Some(exp) = jwt_claims(jwt).and_then(|c| c.get("exp").and_then(Value::as_i64)) else {
        // Unreadable expiry: refresh rather than send a token we cannot vouch for.
        return true;
    };
    chrono::Utc::now().timestamp() + slack_secs >= exp
}

fn jwt_claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    serde_json::from_slice(&b64url_decode(payload)?).ok()
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut lookup = [0xFFu8; 256];
    for (i, c) in ALPHABET.iter().enumerate() {
        lookup[*c as usize] = i as u8;
    }

    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = lookup[c as usize];
        if v == 0xFF {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refreshed_file_is_renamed_into_place_and_leaves_no_temp_behind() {
        let dir = std::env::temp_dir().join(format!("bhai-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, r#"{"tokens":{"access_token":"old"}}"#).unwrap();

        write_atomically(&path, r#"{"tokens":{"access_token":"new"}}"#).unwrap();

        let doc = read_doc(&path).unwrap();
        assert_eq!(
            str_at(&doc, &["tokens", "access_token"]).as_deref(),
            Some("new")
        );
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["auth.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
