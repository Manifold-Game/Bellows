use super::*;

fn event(kind: &str, session: &str, detail: &str) -> Event {
    Event {
        timestamp_ms: now_ms(),
        kind: kind.into(),
        crate_name: "fixture".into(),
        static_key: None,
        action_key: None,
        detail: detail.into(),
        reason: None,
        session_id: Some(session.into()),
        workspace: Some("/workspace".into()),
        duration_ms: None,
    }
}

#[test]
fn rotation_is_bounded_and_readers_include_retained_history() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    for i in 0..7 {
        append_with_limit(&path, &event("miss", &i.to_string(), "test"), 1).unwrap();
    }
    let events = read_log(&path).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].session_id.as_deref(), Some("4"));
    assert_eq!(events[2].session_id.as_deref(), Some("6"));
    assert!(!rotated(&path, 3).exists());
}

#[test]
fn concurrent_writers_and_rotation_keep_complete_records() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    let mut threads = Vec::new();
    for index in 0..4 {
        let path = path.clone();
        threads.push(std::thread::spawn(move || {
            for _ in 0..30 {
                append_with_limit(&path, &event("miss", &index.to_string(), "test"), 2048).unwrap();
            }
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    for file in [path.clone(), rotated(&path, 1), rotated(&path, 2)] {
        let contents = fs::read_to_string(file).unwrap();
        assert!(
            contents
                .lines()
                .all(|line| serde_json::from_str::<Event>(line).is_ok())
        );
    }
    assert!(!read_log(&path).unwrap().is_empty());
}

#[test]
fn latest_session_uses_start_order_even_when_an_older_build_finishes_last() {
    let events = vec![
        event("build_start", "old", ""),
        event("build_start", "new", ""),
        event("miss", "new", "new"),
        event("build_end", "new", ""),
        event("miss", "old", "old"),
        event("build_end", "old", ""),
    ];
    let selection = Selection {
        latest: true,
        ..Default::default()
    };
    let selected = select(events, &selection, "/workspace");
    assert_eq!(selected.len(), 3);
    assert!(
        selected
            .iter()
            .all(|e| e.session_id.as_deref() == Some("new"))
    );
}

#[test]
fn identity_history_names_changes_without_recording_values() {
    let temp = tempfile::tempdir().unwrap();
    let first = Fingerprint {
        key: digest_bytes(b"first"),
        components: BTreeMap::from([(
            "environment changed: RUSTFLAGS".into(),
            digest_bytes(b"--cfg a_secret_value"),
        )]),
    };
    assert!(
        remember_identity(temp.path(), "fixture", &first)
            .unwrap()
            .contains(NOT_CACHED_YET)
    );
    let next = Fingerprint {
        key: digest_bytes(b"next"),
        components: BTreeMap::from([(
            "environment changed: RUSTFLAGS".into(),
            digest_bytes(b"--cfg a_new_secret"),
        )]),
    };
    let detail = remember_identity(temp.path(), "fixture", &next).unwrap();
    assert!(detail.contains("environment changed: RUSTFLAGS"));
    assert_eq!(reason_code("miss", &detail), "environment_changed");
    assert!(!detail.contains("a_secret_value"));
    assert!(
        remember_identity(temp.path(), "fixture", &first)
            .unwrap()
            .contains(ENTRY_GONE)
    );
    for i in 0..20 {
        let mut fp = next.clone();
        fp.key = digest_bytes(i.to_string().as_bytes());
        remember_identity(temp.path(), "fixture", &fp).unwrap();
    }
    let bytes = fs::read(
        temp.path()
            .join("diagnostics")
            .join(format!("{}.json", digest_bytes(b"fixture"))),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Vec<Fingerprint>>(&bytes)
            .unwrap()
            .len(),
        8
    );
    assert!(!String::from_utf8(bytes).unwrap().contains("a_new_secret"));
}

#[test]
fn miss_explanations_and_their_legacy_phrasing_share_reason_codes() {
    for (current, legacy, code) in [
        (
            NOT_CACHED_YET.to_owned(),
            "first observed identity; no earlier local identity to compare (cold cache or new crate)",
            "cold_identity",
        ),
        (
            ENTRY_GONE.to_owned(),
            "previously observed identity has no usable cache entry; it may have been evicted or never published",
            "entry_missing",
        ),
        (
            format!("{BUILD_VARIANT} argument --cfg (a1 → b2) (compared with c3)"),
            "static identity changed: argument --cfg (a1 → b2) (compared with c3)",
            "identity_changed",
        ),
        (
            format!("{BUILD_VARIANT} compiler version changed (compared with c3)"),
            "static identity changed: compiler version changed (compared with c3)",
            "compiler_changed",
        ),
    ] {
        assert_eq!(reason_code("miss", &current), code, "{current}");
        assert_eq!(reason_code("miss", legacy), code, "{legacy}");
    }
    // Explanations describe the miss; none names disabled configuration.
    for detail in [NOT_CACHED_YET, ENTRY_GONE, BUILD_VARIANT] {
        assert!(!detail.contains("disabled"));
    }
    assert_eq!(reason_label("cold_identity"), "not cached yet");
    assert_eq!(reason_label("linked_output"), "linked outputs");
    assert_eq!(reason_label("some_new_reason"), "some new reason");
}

#[test]
fn legacy_events_and_malformed_lines_remain_readable() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    fs::write(&path, "{\"timestamp_ms\":1,\"kind\":\"miss\",\"crate_name\":\"old\",\"static_key\":null,\"action_key\":null,\"detail\":\"input changed: file.rs\"}\n{broken\n").unwrap();
    let events = read_log(&path).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(summarize(&events).reasons[0].reason, "input_changed");
}
