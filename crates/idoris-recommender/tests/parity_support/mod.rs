use idoris_contracts::common::Capability;
use idoris_recommender::memory::{KvQuant, WiredMode};
use idoris_recommender::recommend::{PartialPolicy, Recommendation, TempStatus};
use serde_json::Value;

pub fn partial_policy(value: Option<&Value>) -> PartialPolicy {
    let Some(value) = value else {
        return PartialPolicy::default();
    };
    PartialPolicy {
        wired_mode: value
            .get("wired_mode")
            .and_then(Value::as_str)
            .map(|value| match value {
                "conservative" => WiredMode::Conservative,
                "moderate" => WiredMode::Moderate,
                "aggressive" => WiredMode::Aggressive,
                other => panic!("unknown wired_mode {other}"),
            }),
        context_target: value.get("context_target").and_then(Value::as_u64),
        kv_quant: value
            .get("kv_quant")
            .and_then(Value::as_str)
            .map(|value| match value {
                "fp16" => KvQuant::Fp16,
                "q8" => KvQuant::Q8,
                "q4" => KvQuant::Q4,
                other => panic!("unknown kv_quant {other}"),
            }),
        temp_slots: value
            .get("temp_slots")
            .and_then(Value::as_u64)
            .map(|value| u32::try_from(value).expect("temp_slots fits u32")),
        quality_threshold: value.get("quality_threshold").and_then(Value::as_f64),
        needed_capabilities: value.get("needed_capabilities").map(|values| {
            values
                .as_array()
                .expect("needed_capabilities array")
                .iter()
                .map(|value| capability(value.as_str().expect("capability string")))
                .collect()
        }),
    }
}

