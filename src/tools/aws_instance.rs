//! Scoped EC2 lifecycle backed by an independent EventBridge Scheduler stop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;

use super::{BoxFuture, Tool};

#[cfg(test)]
mod tests;

pub const NAME: &str = "aws_instance";
const TARGET: &str = "arn:aws:scheduler:::aws-sdk:ec2:stopInstances";
const MIN_REMAINING: i64 = 120;
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Arm {
        account: String,
        profile: String,
        region: String,
        instance: String,
        role_arn: String,
        deadline: String,
    },
    Start {
        capability: String,
    },
    Use {
        capability: String,
    },
    Stop {
        capability: String,
    },
    Status {
        capability: String,
    },
    Revoke {
        capability: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capability {
    id: String,
    account: String,
    profile: String,
    region: String,
    instance: String,
    role_arn: String,
    deadline: DateTime<Utc>,
    revoked: bool,
}

impl Capability {
    fn validate(&self) -> Result<(), String> {
        if self.account.len() != 12 || !self.account.bytes().all(|b| b.is_ascii_digit()) {
            return Err("account must be twelve digits".into());
        }
        if !word(&self.profile) || !word(&self.region) || !self.region.contains('-') {
            return Err("invalid profile or region".into());
        }
        let hex = self.instance.strip_prefix("i-").unwrap_or_default();
        if !matches!(hex.len(), 8 | 17) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid instance id".into());
        }
        let prefix = format!("arn:aws:iam::{}:role/", self.account);
        if !self.role_arn.strip_prefix(&prefix).is_some_and(|role| {
            !role.is_empty()
                && role
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/+=,.@_-".contains(&b))
        }) {
            return Err(
                "execution role must be an existing role in the exact account (aws partition)"
                    .into(),
            );
        }
        if uuid::Uuid::parse_str(&self.id).is_err() {
            return Err("invalid capability id".into());
        }
        Ok(())
    }

    fn future(&self) -> Result<(), String> {
        if self.revoked || (self.deadline - Utc::now()).num_seconds() < MIN_REMAINING {
            return Err(
                "capability revoked or stop deadline too close/expired; stop remains available"
                    .into(),
            );
        }
        Ok(())
    }

    fn schedule(&self) -> String {
        format!("bhai-stop-{}", self.id)
    }

    fn expression(&self) -> String {
        format!("at({})", self.deadline.format("%Y-%m-%dT%H:%M:%S"))
    }

    fn target(&self) -> Value {
        json!({"Arn": TARGET, "RoleArn": self.role_arn,
            "Input": json!({"InstanceIds": [self.instance]}).to_string(),
            "RetryPolicy": {"MaximumEventAgeInSeconds": 3600, "MaximumRetryAttempts": 10}})
    }
}

fn word(s: &str) -> bool {
    s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

fn endpoints(command: &mut Command, keys: impl IntoIterator<Item = std::ffi::OsString>) {
    command.env("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "true");
    for key in keys {
        if key.to_string_lossy().starts_with("AWS_ENDPOINT_URL") {
            command.env_remove(key);
        }
    }
}

fn request(args: &Value) -> Result<Request, String> {
    serde_json::from_value(args.clone()).map_err(|e| format!("invalid aws_instance request: {e}"))
}

fn ledger(root: &Path) -> PathBuf {
    root.join(".bhai/sessions/aws-instance")
}

fn secure_path(path: &Path) -> Result<(), String> {
    let mut part = PathBuf::new();
    for c in path.components() {
        part.push(c);
        if let Ok(meta) = std::fs::symlink_metadata(&part)
            && meta.file_type().is_symlink()
        {
            return Err("AWS ledger path must not contain symlinks".into());
        }
    }
    Ok(())
}

fn private_ledger(root: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [root.join(".bhai/sessions"), ledger(root)] {
            let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
            if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
                return Err("AWS ledger directory is not private".into());
            }
        }
    }
    Ok(())
}

fn load(root: &Path, id: &str) -> Result<Capability, String> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Err("invalid capability id".into());
    }
    let path = ledger(root).join(format!("{id}.json"));
    secure_path(&path)?;
    private_ledger(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            return Err("AWS ledger is not a private regular file".into());
        }
    }
    let cap: Capability = serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    cap.validate()?;
    if cap.id != id {
        return Err("ledger capability id mismatch".into());
    }
    Ok(cap)
}

