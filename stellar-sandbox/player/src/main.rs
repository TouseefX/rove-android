use serde_json::Value;
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::ExitCode;

const PREFIX: &str = "ROVE_STELLAR_PLAYER_V1 ";
const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_REPORT_BYTES: usize = 512 * 1024;
const MAX_BUFFERED_LINE_BYTES: usize = MAX_REPORT_BYTES + 1024;
const MAX_PRINTS: usize = 128;
const MAX_PRINT_TEXT_BYTES: usize = 4096;
const MAX_PRINT_TOTAL_BYTES: usize = 16 * 1024;
const MAX_RETURNS: usize = 129;
const TEMPLATE: &str = include_str!("../player-template.rbxmx.in");
const ENCODER: &str = include_str!("../../studio/encode-values.luau");

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rove-stellar: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<OsString>) -> Result<(), String> {
    if args.is_empty() {
        print_help();
        return Ok(());
    }
    let command = args[0].to_str().ok_or("command must be valid UTF-8")?;
    match (command, args.as_slice()) {
        ("pack", [_, source, output]) => pack(Path::new(source), Path::new(output)),
        ("read-report", [_, report]) => read_reports(Some(report)),
        ("read-report", [_]) => read_reports(None),
        ("--help" | "-h" | "help", [_]) => { print_help(); Ok(()) }
        _ => Err("usage: rove-stellar pack <reviewed-script.luau> <model.rbxmx> | read-report [player-log|-]".into()),
    }
}