pub fn assert_recommendation(actual: &Recommendation, expected: &Value) {
    let hardware = &expected["hardware"];
    approx(actual.hardware.ram_gb, num(&hardware["ram_gb"]));
    assert_eq!(actual.hardware.chip, text(&hardware["chip"]));
    assert_eq!(
        actual.hardware.gpu_cores.map(u64::from),
        hardware["gpu_cores"].as_u64()
    );
    assert_eq!(actual.hardware.os, text(&hardware["os"]));

    let policy = &expected["policy"];
    assert_eq!(
        wired_mode(actual.policy.wired_mode),
        text(&policy["wired_mode"])
    );
    assert_eq!(
        actual.policy.context_target,
        policy["context_target"].as_u64().unwrap()
    );
    assert_eq!(kv_quant(actual.policy.kv_quant), text(&policy["kv_quant"]));
    assert_eq!(
        u64::from(actual.policy.temp_slots),
        policy["temp_slots"].as_u64().unwrap()
    );
    approx(
        actual.policy.quality_threshold,
        num(&policy["quality_threshold"]),
    );
    assert_eq!(
        actual
            .policy
            .needed_capabilities
            .iter()
            .copied()
            .map(capability_name)
            .collect::<Vec<_>>(),
        policy["needed_capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>()
    );

    for (actual, key) in [
        (actual.usable_gb, "usable_gb"),
        (actual.reserve_gb, "reserve_gb"),
        (actual.temp_reserve_gb, "temp_reserve_gb"),
        (actual.resident_budget_gb, "resident_budget_gb"),
    ] {
        approx(actual, num(&expected[key]));
    }

    match (&actual.resident, &expected["resident"]) {
        (None, Value::Null) => {}
        (Some(actual), expected) => {
            assert_eq!(actual.id, text(&expected["id"]));
            assert_eq!(actual.ctx, expected["ctx"].as_u64().unwrap());
            for (actual, key) in [
                (actual.score, "score"),
                (actual.quality, "quality"),
                (actual.weights_gb, "weights_gb"),
                (actual.kv_gb, "kv_gb"),
                (actual.footprint_gb, "footprint_gb"),
            ] {
                approx(actual, num(&expected[key]));
            }
            assert_eq!(actual.label, text(&expected["label"]));
        }
        other => panic!("resident mismatch {other:?}"),
    }
    assert_eq!(
        actual.resident_label.as_deref(),
        expected["resident_label"].as_str()
    );

    let expected_temp = expected["temp"].as_array().unwrap();
    assert_eq!(actual.temp.len(), expected_temp.len());
    for (actual, expected) in actual.temp.iter().zip(expected_temp) {
        assert_eq!(actual.id, text(&expected["id"]));
        assert_eq!(
            capability_name(actual.capability),
            text(&expected["capability"])
        );
        assert_eq!(temp_status(actual.status), text(&expected["status"]));
        approx(actual.min_ram_gb, num(&expected["min_ram_gb"]));
        assert_eq!(actual.reason, text(&expected["reason"]));
        let quant = &expected["quant"];
        assert_eq!(actual.quant.label, text(&quant["label"]));
        for (actual, key) in [
            (actual.quant.quality, "quality"),
            (actual.quant.weights_gb, "weights_gb"),
            (actual.quant.kv_gb, "kv_gb"),
            (actual.quant.footprint_gb, "footprint_gb"),
        ] {
            approx(actual, num(&quant[key]));
        }
    }

    let expected_blocked = expected["blocked"].as_array().unwrap();
    assert_eq!(actual.blocked.len(), expected_blocked.len());
    for (actual, expected) in actual.blocked.iter().zip(expected_blocked) {
        assert_eq!(text(&expected["status"]), "BLOCKED");
        assert_eq!(actual.id, text(&expected["id"]));
        approx(actual.min_ram_gb, num(&expected["min_ram_gb"]));
        approx(
            actual.estimated_memory_gb,
            num(&expected["estimated_memory_gb"]),
        );
        assert_eq!(actual.reason, text(&expected["reason"]));
    }

    assert_eq!(
        actual.warnings,
        expected["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| text(value).to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(actual.tradeoff, text(&expected["tradeoff"]));
    assert_eq!(
        actual.recommended_sysctl.iogpu_wired_limit_mb,
        expected["recommended_sysctl"]["iogpu_wired_limit_mb"]
            .as_i64()
            .unwrap()
    );
    match (&actual.override_choice, &expected["override"]) {
        (None, Value::Null) => {}
        (Some(actual), expected) => {
            assert_eq!(actual.id, text(&expected["id"]));
            assert_eq!(actual.active, expected["active"].as_bool().unwrap());
        }
        other => panic!("override mismatch {other:?}"),
    }
}

pub fn assert_projection(actual: &Recommendation, expected: &Value) {
    assert_eq!(
        actual.resident_label.as_deref(),
        expected["resident_label"].as_str()
    );
    for (actual, key) in [
        (actual.usable_gb, "usable_gb"),
        (actual.reserve_gb, "reserve_gb"),
        (actual.temp_reserve_gb, "temp_reserve_gb"),
        (actual.resident_budget_gb, "resident_budget_gb"),
    ] {
        approx(actual, num(&expected[key]));
    }
    let temp = expected["temp"].as_array().unwrap();
    assert_eq!(actual.temp.len(), temp.len());
    for (actual, expected) in actual.temp.iter().zip(temp) {
        assert_eq!(actual.id, text(&expected["id"]));
        assert_eq!(
            capability_name(actual.capability),
            text(&expected["capability"])
        );
        assert_eq!(temp_status(actual.status), text(&expected["status"]));
        assert_eq!(actual.quant.label, text(&expected["quant"]));
    }
    assert_eq!(
        actual
            .blocked
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        expected["blocked_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        actual.recommended_sysctl.iogpu_wired_limit_mb,
        expected["iogpu_wired_limit_mb"].as_i64().unwrap()
    );
}

fn approx(actual: f64, expected: f64) {
    let tolerance = 1e-9_f64.max(expected.abs() * 1e-9);
    assert!(
        (actual - expected).abs() <= tolerance,
        "numeric mismatch actual={actual:?} expected={expected:?}"
    );
}

fn num(value: &Value) -> f64 {
    value.as_f64().expect("number")
}

fn text(value: &Value) -> &str {
    value.as_str().expect("string")
}

fn wired_mode(value: WiredMode) -> &'static str {
    match value {
        WiredMode::Conservative => "conservative",
        WiredMode::Moderate => "moderate",
        WiredMode::Aggressive => "aggressive",
    }
}

fn kv_quant(value: KvQuant) -> &'static str {
    match value {
        KvQuant::Fp16 => "fp16",
        KvQuant::Q8 => "q8",
        KvQuant::Q4 => "q4",
    }
}

fn capability(value: &str) -> Capability {
    match value {
        "chat" => Capability::Chat,
        "reasoning" => Capability::Reasoning,
        "vision" => Capability::Vision,
        "asr" => Capability::Asr,
        "tts" => Capability::Tts,
        "coding" => Capability::Coding,
        "embedding" => Capability::Embedding,
        "rerank" => Capability::Rerank,
        other => panic!("unknown capability {other}"),
    }
}

fn capability_name(value: Capability) -> &'static str {
    match value {
        Capability::Chat => "chat",
        Capability::Reasoning => "reasoning",
        Capability::Vision => "vision",
        Capability::Asr => "asr",
        Capability::Tts => "tts",
        Capability::Coding => "coding",
        Capability::Embedding => "embedding",
        Capability::Rerank => "rerank",
    }
}

fn temp_status(value: TempStatus) -> &'static str {
    match value {
        TempStatus::Ready => "ready",
        TempStatus::RequiresEviction => "requires_eviction",
    }
}
