//! Forward captured Actor commands only after their preceding edits commit.
use super::{actor_shadow::MergeReport, movement, world::WorldRef};
use mlua::Lua;
use serde_json::{json, Value as Json};

pub(crate) fn forward(
    lua: &Lua,
    world: &WorldRef,
    actor: &str,
    status: &Json,
    trace: &MergeReport,
) -> Json {
    let Some(rows) = status["movement_intents"].as_array() else {
        return json!({"forwarded":0,"rejected":0,"error":"invalid movement rows"});
    };
    if rows.len() > 256 || !super::session::json_fits(&status["movement_intents"], 256 * 1024) {
        return json!({"forwarded":0,"rejected":rows.len(),"error":"Actor movement limit exceeded"});
    }
    let mut forwarded = 0;
    let mut rejected = Vec::new();
    let (mut sequence, mut cursor) = (0, 0);
    for (index, row) in rows.iter().enumerate() {
        let seq = row["sequence"].as_u64();
        let at = row["mutation_cursor"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok());
        let reason = if status.get("shadow_merge_error").is_some() {
            Some("Actor mutation merge failed")
        } else if seq.is_none_or(|s| s <= sequence)
            || at.is_none_or(|c| c < cursor || c >= trace.mutation_cursors.len())
        {
            Some("invalid Actor movement ordering")
        } else if !matches!(
            row["kind"].as_str(),
            Some(
                "Move"
                    | "MoveTo"
                    | "CancelMoveTo"
                    | "Jump"
                    | "ChangeState"
                    | "SetStateEnabled"
                    | "PivotTo"
                    | "AssemblyLinearVelocity"
                    | "WalkSpeed"
                    | "Gravity"
            )
        ) {
            Some("unsupported Actor movement command")
        } else if trace
            .first_conflict
            .is_some_and(|conflict| conflict < at.unwrap())
        {
            Some("preceding Actor mutation conflicted")
        } else {
            None
        };
        if let Some(s) = seq {
            sequence = sequence.max(s);
        }
        if let Some(c) = at {
            cursor = cursor.max(c);
        }
        if let Some(reason) = reason {
            rejected.push(json!({"index":index,"reason":reason}));
            continue;
        }
        let mut captured = row.clone();
        captured["actor_origin"] = json!(actor);
        captured["actor_sequence"] = json!(sequence);
        match movement::forward_actor_intent(
            lua,
            world,
            captured,
            trace.mutation_cursors[at.unwrap()],
        ) {
            Ok(()) => forwarded += 1,
            Err(error) => rejected.push(json!({"index":index,"reason":error.to_string()})),
        }
    }
    json!({"forwarded":forwarded,"rejected":rejected.len(),"rejections":rejected})
}