fn print_help() {
    println!("Rove Stellar Player package helper\n\nUsage:\n  rove-stellar pack <reviewed-script.luau> <model.rbxmx>\n  rove-stellar read-report [player-log|-]\n\nPack embeds reviewed Luau into a Roblox client model for an editable experience.\nRead-report extracts bounded ROVE_STELLAR_PLAYER_V1 records as NDJSON.");
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file =
        File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut data = Vec::with_capacity(limit.min(64 * 1024));
    file.take((limit + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if data.len() > limit {
        return Err(format!(
            "{} exceeds the {} byte limit",
            path.display(),
            limit
        ));
    }
    Ok(data)
}

fn validate_xml_text(source: &str) -> Result<(), String> {
    if source.is_empty() || source.len() > MAX_SOURCE_BYTES {
        return Err("source must be between 1 byte and 64 KiB".into());
    }
    if source.chars().any(|ch| {
        let code = ch as u32;
        !(code == 0x9
            || code == 0xA
            || code == 0xD
            || (0x20..=0xD7FF).contains(&code)
            || (0xE000..=0xFFFD).contains(&code)
            || (0x10000..=0x10FFFF).contains(&code))
    }) {
        return Err(
            "source contains a character that cannot be represented in a Roblox XML place".into(),
        );
    }
    Ok(())
}

fn xml_escape(source: &str) -> String {
    let mut escaped = String::with_capacity(source.len());
    for ch in source.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\r' => escaped.push_str("&#13;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn pack(source_path: &Path, output_path: &Path) -> Result<(), String> {
    let source_bytes = read_limited(source_path, MAX_SOURCE_BYTES)?;
    let source =
        std::str::from_utf8(&source_bytes).map_err(|_| "source is not valid UTF-8".to_owned())?;
    validate_xml_text(source)?;
    let digest = hex(&Sha256::digest(&source_bytes));
    let model = render_model(source, &digest)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output_path)
        .map_err(|error| format!("cannot create {}: {error}", output_path.display()))?;
    let write_result = output
        .write_all(model.as_bytes())
        .and_then(|_| output.sync_all());
    if let Err(error) = write_result {
        drop(output);
        let _ = fs::remove_file(output_path);
        return Err(format!("cannot write {}: {error}", output_path.display()));
    }
    println!("{digest}");
    Ok(())
}

fn render_model(source: &str, digest: &str) -> Result<String, String> {
    let expected_tokens = [
        "__ROVE_STELLAR_SOURCE_XML__",
        "__ROVE_STELLAR_SOURCE_SHA256__",
        "__ROVE_STELLAR_ENCODER_XML__",
    ];
    if expected_tokens
        .iter()
        .any(|token| TEMPLATE.matches(token).count() != 1)
    {
        return Err("the built-in Roblox Player template is invalid".into());
    }
    Ok(TEMPLATE
        .replace(expected_tokens[1], &digest)
        .replace(expected_tokens[2], &xml_escape(ENCODER))
        .replace(expected_tokens[0], &xml_escape(source)))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn read_reports(argument: Option<&OsStr>) -> Result<(), String> {
    let input: Box<dyn Read> = if argument.is_none() || argument == Some(OsStr::new("-")) {
        Box::new(io::stdin())
    } else {
        let path = Path::new(argument.expect("argument was checked above"));
        Box::new(
            File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?,
        )
    };
    let mut reader = BufReader::new(input);
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    let mut found = 0usize;
    loop {
        let (line, oversized) = read_bounded_line(&mut reader, MAX_BUFFERED_LINE_BYTES)
            .map_err(|error| format!("cannot read report source: {error}"))?;
        if line.is_empty() && !oversized {
            break;
        }
        if let Some(prefix_length) = report_prefix_length(&line) {
            if oversized {
                return Err("a marked report exceeds the 512 KiB limit".into());
            }
            let report_text = std::str::from_utf8(&line[prefix_length..])
                .map_err(|_| "marked report is not valid UTF-8".to_owned())?
                .trim_end_matches(['\r', '\n']);
            if report_text.len() > MAX_REPORT_BYTES {
                return Err("a marked report exceeds the 512 KiB limit".into());
            }
            let report: Value = serde_json::from_str(report_text)
                .map_err(|error| format!("invalid marked JSON report: {error}"))?;
            validate_report(&report)?;
            serde_json::to_writer(&mut output, &report)
                .map_err(|error| format!("cannot serialize report: {error}"))?;
            output
                .write_all(b"\n")
                .map_err(|error| format!("cannot write report: {error}"))?;
            output
                .flush()
                .map_err(|error| format!("cannot flush report: {error}"))?;
            found += 1;
        }
    }
    if found == 0 {
        return Err("no ROVE_STELLAR_PLAYER_V1 report was found".into());
    }
    Ok(())
}

fn report_prefix_length(line: &[u8]) -> Option<usize> {
    if line.starts_with(PREFIX.as_bytes()) {
        Some(PREFIX.len())
    } else {
        let envelope = b"[FLog::Output] ";
        if let Some(message) = line.strip_prefix(envelope) {
            return message
                .starts_with(PREFIX.as_bytes())
                .then_some(envelope.len() + PREFIX.len());
        }
        let position = line
            .windows(envelope.len())
            .position(|window| window == envelope)?;
        let header = std::str::from_utf8(&line[..position]).ok()?;
        let message_start = position + envelope.len();
        (valid_log_header(header) && line[message_start..].starts_with(PREFIX.as_bytes()))
            .then_some(message_start + PREFIX.len())
    }
}

fn valid_log_header(header: &str) -> bool {
    let Some((timestamp, fields)) = header.split_once(',') else {
        return false;
    };
    let timestamp_bytes = timestamp.as_bytes();
    if timestamp_bytes.len() < 21
        || timestamp_bytes.get(4) != Some(&b'-')
        || timestamp_bytes.get(7) != Some(&b'-')
        || timestamp_bytes.get(10) != Some(&b'T')
        || timestamp_bytes.get(13) != Some(&b':')
        || timestamp_bytes.get(16) != Some(&b':')
        || timestamp_bytes.last() != Some(&b'Z')
        || !timestamp_bytes[0..4].iter().all(u8::is_ascii_digit)
        || !timestamp_bytes[5..7].iter().all(u8::is_ascii_digit)
        || !timestamp_bytes[8..10].iter().all(u8::is_ascii_digit)
        || !timestamp_bytes[11..13].iter().all(u8::is_ascii_digit)
        || !timestamp_bytes[14..16].iter().all(u8::is_ascii_digit)
        || !timestamp_bytes[17..timestamp_bytes.len() - 1]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b'.')
    {
        return false;
    }
    let fields: Vec<&str> = fields.trim_end().split(',').collect();
    if !(3..=4).contains(&fields.len()) {
        return false;
    }
    let metric = fields[0];
    let thread = fields[1];
    let severity = fields[2];
    !metric.is_empty()
        && metric
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && thread.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !thread.is_empty()
        && severity.bytes().all(|byte| byte.is_ascii_digit())
        && !severity.is_empty()
        && fields.get(3).is_none_or(|label| {
            !label.is_empty() && label.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
}

fn read_bounded_line<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut line = Vec::with_capacity(limit.min(4096));
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((line, oversized));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        let copied = consumed.min(limit.saturating_sub(line.len()));
        line.extend_from_slice(&available[..copied]);
        oversized |= copied < consumed;
        reader.consume(consumed);
        if newline.is_some() {
            return Ok((line, oversized));
        }
    }
}

fn validate_report(report: &Value) -> Result<(), String> {
    let object = report.as_object().ok_or("report must be a JSON object")?;
    if object.get("protocol").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported report protocol".into());
    }
    let runtime = object.get("runtime").and_then(Value::as_str);
    let status = object.get("status").and_then(Value::as_str);
    if !matches!(
        (runtime, status),
        (
            Some("roblox-player"),
            Some(
                "ok" | "script_error"
                    | "output_limit"
                    | "sandbox_unavailable"
                    | "harness_error"
                    | "engine_error"
            )
        ) | (Some("unsupported"), Some("unsupported_context"))
    ) {
        return Err("report runtime and status are not a supported Player outcome".into());
    }
    let sandbox = object
        .get("sandbox")
        .and_then(Value::as_bool)
        .ok_or("report sandbox field must be boolean")?;
    if status == Some("ok") && !sandbox {
        return Err("successful report does not confirm sandbox checks".into());
    }
    if !object
        .get("placeId")
        .and_then(Value::as_str)
        .is_some_and(|value| {
            !value.is_empty()
                && value.len() <= 32
                && value.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err("report placeId must be a decimal string".into());
    }
    if !object
        .get("sourceSha256")
        .and_then(Value::as_str)
        .is_some_and(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
    {
        return Err("report sourceSha256 must be a lowercase SHA-256 digest".into());
    }
    let prints = object
        .get("prints")
        .and_then(Value::as_array)
        .ok_or("report prints must be an array")?;
    if prints.len() > MAX_PRINTS {
        return Err("report contains too many print entries".into());
    }
    let mut total_print_bytes = 0usize;
    for entry in prints {
        let entry = entry
            .as_object()
            .ok_or("each print entry must be an object")?;
        if !matches!(
            entry.get("level").and_then(Value::as_str),
            Some("print" | "warn")
        ) {
            return Err("each print level must be print or warn".into());
        }
        let text = entry
            .get("text")
            .and_then(Value::as_str)
            .ok_or("print text must be a string")?;
        if text.len() > MAX_PRINT_TEXT_BYTES {
            return Err("a print entry exceeds 4096 UTF-8 bytes".into());
        }
        total_print_bytes = total_print_bytes
            .checked_add(text.len())
            .ok_or("print size overflow")?;
        if total_print_bytes > MAX_PRINT_TOTAL_BYTES {
            return Err("report print data exceeds 16 KiB".into());
        }
    }
    let returns = object
        .get("returns")
        .and_then(Value::as_array)
        .ok_or("report returns must be an array")?;
    if returns.len() > MAX_RETURNS {
        return Err("report contains too many return entries".into());
    }
    for value in returns {
        let value = value
            .as_object()
            .ok_or("each return entry must be an object")?;
        let kind = value.get("type").and_then(Value::as_str);
        if !kind.is_some_and(|kind| {
            !kind.is_empty()
                && kind.len() <= 32
                && kind
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) {
            return Err("return entry is missing a valid type".into());
        }
    }
    validate_json_size(report, 0)
}

fn validate_json_size(value: &Value, depth: usize) -> Result<(), String> {
    if depth > 24 {
        return Err("report JSON is nested too deeply".into());
    }
    match value {
        Value::Array(items) => {
            if items.len() > 1024 {
                return Err("report JSON array is too large".into());
            }
            for item in items {
                validate_json_size(item, depth + 1)?;
            }
        }
        Value::Object(items) => {
            if items.len() > 128 {
                return Err("report JSON object is too large".into());
            }
            for item in items.values() {
                validate_json_size(item, depth + 1)?;
            }
        }
        Value::String(value) if value.len() > MAX_REPORT_BYTES => {
            return Err("report string exceeds its limit".into())
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample_report() -> Value {
        serde_json::from_str(r#"{"protocol":1,"runtime":"roblox-player","status":"ok","placeId":"12345","sourceSha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","sandbox":true,"prints":[{"level":"print","text":"hello"},{"level":"warn","text":"warning"}],"returns":[{"type":"number","value":42},{"type":"nil"}]}"#).unwrap()
    }

    #[test]
    fn validates_expected_report_and_ordered_nil_return() {
        let report = sample_report();
        validate_report(&report).unwrap();
        let returns = report["returns"].as_array().unwrap();
        assert_eq!(returns[0]["value"], 42);
        assert_eq!(returns[1]["type"], "nil");
    }

    #[test]
    fn rejects_wrong_runtime_status_and_bad_hash() {
        for (field, value) in [
            ("runtime", "studio"),
            ("status", "made_up"),
            ("sourceSha256", "ABC"),
        ] {
            let mut report = sample_report();
            report[field] = Value::String(value.into());
            assert!(validate_report(&report).is_err());
        }
    }

    #[test]
    fn successful_report_requires_sandbox_confirmation() {
        let mut report = sample_report();
        report["sandbox"] = Value::Bool(false);
        assert!(validate_report(&report).is_err());
        report["sandbox"] = Value::Bool(true);
        report["runtime"] = Value::String("unsupported".into());
        assert!(validate_report(&report).is_err());
    }

    #[test]
    fn preserves_known_failure_reports_with_prints() {
        let mut report = sample_report();
        report["status"] = Value::String("script_error".into());
        report["error"] = Value::String("example failure".into());
        validate_report(&report).unwrap();
        assert_eq!(report["prints"][0]["text"], "hello");
        assert_eq!(report["error"], "example failure");
    }

    #[test]
    fn rejects_unbounded_prints_and_unknown_log_level() {
        let mut report = sample_report();
        report["prints"] = serde_json::json!([{"level":"debug","text":"x"}]);
        assert!(validate_report(&report).is_err());
        let huge = "x".repeat(MAX_PRINT_TOTAL_BYTES + 1);
        report["prints"] = serde_json::json!([{"level":"print","text":huge}]);
        assert!(validate_report(&report).is_err());
    }

    #[test]
    fn xml_escaping_handles_text_and_xml_control_characters() {
        assert_eq!(xml_escape("a<&>\r\nb"), "a&lt;&amp;&gt;&#13;\nb");
        assert!(validate_xml_text("return 1\n").is_ok());
        assert!(validate_xml_text("bad\u{1}").is_err());
    }

    #[test]
    fn rendered_model_preserves_all_literal_tokens_and_crlf_source() {
        let source = "print(\"__ROVE_STELLAR_SOURCE_XML__ __ROVE_STELLAR_SOURCE_SHA256__ __ROVE_STELLAR_ENCODER_XML__\")\r\nreturn \"<&]]>\r\n\"";
        let model = render_model(source, &"a".repeat(64)).unwrap();
        assert!(model.contains("__ROVE_STELLAR_SOURCE_XML__"));
        assert!(model.contains("__ROVE_STELLAR_SOURCE_SHA256__"));
        assert!(model.contains("__ROVE_STELLAR_ENCODER_XML__"));
        assert!(model.contains("return \"&lt;&amp;]]&gt;&#13;\n\""));
        assert!(!model.contains("__ROVE_STELLAR_SOURCE_XML__\n"));
    }

    #[test]
    fn lowercase_hex_digest_has_fixed_length() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }

    #[test]
    fn report_prefix_only_matches_line_start_or_known_engine_envelope() {
        assert_eq!(report_prefix_length(PREFIX.as_bytes()), Some(PREFIX.len()));
        assert_eq!(
            report_prefix_length(format!("[FLog::Output] {PREFIX}{{}}").as_bytes()),
            Some("[FLog::Output] ".len() + PREFIX.len())
        );
        let timestamp =
            format!("2026-11-21T05:57:57.611Z,2.611228,c424f40,6 [FLog::Output] {PREFIX}{{}}");
        assert_eq!(
            report_prefix_length(timestamp.as_bytes()),
            Some(timestamp.find(PREFIX).unwrap() + PREFIX.len())
        );
        let info =
            format!("2026-11-21T05:57:57.611Z,2.611228,c424f40,6,Info [FLog::Output] {PREFIX}{{}}");
        assert_eq!(
            report_prefix_length(info.as_bytes()),
            Some(info.find(PREFIX).unwrap() + PREFIX.len())
        );
        assert_eq!(
            report_prefix_length(format!("game text {PREFIX}{{}}").as_bytes()),
            None
        );
        assert_eq!(
            report_prefix_length(b"normal log [FLog::Output] not a report"),
            None
        );
    }

    #[test]
    fn bounded_reader_skips_unrelated_oversized_lines() {
        let input = format!("{}\n{PREFIX}{{}}\n", "x".repeat(1024 * 1024));
        let mut reader = io::BufReader::new(input.as_bytes());
        let (line, oversized) = read_bounded_line(&mut reader, 1024).unwrap();
        assert!(oversized);
        assert!(line.iter().all(|byte| *byte == b'x'));
        let (line, oversized) = read_bounded_line(&mut reader, 1024).unwrap();
        assert!(!oversized);
        assert_eq!(line, format!("{PREFIX}{{}}\n").as_bytes());
    }

    #[test]
    fn report_json_size_is_bounded() {
        let mut object = serde_json::Map::new();
        object.insert("nested".into(), serde_json::json!([{"more": ["ok"]}]));
        validate_json_size(&Value::Object(object), 0).unwrap();
        let mut deep = Value::Null;
        for _ in 0..26 {
            deep = serde_json::json!([deep]);
        }
        assert!(validate_json_size(&deep, 0).is_err());
    }

    #[test]
    fn accepts_maximum_supported_nested_table_return_shape() {
        fn encoded_table(levels: usize) -> Value {
            let child = if levels == 0 {
                serde_json::json!({"type":"nil"})
            } else {
                encoded_table(levels - 1)
            };
            serde_json::json!({
                "type":"table",
                "entries":[{
                    "key":{"type":"number","value":1},
                    "value":child
                }]
            })
        }
        let mut report = sample_report();
        report["returns"] = serde_json::json!([encoded_table(4)]);
        validate_report(&report).unwrap();
    }
}
