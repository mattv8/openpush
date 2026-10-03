//! Regression tests for the DU security review (reports/DU-security-review.md).
use crate::{
    credentials::{parse_credential, tests::credential_json, tests::TOKEN},
    secure_store::{MemoryStore, SecretStore},
    session::Session,
    sync::{self, classify, next_backoff, Failure, MediaOutcome, STABLE_CONNECTION},
    tests::{fixture_at, gateway, input, noop, open, routed, PHRASE},
    AppState, PreparedPublicCopy,
};
use axum::{
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use openpush_client_core::{Cursor, RawSnapshotRecord, SnapshotPurpose};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU16, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const TOKEN2: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{address}")
}

// ---- 1. Per-window app-command ACL -------------------------------------------------------

fn permissions(json: &str) -> HashSet<String> {
    let value: Value = serde_json::from_str(json).unwrap();
    value["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_owned())
        .collect()
}

fn declared_commands() -> Vec<String> {
    let build = include_str!("../build.rs");
    let list = &build[build.find("const COMMANDS").unwrap()..build.find("];").unwrap()];
    list.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

fn allow(command: &str) -> String {
    format!("allow-{}", command.replace('_', "-"))
}

#[test]
fn app_acl_restricts_composer_windows_to_conversation_operations() {
    let commands = declared_commands();
    let registered = include_str!("lib.rs");
    for command in &commands {
        assert!(
            registered.contains(&format!("fn {command}(")),
            "{command} declared in the app manifest but not defined"
        );
    }
    let composer = permissions(include_str!("../capabilities/composer.json"));
    let main = permissions(include_str!("../capabilities/default.json"));
    let app_permissions = |set: &HashSet<String>| {
        set.iter()
            .filter(|p| !p.contains(':'))
            .cloned()
            .collect::<HashSet<_>>()
    };
    let expected: HashSet<String> = crate::COMPOSER_COMMANDS.iter().map(|c| allow(c)).collect();
    assert!(expected.contains("allow-close-head-panel"));
    assert_eq!(app_permissions(&composer), expected);
    for main_only in [
        "configure_server",
        "import_credentials",
        "unlock_sync",
        "publish_attachment",
        "open_composer",
        "set_start_at_login",
        "popout_conversation",
        "hide_head",
    ] {
        assert!(
            !composer.contains(&allow(main_only)),
            "composer can call {main_only}"
        );
        assert!(main.contains(&allow(main_only)), "main lacks {main_only}");
    }
    for command in commands
        .iter()
        .filter(|c| **c != "close_composer" && **c != "close_head_panel")
    {
        assert!(
            main.contains(&allow(command)),
            "main window cannot call {command}"
        );
    }
    // Composer windows cannot minimize/maximize the main shell either.
    assert!(!composer.contains("core:window:allow-minimize"));
    assert!(!main.contains("allow-close-head-panel"));
}

#[test]
fn runtime_window_checks_back_the_acl() {
    let conversation = openpush_client_core::ConversationId::new().to_string();
    let label = format!("composer-{conversation}");
    assert!(crate::require_main("main").is_ok());
    assert_eq!(
        crate::require_main(&label).unwrap_err().code,
        "window-context"
    );
    assert!(crate::check_conversation_scope("main", None).is_ok());
    assert!(crate::check_conversation_scope(&label, Some(&conversation)).is_ok());
    let other = openpush_client_core::ConversationId::new().to_string();
    assert!(crate::check_conversation_scope(&label, Some(&other)).is_err());
    assert!(crate::check_conversation_scope(&label, None).is_err());
    assert!(crate::check_conversation_scope("head-x", Some(&conversation)).is_err());
}

// ---- 2. Serialized import, token rotation and session cancellation -----------------------

fn credential(
    origin: &str,
    vault: &str,
    device: &str,
    token: &str,
) -> Arc<crate::credentials::ImportedCredential> {
    Arc::new(parse_credential(&credential_json(origin, vault, device, token)).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_rotation_replaces_and_cancels_the_session_and_keeps_the_key() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::default());
    let state = AppState::new(root.path().to_path_buf(), store.clone(), noop());
    let (vault, device) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    let origin = "http://127.0.0.1:9";
    // Two concurrent first imports of the same device serialize: one key, one session.
    let (a, b) = tokio::join!(
        crate::activate_import(&state, credential(origin, &vault, &device, TOKEN)),
        crate::activate_import(&state, credential(origin, &vault, &device, TOKEN)),
    );
    a.unwrap();
    b.unwrap();
    let first = state.session().await.unwrap().unwrap();
    let key = store
        .get(&first.binding.db_key_account())
        .unwrap()
        .unwrap()
        .to_vec();
    first
        .client
        .save_compose_draft(
            first.client.create_compose_draft(None).unwrap().draft_id,
            0,
            Default::default(),
        )
        .unwrap();

    let head_generation = state.head_generation.lock().unwrap().current();
    crate::activate_import(&state, credential(origin, &vault, &device, TOKEN2))
        .await
        .unwrap();
    assert_eq!(
        state.head_generation.lock().unwrap().current(),
        head_generation,
        "same-binding restart preserves head topology generation"
    );
    let second = state.session().await.unwrap().unwrap();
    assert!(
        !Arc::ptr_eq(&first, &second),
        "a rotated token gets a fresh session"
    );
    assert!(
        first.cancel.is_cancelled(),
        "the old session's networking is cancelled"
    );
    assert_eq!(second.api.bearer(), TOKEN2);
    assert_eq!(
        store
            .get(&second.binding.db_key_account())
            .unwrap()
            .unwrap()
            .to_vec(),
        key,
        "database key preserved"
    );
    assert_eq!(
        second.client.compose_drafts().unwrap().len(),
        1,
        "database still readable"
    );

    // Switching origin deactivates (and cancels) the session without deleting anything.
    state
        .update_config(|config| config.select_origin("https://other.test"))
        .unwrap();
    assert!(state.session().await.unwrap().is_none());
    assert!(second.cancel.is_cancelled());
    state
        .update_config(|config| config.select_origin(origin))
        .unwrap();
    let restored = state.session().await.unwrap().unwrap();
    assert_eq!(restored.binding, second.binding);
    assert_eq!(restored.client.compose_drafts().unwrap().len(), 1);
    // A credential for another origin is refused while this server is configured.
    let foreign = crate::activate_import(
        &state,
        credential(
            "https://elsewhere.test",
            &vault,
            &uuid::Uuid::new_v4().to_string(),
            TOKEN,
        ),
    )
    .await;
    assert_eq!(foreign.unwrap_err().code, "origin-binding");
    // The full import path refuses before any network request (otherwise this would be `offline`).
    let network_path = crate::import_credential(
        &state,
        credential(
            "https://elsewhere.invalid",
            &vault,
            &uuid::Uuid::new_v4().to_string(),
            TOKEN,
        ),
    )
    .await;
    assert_eq!(network_path.unwrap_err().code, "origin-binding");
    state.close_session().await;
    assert!(restored.cancel.is_cancelled());
}

// ---- 3. Impossible snapshot cuts are abandoned -------------------------------------------

async fn snapshot_mock(stale_reply: &'static str) -> (String, Arc<AtomicUsize>) {
    let stale_hits = Arc::new(AtomicUsize::new(0));
    let hits = stale_hits.clone();
    let router = Router::new()
        .route("/v1/snapshot", get(|| async { Json(json!({"snapshot_version":1,"high_water_cursor":"2","record_count":"2","max_page_size":200})) }))
        .route(
            "/v1/snapshot/records",
            get(move |Query(query): Query<HashMap<String, String>>| {
                let hits = hits.clone();
                async move {
                    if query.get("high_water").map(String::as_str) == Some("5") {
                        hits.fetch_add(1, Ordering::SeqCst);
                        return if stale_reply == "409" {
                            (StatusCode::CONFLICT, Json(json!({"code":"resync_required","reason":"cursor_ahead"})))
                        } else {
                            (StatusCode::OK, Json(json!({"high_water_cursor":"5","records":[],"next_after":null})))
                        };
                    }
                    let after: u64 = query.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
                    let records: Vec<Value> = (1..=2u64).filter(|c| *c > after).map(|c| json!({"cursor":c.to_string(),"envelope":{"malformed":c}})).collect();
                    (StatusCode::OK, Json(json!({"high_water_cursor":"2","records":records,"next_after":null})))
                }
            }),
        );
    (serve(router).await, stale_hits)
}

async fn stale_cut_is_restarted(stale_reply: &'static str) {
    let (origin, stale_hits) = snapshot_mock(stale_reply).await;
    let f = fixture_at(&origin);
    let session = Arc::new(open(&f, &f.binding, &[]));
    session
        .client
        .unlock(&f.profile, &f.header, PHRASE)
        .unwrap();
    // A persisted, partially staged cut (H=5) that the server can no longer serve.
    let stale = session
        .client
        .begin_snapshot(Cursor(5), 5, SnapshotPurpose::Resync)
        .unwrap();
    let raw = |c: u64| RawSnapshotRecord {
        cursor: Cursor(c),
        envelope_json: format!("{{\"malformed\":{c}}}").into_bytes(),
    };
    session
        .client
        .append_snapshot_raw_page(stale.generation, &[raw(1), raw(2)])
        .unwrap();

    sync::snapshot_resync(&session).await.unwrap();
    assert_eq!(
        stale_hits.load(Ordering::SeqCst),
        1,
        "the impossible cut is tried once, then abandoned"
    );
    assert_eq!(
        *session.abandoned_snapshot.lock().unwrap(),
        Some(stale.generation)
    );
    assert_eq!(session.client.receive_cursor().unwrap(), Cursor(2));
    assert_eq!(
        session.client.quarantined().unwrap().len(),
        2,
        "raw malformed records reach core quarantine"
    );
    assert!(!session.client.restore_guarded().unwrap());
    // A later resync never resumes the abandoned generation.
    sync::snapshot_resync(&session).await.unwrap();
    assert_eq!(stale_hits.load(Ordering::SeqCst), 1);
    session.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_cut_rejected_by_server_is_abandoned_and_restarted() {
    stale_cut_is_restarted("409").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_cut_that_ends_early_is_abandoned_and_restarted() {
    stale_cut_is_restarted("empty").await;
}

// ---- 4. Media retry policy ----------------------------------------------------------------

#[test]
fn media_failures_are_classified() {
    use crate::net::NetError;
    let status = |status| Failure::Net(NetError::Status { status, code: None });
    assert_eq!(
        classify(&Failure::Net(NetError::Offline)),
        MediaOutcome::Fatal
    );
    assert_eq!(
        classify(&Failure::Net(NetError::Revoked)),
        MediaOutcome::Fatal
    );
    for transient in [408, 429, 500, 502, 503, 504] {
        assert_eq!(
            classify(&status(transient)),
            MediaOutcome::Retry,
            "{transient}"
        );
    }
    for permanent in [400, 403, 404, 413, 415, 422] {
        assert_eq!(
            classify(&status(permanent)),
            MediaOutcome::Permanent,
            "{permanent}"
        );
    }
    assert_eq!(
        classify(&Failure::Net(NetError::Invalid)),
        MediaOutcome::Permanent
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_media_errors_are_retried_and_not_cached() {
    let reply = Arc::new(AtomicU16::new(503));
    let reserves = Arc::new(AtomicUsize::new(0));
    let (r, n) = (reply.clone(), reserves.clone());
    let router = Router::new().route(
        "/v1/attachments/reserve",
        post(move || {
            let (r, n) = (r.clone(), n.clone());
            async move {
                n.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::from_u16(r.load(Ordering::SeqCst)).unwrap(),
                    Json(json!({"code":"busy"})),
                )
            }
        }),
    );
    let origin = serve(router).await;
    let f = fixture_at(&origin);
    let session = Arc::new(open(&f, &f.binding, &[]));
    session
        .client
        .unlock(&f.profile, &f.header, PHRASE)
        .unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();
    session.set_status(|s| {
        s.gateways = vec![gateway(&gateway_id, true, true)];
        s.gateways_known = true;
    });
    let png = f.dir.path().join("p.png");
    std::fs::write(&png, crate::media::tests::sample_png(8, 8)).unwrap();
    let picked = session.prepare_attachment(&png).unwrap();
    let mut draft = input("draft-new", "", "pic", &["+15555550100"], "0");
    draft.attachment_ids = vec![picked.id.clone()];
    let draft = session.save_draft(&draft).unwrap();
    assert!(
        session
            .send_draft(&routed(
                input(&draft.id, "", "", &[], &draft.revision),
                &gateway_id,
                "sim-1"
            ))
            .unwrap()
            .accepted
    );

    for status in [503, 429, 502] {
        reply.store(status, Ordering::SeqCst);
        sync::work_round(&session).await.unwrap();
        assert!(
            session.transfer_errors.lock().unwrap().is_empty(),
            "{status} must not be cached as permanent"
        );
        assert_eq!(
            session.status.lock().unwrap().work_error,
            Some("media-retry")
        );
        assert_eq!(session.client.pending_uploads().unwrap().len(), 1);
    }
    assert_eq!(reserves.load(Ordering::SeqCst), 3, "retried every round");
    reply.store(400, Ordering::SeqCst);
    sync::work_round(&session).await.unwrap();
    assert_eq!(
        session.transfer_errors.lock().unwrap().len(),
        1,
        "deterministic rejection is shown"
    );
    sync::work_round(&session).await.unwrap();
    assert_eq!(
        reserves.load(Ordering::SeqCst),
        4,
        "permanent failures are not hammered"
    );
    session.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserve_conflict_recovers_only_from_authenticated_duplicate_finalize_proof() {
    let finalized = Arc::new(AtomicUsize::new(0));
    let finalized_count = finalized.clone();
    let router = Router::new()
        .route(
            "/v1/attachments/reserve",
            post(|headers: HeaderMap| async move {
                assert_eq!(
                    headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok()),
                    Some(format!("Bearer {TOKEN}").as_str())
                );
                (
                    StatusCode::CONFLICT,
                    Json(json!({"code":"attachment_reservation_conflict"})),
                )
            }),
        )
        .route(
            "/v1/attachments/{id}/finalize",
            post(move |AxumPath(id): AxumPath<String>, headers: HeaderMap| {
                let finalized_count = finalized_count.clone();
                async move {
                    assert_eq!(
                        headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok()),
                        Some(format!("Bearer {TOKEN}").as_str())
                    );
                    finalized_count.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        Json(json!({"attachment_id":id,"duplicate":true})),
                    )
                }
            }),
        );
    let origin = serve(router).await;
    let f = fixture_at(&origin);
    let session = Arc::new(open(&f, &f.binding, &[]));
    session
        .client
        .unlock(&f.profile, &f.header, PHRASE)
        .unwrap();
    let file = f.dir.path().join("expired-finalized.png");
    std::fs::write(&file, crate::media::tests::sample_png(8, 8)).unwrap();
    let attachment = session.prepare_attachment(&file).unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();
    session.set_status(|status| {
        status.gateways = vec![gateway(&gateway_id, true, true)];
        status.gateways_known = true;
    });
    let mut draft = input("draft-new", "", "upload", &["+15555550100"], "0");
    draft.attachment_ids = vec![attachment.id.clone()];
    let draft = session.save_draft(&draft).unwrap();
    session
        .send_draft(&routed(
            input(&draft.id, "", "", &[], &draft.revision),
            &gateway_id,
            "sim-1",
        ))
        .unwrap();
    let object = session.client.pending_uploads().unwrap().remove(0);

    assert!(sync::upload_one(&session, object).await.is_ok());

    assert_eq!(finalized.load(Ordering::SeqCst), 1);
    assert!(session.client.pending_uploads().unwrap().is_empty());
    assert!(session
        .client
        .attachment_info(attachment.id.parse().unwrap())
        .unwrap()
        .state
        .is_local());
    session.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserve_conflict_does_not_mark_upload_without_duplicate_finalize_proof() {
    let router = Router::new()
        .route(
            "/v1/attachments/reserve",
            post(|| async {
                (
                    StatusCode::CONFLICT,
                    Json(json!({"code":"attachment_reservation_conflict"})),
                )
            }),
        )
        .route(
            "/v1/attachments/{id}/finalize",
            post(|AxumPath(id): AxumPath<String>| async move {
                (
                    StatusCode::OK,
                    Json(json!({"attachment_id":id,"duplicate":false})),
                )
            }),
        );
    let origin = serve(router).await;
    let f = fixture_at(&origin);
    let session = Arc::new(open(&f, &f.binding, &[]));
    session
        .client
        .unlock(&f.profile, &f.header, PHRASE)
        .unwrap();
    let file = f.dir.path().join("not-duplicate.png");
    std::fs::write(&file, crate::media::tests::sample_png(8, 8)).unwrap();
    let attachment = session.prepare_attachment(&file).unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();
    session.set_status(|status| {
        status.gateways = vec![gateway(&gateway_id, true, true)];
        status.gateways_known = true;
    });
    let mut draft = input("draft-new", "", "upload", &["+15555550100"], "0");
    draft.attachment_ids = vec![attachment.id];
    let draft = session.save_draft(&draft).unwrap();
    session
        .send_draft(&routed(
            input(&draft.id, "", "", &[], &draft.revision),
            &gateway_id,
            "sim-1",
        ))
        .unwrap();
    let object = session.client.pending_uploads().unwrap().remove(0);

    assert!(sync::upload_one(&session, object).await.is_err());
    assert_eq!(session.client.pending_uploads().unwrap().len(), 1);
    session.cancel.cancel();
}

// ---- 5. Wake separation and stable backoff ------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_requests_wake_only_the_worker() {
    let f = fixture_at("http://127.0.0.1:9");
    let session: Arc<Session> = Arc::new(open(&f, &f.binding, &[]));
    let (worker_session, live_session) = (session.clone(), session.clone());
    let live =
        tokio::spawn(async move { sync::pause_live(&live_session, Duration::from_secs(30)).await });
    let worker =
        tokio::spawn(
            async move { sync::pause_worker(&worker_session, Duration::from_secs(30)).await },
        );
    tokio::time::sleep(Duration::from_millis(100)).await;
    session.request_work();
    assert!(tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("worker woke")
        .unwrap());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !live.is_finished(),
        "a reconnect wait must not consume the worker's wake-up"
    );
    session.reconnect_wake.notify_one();
    assert!(tokio::time::timeout(Duration::from_secs(2), live)
        .await
        .unwrap()
        .unwrap());
    // Cancellation ends both waits with `false`.
    let waiter = session.clone();
    let cancelled =
        tokio::spawn(async move { sync::pause_worker(&waiter, Duration::from_secs(30)).await });
    session.cancel.cancel();
    assert!(!tokio::time::timeout(Duration::from_secs(2), cancelled)
        .await
        .unwrap()
        .unwrap());
}

#[test]
fn reconnect_backoff_resets_only_after_a_stable_connection() {
    let mut backoff = Duration::from_secs(1);
    for _ in 0..10 {
        backoff = next_backoff(backoff, false);
    }
    assert_eq!(
        backoff,
        Duration::from_secs(60),
        "flapping connections back off to the cap"
    );
    assert_eq!(next_backoff(backoff, false), Duration::from_secs(60));
    assert_eq!(next_backoff(backoff, true), Duration::from_secs(1));
    assert!(STABLE_CONNECTION >= Duration::from_secs(30));
}

// ---- 6. Public-copy confirmation names the image ------------------------------------------

#[test]
fn public_copy_confirmation_names_the_exposed_image() {
    let prepared = PreparedPublicCopy {
        bytes: vec![0; 3000],
        name: "holidayphoto.jpg".into(),
        width: 640,
        height: 480,
    };
    let prompt = crate::public_copy_prompt("holiday photo\u{0007}.jpg", &prepared);
    assert!(prompt.contains("\"holiday photo.jpg\""), "{prompt}");
    assert!(
        prompt.contains("holidayphoto.jpg")
            && prompt.contains("640×480")
            && prompt.contains("3 KiB")
    );
    assert!(prompt.contains("SEPARATE, server-readable"));
    assert!(!prompt.contains('/'), "no local path in the prompt");
}

// ---- 7. Failed sends never strand the UI on a stale revision ------------------------------

#[test]
fn refused_sends_keep_the_revision_and_results_report_it() {
    let f = fixture_at("http://127.0.0.1:9");
    let session = open(&f, &f.binding, &[]);
    session
        .client
        .unlock(&f.profile, &f.header, PHRASE)
        .unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();
    session.set_status(|s| {
        s.gateways = vec![gateway(&gateway_id, true, false)];
        s.gateways_known = true;
    });
    let draft = session
        .save_draft(&input(
            "draft-new",
            "",
            "hi",
            &["+15555550100", "+15555550101"],
            "0",
        ))
        .unwrap();
    let id = draft.id.parse().unwrap();
    // Two recipients require capability-v2 MMS: refusal preserves the draft revision.
    let error = session
        .send_draft(&routed(
            input(&draft.id, "", "", &[], &draft.revision),
            &gateway_id,
            "sim-1",
        ))
        .unwrap_err();
    assert_eq!(error.code, "mms-unsupported");
    assert_eq!(
        session
            .client
            .compose_draft(id)
            .unwrap()
            .unwrap()
            .revision
            .to_string(),
        draft.revision
    );
    // A stale send reports the current revision so the UI can continue.
    let stale = session
        .send_draft(&routed(
            input(&draft.id, "", "", &[], "0"),
            &gateway_id,
            "sim-1",
        ))
        .unwrap_err();
    assert_eq!(
        (stale.code, stale.current_revision.as_deref()),
        ("stale-draft", Some(draft.revision.as_str()))
    );
    assert!(serde_json::to_string(&stale)
        .unwrap()
        .contains("\"currentRevision\""));
    // A successful send (route persisted + atomic send) reports the stored revision.
    let fixed = session
        .save_draft(&input(
            &draft.id,
            "",
            "hi",
            &["+15555550100"],
            &draft.revision,
        ))
        .unwrap();
    let sent = session
        .send_draft(&routed(
            input(&fixed.id, "", "", &[], &fixed.revision),
            &gateway_id,
            "sim-1",
        ))
        .unwrap();
    assert_eq!(
        sent.revision,
        Some(
            session
                .client
                .compose_draft(id)
                .unwrap()
                .unwrap()
                .revision
                .to_string()
        )
    );
}

// ---- 8. A composer cannot send another conversation's draft --------------------------------

#[test]
fn send_rejects_a_draft_from_another_conversation_without_mutation() {
    let f = fixture_at("http://127.0.0.1:9");
    let session = open(&f, &f.binding, &[]);
    session
        .client
        .unlock(&f.profile, &f.header, PHRASE)
        .unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();
    session.set_status(|s| {
        s.gateways = vec![gateway(&gateway_id, true, false)];
        s.gateways_known = true;
    });
    let victim = session
        .save_draft(&input("draft-new", "", "secret", &["+15555550100"], "0"))
        .unwrap();
    let own = session
        .save_draft(&input("draft-new", "", "mine", &["+15555550101"], "0"))
        .unwrap();
    assert_ne!(victim.conversation_id, own.conversation_id);
    // A composer scoped to `own` passes the window scope check with its own conversation ID
    // but names the other conversation's draft (whose stored route differs, forcing a save).
    let label = format!("composer-{}", own.conversation_id);
    assert!(crate::check_conversation_scope(&label, Some(&own.conversation_id)).is_ok());
    let forged = routed(
        input(&victim.id, &own.conversation_id, "", &[], &victim.revision),
        &gateway_id,
        "sim-1",
    );
    assert_eq!(
        session.send_draft(&forged).unwrap_err().code,
        "invalid-draft"
    );
    let stored = session
        .client
        .compose_draft(victim.id.parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.revision.to_string(),
        victim.revision,
        "no revision bump"
    );
    assert_eq!(stored.text, "secret");
    assert!(stored.route.is_none(), "route not persisted");
    assert!(
        session.client.pending_outbox_batch(10).unwrap().is_empty(),
        "nothing queued"
    );
    // The legitimate send for the matching conversation still works.
    let ok = routed(
        input(&own.id, &own.conversation_id, "", &[], &own.revision),
        &gateway_id,
        "sim-1",
    );
    assert!(session.send_draft(&ok).unwrap().accepted);
}
