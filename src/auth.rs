//! Reads the Codex CLI's ChatGPT-subscription credentials from `~/.codex/auth.json`
//! and refreshes them when the access token is close to expiry.
//!
//! Protocol lifted from openai/codex (`codex-rs/login`): the OAuth client id, the
//! refresh endpoint, and the shape of `auth.json`.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Refresh when the access token has less than this many seconds left.
const REFRESH_SLACK_SECS: i64 = 120;

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
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "could not read {}. Run `codex login` first.",
            path.display()
        )
    })?;
    let mut doc: Value = serde_json::from_str(&raw)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    let access = str_at(&doc, &["tokens", "access_token"]).ok_or_else(|| {
        anyhow!(
            "no ChatGPT tokens in {}. Run `codex login`.",
            path.display()
        )
    })?;

    if expires_within(&access, REFRESH_SLACK_SECS) {
        let refresh = str_at(&doc, &["tokens", "refresh_token"])
            .ok_or_else(|| anyhow!("access token expired and no refresh_token is present"))?;
        refresh_tokens(http, &refresh, &mut doc).await?;
        std::fs::write(&path, serde_json::to_string_pretty(&doc)?)
            .with_context(|| format!("could not write refreshed tokens to {}", path.display()))?;
    }

    let access_token = str_at(&doc, &["tokens", "access_token"])
        .ok_or_else(|| anyhow!("no access_token after refresh"))?;
    let account_id = str_at(&doc, &["tokens", "account_id"])
        .or_else(|| str_at(&doc, &["tokens", "id_token"]).and_then(|jwt| account_id_from(&jwt)));

    Ok(Auth {
        access_token,
        account_id,
    })
}

async fn refresh_tokens(
    http: &reqwest::Client,
    refresh_token: &str,
    doc: &mut Value,
) -> Result<()> {
    let resp = http
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
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
