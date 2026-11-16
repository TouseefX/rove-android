#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
fn check(reply: &Value) {
    assert_ne!(reply["ok"], false, "{reply}");
    for key in ["task_errors", "animation_pose_errors", "joint_pose_errors"] {
        assert!(
            reply[key].as_array().is_none_or(|v| v.is_empty()),
            "{reply}"
        );
    }
    if let Some(actors) = reply["actor_runtime"].as_array() {
        for actor in actors {
            check(actor);
            check(&actor["world_effects"]);
        }
    }
}
#[test]
fn packaged_animation_pose_smoke_uses_official_luau_and_real_frame_phases() {
    let mut runtime=Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}}
    ]}),32).unwrap();
    let reply=runtime.call(json!({"op":"execute","source":include_str!("../../examples/animation-poses-smoke.luau"),"timeout_ms":2000})).unwrap();
    check(&reply);
    let job = reply["job_id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reply = runtime.status(job).unwrap();
        check(&reply);
        if reply["completed"] == true {
            assert_eq!(reply["values"][0]["passed"].as_f64(), Some(18.), "{reply}");
            break;
        }
        assert!(Instant::now() < deadline, "{reply}");
        check(&runtime.poll().unwrap());
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn number_pose_playback_drives_and_releases_face_controls() {
    let mut runtime=Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}}
    ]}),32).unwrap();
    let reply=runtime.call(json!({"op":"execute","source":r#"
        local passed=0
        local function check(value,label) assert(value,label);passed+=1 end
        local function near(a,b) return math.abs(a-b)<0.001 end
        local rig=Instance.new("Model",workspace)
        local head=Instance.new("Part",rig);head.Name="Head"
        local face=Instance.new("FaceControls",head)
        local controller=Instance.new("AnimationController",rig)
        local animator=Instance.new("Animator",controller)
        local sequence=Instance.new("KeyframeSequence")
        for _,sample in {{time=0,value=0},{time=2,value=1}} do
            local key=Instance.new("Keyframe",sequence);key.Time=sample.time
            local pose=Instance.new("Pose",key);pose.Name="Head"
            local folder=Instance.new("Folder",pose);folder.Name="FaceControls"
            local number=Instance.new("NumberPose",folder);number.Name="JawDrop";number.Value=sample.value
        end
        local reference=game:GetService("AnimationClipProvider"):RegisterActiveAnimationClip(sequence)
        local animation=Instance.new("Animation");animation.AnimationId=reference
        local track=animator:LoadAnimation(animation)
        track:Play(0,1,0);track.TimePosition=1;task.wait()
        check(near(face.JawDrop,.5),"numeric interpolation")
        track:AdjustWeight(.5,0);task.wait()
        check(near(face.JawDrop,.25),"track weight")
        track:Stop(0);task.wait()
        check(near(face.JawDrop,0),"stop releases animation value")
        face.JawDrop=.75;task.wait()
        check(near(face.JawDrop,.75),"later script writes are preserved")
        return {passed=passed}
    "#,"timeout_ms":2000})).unwrap();
    check(&reply);
    let job = reply["job_id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reply = runtime.status(job).unwrap();
        check(&reply);
        if reply["completed"] == true {
            assert_eq!(reply["values"][0]["passed"].as_f64(), Some(4.), "{reply}");
            break;
        }
        assert!(Instant::now() < deadline, "{reply}");
        check(&runtime.poll().unwrap());
        std::thread::sleep(Duration::from_millis(1));
    }
}
