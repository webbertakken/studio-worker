//! Round-trip tests for the WebSocket worker-channel wire format.
//!
//! Each variant is asserted against a literal JSON fixture so that the
//! Rust side never silently drifts from the TS `workerWs.ts` contract.
use serde_json::json;
use studio_worker::types::{JobClaim, LogEntry, Task, WorkerCapabilities};
use studio_worker::ws::types::{
    HelloFrame, JobOfferClaim, WorkerErrorCode, WorkerInbound, WorkerOutbound, WsCloseCode,
};

fn capabilities() -> WorkerCapabilities {
    WorkerCapabilities {
        machine_name: "rig-01".into(),
        username: "webber".into(),
        agent_version: "0.2.0".into(),
        engine: "synthetic".into(),
        vram_total_gb: 24.0,
        vram_threshold_gb: 12.0,
        auto_enabled: true,
        auto_start: false,
        supported_models: vec!["synthetic".into(), "sdxl".into()],
        task_kinds: vec![],
        supported_models_per_kind: Default::default(),
    }
}

fn sample_claim() -> JobOfferClaim {
    use studio_worker::types::{ImageParams, ModelCliDefaults, ModelEngine, ModelSource};
    JobOfferClaim {
        job_id: "job-1".into(),
        game_id: "game-of-elements".into(),
        asset_name: "game-of-elements/creatures/aurora-fox".into(),
        model: "synthetic".into(),
        vram_gb_estimate: 4.0,
        task: Task::Image(ImageParams {
            prompt: "an aurora fox at dusk".into(),
            width: 1024,
            height: 1024,
            ext: "webp".into(),
            ..Default::default()
        }),
        model_source: ModelSource {
            engine: ModelEngine::Synthetic,
            files: vec![],
            cli_defaults: ModelCliDefaults {
                cfg_scale: 1.0,
                steps: 8,
                width: 1024,
                height: 1024,
                sampling_method: None,
                ..Default::default()
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Inbound (worker → server)
// ---------------------------------------------------------------------------

#[test]
fn inbound_hello_round_trips() {
    let frame = WorkerInbound::Hello(HelloFrame {
        auth_token: "t0k3n".into(),
        capabilities: capabilities(),
    });
    let json = serde_json::to_value(&frame).unwrap();
    assert_eq!(json["type"], "hello");
    assert_eq!(json["authToken"], "t0k3n");
    assert_eq!(json["capabilities"]["machineName"], "rig-01");
    let back: WorkerInbound = serde_json::from_value(json).unwrap();
    match back {
        WorkerInbound::Hello(h) => {
            assert_eq!(h.auth_token, "t0k3n");
            assert_eq!(h.capabilities.machine_name, "rig-01");
        }
        other => panic!("expected Hello, got {other:?}"),
    }
}

#[test]
fn inbound_heartbeat_with_current_job_id_round_trips() {
    let json = json!({
        "type": "heartbeat",
        "capabilities": {
            "machineName": "rig-01",
            "username": "webber",
            "agentVersion": "0.2.0",
            "engine": "synthetic",
            "vramTotalGb": 24.0,
            "vramThresholdGb": 12.0,
            "autoEnabled": true,
            "autoStart": false,
            "supportedModels": [],
        },
        "currentJobId": "job-9",
    });
    let parsed: WorkerInbound = serde_json::from_value(json.clone()).unwrap();
    let serialised = serde_json::to_value(&parsed).unwrap();
    assert_eq!(serialised["type"], "heartbeat");
    assert_eq!(serialised["currentJobId"], "job-9");
    match parsed {
        WorkerInbound::Heartbeat { current_job_id, .. } => {
            assert_eq!(current_job_id.as_deref(), Some("job-9"))
        }
        other => panic!("expected Heartbeat, got {other:?}"),
    }
}

#[test]
fn inbound_heartbeat_without_current_job_id_round_trips() {
    let frame = WorkerInbound::Heartbeat {
        capabilities: capabilities(),
        current_job_id: None,
    };
    let json = serde_json::to_value(&frame).unwrap();
    // Optional + None must serialise as "field absent", not "null".
    assert!(json.get("currentJobId").is_none());
    let back: WorkerInbound = serde_json::from_value(json).unwrap();
    match back {
        WorkerInbound::Heartbeat { current_job_id, .. } => assert!(current_job_id.is_none()),
        other => panic!("expected Heartbeat, got {other:?}"),
    }
}

#[test]
fn inbound_heartbeat_with_explicit_null_current_job_id_parses() {
    let json = json!({
        "type": "heartbeat",
        "capabilities": capabilities_json(),
        "currentJobId": null,
    });
    let parsed: WorkerInbound = serde_json::from_value(json).unwrap();
    match parsed {
        WorkerInbound::Heartbeat { current_job_id, .. } => assert!(current_job_id.is_none()),
        other => panic!("expected Heartbeat, got {other:?}"),
    }
}

#[test]
fn inbound_accept_round_trips() {
    let json = json!({ "type": "accept", "jobId": "job-1" });
    let parsed: WorkerInbound = serde_json::from_value(json.clone()).unwrap();
    match parsed {
        WorkerInbound::Accept { ref job_id } => assert_eq!(job_id, "job-1"),
        ref other => panic!("expected Accept, got {other:?}"),
    }
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, json);
}

#[test]
fn inbound_reject_round_trips() {
    let json = json!({ "type": "reject", "jobId": "job-1", "reason": "model unloaded" });
    let parsed: WorkerInbound = serde_json::from_value(json.clone()).unwrap();
    match parsed {
        WorkerInbound::Reject {
            ref job_id,
            ref reason,
        } => {
            assert_eq!(job_id, "job-1");
            assert_eq!(reason, "model unloaded");
        }
        ref other => panic!("expected Reject, got {other:?}"),
    }
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, json);
}

#[test]
fn inbound_complete_json_round_trips_with_prompt() {
    let json = json!({
        "type": "completeJson",
        "jobId": "job-1",
        "result": { "role": "assistant", "content": "hi" },
        "prompt": "the final prompt",
    });
    let parsed: WorkerInbound = serde_json::from_value(json.clone()).unwrap();
    match parsed {
        WorkerInbound::CompleteJson {
            ref job_id,
            ref result,
            ref prompt,
        } => {
            assert_eq!(job_id, "job-1");
            assert_eq!(result["content"], "hi");
            assert_eq!(prompt.as_deref(), Some("the final prompt"));
        }
        ref other => panic!("expected CompleteJson, got {other:?}"),
    }
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, json);
}

#[test]
fn inbound_complete_json_without_prompt_omits_the_field() {
    let frame = WorkerInbound::CompleteJson {
        job_id: "job-2".into(),
        result: json!([{"text": "a"}, {"text": "b"}]),
        prompt: None,
    };
    let json = serde_json::to_value(&frame).unwrap();
    assert!(json.get("prompt").is_none());
    assert!(json["result"].is_array());
}

#[test]
fn inbound_fail_round_trips() {
    let json = json!({
        "type": "fail",
        "jobId": "job-1",
        "error": "oom",
        "retryable": true,
    });
    let parsed: WorkerInbound = serde_json::from_value(json.clone()).unwrap();
    match parsed {
        WorkerInbound::Fail {
            ref job_id,
            ref error,
            retryable,
        } => {
            assert_eq!(job_id, "job-1");
            assert_eq!(error, "oom");
            assert!(retryable);
        }
        ref other => panic!("expected Fail, got {other:?}"),
    }
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, json);
}

