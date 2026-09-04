use serde_json::json;
use zork_agent::session::tools::{provider_call_definition, DynamicCall};

#[test]
// Contract: docs/zork-agent-architecture.md [TOOL-01]
fn provider_exposes_one_fixed_call_shape_for_all_logical_tools() {
    let definition = provider_call_definition();
    assert_eq!(definition.name, "call");
    assert_eq!(definition.input_schema["type"], "object");
    assert_eq!(
        definition.input_schema["required"],
        json!(["tool", "arguments"])
    );
    assert_eq!(definition.input_schema["additionalProperties"], false);
    assert_eq!(
        definition.input_schema["properties"]["tool"]["type"],
        "string"
    );
    assert_eq!(
        definition.input_schema["properties"]["arguments"]["type"],
        "object"
    );

    let call = DynamicCall::from_value(json!({
        "tool": "file.read",
        "arguments": {"path": "README.md"},
        "comment": "ignored"
    }))
    .unwrap();
    assert_eq!(call.tool, "file.read");
    assert_eq!(call.arguments, json!({"path": "README.md"}));
}
