//! React to a synthetic desktop event; OS observation and avatar delivery are external.
#[cfg(target_family = "wasm")]
mod guest {
    use ene_plugin::*;
    use serde_json::{Value, json};

    #[plugin_fn]
    pub fn ene_initialize() -> FnResult<Json<Value>> {
        Ok(Json(json!({})))
    }

    #[plugin_fn]
    pub fn ene_invoke(Json(operation): Json<Operation>) -> FnResult<Json<Value>> {
        if let Operation::Character {
            target,
            event: CharacterEvent::Window { event },
        } = operation
            && event.kind == WindowEventKind::Focused
            && event.window.application == "Code.exe"
            && event.window.id.device_id == target.device_id
        {
            emit(Event::Character {
                target,
                action: CharacterAction::Expression {
                    name: "happy".into(),
                    duration_ms: 1200,
                },
            })?;
        }
        Ok(Json(json!({})))
    }
}
