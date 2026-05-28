//! Exercise every TaskKind end-to-end through the synthetic engine and
//! verify the wire format (kind → dispatch → result) is internally
//! consistent.  All tests are GPU-free and run on free-tier CI.

use std::io::Cursor;
use studio_worker::config::Config;
use studio_worker::engine;
use studio_worker::types::*;

fn synth_engine() -> Box<dyn engine::Engine> {
    let cfg = Config::default();
    engine::build(&cfg).expect("synthetic engine should build")
}

#[test]
fn dispatch_image_round_trips_through_webp_decoder() {
    let task = Task::Image(ImageParams {
        prompt: "a stone golem".into(),
        width: 512,
        height: 512,
        steps: 20,
        ext: "webp".into(),
        ..Default::default()
    });
    let res = synth_engine().dispatch("synthetic", task).unwrap();
    let (bytes, ext) = match res {
        TaskResult::Image { bytes, ext } => (bytes, ext),
        _ => panic!("expected image"),
    };
    assert_eq!(ext, "webp");
    let img = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .unwrap()
        .decode()
        .expect("real WebP");
    assert_eq!(img.width(), 512);
    assert_eq!(img.height(), 512);
}

#[test]
fn dispatch_llm_returns_chat_completion_shape() {
    let task = Task::Llm(LlmParams {
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "two plus two".into(),
        }],
        max_tokens: 32,
        temperature: 0.5,
        ..Default::default()
    });
    let res = synth_engine().dispatch("synthetic-llm", task).unwrap();
    let json = match res {
        TaskResult::Llm { json } => json,
        _ => panic!("expected llm"),
    };
    assert_eq!(json["object"], "chat.completion");
    let content = json["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(content.starts_with("[synthetic]"));
}

#[test]
fn dispatch_stt_returns_whisper_shape() {
    let task = Task::AudioStt(AudioSttParams {
        input_url: "https://example.com/clip.wav".into(),
        language: Some("nl".into()),
        ..Default::default()
    });
    let res = synth_engine().dispatch("synthetic-stt", task).unwrap();
    let json = match res {
        TaskResult::AudioStt { json } => json,
        _ => panic!("expected stt"),
    };
    assert_eq!(json["language"], "nl");
    assert!(json["text"].as_str().unwrap().starts_with("[synthetic]"));
}

#[test]
fn dispatch_tts_round_trips_through_wav_decoder() {
    let task = Task::AudioTts(AudioTtsParams {
        text: "hello cruel world".into(),
        voice: "default".into(),
        ext: "wav".into(),
        ..Default::default()
    });
    let res = synth_engine().dispatch("synthetic-tts", task).unwrap();
    let (bytes, ext) = match res {
        TaskResult::AudioTts { bytes, ext } => (bytes, ext),
        _ => panic!("expected tts"),
    };
    assert_eq!(ext, "wav");
    // RIFF header
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let reader = hound::WavReader::new(Cursor::new(bytes)).expect("real WAV");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 22_050);
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.bits_per_sample, 16);
}

#[test]
fn dispatch_video_emits_decodable_bytes() {
    let task = Task::Video(VideoParams {
        prompt: "a tiny dragon flapping wings".into(),
        seconds: 2.0,
        width: 256,
        height: 256,
        ext: "mp4".into(),
        ..Default::default()
    });
    let res = synth_engine().dispatch("synthetic-video", task).unwrap();
    let (bytes, ext) = match res {
        TaskResult::Video { bytes, ext } => (bytes, ext),
        _ => panic!("expected video"),
    };
    // Synthetic video downgrades to WebP (no built-in H.264 encoder); the
    // bytes are still real, decodable image bytes.
    assert_eq!(ext, "webp");
    let img = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .unwrap()
        .decode()
        .expect("real WebP");
    assert!(img.width() > 0);
}

#[test]
fn capabilities_flat_models_returns_all_kinds_flattened() {
    let engine = synth_engine();
    let caps = engine.capabilities();
    let flat = caps.flat_models();
    assert!(!flat.is_empty());
    assert!(flat.iter().any(|m| m == "synthetic"));
}

#[test]
fn capabilities_kinds_lists_each_advertised_kind() {
    let engine = synth_engine();
    let kinds = engine.capabilities().kinds();
    assert_eq!(kinds.len(), TaskKind::ALL.len());
}

#[test]
fn capabilities_supports_returns_false_for_unknown_model() {
    let engine = synth_engine();
    let caps = engine.capabilities();
    assert!(!caps.supports(TaskKind::Image, "definitely-not-a-model"));
}

#[test]
fn capabilities_supports_returns_false_for_unsupported_model() {
    let engine = synth_engine();
    let caps = engine.capabilities();
    assert!(!caps.supports(TaskKind::Llm, "definitely-not-a-model"));
}

#[test]
fn build_returns_a_multi_engine_named_multi() {
    let engine = synth_engine();
    // `engine::build` always wraps the available backends in a
    // MultiEngine, even when only synthetic is compiled in.
    assert_eq!(engine.name(), "multi");
}

#[test]
fn capabilities_advertise_every_kind() {
    let engine = synth_engine();
    let caps = engine.capabilities();
    for kind in TaskKind::ALL {
        assert!(
            caps.supported_models_per_kind.contains_key(&kind),
            "{} advertised",
            kind.as_str()
        );
    }
}

