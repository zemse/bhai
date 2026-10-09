use super::*;
use crate::permissions::{Decision, Mode, Policy, Rule, Rules};
use std::os::unix::fs::PermissionsExt;

mod hardening;

struct Fixture {
    tool: AwsInstance,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("bhai-aws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let cli = root.join("aws");
        let script = r##"#!/bin/sh
cd "ROOT" || exit 1
printf '%s\n' "$@" >> argv
shift 7
service=$1
action=$2
shift 2
if test -f fail; then echo 'cloud unavailable' >&2; exit 1; fi
if test -f hang; then exec sleep 2; fi
case "$service:$action" in
sts:get-caller-identity) cat identity ;;
ec2:describe-instances) cat instance ;;
ec2:start-instances) echo start >> mutations; cat started > instance; echo '{}' ;;
ec2:stop-instances) echo stop >> mutations; cat stopping > instance; echo '{}' ;;
scheduler:create-schedule)
  while test "$#" -gt 0; do
    case "$1" in
      --name) name=$2 ;;
      --schedule-expression) expression=$2 ;;
      --target) target=$2 ;;
    esac
    shift 2
  done
  printf '{"Arn":"arn:aws:scheduler:us-east-1:123456789012:schedule/default/%s","Name":"%s","GroupName":"default","State":"ENABLED","ScheduleExpressionTimezone":"UTC","ScheduleExpression":"%s","FlexibleTimeWindow":{"Mode":"OFF"},"Target":%s}' "$name" "$name" "$expression" "$target" > schedule
  echo arm >> mutations
  if test -f bad_arm; then echo '{}' > schedule; fi
  if test -f fail_create; then exit 1; fi
  if test -f hang_create; then exec sleep 2; fi
  echo '{}' ;;
scheduler:get-schedule)
  if test -f fail_readback; then exit 1; fi
  if test -f hang_readback; then exec sleep 2; fi
  cat schedule ;;
*) echo 'unexpected call' >&2; exit 1 ;;
esac
"##;
        std::fs::write(&cli, script.replace("ROOT", root.to_str().unwrap())).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let tool = AwsInstance {
            root,
            cli,
            timeout: Duration::from_secs(1),
        };
        let fixture = Self { tool };
        fixture.set(
            "identity",
            &json!({"Account":"123456789012", "Arn":"arn:aws:iam::123456789012:user/test"}),
        );
        fixture.state("instance", "stopped");
        fixture.state("started", "pending");
        fixture.state("stopping", "stopping");
        fixture
    }

    fn set(&self, file: &str, value: &Value) {
        std::fs::write(self.tool.root.join(file), value.to_string()).unwrap();
    }

    fn state(&self, file: &str, state: &str) {
        self.set(file, &json!({"Reservations":[{"OwnerId":"123456789012", "Instances":[{"InstanceId":"i-0123456789abcdef0", "State":{"Name":state}}]}]}));
    }

    fn arm_args(&self) -> Value {
        json!({"action":"arm", "account":"123456789012", "profile":"test", "region":"us-east-1", "instance":"i-0123456789abcdef0", "role_arn":"arn:aws:iam::123456789012:role/stop", "deadline": (Utc::now() + chrono::Duration::minutes(30)).format("%Y-%m-%dT%H:%M:%SZ").to_string()})
    }

    async fn arm(&self) -> String {
        self.tool.run(&self.arm_args()).await.unwrap()["capability"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn call(&self, action: &str, id: &str) -> Value {
        json!({"action":action, "capability":id})
    }

    fn mutations(&self) -> String {
        std::fs::read_to_string(self.tool.root.join("mutations")).unwrap_or_default()
    }

    fn policy(&self, mode: Mode, rules: Rules) -> Policy {
        let policy = Policy::new(mode, rules, None, self.tool.root.clone()).with_trust(
            crate::permissions::Trust::new(&self.tool.root.join("config"), &self.tool.root),
        );
        policy.trust().unwrap();
        policy
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tool.root);
    }
}

#[tokio::test]
async fn arms_reloads_starts_and_only_observed_stopped_is_stopped() {
    let f = Fixture::new();
    let id = f.arm().await;
    let resumed = AwsInstance {
        root: f.tool.root.clone(),
        cli: f.tool.cli.clone(),
        timeout: f.tool.timeout,
    };
    let start = f.call("start", &id);
    assert!(authorized(&f.tool.root, &start));
    let out = resumed.run(&start).await.unwrap();
    assert_eq!(out["state"], "pending");
    assert_eq!(out["stopped"], false);
    f.state("instance", "running");
    assert!(resumed.run(&f.call("use", &id)).await.is_ok());
    let out = resumed.run(&f.call("stop", &id)).await.unwrap();
    assert_eq!(out["state"], "stopping");
    assert_eq!(out["stopped"], false);
    f.state("instance", "stopped");
    assert_eq!(
        resumed.run(&f.call("status", &id)).await.unwrap()["stopped"],
        true
    );
    assert_eq!(f.mutations(), "arm\nstart\nstop\n");
    let argv = std::fs::read_to_string(f.tool.root.join("argv")).unwrap();
    assert!(argv.contains("--profile\ntest\n--region\nus-east-1\n"));
    assert!(!argv.contains("delete-schedule"));
}

