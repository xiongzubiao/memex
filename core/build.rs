use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=data/litellm_raw.json");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let out_path = Path::new(&out_dir).join("model_catalog.json");

    let raw_path = Path::new("data/litellm_raw.json");
    if !raw_path.exists() {
        fs::write(&out_path, "{}").expect("failed to write empty model_catalog.json");
        return;
    }

    let raw = fs::read_to_string(raw_path).expect("failed to read litellm_raw.json");
    let all: serde_json::Value =
        serde_json::from_str(&raw).expect("invalid JSON in litellm_raw.json");

    let obj = match all.as_object() {
        Some(o) => o,
        None => {
            fs::write(&out_path, "{}").expect("failed to write empty model_catalog.json");
            return;
        }
    };

    // Filter: chat mode only, both token fields present.
    // Keep provider-prefixed entries (e.g. "azure/gpt-5.4-mini") because different
    // providers may have different context window limits for the same base model.
    let mut catalog: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (key, val) in obj {
        // Skip non-object entries (e.g. sample_spec strings)
        let entry = match val.as_object() {
            Some(e) => e,
            None => continue,
        };

        // Only chat mode
        if entry.get("mode").and_then(|v| v.as_str()) != Some("chat") {
            continue;
        }

        let max_input = match entry.get("max_input_tokens").and_then(|v| v.as_u64()) {
            Some(v) => v,
            None => continue,
        };
        let max_output = match entry.get("max_output_tokens").and_then(|v| v.as_u64()) {
            Some(v) => v,
            None => continue,
        };

        catalog.insert(
            key.clone(),
            serde_json::json!({
                "max_input_tokens": max_input,
                "max_output_tokens": max_output,
            }),
        );
    }

    let output = serde_json::to_string(&catalog).expect("failed to serialize catalog");
    fs::write(&out_path, output).expect("failed to write model_catalog.json");
}
