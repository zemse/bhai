use super::*;

#[tokio::test]
async fn arm_failure_or_timeout_after_cloud_creation_keeps_schedule_without_capability() {
    for fault in [
        "fail_create",
        "hang_create",
        "fail_readback",
        "hang_readback",
    ] {
        let f = Fixture::new();
        f.set(fault, &json!(true));
        let error = f.tool.run(&f.arm_args()).await.unwrap_err();
        assert_eq!(f.mutations(), "arm\n", "{fault}: {error}");
        if fault.starts_with("hang") {
            assert!(error.contains("timed out"), "{fault}: {error}");
        }
        assert!(!ledger(&f.tool.root).exists());
        assert!(f.tool.root.join("schedule").exists());
        let argv = std::fs::read_to_string(f.tool.root.join("argv")).unwrap();
        assert!(!argv.contains("delete-schedule"));
        assert!(!argv.contains("start-instances"));
    }
}

#[tokio::test]
async fn final_directory_privacy_is_checked_on_load_and_save() {
    for directory in [".bhai/sessions", ".bhai/sessions/aws-instance"] {
        let f = Fixture::new();
        let id = f.arm().await;
        let cap = load(&f.tool.root, &id).unwrap();
        let path = f.tool.root.join(directory);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(load(&f.tool.root, &id).is_err());
        assert!(save(&f.tool.root, &cap).is_err());
        assert!(!authorized(&f.tool.root, &f.call("stop", &id)));
    }
    let f = Fixture::new();
    let dir = ledger(&f.tool.root);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(f.tool.run(&f.arm_args()).await.is_err());
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
}

#[tokio::test]
async fn symlink_directory_is_refused_without_writing_external_ledger() {
    let f = Fixture::new();
    let external = f.tool.root.join("external");
    std::fs::create_dir(&external).unwrap();
    std::fs::create_dir(f.tool.root.join(".bhai")).unwrap();
    std::os::unix::fs::symlink(&external, f.tool.root.join(".bhai/sessions")).unwrap();
    assert!(f.tool.run(&f.arm_args()).await.is_err());
    assert_eq!(std::fs::read_dir(external).unwrap().count(), 0);
}

#[tokio::test]
async fn revoke_does_not_extend_lease_or_remove_deadline() {
    let f = Fixture::new();
    let id = f.arm().await;
    let before = load(&f.tool.root, &id).unwrap();
    let schedule = std::fs::read(f.tool.root.join("schedule")).unwrap();
    f.tool.run(&f.call("revoke", &id)).await.unwrap();
    let after = load(&f.tool.root, &id).unwrap();
    assert!(after.revoked);
    assert_eq!(before.deadline, after.deadline);
    assert_eq!(before.account, after.account);
    assert_eq!(before.region, after.region);
    assert_eq!(before.instance, after.instance);
    assert_eq!(
        std::fs::read(f.tool.root.join("schedule")).unwrap(),
        schedule
    );
    for action in ["start", "use"] {
        assert!(!authorized(&f.tool.root, &f.call(action, &id)));
        assert!(f.tool.run(&f.call(action, &id)).await.is_err());
    }
    assert!(authorized(&f.tool.root, &f.call("stop", &id)));
    assert_eq!(f.mutations(), "arm\n");
}