fn save(root: &Path, cap: &Capability) -> Result<(), String> {
    let dir = ledger(root);
    secure_path(&dir)?;
    crate::sessions::private_dir(&dir).map_err(|e| e.to_string())?;
    private_ledger(root)?;
    cap.validate()?;
    let tmp = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let path = dir.join(format!("{}.json", cap.id));
    secure_path(&path)?;
    crate::sessions::private_write(
        &tmp,
        &serde_json::to_string(cap).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

/// Only local, protected evidence can authorize a scoped call. Execution rechecks AWS.
pub fn authorized(root: &Path, args: &Value) -> bool {
    let Ok(req) = request(args) else { return false };
    let (id, needs_future) = match &req {
        Request::Stop { capability }
        | Request::Status { capability }
        | Request::Revoke { capability } => (capability, false),
        Request::Start { capability } | Request::Use { capability } => (capability, true),
        Request::Arm { .. } => return false,
    };
    load(root, id).is_ok_and(|cap| !needs_future || cap.future().is_ok())
}

struct Authorization {
    revision: u64,
    decision: crate::permissions::Decision,
}

pub struct AwsInstance {
    root: PathBuf,
    cli: PathBuf,
    timeout: Duration,
    authorization: Option<(Arc<crate::permissions::Policy>, Arc<crate::judge::Judge>)>,
}

impl Default for AwsInstance {
    fn default() -> Self {
        Self {
            root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            cli: PathBuf::from("aws"),
            timeout: Duration::from_secs(30),
            authorization: None,
        }
    }
}

impl AwsInstance {
    pub fn at(root: PathBuf) -> Self {
        Self {
            root,
            ..Self::default()
        }
    }

    pub fn with_authorization(
        mut self,
        policy: Arc<crate::permissions::Policy>,
        judge: Arc<crate::judge::Judge>,
    ) -> Self {
        self.authorization = Some((policy, judge));
        self
    }

    async fn authorize(
        &self,
        req: &Request,
        cap: &Capability,
    ) -> Result<Option<Authorization>, String> {
        use crate::judge::{
            Verdict,
            authorization::{Kind, Status},
        };
        let Some((policy, judge)) = &self.authorization else {
            return Ok(None);
        };
        if policy.mode() != crate::permissions::Mode::Auto || !judge.is_on() {
            return Ok(None);
        }
        let args = serde_json::to_value(req).map_err(|error| error.to_string())?;
        let decision = policy.check(NAME, &args, true);
        if matches!(decision, crate::permissions::Decision::Deny(_)) {
            return Err("AWS operation denied by current permission rules".into());
        }
        judge.update_authorization().await.map_err(|error| {
            format!("AWS authorization is unresolved: {error:#}; cloud deadline was not cancelled")
        })?;
        let memory = judge.authorization();
        if !memory.pending.is_empty() {
            return Err(
                "AWS authorization changed during update; cloud deadline was not cancelled".into(),
            );
        }
        if matches!(req, Request::Start { .. } | Request::Use { .. })
            || memory
                .entries
                .iter()
                .any(|entry| entry.status == Status::Active && entry.note.kind != Kind::Grant)
        {
            let target = serde_json::to_string(req).map_err(|error| error.to_string())?;
            let detail = format!(
                "scoped account {} profile {} region {} instance {} deadline {}",
                cap.account, cap.profile, cap.region, cap.instance, cap.deadline
            );
            match judge.decide_writing(NAME, &target, &detail, &[]).await {
                Ok(Verdict::Approve { .. }) => {}
                Ok(Verdict::Deny { reason }) => {
                    return Err(format!("AWS authorization veto: {reason}"));
                }
                Err(reason) => {
                    return Err(format!(
                        "AWS authorization has no verdict: {reason:?}; cloud deadline was not cancelled"
                    ));
                }
            }
        }
        let authorization = Authorization {
            revision: memory.revision,
            decision,
        };
        self.unchanged(Some(&authorization), &args)?;
        Ok(Some(authorization))
    }

    fn unchanged(&self, authorization: Option<&Authorization>, args: &Value) -> Result<(), String> {
        if let (Some(authorization), Some((policy, judge))) = (authorization, &self.authorization) {
            let memory = judge.authorization();
            if memory.revision != authorization.revision
                || !memory.pending.is_empty()
                || policy.check(NAME, args, true) != authorization.decision
            {
                return Err("AWS authorization changed during the operation; cloud deadline was not cancelled".into());
            }
        }
        Ok(())
    }

    async fn aws(&self, cap: &Capability, args: &[String]) -> Result<Value, String> {
        let mut command = Command::new(&self.cli);
        command
            .args([
                "--profile",
                &cap.profile,
                "--region",
                &cap.region,
                "--output",
                "json",
                "--no-cli-pager",
            ])
            .args(args)
            .kill_on_drop(true)
            .env("AWS_PAGER", "")
            .env("AWS_CLI_AUTO_PROMPT", "off");
        endpoints(&mut command, std::env::vars_os().map(|(key, _)| key));
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .map_err(|_| "AWS CLI timed out; cloud stop schedule was not cancelled".to_string())?
            .map_err(|e| format!("AWS CLI: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "AWS CLI failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        if output.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(json!({}));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("AWS CLI returned invalid JSON: {e}"))
    }

    async fn identity(&self, cap: &Capability) -> Result<(), String> {
        let value = self
            .aws(cap, &["sts".into(), "get-caller-identity".into()])
            .await?;
        if value["Account"] != cap.account
            || !value["Arn"].as_str().is_some_and(|arn| {
                arn.starts_with("arn:aws:") && arn.split(':').nth(4) == Some(cap.account.as_str())
            })
        {
            return Err("caller account/partition mismatch".into());
        }
        Ok(())
    }

    async fn state(&self, cap: &Capability) -> Result<String, String> {
        let value = self
            .aws(
                cap,
                &[
                    "ec2".into(),
                    "describe-instances".into(),
                    "--instance-ids".into(),
                    cap.instance.clone(),
                ],
            )
            .await?;
        let reservations = value["Reservations"]
            .as_array()
            .ok_or("missing reservations")?;
        let instances: Vec<&Value> = reservations
            .iter()
            .filter_map(|r| r["Instances"].as_array())
            .flatten()
            .collect();
        if instances.len() != 1
            || instances[0]["InstanceId"] != cap.instance
            || reservations.iter().any(|r| r["OwnerId"] != cap.account)
        {
            return Err("describe instance scope mismatch".into());
        }
        instances[0]["State"]["Name"]
            .as_str()
            .map(str::to_string)
            .ok_or("missing instance state".into())
    }

    async fn deadline(&self, cap: &Capability) -> Result<(), String> {
        let value = self
            .aws(
                cap,
                &[
                    "scheduler".into(),
                    "get-schedule".into(),
                    "--name".into(),
                    cap.schedule(),
                    "--group-name".into(),
                    "default".into(),
                ],
            )
            .await?;
        let arn = format!(
            "arn:aws:scheduler:{}:{}:schedule/default/{}",
            cap.region,
            cap.account,
            cap.schedule()
        );
        let input: Value =
            serde_json::from_str(value["Target"]["Input"].as_str().unwrap_or_default())
                .map_err(|_| "invalid stop target input")?;
        if value["Arn"] != arn
            || value["Name"] != cap.schedule()
            || value["GroupName"] != "default"
            || value["State"] != "ENABLED"
            || value["ScheduleExpressionTimezone"] != "UTC"
            || value["ScheduleExpression"] != cap.expression()
            || value["FlexibleTimeWindow"]["Mode"] != "OFF"
            || value["Target"]["Arn"] != TARGET
            || value["Target"]["RoleArn"] != cap.role_arn
            || value["Target"]["RetryPolicy"] != cap.target()["RetryPolicy"]
            || input != json!({"InstanceIds": [cap.instance]})
            || value.get("StartDate").is_some()
            || value.get("EndDate").is_some()
        {
            return Err("cloud stop deadline scope/state/target mismatch".into());
        }
        cap.future()
    }

    async fn run(&self, args: &Value) -> Result<Value, String> {
        let _lock = LOCK.lock().await;
        let req = request(args)?;
        if let Request::Arm {
            account,
            profile,
            region,
            instance,
            role_arn,
            deadline,
        } = req
        {
            let parsed = DateTime::parse_from_rfc3339(&deadline).map_err(|e| e.to_string())?;
            if parsed.offset().local_minus_utc() != 0 || parsed.timestamp_subsec_nanos() != 0 {
                return Err("deadline must be UTC with whole seconds".into());
            }
            let cap = Capability {
                id: uuid::Uuid::new_v4().to_string(),
                account,
                profile,
                region,
                instance,
                role_arn,
                deadline: parsed.with_timezone(&Utc),
                revoked: false,
            };
            cap.validate()?;
            let remaining = (cap.deadline - Utc::now()).num_seconds();
            if !(300..=86400).contains(&remaining) {
                return Err("deadline must be 5 minutes to 24 hours in the future".into());
            }
            self.identity(&cap).await?;
            if self.state(&cap).await? != "stopped" {
                return Err("arming requires the exact existing instance to be stopped".into());
            }
            self.aws(
                &cap,
                &[
                    "scheduler".into(),
                    "create-schedule".into(),
                    "--name".into(),
                    cap.schedule(),
                    "--group-name".into(),
                    "default".into(),
                    "--schedule-expression".into(),
                    cap.expression(),
                    "--schedule-expression-timezone".into(),
                    "UTC".into(),
                    "--state".into(),
                    "ENABLED".into(),
                    "--flexible-time-window".into(),
                    json!({"Mode":"OFF"}).to_string(),
                    "--target".into(),
                    cap.target().to_string(),
                ],
            )
            .await?;
            self.deadline(&cap).await?;
            self.identity(&cap).await?;
            if self.state(&cap).await? != "stopped" {
                return Err("instance changed state while arming; no capability issued".into());
            }
            cap.future()?;
            save(&self.root, &cap)?;
            return Ok(
                json!({"capability": cap.id, "account": cap.account, "profile": cap.profile,
                    "region": cap.region, "instance": cap.instance, "deadline": cap.deadline,
                    "schedule": cap.schedule(), "state": "stopped", "cloud_configuration_verified": true,
                    "limitation": "Schedule readback verifies configuration, not execution-role trust or StopInstances permission. Delivery is not a guaranteed stop or hard real-time deadline."}),
            );
        }
        let id = match &req {
            Request::Start { capability }
            | Request::Use { capability }
            | Request::Stop { capability }
            | Request::Status { capability }
            | Request::Revoke { capability } => capability,
            Request::Arm { .. } => unreachable!(),
        };
        let mut cap = load(&self.root, id)?;
        let revision = self.authorize(&req, &cap).await?;
        if matches!(req, Request::Revoke { .. }) {
            self.unchanged(revision.as_ref(), args)?;
            cap.revoked = true;
            save(&self.root, &cap)?;
            return Ok(json!({"revoked": true, "stop_available": true}));
        }
        self.identity(&cap).await?;
        if matches!(req, Request::Start { .. } | Request::Use { .. }) {
            cap.future()?;
            self.deadline(&cap).await?;
            let state = self.state(&cap).await?;
            if matches!(req, Request::Start { .. }) {
                if state != "stopped" {
                    return Err("start requires stopped instance".into());
                }
                cap.future()?;
                self.unchanged(revision.as_ref(), args)?;
                self.aws(
                    &cap,
                    &[
                        "ec2".into(),
                        "start-instances".into(),
                        "--instance-ids".into(),
                        cap.instance.clone(),
                    ],
                )
                .await?;
            } else if state != "running" {
                return Err("use requires running instance".into());
            }
        } else if matches!(req, Request::Stop { .. }) {
            self.unchanged(revision.as_ref(), args)?;
            self.aws(
                &cap,
                &[
                    "ec2".into(),
                    "stop-instances".into(),
                    "--instance-ids".into(),
                    cap.instance.clone(),
                ],
            )
            .await?;
        }
        let state = self.state(&cap).await?;
        self.unchanged(revision.as_ref(), args)?;
        Ok(
            json!({"capability": cap.id, "account": cap.account, "region": cap.region, "instance": cap.instance, "deadline": cap.deadline, "state": state, "stopped": state == "stopped", "revoked": cap.revoked}),
        )
    }
}

impl Tool for AwsInstance {
    fn name(&self) -> &str {
        NAME
    }
    fn schema(&self) -> Value {
        json!({"type":"function", "name":NAME,
            "description":"Scoped AWS lifecycle. Arm an existing stopped EC2 instance using an existing Scheduler execution role and independent UTC stop deadline (5 minutes to 24 hours). Arm requires user approval. Use validates running state and deadline, not arbitrary remote commands. Stop remains available after expiry/revocation. No launch, termination, IAM creation or storage deletion. Readback verifies schedule configuration, not execution-role trust or stop permission. Scheduler delivery is not a guaranteed stop or hard real-time deadline; the existing role must trust scheduler.amazonaws.com and permit ec2:StopInstances. Auto cannot start before native arming is approved by the user. Never cancel the deadline.",
            "parameters":{"type":"object", "properties":{
                "action":{"type":"string","enum":["arm","start","use","stop","status","revoke"]},
                "capability":{"type":"string"}, "account":{"type":"string"}, "profile":{"type":"string"}, "region":{"type":"string"}, "instance":{"type":"string"}, "role_arn":{"type":"string"}, "deadline":{"type":"string"}},
                "required":["action"], "additionalProperties":false}})
    }
    fn needs_approval(&self) -> bool {
        true
    }
    fn describe(&self, args: &Value) -> Result<String, String> {
        request(args)?;
        Ok(format!("AWS instance {}", args))
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match self.run(args).await {
                Ok(value) => (value.to_string(), true),
                Err(error) => (error, false),
            }
        })
    }
}
