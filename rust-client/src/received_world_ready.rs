//! Python World.avatar's non-streaming load gate. This does not invent a
//! streamed region or send StreamingReady for a non-streaming world.
use rove_client_core::world::WorldView;
use serde_json::Value;
pub(crate) fn can_simulate(
    view: &WorldView,
    root: &str,
    region_received: bool,
    initial_world: bool,
) -> bool {
    if region_received {
        return true;
    }
    if !initial_world {
        return false;
    }
    let mut id = root.to_owned();
    // Bounded ancestry also rejects malformed parent cycles.
    for _ in 0..128 {
        if view.class_name(&id) == Some("Workspace") {
            return view.get_property(&id, "StreamingEnabled") == Some(Value::Bool(false));
        }
        let Some(parent) = view.parent(&id) else {
            return false;
        };
        id = parent.to_string();
    }
    false
}
#[cfg(test)]
mod tests {
    use super::*;
    use rove_client_core::world::WorldStore;
    use serde_json::json;
    #[test]
    fn nonstreaming_requires_received_false_and_complete_initial_world() {
        for streaming in [None, Some(true), Some(false)] {
            let mut properties = json!({});
            if let Some(v) = streaming {
                properties["StreamingEnabled"] = json!(v);
            }
            let world = WorldStore::from_snapshot(&json!({"version":1,"nodes":[
                {"id":"1:1","class_name":"Workspace","properties":properties},
                {"id":"1:2","class_name":"Model","parent":"1:1","properties":{}},
                {"id":"1:3","class_name":"Part","parent":"1:2","properties":{}}
            ]}))
            .unwrap();
            assert!(!can_simulate(&world.view(), "1:3", false, false));
            assert_eq!(
                can_simulate(&world.view(), "1:3", false, true),
                streaming == Some(false)
            );
            assert!(!can_simulate(&world.view(), "missing", false, true));
            assert!(can_simulate(&world.view(), "1:3", true, false));
            assert!(can_simulate(&world.view(), "1:3", true, true));
        }
    }
}
