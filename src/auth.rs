//! Reads the Codex CLI's ChatGPT-subscription credentials from `~/.codex/auth.json`
//! and refreshes them when the access token is close to expiry.
//!
//! Protocol lifted from openai/codex (`codex-rs/login`): the OAuth client id, the
//! refresh endpoint, and the shape of `auth.json`.

use std::io::Write as _;
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
    refresh_with(path, |token| exchange(http, token)).await
}

/// What the token endpoint answered, when it answered at all.
enum Exchange {
    Tokens(Value),
    Refused { status: u16, body: String },
}

/// [`refresh`] with the call to the token endpoint passed in. The lock is per process,
/// so the `codex` CLI or another bhai can spend the refresh token between our read and
/// the server's answer; a refusal then finds a different refresh token in the file, and
/// that file is the rotation to adopt.
async fn refresh_with<F, Fut>(path: &Path, exchange: F) -> Result<Value>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<Exchange>>,
{
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
    let refreshed = match exchange(refresh_token.clone()).await? {
        Exchange::Tokens(refreshed) => refreshed,
        Exchange::Refused { status, body } => {
            if let Ok(now) = read_doc(path)
                && str_at(&now, &["tokens", "refresh_token"]).is_some_and(|t| t != refresh_token)
            {
                return Ok(now);
            }
            if refusal_code(&body).as_deref() == Some("refresh_token_reused") {
                bail!(
                    "token refresh failed ({status}): the refresh token was already spent, \
                     and {} still holds it. Try `codex login` again.",
                    path.display()
                );
            }
            bail!("token refresh failed ({status}): {body}. Try `codex login` again.");
        }
    };
    apply_refreshed(&mut doc, &refreshed)?;
    write_atomically(path, &serde_json::to_string_pretty(&doc)?)
        .with_context(|| format!("could not write refreshed tokens to {}", path.display()))?;
    Ok(doc)
}

/// The error code of a refusal: `{"error": "<code>"}` or `{"error": {"code": "<code>"}}`.
fn refusal_code(body: &str) -> Option<String> {
    let body: Value = serde_json::from_str(body).ok()?;
    let error = body.get("error")?;
    error
        .as_str()
        .or_else(|| error.get("code").and_then(Value::as_str))
        .map(str::to_string)
}

/// Write through a neighbouring temp file and rename, so a concurrent reader sees either
/// the old credentials or the new ones and never half of a file. The temp file is created
/// 0600 and synced before the rename, so the rename never exposes a world-readable file.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("auth");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn exchange(http: &reqwest::Client, refresh_token: String) -> Result<Exchange> {
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
        return Ok(Exchange::Refused {
            status: status.as_u16(),
            body,
        });
    }
    let refreshed = serde_json::from_str(&body).context("token refresh returned non-JSON")?;
    Ok(Exchange::Tokens(refreshed))
}

fn apply_refreshed(doc: &mut Value, refreshed: &Value) -> Result<()> {
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

    #[cfg(unix)]
    #[test]
    fn a_refreshed_file_is_readable_only_by_the_user() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("bhai-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_atomically(&path, r#"{"tokens":{"access_token":"new"}}"#).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An unsigned JWT whose `exp` is `secs` from now.
    fn jwt_expiring_in(secs: i64) -> String {
        fn b64url(bytes: &[u8]) -> String {
            const ALPHABET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let n = chunk
                    .iter()
                    .enumerate()
                    .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
                for i in 0..=chunk.len() {
                    out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
                }
            }
            out
        }
        let exp = chrono::Utc::now().timestamp() + secs;
        let claims = json!({ "exp": exp }).to_string();
        format!("e30.{}.sig", b64url(claims.as_bytes()))
    }

    fn auth_file(access: &str, refresh: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("bhai-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        let doc = json!({ "tokens": { "access_token": access, "refresh_token": refresh } });
        std::fs::write(&path, doc.to_string()).unwrap();
        (dir, path)
    }

    fn token(doc: &Value, key: &str) -> Option<String> {
        str_at(doc, &["tokens", key])
    }

    #[test]
    fn the_test_jwt_carries_its_expiry() {
        assert!(expires_within(&jwt_expiring_in(-60), REFRESH_SLACK_SECS));
        assert!(!expires_within(&jwt_expiring_in(3600), REFRESH_SLACK_SECS));
    }

    #[tokio::test]
    async fn a_refresh_writes_the_new_tokens() {
        let (dir, path) = auth_file(&jwt_expiring_in(-60), "rt-old");
        let fresh = jwt_expiring_in(3600);

        let doc = refresh_with(&path, |sent| {
            assert_eq!(sent, "rt-old");
            let fresh = fresh.clone();
            async move {
                Ok(Exchange::Tokens(
                    json!({ "access_token": fresh, "refresh_token": "rt-new" }),
                ))
            }
        })
        .await
        .unwrap();

        assert_eq!(token(&doc, "access_token"), Some(fresh));
        let on_disk = read_doc(&path).unwrap();
        assert_eq!(token(&on_disk, "refresh_token").as_deref(), Some("rt-new"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_reused_token_adopts_the_rotation_another_process_wrote() {
        let (dir, path) = auth_file(&jwt_expiring_in(-60), "rt-old");
        let theirs = jwt_expiring_in(3600);
        let rotated = json!({ "tokens": { "access_token": theirs, "refresh_token": "rt-theirs" } })
            .to_string();

        // The `codex` CLI spends the token and writes its rotation while ours is in flight.
        let doc = refresh_with(&path, |_| {
            std::fs::write(&path, &rotated).unwrap();
            async {
                Ok(Exchange::Refused {
                    status: 401,
                    body: r#"{"error":{"code":"refresh_token_reused"}}"#.to_string(),
                })
            }
        })
        .await
        .unwrap();

        assert_eq!(token(&doc, "access_token"), Some(theirs));
        assert_eq!(token(&doc, "refresh_token").as_deref(), Some("rt-theirs"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), rotated);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_reused_token_with_nothing_newer_on_disk_is_an_error() {
        let (dir, path) = auth_file(&jwt_expiring_in(-60), "rt-old");
        let before = std::fs::read_to_string(&path).unwrap();

        let err = refresh_with(&path, |_| async {
            Ok(Exchange::Refused {
                status: 401,
                body: r#"{"error":"refresh_token_reused"}"#.to_string(),
            })
        })
        .await
        .unwrap_err();

        assert!(err.to_string().contains("already spent"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refusal_code_is_read_from_either_shape() {
        assert_eq!(
            refusal_code(r#"{"error":"refresh_token_reused"}"#).as_deref(),
            Some("refresh_token_reused")
        );
        assert_eq!(
            refusal_code(r#"{"error":{"code":"refresh_token_expired","message":"x"}}"#).as_deref(),
            Some("refresh_token_expired")
        );
        assert_eq!(refusal_code("bad gateway"), None);
    }
}
