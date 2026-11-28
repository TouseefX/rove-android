//! Running a verified `155` program in this workspace's own Luau.
//!
//! The signed body is Luau bytecode, not source. `mlua` in `luau` mode loads
//! bytecode through `ChunkMode::Binary` — the same call `rustudio-engine`
//! already uses for CoreScripts — so a verified challenge needs no C++ host and
//! no `rust-runtime-sys`: the VM that runs a place's scripts runs this too, and
//! Android gets it from the APK it already ships.
//!
//! Two things happen before the chunk is loaded:
//!
//! 1. **`bytecode_format::normalize`.** A signed body's instruction opcodes are
//!    multiplied by an odd constant (227 in the current build) as an encoding,
//!    not a cipher: the loader would see `LOP_*` numbers far outside its table
//!    and refuse the chunk. `normalize` finds the multiplier, proves the
//!    resulting program structurally sound, and rewrites only the opcode bytes —
//!    every other byte, including the signature trailer, is preserved.
//! 2. **An isolated environment.** The program is Roblox's, not a place script's:
//!    it expects `game.JobId`, `Random.new(seed)` seeded with the two 32-bit
//!    arguments as one 64-bit value, `bit32`, `newproxy` with `__namecall`,
//!    `RunService:IsStudio()` answering `false`, `UserSettings()`, and
//!    `os.exit == nil`. It is given a fresh `Lua` with that environment and
//!    nothing else — a challenge program must not be able to reach a place's
//!    globals, and this one cannot.

use crate::program_check::{native_unsigned_result, ProgramError, VerifiedProgram};
use serde_json::{json, Value};

/// What the VM reported about the run, in the summary's own terms.
pub struct ProgramRun {
    pub answer: u32,
    pub format: Value,
    pub integrity: Option<Value>,
}

/// Run a verified program for one job id and return its answer.
///
/// The `(second, first)` order is [`VerifiedProgram::arguments`]: the server
/// builds the challenge so that the program's first argument is the second word
/// of the packet. It is not a detail to get either way round — the program's
/// arithmetic mixes them, so a swapped call returns a number the server rejects
/// without saying why.
pub fn run_verified_program(
    verified: &VerifiedProgram,
    job_id: &str,
) -> Result<ProgramRun, ProgramError> {
    let original = verified.original_bytes();
    let normalized = crate::bytecode_format::normalize(original, None)
        .map_err(|error| ProgramError::from_reason(error.reason()))?;
    let [second, first] = verified.arguments();
    let value = evaluate(&normalized.bytes, job_id, second, first)?;
    let answer = native_unsigned_result(value)?;
    Ok(ProgramRun {
        answer,
        format: normalized.metadata,
        integrity: None,
    })
}

/// Load and call the chunk in a VM of its own.
///
/// `pub(crate)` so the tests can run the pinned compiler fixture through the
/// same path a challenge body takes — a VM test that built its own environment
/// would prove nothing about the one the challenge is given.
pub(crate) fn evaluate(
    bytes: &[u8],
    job_id: &str,
    second: u32,
    first: u32,
) -> Result<f64, ProgramError> {
    let lua = mlua::Lua::new();
    let environment = challenge_environment(&lua, job_id)
        .map_err(|_| ProgramError::from_reason("challenge-environment-failed"))?;
    let chunk = lua
        .load(bytes)
        .set_mode(mlua::chunk::ChunkMode::Binary)
        .set_name("=SignedProgramChallenge")
        .set_environment(environment)
        .into_function()
        .map_err(|_| ProgramError::from_reason("verified-program-bytecode-rejected"))?;
    let value: mlua::Value = chunk
        .call((f64::from(second), f64::from(first)))
        .map_err(|_| ProgramError::from_reason("verified-program-runtime-failed"))?;
    match value {
        mlua::Value::Number(number) => Ok(number),
        // A program that returns an integer rather than a float has answered
        // just as plainly; anything else did not answer with a number, and a
        // fabricated zero would be the answer this whole module refuses to make
        // up.
        mlua::Value::Integer(number) => Ok(number as f64),
        _ => Err(ProgramError::from_reason("program-returned-no-number")),
    }
}