#[test]
fn inbound_log_batch_round_trips() {
    let entry = LogEntry {
        ts: "2026-05-25T12:00:00Z".into(),
        level: "info".into(),
        category: "engine".into(),
        message: "starting dispatch".into(),
        job_id: Some("job-1".into()),
    };
    let frame = WorkerInbound::LogBatch {
        entries: vec![entry.clone()],
    };
    let json = serde_json::to_value(&frame).unwrap();
    assert_eq!(json["type"], "logBatch");
    assert_eq!(json["entries"][0]["category"], "engine");
    let back: WorkerInbound = serde_json::from_value(json).unwrap();
    match back {
        WorkerInbound::LogBatch { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].message, "starting dispatch");
        }
        other => panic!("expected LogBatch, got {other:?}"),
    }
}

#[test]
fn inbound_log_batch_empty_round_trips() {
    let frame = WorkerInbound::LogBatch { entries: vec![] };
    let json = serde_json::to_value(&frame).unwrap();
    assert_eq!(json["entries"].as_array().unwrap().len(), 0);
}

#[test]
fn inbound_ready_for_more_round_trips() {
    let json = json!({ "type": "readyForMore" });
    let parsed: WorkerInbound = serde_json::from_value(json.clone()).unwrap();
    assert!(matches!(parsed, WorkerInbound::ReadyForMore));
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, json);
}

