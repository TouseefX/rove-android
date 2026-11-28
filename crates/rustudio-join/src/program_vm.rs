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

/// PCG32 (`XSH-RR`), increment 105 — Roblox's `Random`, the same arithmetic
/// `rustudio_engine::ltypes::LRandom` uses and the same increment
/// (`rustudio_core::mech::RANDOM_PCG_INC`).
///
/// **This is not a `math.random` stand-in.** A `155` program seeds `Random.new`
/// with its two arguments as one 64-bit value and reads `NextInteger`, so the
/// sequence *is* part of the answer: a different generator, or one that keeps
/// its state in an `f64` and loses eleven bits, returns a number the server
/// refuses. It is written out here rather than imported from
/// `rustudio-engine`'s `LRandom` because this crate must not depend on the
/// engine (the engine depends on the world; a join does not); the state is a
/// `u64` in Rust, where it belongs, and never a Lua number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pcg32 {
    state: u64,
    inc: u64,
}

impl Pcg32 {
    /// `pcg32_srandom(seed, inc)`, which is how Studio's `Random.new` seeds.
    pub fn new(seed: u64) -> Self {
        let mut r = Pcg32 {
            state: 0,
            inc: rustudio_core::mech::RANDOM_PCG_INC,
        };
        r.next_u32();
        r.state = r.state.wrapping_add(seed);
        r.next_u32();
        r
    }

    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(self.inc);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        xorshifted.rotate_right((old >> 59) as u32)
    }

    /// Two draws, the second in the high half.
    pub fn next_u64(&mut self) -> u64 {
        let lo = self.next_u32() as u64;
        ((self.next_u32() as u64) << 32) | lo
    }

    /// `NextNumber()`: the 64 bits × 2⁻⁶⁴.
    pub fn next_unit(&mut self) -> f64 {
        self.next_u64() as f64 * (-64f64).exp2()
    }

    /// `NextInteger(min, max)`, both inclusive: one 32-bit draw scaled by the
    /// span, exactly as the engine's `LRandom::next_int` does it.
    pub fn next_int(&mut self, min: i64, max: i64) -> i64 {
        let span = (max.wrapping_sub(min) as u64).wrapping_add(1);
        if span == 0 {
            return self.next_u64() as i64;
        }
        if span <= 1 << 32 {
            return min.wrapping_add(((self.next_u32() as u64 * span) >> 32) as i64);
        }
        min.wrapping_add(((self.next_u64() as u128 * span as u128) >> 64) as i64)
    }
}

/// The `Random` a program gets: an object with `Random`'s type name, its
/// `tostring`, and its three methods, backed by [`Pcg32`].
struct ChallengeRandom {
    rng: Pcg32,
}

impl mlua::UserData for ChallengeRandom {
    fn add_fields<F: mlua::UserDataFields<Self>>(f: &mut F) {
        // `typeof(v) == "Random"`, and `tostring(v) == "Random"`: a program
        // that names its own types with `newproxy` compares these.
        f.add_meta_field("__type", "Random");
    }

    fn add_methods<M: mlua::UserDataMethods<Self>>(m: &mut M) {
        m.add_method_mut("NextNumber", |_, this, (a, b): (Option<f64>, Option<f64>)| {
            match (a, b) {
                (None, None) => Ok(this.rng.next_unit()),
                (Some(lo), Some(hi)) => {
                    // `NextNumber(a, b)`: the ends in order, `min·(1 − t) +
                    // max·t` kept inside the interval.
                    let (lo, hi) = if lo > hi { (hi, lo) } else { (lo, hi) };
                    let t = this.rng.next_unit();
                    let r = lo * (1.0 - t) + hi * t;
                    Ok(if r < lo { lo } else { hi.min(r) })
                }
                // One end without the other is not a range, and answering it
                // as a unit number would be inventing an argument.
                _ => Err(mlua::Error::RuntimeError(
                    "Random:NextNumber requires 0 or 2 arguments".into(),
                )),
            }
        });
        m.add_method_mut("NextInteger", |_, this, (a, b): (f64, f64)| {
            let (lo, hi) = (a.floor() as i64, b.floor() as i64);
            if hi < lo {
                return Err(mlua::Error::RuntimeError(
                    "invalid argument #2 to 'NextInteger' (interval is empty)".into(),
                ));
            }
            Ok(this.rng.next_int(lo, hi) as f64)
        });
        m.add_meta_method(mlua::MetaMethod::ToString, |_, _, ()| Ok("Random"));
    }
}