#[test]
fn endpoint_overrides_are_removed_and_config_endpoints_ignored() {
    let mut command = Command::new("aws");
    let keys = [
        "AWS_ENDPOINT_URL",
        "AWS_ENDPOINT_URL_EC2",
        "AWS_ENDPOINT_URL_STS",
        "AWS_ENDPOINT_URL_SCHEDULER",
        "AWS_ENDPOINT_URLS",
    ];
    for key in keys {
        command.env(key, "https://untrusted.example");
    }
    command.env("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "false");
    endpoints(&mut command, keys.map(std::ffi::OsString::from));
    let envs: Vec<_> = command.as_std().get_envs().collect();
    for key in keys {
        assert!(envs.iter().any(|(k, v)| *k == key && v.is_none()), "{key}");
    }
    assert!(
        envs.iter()
            .any(|(k, v)| *k == "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS"
                && *v == Some(std::ffi::OsStr::new("true")))
    );
}

#[tokio::test]
async fn unknown_fields_cannot_change_scope_flags_or_supply_evidence() {
    let f = Fixture::new();
    let id = f.arm().await;
    for field in [
        "profile",
        "region",
        "instance",
        "account",
        "endpoint_url",
        "flags",
        "receipt",
        "deadline",
    ] {
        let mut args = f.call("start", &id);
        args[field] = json!("--endpoint-url=https://untrusted.example");
        assert!(!authorized(&f.tool.root, &args), "{field}");
        assert!(f.tool.run(&args).await.is_err(), "{field}");
    }
    for field in ["profile", "region"] {
        let mut args = f.arm_args();
        args[field] = json!("--no-verify-ssl");
        assert!(f.tool.run(&args).await.is_err());
    }
    assert_eq!(f.mutations(), "arm\n");
}

#[test]
fn raw_prefix_rules_cannot_override_launch_guard_but_explicit_deny_and_ask_win() {
    let f = Fixture::new();
    let command = "env AWS_PROFILE=test /usr/local/bin/aws --region us-east-1 ec2 run-instances";
    let args = json!({"command":command});
    for allow in ["Bash(*)", "Bash(aws:*)", "Bash(env:*)"] {
        let mut rules = Rules::default();
        rules.allow.push(Rule::parse(allow).unwrap());
        assert_eq!(
            f.policy(Mode::Auto, rules).check("bash", &args, true),
            Decision::Ask
        );
    }
    let mut rules = Rules::default();
    rules
        .deny
        .push(Rule::parse("Bash(aws ec2 run-instances:*)").unwrap());
    let simple = json!({"command":"env AWS_PROFILE=test /usr/local/bin/aws ec2 run-instances --image-id ami-123"});
    assert!(matches!(
        f.policy(Mode::Auto, rules.clone())
            .check("bash", &simple, true),
        Decision::Deny(_)
    ));
    assert!(matches!(
        f.policy(Mode::Bypass, rules).check("bash", &simple, true),
        Decision::Deny(_)
    ));
    let mut rules = Rules::default();
    rules.ask.push(Rule::parse("Bash(aws:*)").unwrap());
    assert_eq!(
        f.policy(Mode::Bypass, rules).check("bash", &args, true),
        Decision::Ask
    );
    for safe in [
        "aws ec2 describe-instances",
        "aws ec2 stop-instances --instance-ids i-11111111",
        "echo 'aws ec2 run-instances'",
    ] {
        assert!(!crate::permissions::bash::aws_launch(safe));
    }
}

#[tokio::test]
async fn subsequent_user_restrictions_veto_an_existing_start_capability() {
    let mut f = Fixture::new();
    let id = f.arm().await;
    let (judge, backend) = crate::judge::fake::judge(
        crate::judge::fake::Answers::Verdict(crate::judge::Verdict::Deny {
            reason: "user prohibited another start".into(),
        }),
        &f.tool.root,
    );
    let restriction = "do not start that instance again";
    judge.start_user_turn(restriction, None);
    backend.authorization_replies.lock().unwrap().insert(restriction.into(), json!({"candidates":[{
        "kind":"restriction", "quote":restriction, "scope":"instance i-11111111111111111", "action":"do not start instance again", "lifetime":"until explicitly changed"
    }]}));
    f.tool.authorization = Some((
        Arc::new(f.policy(Mode::Auto, Rules::default())),
        Arc::new(judge),
    ));
    let error = f.tool.run(&f.call("start", &id)).await.unwrap_err();
    assert!(error.contains("user prohibited"), "{error}");
    assert_eq!(f.mutations(), "arm\n");
    let calls = backend.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].detail.contains("123456789012"));
    assert!(calls[0].detail.contains("us-east-1"));
    assert!(
        calls[0]
            .user_context
            .iter()
            .any(|text| text.contains(restriction))
    );
}

#[tokio::test]
async fn an_approved_ask_rule_does_not_cancel_scoped_cleanup() {
    let mut f = Fixture::new();
    let id = f.arm().await;
    let (judge, _) = crate::judge::fake::judge(crate::judge::fake::Answers::Hang, &f.tool.root);
    judge.update_authorization().await.unwrap();
    let mut rules = Rules::default();
    rules.ask.push(Rule::parse("Aws_instance").unwrap());
    let policy = Arc::new(f.policy(Mode::Auto, rules));
    assert_eq!(
        policy.check(NAME, &f.call("stop", &id), true),
        Decision::Ask
    );
    f.tool.authorization = Some((policy, Arc::new(judge)));
    f.tool.run(&f.call("stop", &id)).await.unwrap();
    assert_eq!(f.mutations(), "arm\nstop\n");
}

#[tokio::test]
async fn a_start_lease_does_not_authorize_unrelated_paid_work() {
    let mut f = Fixture::new();
    let id = f.arm().await;
    let (judge, backend) = crate::judge::fake::judge(
        crate::judge::fake::Answers::Verdict(crate::judge::Verdict::Deny {
            reason: "paid start unrelated to local tests".into(),
        }),
        &f.tool.root,
    );
    judge.start_user_turn("only run local unit tests", None);
    f.tool.authorization = Some((
        Arc::new(f.policy(Mode::Auto, Rules::default())),
        Arc::new(judge),
    ));
    assert!(f.tool.run(&f.call("start", &id)).await.is_err());
    assert_eq!(backend.calls.lock().unwrap().len(), 1);
    assert_eq!(f.mutations(), "arm\n");
}

