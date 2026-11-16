#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::json;
#[test]
fn remote_proxy_arguments_preserve_nil_positions_and_reject_engine_userdata() {
    let mut vm=Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"ws"},"nodes":[
  {"id":"game","class_name":"DataModel","name":"game","properties":{},"server_owned":true},
  {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{},"server_owned":true},
  {"id":"event","class_name":"RemoteEvent","name":"Event","parent":"ws","properties":{},"server_owned":true}
 ]}),32).unwrap();
    let result=vm.execute_control("workspace.Event:FireServer(newproxy(), {newproxy(true), 7}, {probe=newproxy()}, 9); assert(not pcall(function() workspace.Event:FireServer(RaycastParams.new()) end)); return true".into(),2000).unwrap();
    successful(&result).unwrap();
    let events = vm.take_remote_server_events();
    assert_eq!(events.len(), 1, "{result}");
    assert_eq!(
        events[0]["arguments"],
        json!([null,{"$remote_array":[null,7.0]},{"$remote_map":[["probe",null]]},9.0])
    );
}
