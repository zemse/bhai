use super::*;

const SCOPE: [&str; 6] = [
    "account", "profile", "region", "instance", "role_arn", "deadline",
];
const ACTIONS: [&str; 5] = ["start", "use", "stop", "status", "revoke"];

fn api_args(mut args: Value, placeholder: &Value) -> Value {
    let fields: &[&str] = if args["action"] == "arm" {
        &["capability"]
    } else {
        &SCOPE
    };
    for field in fields {
        args[*field] = placeholder.clone();
    }
    args
}

#[test]
fn schema_keeps_action_specific_fields_optional_in_the_api_request() {
    let f = Fixture::new();
    let schema = f.tool.schema();
    let body = crate::client::request_body("test", "low", "test", "", &[schema.clone()], &[]);
    assert_eq!(body["tools"][0], schema);
    assert_eq!(schema["strict"], false);
    assert_eq!(schema["parameters"]["required"], json!(["action"]));
    assert_eq!(schema["parameters"]["additionalProperties"], false);
}

#[test]
fn api_placeholders_preserve_the_action_specific_request() {
    let f = Fixture::new();
    let id = uuid::Uuid::new_v4().to_string();
    for args in std::iter::once(f.arm_args()).chain(ACTIONS.map(|action| f.call(action, &id))) {
        let expected = serde_json::to_value(request(&args).unwrap()).unwrap();
        for placeholder in [json!(""), Value::Null] {
            let args = api_args(args.clone(), &placeholder);
            assert_eq!(
                serde_json::to_value(request(&args).unwrap()).unwrap(),
                expected
            );
            assert!(f.tool.describe(&args).is_ok());
        }
    }
}

#[test]
fn unknown_and_nonempty_irrelevant_fields_remain_rejected() {
    let f = Fixture::new();
    let id = uuid::Uuid::new_v4().to_string();
    for args in std::iter::once(f.arm_args()).chain(ACTIONS.map(|action| f.call(action, &id))) {
        for value in [
            json!(""),
            Value::Null,
            json!("scope"),
            json!(false),
            json!({}),
        ] {
            let mut unknown = api_args(args.clone(), &json!(""));
            unknown["endpoint_url"] = value;
            assert!(request(&unknown).unwrap_err().contains("unknown field"));
            assert!(f.tool.describe(&unknown).is_err());
            assert!(!authorized(&f.tool.root, &unknown));
        }
        let fields: &[&str] = if args["action"] == "arm" {
            &["capability"]
        } else {
            &SCOPE
        };
        for field in fields {
            for value in [
                json!("scope"),
                json!(" "),
                json!(false),
                json!(0),
                json!([]),
                json!({}),
            ] {
                let mut invalid = api_args(args.clone(), &Value::Null);
                invalid[*field] = value;
                assert!(request(&invalid).is_err(), "{invalid}");
                assert!(f.tool.describe(&invalid).is_err());
                assert!(!authorized(&f.tool.root, &invalid));
            }
        }
    }
    for args in [
        json!({}),
        json!({"action":"launch", "capability":""}),
        json!({"action":null}),
        json!([]),
    ] {
        assert!(request(&args).is_err());
    }
}

#[test]
fn placeholders_do_not_supply_missing_or_null_required_fields() {
    let f = Fixture::new();
    let id = uuid::Uuid::new_v4().to_string();
    for args in std::iter::once(f.arm_args()).chain(ACTIONS.map(|action| f.call(action, &id))) {
        let fields: &[&str] = if args["action"] == "arm" {
            &SCOPE
        } else {
            &["capability"]
        };
        for field in fields.iter().copied().chain(std::iter::once("action")) {
            let mut missing = api_args(args.clone(), &json!(""));
            missing.as_object_mut().unwrap().remove(field);
            assert!(request(&missing).is_err(), "{missing}");
            missing[field] = Value::Null;
            assert!(request(&missing).is_err(), "{missing}");
        }
    }
}

#[tokio::test]
async fn empty_required_values_fail_before_any_cli_call() {
    let f = Fixture::new();
    for field in SCOPE {
        let mut args = api_args(f.arm_args(), &json!(""));
        args[field] = json!("");
        assert!(f.tool.run(&args).await.is_err(), "{args}");
    }
    for action in ACTIONS {
        let args = api_args(f.call(action, ""), &Value::Null);
        assert!(!authorized(&f.tool.root, &args));
        assert!(f.tool.run(&args).await.is_err(), "{args}");
    }
    assert!(!f.tool.root.join("argv").exists());
    assert!(!ledger(&f.tool.root).exists());
    assert!(f.mutations().is_empty());
}

#[tokio::test]
async fn api_shaped_calls_keep_approval_scope_and_expired_cleanup_boundaries() {
    let f = Fixture::new();
    let compact = f.arm_args();
    let arm = api_args(compact.clone(), &json!(""));
    assert!(!authorized(&f.tool.root, &arm));
    for mode in [Mode::Ask, Mode::Auto, Mode::Bypass] {
        let policy = f.policy(mode, Rules::default());
        let expected = match mode {
            Mode::Ask | Mode::Auto => Decision::Ask,
            Mode::Bypass => Decision::Allow("bypass mode".into()),
        };
        assert_eq!(policy.check(NAME, &compact, true), expected);
        assert_eq!(policy.check(NAME, &arm, true), expected);
        assert!(policy.judgeable(NAME, &arm).is_err());
    }
    assert!(matches!(
        f.policy(Mode::Auto, Rules::default()).judgeable(NAME, &arm),
        Err(crate::permissions::Reserved::Protected(message)) if message.contains("arm")
    ));
    let id = f.tool.run(&arm).await.unwrap()["capability"]
        .as_str()
        .unwrap()
        .to_string();
    let cap = serde_json::to_value(load(&f.tool.root, &id).unwrap()).unwrap();
    for field in SCOPE {
        assert_eq!(cap[field], arm[field]);
    }
    let schedule = std::fs::read(f.tool.root.join("schedule")).unwrap();
    let start = api_args(f.call("start", &id), &Value::Null);
    assert!(authorized(&f.tool.root, &start));
    let mut retargeted = start.clone();
    retargeted["region"] = json!("eu-west-1");
    assert!(f.tool.run(&retargeted).await.is_err());
    assert!(!authorized(&f.tool.root, &retargeted));
    assert_eq!(f.mutations(), "arm\n");
    f.tool.run(&start).await.unwrap();
    f.state("instance", "running");
    f.tool
        .run(&api_args(f.call("use", &id), &json!("")))
        .await
        .unwrap();
    let mut cap = load(&f.tool.root, &id).unwrap();
    cap.deadline = Utc::now() - chrono::Duration::minutes(1);
    save(&f.tool.root, &cap).unwrap();
    f.tool
        .run(&api_args(f.call("revoke", &id), &Value::Null))
        .await
        .unwrap();
    for action in ["start", "use"] {
        let args = api_args(f.call(action, &id), &json!(""));
        assert!(!authorized(&f.tool.root, &args));
        assert!(f.tool.run(&args).await.is_err());
    }
    let stop = api_args(f.call("stop", &id), &json!(""));
    assert!(authorized(&f.tool.root, &stop));
    f.tool.run(&stop).await.unwrap();
    f.state("instance", "stopped");
    let status = api_args(f.call("status", &id), &Value::Null);
    assert!(authorized(&f.tool.root, &status));
    assert_eq!(f.tool.run(&status).await.unwrap()["stopped"], true);
    assert_eq!(
        std::fs::read(f.tool.root.join("schedule")).unwrap(),
        schedule
    );
    assert_eq!(f.mutations(), "arm\nstart\nstop\n");
}