/// The globals a signed challenge program expects, and no others.
///
/// Every one of these is something the program reads: the job id it checks
/// against the session, `Random` seeded the way the native host seeds it, the
/// bit library its mixing uses, `newproxy`/`__namecall` where it builds its
/// wrapper, a `RunService` that says "not Studio", `UserSettings`, and an `os`
/// table with no `exit`. `os.exit` specifically: upstream's check is that a
/// client which cannot exit is one the program's own guard accepts.
fn challenge_environment(lua: &mlua::Lua, job_id: &str) -> mlua::Result<mlua::Table> {
    let env = lua.create_table()?;

    // `game`, with the one field the program reads.
    let game = lua.create_table()?;
    game.set("JobId", job_id)?;
    game.set("PlaceId", 0)?;
    // `game:GetService` answers the two services a challenge program names;
    // anything else is refused rather than invented, as on a real client the
    // service would exist and here it does not.
    let run_service = lua.create_table()?;
    run_service.set(
        "IsStudio",
        lua.create_function(|_, ()| Ok(false))?,
    )?;
    let user_settings = lua.create_function(|lua, ()| {
        let settings = lua.create_table()?;
        settings.set("GetService", lua.create_function(|_, _: mlua::Value| Ok(false))?)?;
        Ok(settings)
    })?;
    let services = lua.create_table()?;
    services.set("RunService", run_service)?;
    game.set(
        "GetService",
        lua.create_function(move |_, (_this, name): (mlua::Value, String)| match name.as_str() {
            "RunService" => Ok(services.get::<mlua::Value>("RunService")?),
            _ => Ok(mlua::Value::Nil),
        })?,
    )?;
    env.set("game", game)?;

    // `Random.new(seed)` with a 64-bit seed built from the two 32-bit words —
    // the native host's own seeding, so the program's sequence matches.
    let random = lua.create_table()?;
    random.set(
        "new",
        lua.create_function(|lua, seed: Option<mlua::Value>| {
            let seed = match seed {
                Some(mlua::Value::Number(n)) => n as u64,
                Some(mlua::Value::Integer(i)) => i as u64,
                _ => 0u64,
            };
            // Luau's own generator: xoshiro256**, seeded through splitmix64.
            // Written here rather than wrapped because this must be the same
            // arithmetic the program expects from `Random.new`.
            let mut state = [0u64; 4];
            let mut z = seed;
            for slot in &mut state {
                z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut x = z;
                x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                *slot = x ^ (x >> 31);
            }
            let lua = lua;
            let object = lua.create_table()?;
            // The generator's state lives in a table rather than in the
            // closure's captured variables: a `Fn` closure — which is what a
            // Luau function bound through `mlua` must be — cannot advance a
            // captured `state`, and a `FnMut` one is not what `create_function`
            // takes. The table is the object's own, so two `Random.new` calls
            // cannot share a sequence.
            let state_table = lua.create_table()?;
            for (index, word) in state.iter().enumerate() {
                state_table.set(index + 1, *word as f64)?;
            }
            let words = state_table.clone();
            let next = lua.create_function(move |_, ()| {
                // 64 bits from two 32-bit halves, as `Random:NextInteger`
                // consumes them; the state advances in place.
                let mut s = [0u64; 4];
                for index in 0..4 {
                    let word: f64 = words.get(index + 1)?;
                    s[index] = word as u64;
                }
                let result = s[0]
                    .wrapping_add(s[3])
                    .rotate_left(23)
                    .wrapping_add(s[0]);
                let t = s[1] << 17;
                s[2] ^= s[0];
                s[3] ^= s[1];
                s[1] ^= s[2];
                s[0] ^= s[3];
                s[2] ^= t;
                s[3] = s[3].rotate_left(45);
                for (index, word) in s.iter().enumerate() {
                    words.set(index + 1, *word as f64)?;
                }
                Ok((result >> 11) as f64 / (1u64 << 53) as f64)
            })?;
            object.set("NextNumber", next.clone())?;
            object.set(
                "NextInteger",
                lua.create_function(|_, (this, lo, hi): (mlua::Table, f64, f64)| {
                    let next: mlua::Function = this.get("NextNumber")?;
                    let unit: f64 = next.call(())?;
                    let lo = lo.trunc();
                    let hi = hi.trunc();
                    let span = (hi - lo + 1.0).max(1.0);
                    Ok((lo + (unit * span).floor()).clamp(lo, hi))
                })?,
            )?;
            Ok(object)
        })?,
    )?;
    env.set("Random", random)?;

    // `bit32`: Luau's own, not a reimplementation.
    env.set("bit32", lua.globals().get::<mlua::Value>("bit32")?)?;
    // `math`, `string`, `table`, `tostring`, `type`, `select`, `assert`,
    // `error`, `pcall`, `xpcall` — the plain library a program may use.
    for name in [
        "math", "string", "table", "utf8", "coroutine", "tostring", "tonumber", "type",
        "assert", "error", "pcall", "xpcall", "select", "next", "ipairs", "pairs",
        "rawget", "rawset", "rawequal", "setmetatable", "getmetatable", "unpack",
        "newproxy",
    ] {
        let value: mlua::Value = lua.globals().get(name)?;
        env.set(name, value)?;
    }
    // `os` **without `exit`**: the guard a client is accepted by is that it
    // cannot leave the process from a program. `time` and `clock` are copied
    // from the host's own table rather than reimplemented, so a program that
    // reads them gets the same numbers it would anywhere.
    let os = lua.create_table()?;
    if let Ok(mlua::Value::Table(host)) = lua.globals().get::<mlua::Value>("os") {
        for name in ["time", "clock", "date"] {
            os.set(name, host.get::<mlua::Value>(name)?)?;
        }
    }
    env.set("os", os)?;
    env.set("UserSettings", user_settings)?;
    Ok(env)
}