#[test]
fn inbound_unknown_type_fails_to_parse() {
    let json = json!({ "type": "totallyMadeUp", "jobId": "j" });
    let err = serde_json::from_value::<WorkerInbound>(json).unwrap_err();
    assert!(err.to_string().contains("totallyMadeUp"));
}

// ---------------------------------------------------------------------------
// Outbound (server → worker)
// ---------------------------------------------------------------------------

#[test]
fn outbound_welcome_round_trips() {
    let json = json!({
        "type": "welcome",
        "workerId": "w-1",
        "serverTime": "2026-05-25T12:00:00Z",
    });
    let parsed: WorkerOutbound = serde_json::from_value(json.clone()).unwrap();
    match parsed {
        WorkerOutbound::Welcome {
            ref worker_id,
            ref server_time,
        } => {
            assert_eq!(worker_id, "w-1");
            assert_eq!(server_time, "2026-05-25T12:00:00Z");
        }
        ref other => panic!("expected Welcome, got {other:?}"),
    }
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, json);
}

#[test]
fn outbound_offer_round_trips() {
    let synth_source = json!({
        "engine": "synthetic",
        "files": [],
        "cliDefaults": {
            "cfgScale": 1.0,
            "steps": 8,
            "width": 1024,
            "height": 1024,
        },
    });
    let claim_json = json!({
        "jobId": "job-1",
        "gameId": "game-of-elements",
        "assetName": "game-of-elements/creatures/aurora-fox",
        "model": "synthetic-image",
        "vramGbEstimate": 4.0,
        "task": {
            "kind": "image",
            "prompt": "an aurora fox at dusk",
            "width": 1024,
            "height": 1024,
            "ext": "webp",
        },
        "modelSource": synth_source,
    });
    let json = json!({ "type": "offer", "claim": claim_json });
    let parsed: WorkerOutbound = serde_json::from_value(json).unwrap();
    match parsed {
        WorkerOutbound::Offer { ref claim } => {
            assert_eq!(claim.job_id, "job-1");
            match &claim.task {
                Task::Image(p) => {
                    assert_eq!(p.ext, "webp");
                    assert_eq!(p.prompt, "an aurora fox at dusk");
                }
                other => panic!("expected image task, got {:?}", other.kind()),
            }
        }
        ref other => panic!("expected Offer, got {other:?}"),
    }
}

#[test]
fn outbound_offer_carrying_a_multimodal_task_round_trips() {
    let synth_source = json!({
        "engine": "synthetic",
        "files": [],
        "cliDefaults": {
            "cfgScale": 1.0,
            "steps": 8,
            "width": 1024,
            "height": 1024,
        },
    });
    let claim_json = json!({
        "jobId": "job-7",
        "gameId": "game-of-elements",
        "assetName": "game-of-elements/dialogue/elder-line",
        "model": "smol-llm",
        "vramGbEstimate": 6.0,
        "task": {
            "kind": "llm",
            "messages": [{ "role": "user", "content": "hi" }],
            "maxTokens": 32,
            "temperature": 0.5,
        },
        "modelSource": synth_source,
    });
    let json = json!({ "type": "offer", "claim": claim_json });
    let parsed: WorkerOutbound = serde_json::from_value(json).unwrap();
    match parsed {
        WorkerOutbound::Offer { claim } => {
            assert_eq!(claim.job_id, "job-7");
            assert!(matches!(claim.task, Task::Llm(_)));
        }
        other => panic!("expected Offer with task, got {other:?}"),
    }
}

#[test]
fn outbound_offer_without_task_or_model_source_fails_to_deserialise() {
    let json = json!({
        "type": "offer",
        "claim": {
            "jobId": "job-no-task",
            "gameId": "g",
            "assetName": "g/x/y",
            "model": "flux1-dev",
            "vramGbEstimate": 12.0,
        },
    });
    let err = serde_json::from_value::<WorkerOutbound>(json).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("task") || msg.contains("modelSource"),
        "expected missing-required-field error, got: {msg}"
    );
}

#[test]
fn outbound_heartbeat_ack_round_trips() {
    let json = json!({ "type": "heartbeatAck" });
    let parsed: WorkerOutbound = serde_json::from_value(json.clone()).unwrap();
    assert!(matches!(parsed, WorkerOutbound::HeartbeatAck));
    assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
}

