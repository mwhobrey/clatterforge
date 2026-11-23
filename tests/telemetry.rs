use dev_conductor::{
    codex::{JsonlObserver, ProtocolReceipt},
    inference::{config::Target, types::Usage},
    telemetry::{ApiPricing, api_cost, duration, usage_summary},
};
use serde_json::{Value, json};

fn usage(input: Option<u64>, output: Option<u64>, cached: Option<u64>) -> Usage {
    Usage {
        prompt_tokens: input,
        output_tokens: output,
        cached_tokens: cached,
        ..Default::default()
    }
}
fn priced() -> Target {
    serde_json::from_value(json!({
        "id":"gateway", "provider":"openai_local", "model":"fixture:model", "endpoint":"http://127.0.0.1:1234/v1/",
        "expected_artifact_digest":null,"context_limit":4096,"output_limit":128,
        "capabilities":{"streaming":true,"tools":false,"required_tool_choice":false,"parallel_tools":false,"structured_output":false,"media":false,"reasoning_control":false},
        "capability_source":"fixture","priority":0,"resource_id":"fixture",
        "api_pricing":{"currency":"USD","input_micros_per_million":2000000,"output_micros_per_million":4000000,"cached_input_micros_per_million":1000000}
    })).unwrap()
}

#[test]
fn complete_usage_and_cached_cost_are_measured_and_estimated_separately() {
    let target = priced();
    target.validate().unwrap();
    let u = usage(Some(100), Some(20), Some(40));
    let counts = usage_summary(&[&u]);
    assert_eq!(counts["input"]["total"], 100);
    assert_eq!(counts["cached_input"]["total"], 40);
    let cost = api_cost(&[(&target, &u)]);
    assert_eq!(cost["amount"], "0.000240");
    assert!(
        cost["basis"]
            .as_str()
            .unwrap()
            .contains("not provider charges")
    );
}

#[test]
fn partial_usage_retains_reported_counts_without_inventing_totals_or_cost() {
    let a = usage(Some(100), Some(20), Some(0));
    let b = usage(None, Some(5), None);
    let counts = usage_summary(&[&a, &b]);
    assert!(counts["input"]["total"].is_null());
    assert_eq!(counts["input"]["reported_sum"], 100);
    assert_eq!(counts["input"]["reports"], 1);
    assert_eq!(counts["output"]["total"], 25);
    assert!(api_cost(&[(&priced(), &a), (&priced(), &b)]).is_null());
    assert!(api_cost(&[(&priced(), &usage(Some(10), Some(0), None))]).is_null());
    assert!(api_cost(&[(&priced(), &usage(Some(10), Some(0), Some(11)))]).is_null());
    assert!(usage_summary(&[])["input"]["total"].is_null());
    let overflow = usage(Some(u64::MAX), None, None);
    assert!(usage_summary(&[&overflow, &overflow])["input"]["reported_sum"].is_null());
}

#[test]
fn later_partial_reports_merge_cumulative_counts_without_double_counting() {
    let mut u = usage(Some(10), None, Some(2));
    u.merge_report(&usage(None, Some(3), None));
    u.merge_report(&usage(None, Some(4), None));
    assert_eq!(u.prompt_tokens, Some(10));
    assert_eq!(u.output_tokens, Some(4));
    assert_eq!(u.cached_tokens, Some(2));
}

#[test]
fn estimates_round_once_after_sum_and_handle_zero_missing_rates_and_currency() {
    let mut target = priced();
    target
        .api_pricing
        .as_mut()
        .unwrap()
        .input_micros_per_million = 250000;
    let u = usage(Some(1), Some(0), Some(0));
    assert_eq!(api_cost(&[(&target, &u)])["amount"], "0.000000");
    assert_eq!(
        api_cost(&[(&target, &u), (&target, &u)])["amount"],
        "0.000001"
    );
    target
        .api_pricing
        .as_mut()
        .unwrap()
        .cached_input_micros_per_million = None;
    assert_eq!(api_cost(&[(&target, &u)])["amount"], "0.000000");
    assert!(api_cost(&[(&target, &usage(Some(1), Some(0), Some(1)))]).is_null());
    let mut other = target.clone();
    other.api_pricing.as_mut().unwrap().currency = "EUR".into();
    assert!(api_cost(&[(&target, &u), (&other, &u)]).is_null());
    let p = target.api_pricing.as_mut().unwrap();
    p.cached_input_micros_per_million = Some(p.input_micros_per_million);
    assert_eq!(
        api_cost(&[(&target, &usage(Some(2), Some(0), None))])["amount"],
        "0.000001"
    );
    target.api_pricing.as_mut().unwrap().currency = "bad currency".into();
    assert!(target.validate().is_err());
}

#[test]
fn local_inference_has_no_api_estimate_and_legacy_target_fingerprint_is_stable() {
    let mut target = priced();
    target.api_pricing = None;
    let old = serde_json::to_value(&target).unwrap();
    assert!(old.get("api_pricing").is_none());
    let restored: Target = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(
        restored.fingerprint(),
        dev_conductor::digest(&serde_json::to_vec(&restored).unwrap())
    );
    assert_eq!(serde_json::to_value(&restored).unwrap(), old);
    assert!(api_cost(&[(&target, &usage(Some(2), Some(3), Some(0)))]).is_null());
    target.provider = dev_conductor::inference::types::ProviderKind::Ollama;
    target.api_pricing = Some(ApiPricing {
        currency: "USD".into(),
        input_micros_per_million: 0,
        output_micros_per_million: 0,
        cached_input_micros_per_million: Some(0),
    });
    assert!(target.validate().is_err());
    assert!(api_cost(&[(&target, &usage(Some(2), Some(3), Some(0)))]).is_null());
}

#[test]
fn codex_usage_is_optional_and_does_not_change_completion_or_legacy_receipts() {
    let old = json!({"thread_id":"x","turn_started":true,"turn_completed":true,"agent_message_hash":"hash","events":4,"error":null});
    let restored: ProtocolReceipt = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(serde_json::to_value(restored).unwrap(), old);
    for usage in [
        json!({"input_tokens":12,"output_tokens":3,"cached_input_tokens":5}),
        json!({"input_tokens":12}),
        Value::Null,
    ] {
        let mut observer = JsonlObserver::default();
        for event in [
            json!({"type":"thread.started","thread_id":"x"}),
            json!({"type":"turn.started"}),
            json!({"type":"item.completed","item":{"type":"agent_message","text":"usable"}}),
            json!({"type":"turn.completed","usage":usage}),
        ] {
            observer.feed(format!("{event}\n").as_bytes());
        }
        let receipt = observer.finish();
        assert!(receipt.completed());
        if usage.is_object() {
            assert_eq!(receipt.usage.unwrap().prompt_tokens, Some(12));
        } else {
            assert!(receipt.usage.is_none());
        }
    }
}

#[test]
fn timing_never_invents_end_or_negative_duration() {
    assert_eq!(duration(Some(100), Some(250)), Some(150));
    assert_eq!(duration(Some(100), None), None);
    assert_eq!(duration(Some(250), Some(100)), None);
}
