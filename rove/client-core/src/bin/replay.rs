use rove_client_core::{session::SessionCore, schema::parse_schema};
use std::fs;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: replay <dir-with-ok-*>");
    let schema_path = std::path::Path::new(&dir).join("ok-0.bin");
    let globals_path = std::path::Path::new(&dir).join("ok-1.bin");
    let schema_payload = fs::read(&schema_path).expect("read ok-0 schema");
    let globals_payload = fs::read(&globals_path).expect("read ok-1 globals");
    let mut files: Vec<_> = fs::read_dir(&dir).unwrap().flatten()
        .map(|e| e.path()).collect();
    files.sort();
    let mut core = SessionCore::with_schema(
        parse_schema(&schema_payload).expect("schema parse"), 8, 64 << 20).expect("core");
    core.ingest(&globals_payload).expect("globals ingest");
    if let Some(schema) = core.schema() {
        for (i, c) in schema.classes.iter().take(6).enumerate() {
            eprintln!("SCHEMA class{} {} props={:?} n={}", i, c.name, c.properties, c.properties.len());
        }
    }
    // replay ok-2.. onward
    for path in files.into_iter().skip(2) {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("ok-") { continue; }
        let payload = fs::read(&path).unwrap();
        match core.ingest(&payload) {
            Ok(_) => {}
            Err(e) => eprintln!("{}: {} ({} bytes)", name, e, payload.len()),
        }
    }
    // now bad packets
    let bad_dir = std::path::Path::new("/tmp/badpackets");
    if let Ok(entries) = fs::read_dir(bad_dir) {
        let mut bads: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        bads.sort();
        for path in bads {
            let payload = fs::read(&path).unwrap();
            match core.ingest(&payload) {
                Ok(_) => println!("BAD {} DECODED OK ({} bytes)", path.display(), payload.len()),
                Err(e) => println!("BAD {} fails: {} ({} bytes)", path.display(), e, payload.len()),
            }
        }
    }
}