/// The summary a report shows for a run, without the answer (which is the
/// server's).
pub fn report(program: &ProgramRun) -> Value {
    json!({
        "answer_prepared": true,
        "bytecode": program.format,
        "runtime_integrity": program.integrity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compiled `calculation.luau` fixture (compiler 0.730, `-O1 -g1`),
    /// run the way a challenge body is: normalized, loaded as a binary chunk in
    /// the restricted environment, called with two numbers.
    ///
    /// It returns `bit32.bxor(a + b, 0x1234)`, so the answer is arithmetic this
    /// test can predict from the source — which is the whole point. A VM that
    /// loaded the chunk but ran it in the wrong environment would fail on
    /// `bit32` or on `...` rather than quietly returning a number.
    const CALCULATION: &[u8] = include_bytes!("../tests/fixtures/luau-v9/calculation.luac");

    #[test]
    fn a_verified_program_runs_in_the_restricted_environment() {
        let normalized = crate::bytecode_format::normalize(CALCULATION, None)
            .expect("the pinned fixture needs no rewriting");
        // `calculation.luau` in full:
        //
        //     local a, b = ...
        //     local values = {marker = "owned-v9", enabled = true}
        //     values.sum = a + b
        //     if a < b then values.sum += 1 end
        //     local function calculate(value) return bit32.bxor(value, 0x1234) end
        //     return calculate(values.sum), values.marker
        //
        // so with `(a, b) = (4, 5)`: `4 + 5 = 9`, plus one because `4 < 5`,
        // `bit32.bxor(10, 0x1234) = 4670`, and `evaluate` reads the first return.
        let (a, b) = (4u32, 5u32);
        let sum = a + b + u32::from(a < b);
        let value = evaluate(
            &normalized.bytes,
            "00000000-0000-4000-8000-000000000001",
            a,
            b,
        )
        .expect("the fixture runs");
        assert_eq!(value, f64::from(sum ^ 0x1234));
        assert_eq!(value, 4670.0, "the number the source's arithmetic gives");
    }

    #[test]
    fn a_program_with_no_environment_cannot_reach_the_host() {
        // The environment is built fresh per run and holds nothing of the
        // caller's: a plain `print` is not in it, and neither is `require`.
        let lua = mlua::Lua::new();
        let env = challenge_environment(&lua, "00000000-0000-4000-8000-000000000001")
            .expect("the environment builds");
        for name in ["print", "require", "loadstring", "dofile", "collectgarbage"] {
            let value: mlua::Value = env.get(name).expect("a table read");
            assert!(
                matches!(value, mlua::Value::Nil),
                "{name} must not be in a challenge's environment"
            );
        }
        // And what the program does need is there.
        for name in ["Random", "bit32", "os", "UserSettings", "newproxy"] {
            let value: mlua::Value = env.get(name).expect("a table read");
            assert!(
                !matches!(value, mlua::Value::Nil),
                "{name} must be in a challenge's environment"
            );
        }
        let os: mlua::Table = env.get("os").expect("os");
        let exit: mlua::Value = os.get("exit").expect("a table read");
        assert!(matches!(exit, mlua::Value::Nil), "a program cannot exit");
    }
}