#[tokio::test]
async fn expired_and_revoked_capabilities_retain_deterministic_stop_without_judge() {
    let f = Fixture::new();
    let id = f.arm().await;
    let mut cap = load(&f.tool.root, &id).unwrap();
    cap.deadline = Utc::now() - chrono::Duration::minutes(1);
    save(&f.tool.root, &cap).unwrap();
    let start = f.call("start", &id);
    assert!(!authorized(&f.tool.root, &start));
    assert!(f.tool.run(&start).await.is_err());
    f.tool.run(&f.call("revoke", &id)).await.unwrap();
    let stop = f.call("stop", &id);
    assert!(authorized(&f.tool.root, &stop));
    let policy = f.policy(Mode::Auto, Rules::default());
    assert!(matches!(
        policy.check(NAME, &stop, true),
        Decision::Allow(_)
    ));
    f.tool.run(&stop).await.unwrap();
    assert_eq!(f.mutations(), "arm\nstop\n");
}

#[tokio::test]
async fn deny_and_ask_precede_cleanup_capability_and_allow_cannot_arm() {
    let f = Fixture::new();
    let id = f.arm().await;
    let stop = f.call("stop", &id);
    let mut rules = Rules::default();
    rules.deny.push(Rule::parse("Aws_instance").unwrap());
    assert!(matches!(
        f.policy(Mode::Auto, rules).check(NAME, &stop, true),
        Decision::Deny(_)
    ));
    let mut rules = Rules::default();
    rules.ask.push(Rule::parse("Aws_instance").unwrap());
    assert_eq!(
        f.policy(Mode::Auto, rules).check(NAME, &stop, true),
        Decision::Ask
    );
    let mut rules = Rules::default();
    rules.allow.push(Rule::parse("Aws_instance").unwrap());
    assert_eq!(
        f.policy(Mode::Auto, rules).check(NAME, &f.arm_args(), true),
        Decision::Ask
    );
}

#[tokio::test]
async fn arm_checks_identity_resource_and_stopped_before_any_cloud_mutation() {
    for failure in ["account", "instance", "owner", "running"] {
        let f = Fixture::new();
        match failure {
            "account" => f.set("identity", &json!({"Account":"999999999999", "Arn":"arn:aws:iam::999999999999:user/test"})),
            "instance" => f.set("instance", &json!({"Reservations":[{"OwnerId":"123456789012", "Instances":[{"InstanceId":"i-11111111", "State":{"Name":"stopped"}}]}]})),
            "owner" => f.set("instance", &json!({"Reservations":[{"OwnerId":"999999999999", "Instances":[{"InstanceId":"i-0123456789abcdef0", "State":{"Name":"stopped"}}]}]})),
            _ => f.state("instance", "running"),
        }
        assert!(f.tool.run(&f.arm_args()).await.is_err(), "{failure}");
        assert!(f.mutations().is_empty(), "{failure}");
        assert!(!ledger(&f.tool.root).exists());
    }
}

#[tokio::test]
async fn failed_readback_never_issues_capability_and_never_cancels_schedule() {
    let f = Fixture::new();
    f.set("bad_arm", &json!(true));
    assert!(f.tool.run(&f.arm_args()).await.is_err());
    assert_eq!(f.mutations(), "arm\n");
    assert!(!ledger(&f.tool.root).exists());
    assert!(f.tool.root.join("schedule").exists());
}

#[tokio::test]
async fn missing_or_mismatched_cloud_deadline_never_starts() {
    for field in [
        "Arn",
        "Name",
        "GroupName",
        "State",
        "ScheduleExpressionTimezone",
        "ScheduleExpression",
        "FlexibleTimeWindow",
        "Target",
        "StartDate",
        "EndDate",
    ] {
        let f = Fixture::new();
        let id = f.arm().await;
        let path = f.tool.root.join("schedule");
        let mut schedule: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        schedule[field] = json!("wrong");
        f.set("schedule", &schedule);
        assert!(f.tool.run(&f.call("start", &id)).await.is_err(), "{field}");
        assert_eq!(f.mutations(), "arm\n");
    }
    let f = Fixture::new();
    let id = f.arm().await;
    std::fs::remove_file(f.tool.root.join("schedule")).unwrap();
    assert!(f.tool.run(&f.call("start", &id)).await.is_err());
    assert_eq!(f.mutations(), "arm\n");
}