#[tokio::test]
async fn clean_stop_does_not_call_an_unavailable_judge() {
    let mut f = Fixture::new();
    let id = f.arm().await;
    let (judge, backend) =
        crate::judge::fake::judge(crate::judge::fake::Answers::Hang, &f.tool.root);
    judge.update_authorization().await.unwrap();
    let classified = backend.authorization_calls.lock().unwrap().len();
    *backend.authorization_failure.lock().unwrap() =
        Some((crate::judge::authorization::Stage::Extract, true));
    f.tool.authorization = Some((
        Arc::new(f.policy(Mode::Auto, Rules::default())),
        Arc::new(judge),
    ));
    f.tool.run(&f.call("stop", &id)).await.unwrap();
    assert!(backend.calls.lock().unwrap().is_empty());
    assert_eq!(
        backend.authorization_calls.lock().unwrap().len(),
        classified
    );
    assert_eq!(f.mutations(), "arm\nstop\n");
}

#[tokio::test]
async fn failed_or_changed_user_authorization_cannot_start_the_instance() {
    let mut f = Fixture::new();
    let id = f.arm().await;
    let (judge, backend) = crate::judge::fake::judge(
        crate::judge::fake::Answers::Verdict(crate::judge::Verdict::Approve {
            reason: "fine".into(),
        }),
        &f.tool.root,
    );
    let judge = Arc::new(judge);
    f.tool.authorization = Some((
        Arc::new(f.policy(Mode::Auto, Rules::default())),
        Arc::clone(&judge),
    ));
    let cap = load(&f.tool.root, &id).unwrap();
    let req = Request::Start {
        capability: id.clone(),
    };
    let revision = f.tool.authorize(&req, &cap).await.unwrap();
    judge.start_user_turn("do not start the instance", None);
    assert!(
        f.tool
            .unchanged(revision.as_ref(), &f.call("start", &id))
            .is_err()
    );
    *backend.authorization_failure.lock().unwrap() =
        Some((crate::judge::authorization::Stage::Extract, false));
    assert!(f.tool.run(&f.call("start", &id)).await.is_err());
    assert_eq!(f.mutations(), "arm\n");
    assert!(f.tool.root.join("schedule").exists());
}

#[tokio::test]
async fn explicit_checkout_root_reuses_the_permission_ledger() {
    let f = Fixture::new();
    let id = f.arm().await;
    let mut tool = AwsInstance::at(f.tool.root.clone());
    tool.cli = f.tool.cli.clone();
    tool.timeout = f.tool.timeout;
    let output = tool.run(&f.call("status", &id)).await.unwrap();
    assert_eq!(
        output["instance"],
        load(&f.tool.root, &id).unwrap().instance
    );
    assert!(authorized(&f.tool.root, &f.call("stop", &id)));
}

#[tokio::test]
async fn scoped_capabilities_do_not_bypass_project_trust() {
    let f = Fixture::new();
    let id = f.arm().await;
    let policy = Policy::new(Mode::Auto, Rules::default(), None, f.tool.root.clone()).with_trust(
        crate::permissions::Trust::new(&f.tool.root.join("config"), &f.tool.root),
    );
    assert!(!policy.trusted());
    assert_eq!(
        policy.check(NAME, &f.call("stop", &id), true),
        Decision::Ask
    );
    assert_eq!(
        policy.check(NAME, &f.call("start", &id), true),
        Decision::Ask
    );
}

#[tokio::test]
async fn native_arm_is_reserved_for_user_and_persists_exact_resource_association() {
    let f = Fixture::new();
    let policy = f.policy(Mode::Auto, Rules::default());
    let args = f.arm_args();
    assert_eq!(policy.check(NAME, &args, true), Decision::Ask);
    assert!(
        matches!(policy.judgeable(NAME, &args), Err(crate::permissions::Reserved::Protected(message)) if message.contains("arm"))
    );
    assert_eq!(
        f.policy(Mode::Ask, Rules::default())
            .check(NAME, &args, true),
        Decision::Ask
    );
    let output = f.tool.run(&args).await.unwrap();
    assert!(
        output["limitation"]
            .as_str()
            .unwrap()
            .contains("not execution-role")
    );
    let id = output["capability"].as_str().unwrap();
    let cap = load(&f.tool.root, id).unwrap();
    for key in ["account", "profile", "region", "instance", "role_arn"] {
        assert_eq!(serde_json::to_value(&cap).unwrap()[key], args[key]);
    }
    assert!(crate::permissions::rules::is_protected(
        &ledger(&f.tool.root),
        None
    ));
    assert!(
        crate::permissions::bash::parse(&format!(
            "cat {}/{}.json",
            ledger(&f.tool.root).display(),
            id
        ))
        .unwrap()
        .iter()
        .any(crate::permissions::bash::mentions_protected)
    );
}
