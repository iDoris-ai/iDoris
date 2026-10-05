#![allow(clippy::unwrap_used)]

use idoris_recommender::memory::{
    KvQuant, ModelArch, QuantSpec, bytes_to_gb, bytes_to_gib, bytes_to_mib, footprint_gb, kv_bytes,
    lookup_quant, weights_gb,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Vectors {
    units: Vec<Unit>,
    weights: Vec<Weight>,
    kv: KvVector,
    moe_total_params: Moe,
}
#[derive(Debug, Deserialize)]
struct Unit {
    bytes: f64,
    gb: f64,
    mib: f64,
    gib: f64,
}
#[derive(Debug, Deserialize)]
struct Weight {
    params_total_b: f64,
    bpp: f64,
    weights_gb: Option<f64>,
    expected_gb: f64,
}
#[derive(Debug, Deserialize)]
struct KvVector {
    arch: ModelArch,
    ctx: u64,
    quant: KvQuant,
    expected_bytes: f64,
    expected_mib: f64,
}
#[derive(Debug, Deserialize)]
struct Moe {
    params_total_b: f64,
    bpp: f64,
    arch: ModelArch,
    ctx: u64,
    kv_quant: KvQuant,
    overhead_gb: f64,
    expected_footprint_gb: f64,
}

fn vectors() -> Vectors {
    serde_json::from_str(include_str!("../../../testdata/recommender/memory.json")).unwrap()
}

fn close(left: f64, right: f64) {
    assert!((left - right).abs() <= 1e-12, "{left} != {right}");
}

#[test]
fn shared_units_weights_and_kv_match_ts_reference() {
    let vectors = vectors();
    for unit in vectors.units {
        close(bytes_to_gb(unit.bytes), unit.gb);
        close(bytes_to_mib(unit.bytes), unit.mib);
        close(bytes_to_gib(unit.bytes), unit.gib);
    }
    for item in vectors.weights {
        close(
            weights_gb(
                item.params_total_b,
                &QuantSpec {
                    label: None,
                    bpp: Some(item.bpp),
                    weights_gb: item.weights_gb,
                    quality: 1.0,
                },
            )
            .unwrap(),
            item.expected_gb,
        );
    }
    let kv = vectors.kv;
    close(kv_bytes(kv.arch, kv.ctx, kv.quant), kv.expected_bytes);
    close(bytes_to_mib(kv.expected_bytes), kv.expected_mib);
    close(
        kv_bytes(kv.arch, kv.ctx, KvQuant::Q8),
        kv_bytes(kv.arch, kv.ctx, KvQuant::Fp16) / 2.0,
    );
}

#[test]
fn measured_weight_wins_and_missing_weight_information_is_loud() {
    let measured = QuantSpec {
        label: Some("measured".into()),
        bpp: Some(99.0),
        weights_gb: Some(7.36),
        quality: 1.0,
    };
    assert_eq!(weights_gb(100.0, &measured).unwrap(), 7.36);
    assert!(
        weights_gb(
            8.0,
            &QuantSpec {
                label: None,
                bpp: None,
                weights_gb: None,
                quality: 1.0
            },
        )
        .is_err()
    );
}

#[test]
fn moe_footprint_uses_total_params_and_default_formula_parts() {
    let moe = vectors().moe_total_params;
    close(
        footprint_gb(
            moe.params_total_b,
            &QuantSpec {
                label: None,
                bpp: Some(moe.bpp),
                weights_gb: None,
                quality: 1.0,
            },
            moe.arch,
            moe.ctx,
            moe.kv_quant,
            Some(moe.overhead_gb),
        )
        .unwrap(),
        moe.expected_footprint_gb,
    );
    assert_eq!(lookup_quant("q4_k_m").unwrap().bpp, 0.55);
    assert!(lookup_quant("unknown").is_none());
}
