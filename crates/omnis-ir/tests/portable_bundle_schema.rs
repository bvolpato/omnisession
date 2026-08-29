use omnis_ir::Provider;
use serde_json::Value;

#[test]
fn public_schema_accepts_every_serialized_provider() {
    let schema: Value = serde_json::from_str(include_str!(
        "../../../schemas/omnisession-bundle-v1.schema.json"
    ))
    .expect("valid public bundle schema");
    let provider_values = schema
        .pointer("/$defs/provider/enum")
        .and_then(Value::as_array)
        .expect("provider enum in public bundle schema");

    for provider in Provider::ALL {
        let serialized = serde_json::to_value(provider).expect("serialized provider");
        assert!(
            provider_values.contains(&serialized),
            "public bundle schema rejects serialized provider {serialized}"
        );
    }
    assert!(provider_values.contains(&Value::String("open-code".to_owned())));
}
