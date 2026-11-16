//! A single local target pose; this never grants authority over a character.
use super::*;
use serde_json::Value;

#[derive(Clone, Debug)]
pub(super) struct TargetPose {
    pub workspace: Option<String>,
    pub received: bool,
    pub observed: u64,
    pub frame: Option<([f64; 3], [f64; 9])>,
}
impl TargetPose {
    pub fn parse(value: &Value, view: &WorldView) -> Result<Option<Self>, LivePhysicsError> {
        let observed = value["received_sequence"].as_u64().filter(|s| *s <= view.sequence())
            .ok_or(LivePhysicsError::InvalidIntent)?;
        match value["use_received"].as_bool().ok_or(LivePhysicsError::InvalidIntent)? {
            true => Ok(None),
            false => {
                let workspace = match value.get("workspace_id").ok_or(LivePhysicsError::InvalidIntent)? {
                    Value::Null => None,
                    Value::String(id) if !id.is_empty() && id.len() <= 1024 => Some(id.clone()),
                    _ => return Err(LivePhysicsError::InvalidIntent),
                };
                let received = value["received"].as_bool().ok_or(LivePhysicsError::InvalidIntent)?;
                let frame = if value.get("cframe").ok_or(LivePhysicsError::InvalidIntent)?.is_null() { None } else {
                    let cf = &value["cframe"]["$cframe"];
                    let position = received_vector(cf.get("position").cloned())
                        .filter(|v| v.iter().all(|x| x.abs() < 1e9)).ok_or(LivePhysicsError::InvalidIntent)?;
                    let values = cf["rotation"].as_array().filter(|v| v.len()==9)
                        .ok_or(LivePhysicsError::InvalidIntent)?;
                    let mut rotation = [0.;9];
                    for (a,b) in rotation.iter_mut().zip(values) {
                        *a = b.as_f64().ok_or(LivePhysicsError::InvalidIntent)?;
                    }
                    if !crate::physics_sender::validate_rotation(rotation) { return Err(LivePhysicsError::InvalidIntent); }
                    Some((position,rotation))
                };
                Ok(Some(Self { workspace, received, observed, frame }))
            }
        }
    }
    pub fn stale(&self, view: &WorldView, id: &str) -> bool {
        // A new received pose cannot reattach a locally detached/destroyed target.
        // Resume only after the VM observes a usable target again.
        if self.frame.is_none() { return false; }
        self.observed > view.sequence() || ["CFrame","ReplicationPV","Position","Orientation","Rotation"]
            .iter().any(|key| view.read_only_node(id)
                .and_then(|node| node.metadata_field("property_revisions",key))
                .and_then(|v| v.as_u64()).is_some_and(|r| r>self.observed))
    }
    pub fn resolve(&self, offset: [f64;3]) -> Option<[f64;3]> {
        let (position,rotation) = self.frame?;
        let goal = std::array::from_fn(|i| position[i]+(0..3).map(|j| rotation[i*3+j]*offset[j]).sum::<f64>());
        goal.iter().all(|v| v.is_finite() && v.abs()<1e9).then_some(goal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use rove_client_core::world::WorldStore;
    #[test]
    fn observations_require_finite_complete_frames_and_current_revisions() {
        let world=WorldStore::from_snapshot(&json!({"version":1,"sequence":4,"nodes":[]})).unwrap();
        let sample=json!({"received_sequence":4,"use_received":false,"received":false,"workspace_id":"ws",
            "cframe":{"$cframe":{"position":[10,2,0],"rotation":[1,0,0,0,1,0,0,0,1]}}});
        assert_eq!(TargetPose::parse(&sample,&world.view()).unwrap().unwrap().resolve([2.,0.,0.]),Some([12.,2.,0.]));
        for (pointer,value) in [
            ("/received_sequence",json!(5)),("/use_received",json!(0)),("/received",json!("yes")),
            ("/cframe/$cframe/rotation",json!([0,0,0,0,0,0,0,0,0])),
            ("/cframe/$cframe/position",json!([1e10,0,0])),("/workspace_id",json!(vec!["ws"]))
        ] {
            let mut bad=sample.clone();*bad.pointer_mut(pointer).unwrap()=value;
            assert!(TargetPose::parse(&bad,&world.view()).is_err(),"{pointer}");
        }
        let mut missing=sample;missing["cframe"]=Value::Null;
        assert!(TargetPose::parse(&missing,&world.view()).unwrap().unwrap().resolve([0.;3]).is_none());
    }
}
