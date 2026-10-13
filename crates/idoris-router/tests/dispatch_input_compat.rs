use idoris_router::dispatch::DispatchInput;

#[test]
fn external_callers_can_construct_dispatch_input_with_existing_fields() {
    let input = DispatchInput {
        requested_model: Some("model-a"),
        prompt: "hello",
        affinity_key: Some("session-a"),
    };

    assert_eq!(input.requested_model, Some("model-a"));
    assert_eq!(input.prompt, "hello");
    assert_eq!(input.affinity_key, Some("session-a"));
}