#[tokio::test]
async fn cloud_failure_and_timeout_fail_closed_and_keep_deadline() {
    for failure in ["fail", "hang"] {
        let mut f = Fixture::new();
        let id = f.arm().await;
        f.tool.timeout = Duration::from_millis(50);
        f.set(failure, &json!(true));
        assert!(f.tool.run(&f.call("start", &id)).await.is_err());
        assert!(f.tool.run(&f.call("stop", &id)).await.is_err());
        assert!(f.tool.root.join("schedule").exists());
        assert_eq!(f.mutations(), "arm\n");
    }
}

#[tokio::test]
async fn exact_target_identity_and_state_are_rechecked_on_start() {
    for failure in ["identity", "running", "input", "role", "target", "retry"] {
        let f = Fixture::new();
        let id = f.arm().await;
        let mut schedule: Value =
            serde_json::from_slice(&std::fs::read(f.tool.root.join("schedule")).unwrap()).unwrap();
        match failure {
            "identity" => f.set(
                "identity",
                &json!({"Account":"999999999999", "Arn":"arn:aws:iam::999999999999:user/test"}),
            ),
            "running" => f.state("instance", "running"),
            "input" => schedule["Target"]["Input"] = json!("{\"InstanceIds\":[\"i-11111111\"]}"),
            "role" => schedule["Target"]["RoleArn"] = json!("arn:aws:iam::123456789012:role/other"),
            "target" => {
                schedule["Target"]["Arn"] =
                    json!("arn:aws:scheduler:::aws-sdk:ec2:terminateInstances")
            }
            _ => schedule["Target"]["RetryPolicy"] = json!({}),
        }
        f.set("schedule", &schedule);
        assert!(
            f.tool.run(&f.call("start", &id)).await.is_err(),
            "{failure}"
        );
        assert_eq!(f.mutations(), "arm\n");
    }
}

#[tokio::test]
async fn rejects_unknown_actions_flags_injection_and_model_receipts() {
    let f = Fixture::new();
    for args in [
        json!({"action":"terminate"}),
        json!({"action":"arm", "endpoint_url":"http://evil"}),
        json!({"action":"start", "capability":"../fake"}),
        json!({"action":"stop", "capability":uuid::Uuid::new_v4().to_string(), "receipt":{"armed":true}}),
    ] {
        assert!(f.tool.run(&args).await.is_err());
    }
    for field in ["profile", "region", "instance", "role_arn"] {
        let mut args = f.arm_args();
        args[field] = json!("test; $(touch /tmp/bhai-aws-injected)");
        assert!(f.tool.run(&args).await.is_err());
    }
    assert!(!f.tool.root.join("argv").exists());
}

#[tokio::test]
async fn ledger_is_private_and_rejects_symlinks_or_altered_ids() {
    let f = Fixture::new();
    let id = f.arm().await;
    let path = ledger(&f.tool.root).join(format!("{id}.json"));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!authorized(&f.tool.root, &f.call("stop", &id)));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut cap = load(&f.tool.root, &id).unwrap();
    cap.id = uuid::Uuid::new_v4().to_string();
    crate::sessions::private_write(&path, &serde_json::to_string(&cap).unwrap()).unwrap();
    assert!(load(&f.tool.root, &id).is_err());
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(f.tool.root.join("schedule"), &path).unwrap();
    assert!(load(&f.tool.root, &id).is_err());
}

#[test]
fn raw_launch_guard_handles_wrappers_and_explicit_modes() {
    let f = Fixture::new();
    for command in [
        "aws ec2 start-instances --instance-ids i-11111111",
        "env AWS_PROFILE=test /usr/local/bin/aws --region us-east-1 ec2 run-instances",
        "timeout 5 aws ec2 start-instances",
        "env -u HOME aws ec2 start-instances",
        "nice -n 5 timeout --signal KILL 10 aws ec2 start-instances",
        "aws ec2 --region us-east-1 start-instances",
        "find . -exec aws ec2 run-instances \\;",
        "X=x aws ec2 start-instances",
        "exec /usr/local/bin/aws ec2 run-instances",
        "aws ec2 start-instances $(cat ids)",
    ] {
        assert!(crate::permissions::bash::aws_launch(command), "{command}");
        let mut rules = Rules::default();
        let mut allow = Rule::parse("Bash(*)").unwrap();
        allow.user = true;
        rules.allow.push(allow);
        let args = json!({"command":command});
        let policy = f.policy(Mode::Auto, rules);
        assert_eq!(policy.check("bash", &args, true), Decision::Ask);
        assert!(
            matches!(policy.judgeable("bash", &args), Err(crate::permissions::Reserved::Protected(message)) if message.contains("AWS lifecycle"))
        );
        assert!(matches!(
            f.policy(Mode::Bypass, Rules::default())
                .check("bash", &args, true),
            Decision::Allow(_)
        ));
        assert_eq!(
            f.policy(Mode::Ask, Rules::default())
                .check("bash", &args, true),
            Decision::Ask
        );
    }
}
