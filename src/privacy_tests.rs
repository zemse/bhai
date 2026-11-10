use super::*;
use crate::sessions::{Header, Writer};
use serde_json::json;

fn root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("bhai-private-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn private_raw_logging_refuses_activation_without_deleting_shared_logs() {
    let root = root();
    let path = root.join("shared-sse.log");
    std::fs::write(&path, "OTHER_SESSION_AND_CURRENT_STREAM").unwrap();
    assert!(
        raw_logging_disabled(Some(path.as_os_str()))
            .unwrap_err()
            .to_string()
            .contains("BHAI_DEBUG_SSE")
    );
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "OTHER_SESSION_AND_CURRENT_STREAM"
    );
    assert!(raw_logging_disabled(None).is_ok());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn private_legacy_resumed_compaction_recovers_prompts_before_erasure() {
    let root = root();
    let dir = root.join(crate::sessions::DIR);
    let path = crate::sessions::path(&dir, "resumed");
    let mut writer = Writer::create(
        &dir,
        Header::new("resumed", "general", "m", "medium", &root),
    );
    writer.append(&json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":"LEGACY_ORIGINAL_SECRET"}]})).unwrap();
    writer.compact("test", 100, 10, &[json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":"summary without original wording"}]})]).unwrap();
    drop(writer);
    let loaded = crate::sessions::load(&path).unwrap();
    assert!(
        !serde_json::to_string(&loaded.items)
            .unwrap()
            .contains("LEGACY_ORIGINAL_SECRET")
    );
    let mut writer = Writer::resume(&dir, &loaded).unwrap();
    let inputs = writer.recorded_prompts().unwrap();
    assert!(inputs.contains(&"LEGACY_ORIGINAL_SECRET".to_string()));
    let history = root.join(".config/bhai/history.jsonl");
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(&history, "{\"text\":\"LEGACY_ORIGINAL_SECRET\"}\n{\"text\":\"LEGACY_ORIGINAL_SECRET\",\"session\":\"unrelated\"}\n").unwrap();
    writer.erase().unwrap();
    erase_history(Some(&root), &inputs, &["resumed"]).unwrap();
    let kept: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(history).unwrap().trim()).unwrap();
    assert_eq!(kept["session"], "unrelated");
    assert!(!path.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn private_transcript_erasure_and_clear_do_not_replay_private_items() {
    let root = root();
    let dir = root.join(crate::sessions::DIR);
    let path = crate::sessions::path(&dir, "current");
    let privacy = Arc::new(Privacy::default());
    CURRENT.scope(RefCell::new(Arc::clone(&privacy)), async {
        let mut writer = Writer::create(&dir, Header::new("current", "general", "m", "medium", &root));
        writer.append(&json!({"role":"user", "content":"PUBLIC_BEFORE"})).unwrap();
        let owned = crate::sessions::state_dir(&root, "current");
        std::fs::create_dir_all(&owned).unwrap();
        std::fs::write(owned.join("child-one.jsonl"), "CHILD_PRIVATE_MARKER").unwrap();
        let unrelated = dir.join("unrelated.jsonl");
        std::fs::write(&unrelated, "KEEP_OTHER_SESSION").unwrap();
        privacy.begin();
        writer.erase().unwrap();
        privacy.finish(true);
        writer.append(&json!({"role":"user", "content":[{"type":"input_text", "text":"PRIVATE_NEVER_SAVE"},{"type":"input_image", "image_url":"data:image/png;base64,PRIVATE_ATTACHMENT"}]})).unwrap();
        writer.compact("private", 50, 20, &[json!("PRIVATE_SUMMARY")]).unwrap();
        assert!(!path.exists());
        assert!(!owned.exists());
        assert_eq!(std::fs::read_to_string(&unrelated).unwrap(), "KEEP_OTHER_SESSION");
        assert!(writer.fork(&[json!("PRIVATE_FORK")], None).is_err());
        assert!(crate::sessions::private_write(&root.join("debug.json"), "PRIVATE_DEBUG").is_err());
        privacy.clear();
        writer.append(&json!({"role":"user", "content":"FRESH_PUBLIC"})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("FRESH_PUBLIC"));
        assert!(!text.contains("PRIVATE_NEVER_SAVE") && !text.contains("PRIVATE_ATTACHMENT") && !text.contains("PUBLIC_BEFORE"));
    }).await;
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn private_history_erasure_keeps_other_prompts() {
    let root = root();
    let dir = root.join(".config/bhai");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("history.jsonl");
    std::fs::write(
        &path,
        "{\"text\":\"KEEP_UNRELATED\"}\n{\"text\":\"CURRENT_SECRET\"}\n",
    )
    .unwrap();
    erase_history(Some(&root), &["CURRENT_SECRET".into()], &["current"]).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"text\":\"KEEP_UNRELATED\"}\n"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn private_erasure_failure_is_latched_and_exports_cannot_cross_clear() {
    let privacy = Privacy::default();
    let epoch = privacy.epoch();
    privacy.begin();
    privacy.finish(false);
    assert!(privacy.active() && !privacy.ready());
    assert!(privacy.public(|| Ok(())).is_err());
    privacy.finish(true);
    assert!(privacy.active() && privacy.ready());
    privacy.clear();
    assert!(!privacy.active() && privacy.ready());
    assert!(privacy.public_at(epoch, || Ok(())).is_err());
}

#[tokio::test]
async fn private_recording_policy_is_task_local() {
    let privacy = Arc::new(Privacy::default());
    privacy.begin();
    CURRENT
        .scope(RefCell::new(privacy), async {
            assert!(active());
            assert!(!tokio::spawn(async { active() }).await.unwrap());
            assert!(
                crate::tools::Registry::new(Vec::new())
                    .get("agent")
                    .is_none()
            );
        })
        .await;
    assert!(!active());
}