#[test]
fn task_kind_matches_for_every_variant() {
    assert_eq!(
        Task::Image(ImageParams {
            prompt: "x".into(),
            width: 1,
            height: 1,
            steps: 1,
            ext: "webp".into(),
            ..Default::default()
        })
        .kind(),
        TaskKind::Image
    );
    assert_eq!(
        Task::Llm(LlmParams {
            messages: vec![],
            max_tokens: 1,
            temperature: 0.0,
            ..Default::default()
        })
        .kind(),
        TaskKind::Llm
    );
    assert_eq!(
        Task::AudioStt(AudioSttParams {
            input_url: "http://x".into(),
            language: None,
            ..Default::default()
        })
        .kind(),
        TaskKind::AudioStt
    );
    assert_eq!(
        Task::AudioTts(AudioTtsParams {
            text: "x".into(),
            voice: "v".into(),
            ext: "wav".into(),
            ..Default::default()
        })
        .kind(),
        TaskKind::AudioTts
    );
    assert_eq!(
        Task::Video(VideoParams {
            prompt: "x".into(),
            seconds: 1.0,
            width: 256,
            height: 256,
            ext: "mp4".into(),
            ..Default::default()
        })
        .kind(),
        TaskKind::Video
    );
}

#[test]
fn task_result_kind_matches_for_every_variant() {
    let cases = [
        (
            TaskResult::Image {
                bytes: vec![1],
                ext: "webp".into(),
            },
            TaskKind::Image,
        ),
        (
            TaskResult::Llm {
                json: serde_json::json!({}),
            },
            TaskKind::Llm,
        ),
        (
            TaskResult::AudioStt {
                json: serde_json::json!({}),
            },
            TaskKind::AudioStt,
        ),
        (
            TaskResult::AudioTts {
                bytes: vec![1],
                ext: "wav".into(),
            },
            TaskKind::AudioTts,
        ),
        (
            TaskResult::Video {
                bytes: vec![1],
                ext: "mp4".into(),
            },
            TaskKind::Video,
        ),
    ];
    for (r, expected) in cases {
        assert_eq!(r.kind(), expected);
    }
}

#[test]
fn task_kind_as_str_round_trips_with_serde() {
    assert_eq!(TaskKind::Image.as_str(), "image");
    assert_eq!(TaskKind::Llm.as_str(), "llm");
    assert_eq!(TaskKind::AudioStt.as_str(), "audio_stt");
    assert_eq!(TaskKind::AudioTts.as_str(), "audio_tts");
    assert_eq!(TaskKind::Video.as_str(), "video");
}

#[test]
fn image_params_uses_serde_defaults_for_missing_fields() {
    let json = serde_json::json!({ "prompt": "x" });
    let p: ImageParams = serde_json::from_value(json).unwrap();
    assert_eq!(p.width, 512);
    assert_eq!(p.height, 512);
    assert_eq!(p.steps, 20);
    assert_eq!(p.ext, "webp");
    assert_eq!(p.seed, None);
}

#[test]
fn llm_params_uses_serde_defaults_for_missing_fields() {
    let json = serde_json::json!({ "messages": [] });
    let p: LlmParams = serde_json::from_value(json).unwrap();
    assert_eq!(p.max_tokens, 512);
    assert!((p.temperature - 0.7).abs() < 1e-6);
}

#[test]
fn audio_tts_params_uses_serde_defaults_for_missing_fields() {
    let json = serde_json::json!({ "text": "hi" });
    let p: AudioTtsParams = serde_json::from_value(json).unwrap();
    assert_eq!(p.voice, "default");
    assert_eq!(p.ext, "wav");
}

#[test]
fn video_params_uses_serde_defaults_for_missing_fields() {
    let json = serde_json::json!({ "prompt": "x" });
    let p: VideoParams = serde_json::from_value(json).unwrap();
    assert!((p.seconds - 2.0).abs() < 1e-6);
    assert_eq!(p.width, 512);
    assert_eq!(p.height, 512);
    assert_eq!(p.ext, "mp4");
}

#[test]
fn job_claim_without_task_or_model_source_fails_to_deserialise() {
    // No legacy fallback: an offer that lacks `task` or `modelSource`
    // is a protocol violation, not a synthesisable image job.
    let json = serde_json::json!({
        "jobId": "j-1",
        "gameId": "g-1",
        "assetName": "g-1/creatures/x",
        "model": "synthetic",
        "vramGbEstimate": 1.0,
    });
    let err = serde_json::from_value::<JobClaim>(json).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("task") || msg.contains("modelSource"),
        "expected missing-required-field error, got: {msg}"
    );
}

#[test]
fn job_claim_with_full_task_and_model_source_round_trips() {
    let json = serde_json::json!({
        "jobId": "j-1",
        "gameId": "g-1",
        "assetName": "g-1/creatures/x",
        "model": "synthetic-image",
        "vramGbEstimate": 1.0,
        "task": {
            "kind": "image",
            "prompt": "a koi",
            "width": 1024,
            "height": 1024,
            "ext": "webp",
        },
        "modelSource": {
            "engine": "synthetic",
            "files": [],
            "cliDefaults": { "cfgScale": 1.0, "steps": 8, "width": 1024, "height": 1024 },
        },
    });
    let claim: JobClaim = serde_json::from_value(json).unwrap();
    assert_eq!(claim.task.kind(), TaskKind::Image);
    let res = synth_engine().dispatch(&claim.model, claim.task).unwrap();
    assert_eq!(res.kind(), TaskKind::Image);
}
