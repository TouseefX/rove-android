//! Exercise the same child-process protocol the dashboard uses, with a real VM.
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn spawn(extra: &[&str]) -> (ChildGuard, mpsc::Receiver<Value>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rove-terminal"))
        .arg("--worker")
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if let Ok(value) = serde_json::from_str(&line) {
                if tx.send(value).is_err() {
                    break;
                }
            }
        }
    });
    (ChildGuard(child), rx)
}
fn send(child: &mut ChildGuard, command: Value) {
    let input = child.0.stdin.as_mut().unwrap();
    writeln!(input, "{command}").unwrap();
    input.flush().unwrap();
}
fn until(rx: &mpsc::Receiver<Value>, predicate: impl Fn(&Value) -> bool) -> Value {
    let start = Instant::now();
    let mut last = Value::Null;
    while start.elapsed() < Duration::from_secs(12) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(value) => {
                if predicate(&value) {
                    return value;
                }
                last = value;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("worker closed early: {e}; last={last}"),
        }
    }
    panic!("timed out; last={last}")
}
fn has_value(event: &Value, expected: &Value) -> bool {
    event["reply"]["values"]
        .as_array()
        .is_some_and(|values| values.contains(expected))
        || event["reply"]["completed_jobs"]
            .as_array()
            .is_some_and(|jobs| {
                jobs.iter().any(|job| {
                    job["values"]
                        .as_array()
                        .is_some_and(|values| values.contains(expected))
                })
            })
}
#[test]
fn dashboard_executes_persistent_yielding_luau_and_leaves_cleanly() {
    let (mut child, rx) = spawn(&["--offline", "--seconds", "20"]);
    let ready = until(&rx, |e| e["data"]["luau_ready"] == true);
    assert_eq!(ready["data"]["phase"], "sandbox");
    assert!(ready["data"]["rss_bytes"].as_u64().unwrap() > 0);
    assert!(ready["data"]["rtt_ms"].is_null());
    send(
        &mut child,
        json!({"op":"execute","source":"_G.tui_count = 41; print('terminal vm connected'); return _G.tui_count + 1"}),
    );
    let response = until(&rx, |e| has_value(e, &json!(42.0)));
    let id = response["reply"]["job_id"].as_u64().unwrap();
    send(&mut child, json!({"op":"status","job_id":id}));
    until(&rx, |e| e["kind"] == "job" && has_value(e, &json!(42.0)));
    send(
        &mut child,
        json!({"op":"execute","source":"task.wait(0.15); return _G.tui_count + 2"}),
    );
    until(&rx, |e| has_value(e, &json!(43.0)));
    send(
        &mut child,
        json!({"op":"execute","source":"error('an ordinary script error')"}),
    );
    until(&rx, |e| e["reply"]["ok"] == false);
    send(
        &mut child,
        json!({"op":"execute","source":"return 'recovered'"}),
    );
    until(&rx, |e| has_value(e, &json!("recovered")));
    send(&mut child, json!({"op":"leave"}));
    let end = until(&rx, |e| e["kind"] == "finished");
    assert_eq!(end["report"]["ok"], true);
    assert_eq!(end["report"]["stopped_by_user"], true);
    assert!(child.0.wait().unwrap().success());
}
#[test]
fn closing_terminal_input_stops_its_worker() {
    let (mut child, rx) = spawn(&["--offline", "--seconds", "20"]);
    until(&rx, |e| e["data"]["luau_ready"] == true);
    child.0.stdin.take();
    let end = until(&rx, |e| e["kind"] == "finished");
    assert_eq!(end["report"]["stopped_by_user"], true);
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn whole_process_ram_budget_grows_after_a_real_large_allocation() {
    let (mut child, rx) = spawn(&["--offline", "--seconds", "20", "--lua-memory-mib", "256"]);
    let ready = until(&rx, |e| e["data"]["luau_ready"] == true);
    assert_eq!(ready["data"]["ram_buffer_bytes"], 10_000_000);
    assert_eq!(ready["data"]["ram_budget_scope"], "client_process");
    send(&mut child, json!({"op":"execute","source":
        "_G.ram_budget_test = buffer.create(140000000); buffer.fill(_G.ram_budget_test, 0, 42); return buffer.len(_G.ram_budget_test)"}));
    until(&rx, |e| has_value(e, &json!(140_000_000.0)));
    let sample = until(&rx, |e| e["data"]["rss_bytes"].as_u64().is_some_and(|v| v >= 128_000_000));
    let used = sample["data"]["rss_bytes"].as_u64().unwrap();
    let budget = sample["data"]["ram_budget_bytes"].as_u64().unwrap();
    assert!(budget > used + 10_000_000, "{sample}");
    assert_eq!((budget - 128_000_000) % 10_000_000, 0);
    assert_eq!(sample["data"]["ram_budget_is_soft"], true);
    send(&mut child, json!({"op":"leave"}));
    until(&rx, |e| e["kind"] == "finished");
    assert!(child.0.wait().unwrap().success());
}
#[test]
fn missing_cookie_is_a_reported_failure_not_a_fake_connection() {
    let (mut child, rx) = spawn(&[
        "--place-id",
        "189707",
        "--credential-file",
        "missing-dashboard-test-cookie.txt",
    ]);
    let end = until(&rx, |e| e["kind"] == "finished");
    assert_eq!(end["report"]["ok"], false);
    assert!(end["report"]["error"].is_string());
    assert!(end["report"]["connected_seconds"].is_null());
    assert!(child.0.wait().unwrap().success());
}
