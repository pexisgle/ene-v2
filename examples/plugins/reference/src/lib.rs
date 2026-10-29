//! Deterministic contract fixtures, not production speech or autonomous task engines.
#[cfg(target_family = "wasm")]
mod guest {
    use ene_plugin::*;
    use serde_json::{Value, json};
    use std::cell::RefCell;
    thread_local! { static PENDING: RefCell<Option<String>> = const { RefCell::new(None) }; }
    #[plugin_fn]
    pub fn ene_initialize(_: Json<Value>) -> FnResult<Json<Value>> {
        Ok(Json(
            json!({"events":true,"state":true,"consultation":true,"cancellation":true,"formats":["pcm_s16le"]}),
        ))
    }
    #[plugin_fn]
    pub fn ene_invoke(Json(operation): Json<Value>) -> FnResult<Json<Value>> {
        match operation["type"].as_str() {
            Some("character") => {
                if matches!(
                    operation["event"]["type"].as_str(),
                    Some("activated" | "conversation_finished")
                ) {
                    let count = service(ServiceRequest::StateRead {
                        key: "visits".into(),
                    })?
                    .as_u64()
                    .unwrap_or(0)
                        + 1;
                    service(ServiceRequest::StateWrite {
                        key: "visits".into(),
                        value: json!(count),
                    })?;
                    emit(serde_json::from_value(
                        json!({"type":"character","target":operation["target"],"action":{"type":"expression","name":"happy","duration_ms":1500}}),
                    )?)?;
                    return Ok(Json(json!({"visits":count})));
                }
            }
            Some("harness") => {
                let input = &operation["input"];
                let event = PENDING.with(|pending| -> FnResult<Value> {
                    let mut pending=pending.borrow_mut();
                    Ok(match input["type"].as_str() {
                        Some("cancel") => { *pending=None; json!({"type":"cancelled"}) }
                        Some("start") => {
                            if pending.is_some() { return Err(extism_pdk::Error::msg("task already active").into()); }
                            *pending=Some("model-1".into());
                            json!({"type":"model_request","operation_id":"model-1","request":{"endpoint":"https://example.com/api","options":{},"model":"reference","preamble":"","messages":[{"role":"user","text":input["instruction"]}],"max_tokens":64}})
                        }
                        Some("result") => {
                            if input["operation_id"].as_str()!=pending.as_deref() || pending.is_none() { return Err(extism_pdk::Error::msg("wrong operation result").into()); }
                            if pending.as_deref()==Some("model-1") {
                                *pending=Some("tool-1".into());
                                json!({"type":"tool_request","operation_id":"tool-1","name":"reference.inspect","arguments":{}})
                            } else {
                                *pending=Some("question-1".into());
                                json!({"type":"question","question_id":"question-1","text":"参照タスクを完了しますか？"})
                            }
                        }
                        Some("answer") => {
                            if input["question_id"].as_str()!=pending.as_deref() || pending.is_none() { return Err(extism_pdk::Error::msg("wrong consultation").into()); }
                            *pending=None;
                            json!({"type":"completed","report":input["answer"]})
                        }
                        _ => return Err(extism_pdk::Error::msg("unsupported input").into()),
                    })
                })?;
                emit(serde_json::from_value(
                    json!({"type":"harness","event":event}),
                )?)?;
            }
            Some("speech") => {
                let input = &operation["input"];
                let event = if input["type"] == "synthesize" {
                    json!({"type":"audio","chunk":{"utterance_id":input["utterance_id"],"generation":input["generation"],"sequence":0,"format":"pcm_s16le","sample_rate":16000,"channels":1,"data_base64":"AAA=","done":true}})
                } else {
                    json!({"type":"transcript","text":"reference transcript"})
                };
                emit(serde_json::from_value(event)?)?;
            }
            _ => return Err(extism_pdk::Error::msg("unsupported operation").into()),
        }
        Ok(Json(json!({})))
    }
}
