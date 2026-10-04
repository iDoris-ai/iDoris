#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_contracts::common::{Capability, PrivacyClass, Tier};
use idoris_contracts::load_policy::LoadMode;
use idoris_contracts::{Contract, RoutingPolicy, TaskProfile};
use idoris_router::routing_policy::{MatchedRule, decide};
use serde_json::{Value, json};

fn policy(rules: Value, default: Value) -> RoutingPolicy {
    let policy: RoutingPolicy = serde_json::from_value(json!({
        "routing_policy": {"version": 1, "rules": rules, "default": default}
    }))
    .unwrap();
    policy.validate().unwrap();
    policy
}

fn profile(value: Value) -> TaskProfile {
    serde_json::from_value(value).unwrap()
}

#[test]
fn conditions_are_conjunctive_and_require_every_capability() {
    let policy = policy(
        json!([{
            "if": {"privacy": "any", "intent": "coding", "complexity": "complex",
                "capabilities": ["coding", "chat"]},
            "then": {"tiers": ["remote"]}
        }]),
        json!({"tiers": ["local"]}),
    );
    let positive = json!({"privacy": "any", "intent": "coding", "complexity": "complex",
        "capabilities": ["vision", "chat", "coding"]});
    assert_eq!(
        decide(&policy, &profile(positive.clone())).tiers,
        [Tier::Remote]
    );
    // Change only one field at a time; a partial match must use the default.
    for (field, value) in [
        ("privacy", json!("local_only")),
        ("intent", json!("chat")),
        ("complexity", json!("simple")),
        ("capabilities", json!(["coding"])),
    ] {
        let mut negative = positive.clone();
        negative[field] = value;
        let decision = decide(&policy, &profile(negative));
        assert_eq!(decision.matched_rule, MatchedRule::Default, "{field}");
        assert_eq!(decision.tiers, [Tier::Local], "{field}");
    }
}

#[test]
fn conflicting_rules_use_the_first_match_in_file_order() {
    let first = json!({"if": {"intent": "coding"},
        "then": {"tiers": ["remote"], "fail_closed": true,
            "capability": "coding", "load": "on_demand"}});
    let second = json!({"if": {"intent": "coding"}, "then": {"tiers": ["lora"]}});
    let task = profile(json!({"privacy": "any", "intent": "coding"}));
    let decision = decide(
        &policy(json!([first, second]), json!({"tiers": ["local"]})),
        &task,
    );
    assert_eq!(decision.matched_rule, MatchedRule::Rule(0));
    assert_eq!(decision.tiers, [Tier::Remote]);
    assert!(decision.fail_closed);
    assert_eq!(decision.capability, Some(Capability::Coding));
    assert_eq!(decision.load, Some(LoadMode::OnDemand));
    let reversed = decide(
        &policy(json!([second, first]), json!({"tiers": ["local"]})),
        &task,
    );
    assert_eq!(reversed.tiers, [Tier::Lora]);
    assert_eq!(reversed.capability, None);
    assert_eq!(reversed.load, None);
}

#[test]
fn unmatched_and_empty_rule_sets_use_default_metadata() {
    let task = profile(json!({"privacy": "any", "intent": "chat"}));
    for rules in [
        json!([]),
        json!([{"if": {"intent": "coding"},
        "then": {"tiers": ["local"]}}]),
    ] {
        let decision = decide(
            &policy(
                rules,
                json!({"tiers": ["remote", "lora"],
            "fail_closed": true, "capability": "vision", "load": "resident"}),
            ),
            &task,
        );
        assert_eq!(decision.matched_rule, MatchedRule::Default);
        assert_eq!(decision.tiers, [Tier::Remote, Tier::Lora]);
        assert!(decision.fail_closed);
        assert_eq!(decision.capability, Some(Capability::Vision));
        assert_eq!(decision.load, Some(LoadMode::Resident));
    }
}

#[test]
fn omitted_tiers_and_fail_closed_use_ts_action_defaults() {
    let policy = policy(json!([]), json!({"capability": "chat"}));
    let decision = decide(&policy, &profile(json!({"privacy": "any"})));
    assert_eq!(decision.tiers, [Tier::Local]);
    assert!(!decision.fail_closed);
    assert_eq!(decision.load, None);
}

#[test]
fn remote_only_rule_cannot_relax_local_only_or_fall_through() {
    let policy = policy(
        json!([
            {"if": {"intent": "banner"}, "then": {"tiers": ["remote"], "fail_closed": false}},
            {"if": {"privacy": "local_only"}, "then": {"tiers": ["local"]}}
        ]),
        json!({"tiers": ["lora"]}),
    );
    let mut task = profile(json!({"privacy": "local_only", "intent": "banner"}));
    let decision = decide(&policy, &task);
    assert_eq!(decision.matched_rule, MatchedRule::Rule(0));
    assert!(decision.tiers.is_empty());
    assert!(decision.fail_closed);
    task.privacy = Some(PrivacyClass::Any);
    let positive = decide(&policy, &task);
    assert_eq!(positive.tiers, [Tier::Remote]);
    assert!(!positive.fail_closed);
}

#[test]
fn privacy_intersection_preserves_lora_and_requested_order() {
    let policy = policy(
        json!([]),
        json!({"tiers": ["lora", "remote", "local"],
        "fail_closed": false}),
    );
    for (privacy, expected) in [
        ("local_only", vec![Tier::Lora, Tier::Local]),
        ("any", vec![Tier::Lora, Tier::Remote, Tier::Local]),
    ] {
        let decision = decide(&policy, &profile(json!({"privacy": privacy})));
        assert_eq!(decision.tiers, expected);
        assert_eq!(decision.fail_closed, privacy == "local_only");
    }
}

#[test]
fn empty_conditions_match_but_missing_required_capabilities_do_not() {
    for (condition, expected) in [
        (json!({}), MatchedRule::Rule(0)),
        (json!({"capabilities": []}), MatchedRule::Rule(0)),
        (json!({"capabilities": ["chat"]}), MatchedRule::Default),
    ] {
        let policy = policy(
            json!([{"if": condition, "then": {"tiers": ["lora"]}}]),
            json!({"tiers": ["local"]}),
        );
        let decision = decide(&policy, &TaskProfile::default());
        assert_eq!(decision.matched_rule, expected);
    }
}

#[test]
fn absent_privacy_fails_closed_even_for_remote_default() {
    let policy = policy(
        json!([]),
        json!({"tiers": ["remote"], "fail_closed": false}),
    );
    let decision = decide(&policy, &TaskProfile::default());
    assert_eq!(decision.matched_rule, MatchedRule::Default);
    assert!(decision.tiers.is_empty());
    assert!(decision.fail_closed);
}