#[test]
fn outbound_complete_ack_round_trips() {
    let json = json!({ "type": "completeAck", "jobId": "job-1" });
    let parsed: WorkerOutbound = serde_json::from_value(json.clone()).unwrap();
    match parsed {
        WorkerOutbound::CompleteAck { ref job_id } => assert_eq!(job_id, "job-1"),
        ref other => panic!("expected CompleteAck, got {other:?}"),
    }
    assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
}

#[test]
fn outbound_fail_ack_round_trips() {
    let json = json!({ "type": "failAck", "jobId": "job-1" });
    let parsed: WorkerOutbound = serde_json::from_value(json.clone()).unwrap();
    match parsed {
        WorkerOutbound::FailAck { ref job_id } => assert_eq!(job_id, "job-1"),
        ref other => panic!("expected FailAck, got {other:?}"),
    }
    assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
}

#[test]
fn outbound_error_round_trips_every_code() {
    for code in [
        WorkerErrorCode::AuthFailed,
        WorkerErrorCode::ProtocolViolation,
        WorkerErrorCode::DuplicateWorker,
        WorkerErrorCode::WorkerDeleted,
        WorkerErrorCode::InternalError,
    ] {
        let frame = WorkerOutbound::Error {
            code,
            message: "kaboom".into(),
        };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["message"], "kaboom");
        let back: WorkerOutbound = serde_json::from_value(json).unwrap();
        match back {
            WorkerOutbound::Error { code: c, .. } => assert_eq!(c, code),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}

// Confirm the wire string matches the TS contract for every code.
#[test]
fn outbound_error_codes_use_snake_case_wire_strings() {
    let pairs = [
        (WorkerErrorCode::AuthFailed, "auth_failed"),
        (WorkerErrorCode::ProtocolViolation, "protocol_violation"),
        (WorkerErrorCode::DuplicateWorker, "duplicate_worker"),
        (WorkerErrorCode::WorkerDeleted, "worker_deleted"),
        (WorkerErrorCode::InternalError, "internal_error"),
    ];
    for (code, wire) in pairs {
        let s = serde_json::to_value(code).unwrap();
        assert_eq!(s.as_str(), Some(wire), "wire mismatch for {code:?}");
    }
}

#[test]
fn outbound_unknown_type_fails_to_parse() {
    let json = json!({ "type": "wat" });
    let err = serde_json::from_value::<WorkerOutbound>(json).unwrap_err();
    assert!(err.to_string().contains("wat"));
}

// ---------------------------------------------------------------------------
// Close codes
// ---------------------------------------------------------------------------

#[test]
fn close_code_numeric_values_match_the_ts_contract() {
    assert_eq!(WsCloseCode::Normal as u16, 1000);
    assert_eq!(WsCloseCode::AuthFailed as u16, 4001);
    assert_eq!(WsCloseCode::ProtocolViolation as u16, 4002);
    assert_eq!(WsCloseCode::DuplicateWorker as u16, 4003);
    assert_eq!(WsCloseCode::WorkerDeleted as u16, 4004);
}

#[test]
fn close_code_for_error_maps_every_variant() {
    let pairs = [
        (WorkerErrorCode::AuthFailed, WsCloseCode::AuthFailed),
        (
            WorkerErrorCode::ProtocolViolation,
            WsCloseCode::ProtocolViolation,
        ),
        (
            WorkerErrorCode::DuplicateWorker,
            WsCloseCode::DuplicateWorker,
        ),
        (WorkerErrorCode::WorkerDeleted, WsCloseCode::WorkerDeleted),
        (WorkerErrorCode::InternalError, WsCloseCode::Normal),
    ];
    for (err, expected) in pairs {
        assert_eq!(
            WsCloseCode::from_error_code(err) as u16,
            expected as u16,
            "wrong close code for {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn capabilities_json() -> serde_json::Value {
    json!({
        "machineName": "rig-01",
        "username": "webber",
        "agentVersion": "0.2.0",
        "engine": "synthetic",
        "vramTotalGb": 24.0,
        "vramThresholdGb": 12.0,
        "autoEnabled": true,
        "autoStart": false,
        "supportedModels": [],
    })
}

#[test]
fn _example_helpers_compile() {
    let _ = capabilities();
    let _ = sample_claim();
    let _ = capabilities_json();
}

// Touch a function from the import surface so unused-import warnings
// from the helpers are quiet.
#[allow(dead_code)]
fn _unused_helper_touch(c: &JobClaim) -> &str {
    &c.asset_name
}