/// What an instance a program is handed stands for.
///
/// mlua's `UserData` methods are one set per *type*, so the three instances
/// share one struct and answer by role: `game` has a job id and services,
/// `RunService` answers the runtime questions, `UserSettings` has its own
/// settings service. Anything a role does not have answers `nil`, which is what
/// the property not existing looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstanceRole {
    Game,
    RunService,
    UserSettings,
}

/// One instance a program may touch, by the properties a program reads about
/// it: `ClassName`, `Name`, `IsA`, and the service lookup.
///
/// **`UserSettings` is not a table.** The bytecode checks
/// `tostring(UserSettings())` byte by byte against `"GlobalBasicSettings"`, and
/// a table prints `table: 0x…`; `typeof(x)` answers `"Instance"` only for
/// something whose metatable says so. `game` is not a table for the same
/// reason: a program that type-checks its environment is checking exactly this.
struct ChallengeInstance {
    class: &'static str,
    name: &'static str,
    role: InstanceRole,
    /// `game`'s issued job id.
    job_id: Option<String>,
    /// The one service `game:GetService` answers (`RunService`).
    run_service: Option<mlua::AnyUserData>,
    /// `UserSettings:GetService("UserGameSettings")`.
    settings: Option<mlua::AnyUserData>,
}

impl mlua::UserData for ChallengeInstance {
    fn add_fields<F: mlua::UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Instance");
        f.add_field_method_get("ClassName", |_, this| Ok(this.class));
        f.add_field_method_get("Name", |_, this| Ok(this.name));
        f.add_field_method_get("JobId", |_, this| Ok(this.job_id.clone()));
        // A real client's DataModel reports these; `0` is what a place that has
        // no ids of its own reports, and no program checks them for equality
        // with anything but itself.
        f.add_field_method_get("PlaceId", |_, this| {
            Ok(match this.role {
                InstanceRole::Game => Some(0.0),
                _ => None,
            })
        });
        f.add_field_method_get("GameId", |_, this| {
            Ok(match this.role {
                InstanceRole::Game => Some(0.0),
                _ => None,
            })
        });
    }

    fn add_methods<M: mlua::UserDataMethods<Self>>(m: &mut M) {
        m.add_method("IsA", |_, this, class: String| {
            // The one hierarchy a program asks about: `Instance`, plus the
            // object's own class. Nothing is claimed to be a `BasePart`.
            Ok(class == "Instance" || class == this.class)
        });
        m.add_method("IsDescendantOf", |_, _, _: mlua::Value| Ok(true));
        m.add_method("GetService", |_, this, name: String| {
            Ok(match (this.role, name.as_str()) {
                (InstanceRole::Game, "RunService") => this
                    .run_service
                    .clone()
                    .map(mlua::Value::UserData)
                    .unwrap_or(mlua::Value::Nil),
                (InstanceRole::UserSettings, "UserGameSettings") => this
                    .settings
                    .clone()
                    .map(mlua::Value::UserData)
                    .unwrap_or(mlua::Value::Nil),
                // Anything else is a service this client does not have, and a
                // service invented on demand would be a program reading a world
                // that is not here.
                _ => mlua::Value::Nil,
            })
        });
        m.add_method("FindService", |_, this, name: String| {
            Ok(match (this.role, name.as_str()) {
                (InstanceRole::Game, "RunService") => this
                    .run_service
                    .clone()
                    .map(mlua::Value::UserData)
                    .unwrap_or(mlua::Value::Nil),
                (InstanceRole::UserSettings, "UserGameSettings") => this
                    .settings
                    .clone()
                    .map(mlua::Value::UserData)
                    .unwrap_or(mlua::Value::Nil),
                _ => mlua::Value::Nil,
            })
        });
        m.add_method("IsUserFeatureEnabled", |_, _, _: String| Ok(false));
        // `RunService`'s own questions, answered by role: a client that is not
        // Studio, is running, and is the client half.
        //
        // Each is a closure of its own rather than one shared helper: an `mlua`
        // callback must be `'static`, so a helper *function* (not a closure
        // capturing anything) or the same three lines repeated. Repeated, so
        // there is nothing for a future edit to capture by accident.
        m.add_method("IsStudio", |_, this, ()| {
            Ok((this.role == InstanceRole::RunService).then_some(false))
        });
        m.add_method("IsClient", |_, this, ()| {
            Ok((this.role == InstanceRole::RunService).then_some(true))
        });
        m.add_method("IsServer", |_, this, ()| {
            Ok((this.role == InstanceRole::RunService).then_some(false))
        });
        m.add_method("IsRunning", |_, this, ()| {
            Ok((this.role == InstanceRole::RunService).then_some(true))
        });
        m.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| Ok(this.name));
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
///
/// **What is deliberately absent**: `print`, `require`, `loadstring`,
/// `dofile`, `collectgarbage`, and the host's own `Random`. A signed program is
/// Roblox's, not a place's, and it gets the globals such a program reads and no
/// way into this process.
fn challenge_environment(lua: &mlua::Lua, job_id: &str) -> mlua::Result<mlua::Table> {
    let env = lua.create_table()?;

    let run_service = lua.create_userdata(ChallengeInstance {
        class: "RunService",
        name: "Run Service",
        role: InstanceRole::RunService,
        job_id: None,
        run_service: None,
        settings: None,
    })?;

    let settings_service = lua.create_userdata(ChallengeInstance {
        class: "UserGameSettings",
        name: "UserGameSettings",
        role: InstanceRole::UserSettings,
        job_id: None,
        run_service: None,
        settings: None,
    })?;
    let user_settings = lua.create_userdata(ChallengeInstance {
        class: "UserSettings",
        name: "GlobalBasicSettings",
        role: InstanceRole::UserSettings,
        job_id: None,
        run_service: None,
        settings: Some(settings_service),
    })?;
    let user_settings_fn = user_settings.clone();
    env.set(
        "UserSettings",
        lua.create_function(move |_, ()| Ok(user_settings_fn.clone()))?,
    )?;

    let game = lua.create_userdata(ChallengeInstance {
        class: "DataModel",
        name: "Game",
        role: InstanceRole::Game,
        job_id: Some(job_id.to_owned()),
        run_service: Some(run_service),
        settings: None,
    })?;
    env.set("game", game)?;

    // `Random.new(seed)`.
    let random = lua.create_table()?;
    random.set(
        "new",
        lua.create_function(|lua, seed: Option<mlua::Value>| {
            // The native host converts the two 32-bit words to one number and
            // seeds with it. `as i64 as u64` and not `as u64`: a value at or
            // above 2^63 is a negative `i64` in the program, and Rust's float
            // to-integer conversion saturates rather than wrapping, so the two
            // spellings differ exactly where a seeded sequence would.
            let seed = match seed {
                Some(mlua::Value::Number(n)) => n.trunc() as i64 as u64,
                Some(mlua::Value::Integer(i)) => i as u64,
                Some(other) => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "invalid argument #1 to 'new' (number expected, got {})",
                        other.type_name()
                    )))
                }
                None => 0,
            };
            lua.create_userdata(ChallengeRandom {
                rng: Pcg32::new(seed),
            })
        })?,
    )?;
    env.set("Random", random)?;

    // `bit32`: Luau's own, not a reimplementation.
    env.set("bit32", lua.globals().get::<mlua::Value>("bit32")?)?;
    // `math`, `string`, `table`, `tostring`, `type`, `select`, `assert`,
    // `error`, `pcall`, `xpcall` — the plain library a program may use.
    //
    // `typeof` is in this list: over a `__type`-carrying userdata it answers
    // the Roblox type (`"Random"`, `"Instance"`), and a program that tells a
    // service from a table uses it.
    for name in [
        "math", "string", "table", "utf8", "coroutine", "tostring", "tonumber", "type",
        "typeof", "assert", "error", "pcall", "xpcall", "select", "next", "ipairs", "pairs",
        "rawget", "rawset", "rawequal", "setmetatable", "getmetatable", "unpack", "newproxy",
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
        for name in ["Random", "bit32", "os", "UserSettings", "newproxy", "typeof", "game"] {
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
    /// **The two strings the `155` bytecode compares.** `tostring` of the
    /// settings object is `"GlobalBasicSettings"` (the check is a byte sum of
    /// 1924, which is that spelling) and `typeof` of an instance is
    /// `"Instance"`. A table fails both, which is why they are userdata.
    #[test]
    fn the_environment_is_shaped_like_a_client() {
        let lua = mlua::Lua::new();
        let env = challenge_environment(&lua, "00000000-0000-4000-8000-000000000001")
            .expect("the environment builds");
        lua.globals().set("__env", env).unwrap();
        let answer: (String, String, String, bool, bool, bool) = lua
            .load(
                r#"
                local env = __env
                return tostring(env.UserSettings()),
                    typeof(env.UserSettings()),
                    typeof(env.game),
                    env.game:GetService("RunService"):IsStudio(),
                    env.game:GetService("RunService"):IsClient(),
                    env.UserSettings():GetService("UserGameSettings") ~= nil
                "#,
            )
            .set_environment(lua.globals().get::<mlua::Table>("__env").unwrap())
            .eval()
            .expect("the program's own checks run");
        assert_eq!(answer.0, "GlobalBasicSettings");
        assert_eq!(answer.1, "Instance");
        assert_eq!(answer.2, "Instance");
        assert!(!answer.3, "a client is not Studio");
        assert!(answer.4, "a client is the client");
        assert!(answer.5, "its settings service exists");
        // And the byte sum the bytecode actually compares: 1924.
        let sum: i64 = answer.0.bytes().map(|b| i64::from(b)).sum();
        assert_eq!(sum, 1924, "the bytecode's own check");
        // The job id is the session's, which is what `game.JobId` is for.
        let job: String = lua
            .load("return __env.game.JobId")
            .set_environment(lua.globals().get::<mlua::Table>("__env").unwrap())
            .eval()
            .expect("a field read");
        assert_eq!(job, "00000000-0000-4000-8000-000000000001");
    }

    /// `Random` is PCG32 with increment 105, and the sequence is part of the
    /// answer: a generator that keeps its state in an `f64` loses eleven bits
    /// per word and returns a number the server refuses.
    #[test]
    fn random_is_pcg32_with_studios_increment_and_state() {
        let mut rng = Pcg32::new(0);
        assert_eq!(rng.inc, rustudio_core::mech::RANDOM_PCG_INC);
        // The first four 32-bit draws from seed 0. Pinned here so a change to
        // the arithmetic is a test failure rather than a wrong `155` answer.
        let draws: Vec<u32> = (0..4).map(|_| rng.next_u32()).collect();
        assert_eq!(draws, GOLDEN_SEED_ZERO, "PCG32 from seed 0");
        // `NextInteger` uses one draw scaled by the span, not a unit number.
        let mut rng = Pcg32::new(12345);
        for _ in 0..64 {
            let value = rng.next_int(7, 11);
            assert!((7..=11).contains(&value), "NextInteger stays in its interval");
        }
        // A full-width span draws 64 bits rather than wrapping to nothing.
        let mut rng = Pcg32::new(1);
        assert_ne!(rng.next_int(i64::MIN, i64::MAX), 0);
    }

    /// PCG32's first four 32-bit draws from seed 0, computed from the published
    /// multiplier and increment in an independent implementation (not by this
    /// one): a wrong rotation, a wrong shift or a state kept in a float all
    /// move these numbers.
    const GOLDEN_SEED_ZERO: [u32; 4] = [0xc867_4a3c, 0x8d2c_333b, 0x1b8d_da22, 0x0469_409e];

}
