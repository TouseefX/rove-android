//! One local Workspace setting, independent of the lifetime of its character.
use super::{received, LivePhysicsError, WorldView, MAX_LOCAL_GRAVITY};
use serde_json::Value;

#[derive(Debug)]
struct Setting {
    workspace: String,
    scope: u64,
    value: f64,
    received_value: f64,
    revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rove_client_core::world::WorldStore;
    use serde_json::json;

    #[test]
    fn settings_reject_foreign_targets_and_do_not_cross_session_scopes() {
        let world = WorldStore::from_snapshot(&json!({"version":1,"sequence":1,"nodes":[
            {"id":"game","class_name":"DataModel","properties":{}},
            {"id":"ws","class_name":"Workspace","parent":"game","properties":{"Gravity":10}},
            {"id":"part","class_name":"Part","parent":"ws","properties":{}}
        ]})).unwrap();
        let view = world.view();
        let mut setting = GravitySetting::default();
        let command = |id, observed| json!({"instance_id":id,"received_sequence":observed,"value":0});
        assert_eq!(setting.apply(&view, &command("ws",1), 7, Some("ws")).unwrap(), Some(0.));
        assert_eq!(setting.apply(&view, &command("part",1), 7, None).unwrap(), None);
        assert_eq!(setting.apply(&view, &command("ws",1), 7, Some("other")).unwrap(), None);
        assert_eq!(setting.apply(&view, &command("ws",2), 7, None).unwrap(), None);
        assert_eq!(setting.effective(&view, "ws",7), Some(0.));
        assert_eq!(setting.accepted, 1);
        assert_eq!(setting.effective(&view, "ws",8), None);
        assert_eq!(setting.effective(&view, "ws",7), None);
    }
}

#[derive(Debug, Default)]
pub(super) struct GravitySetting {
    setting: Option<Setting>,
    pub accepted: u64,
}

fn revision(view: &WorldView, id: &str) -> u64 {
    view.read_only_node(id)
        .and_then(|node| node.metadata_field("property_revisions", "Gravity"))
        .and_then(|value| value.as_u64()).unwrap_or(0)
}

impl GravitySetting {
    pub fn apply(&mut self, view: &WorldView, intent: &Value, scope: u64,
        expected_workspace: Option<&str>) -> Result<Option<f64>, LivePhysicsError> {
        let id = intent["instance_id"].as_str().filter(|id| id.len() <= 1024)
            .ok_or(LivePhysicsError::InvalidIntent)?;
        let value = intent["value"].as_f64()
            .filter(|v| v.is_finite() && (0.0..=MAX_LOCAL_GRAVITY).contains(v))
            .ok_or(LivePhysicsError::InvalidIntent)?;
        let observed = intent["received_sequence"].as_u64()
            .ok_or(LivePhysicsError::InvalidIntent)?;
        if view.class_name(id) != Some("Workspace")
            || view.parent(id).is_some_and(|parent| view.class_name(parent) != Some("DataModel"))
            || expected_workspace.is_some_and(|expected| expected != id)
        {
            return Ok(None);
        }
        let revision = revision(view, id);
        if observed < revision || observed > view.sequence() {
            return Ok(None);
        }
        let Some(received_value) = received::property(view, id, "Gravity")
            .and_then(|v| v.as_f64()).filter(|v| v.is_finite() && *v >= 0.)
        else { return Ok(None); };
        self.setting = Some(Setting { workspace: id.into(), scope, value, received_value, revision });
        self.accepted += 1;
        Ok(Some(value))
    }

    pub fn effective(&mut self, view: &WorldView, workspace: &str, scope: u64) -> Option<f64> {
        let setting = self.setting.as_ref()?;
        let current = received::property(view, workspace, "Gravity").and_then(|v| v.as_f64());
        if setting.workspace != workspace || setting.scope != scope
            || current != Some(setting.received_value)
            || revision(view, workspace) != setting.revision
        {
            self.setting = None;
            return None;
        }
        Some(setting.value)
    }
}
