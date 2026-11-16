use super::{
    bindables, instance_helpers, remotes, signals,
    world::{self, World, WorldRef},
};
use mlua::Lua;
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn setup() -> (Lua, WorldRef) {
    let snapshot = json!({"version":1,"sequence":1,"services":{"Players":"players"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
        {"id":"players","class_name":"Players","name":"Players","parent":"game","properties":{"LocalPlayer":{"$ref":"player"}},"server_owned":true},
        {"id":"player","class_name":"Player","name":"Tester","parent":"players","properties":{"UserId":12,"SimulationRadius":128},"server_owned":true},
        {"id":"part","class_name":"Part","name":"Part","parent":"game","properties":{"Anchored":false,"NetworkOwnerV3":7},"server_owned":true},
        {"id":"bindable","class_name":"BindableFunction","name":"BindableFunction","parent":"game","properties":{},"server_owned":false}
    ]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    signals::install(&lua, &world).unwrap();
    world::install(&lua, &world).unwrap();
    bindables::install(&lua, &world).unwrap();
    remotes::install(&lua, &world).unwrap();
    instance_helpers::install(&lua, &world).unwrap();
    (lua, world)
}

#[test]
fn callback_helpers_use_the_bindable_callback_the_invoke_path_uses() {
    let (lua, _world) = setup();
    let same: bool = lua
        .load(
            r#"
        local object = game:FindFirstChild("BindableFunction")
        local callback = function(value) return value + 1 end
        setcallbackfunction(object, "OnInvoke", callback)
        return getcallbackfunction(object, "OnInvoke") == callback
            and getcallbackvalue(object, "OnInvoke") == callback
    "#,
        )
        .eval()
        .unwrap();
    assert!(same);
}

#[test]
fn callback_thread_and_instance_cache_are_local() {
    let (lua, _world) = setup();
    let same: bool = lua
        .load(
            r#"
        local object = game:FindFirstChild("BindableFunction")
        local thread = coroutine.create(function() end)
        setcallbackthread(object, "OnInvoke", thread)
        return getcallbackthread(object, "OnInvoke") == thread
    "#,
        )
        .eval()
        .unwrap();
    assert!(same);
    let same_ref_and_listed: bool = lua
        .load(
            r#"
        local part = game:FindFirstChild("Part")
        local bindable = game:FindFirstChild("BindableFunction")
        local found = {}
        for _, item in ipairs(getinstancecache()) do
            assert(compareinstances(item, cloneref(item)))
            found[item.Name] = true
        end
        return clonereference(part) == part
            and found.Part == true
            and found.BindableFunction == true
            and found.game == true
    "#,
        )
        .eval()
        .unwrap();
    assert!(same_ref_and_listed);
}

#[test]
fn local_radius_and_owner_queries_require_received_values() {
    let (lua, _world) = setup();
    let radius: f64 = lua.load("return getsimulationradius()").eval().unwrap();
    let owner: bool = lua
        .load(r#"return getnetworkowner(game:FindFirstChild("Part"))"#)
        .eval()
        .unwrap();
    assert_eq!(radius, 128.0);
    assert!(owner);
}

#[test]
fn hidden_property_and_scriptability_helpers_use_real_values_and_instance_scoped_overrides() {
    let (lua, _world) = setup();
    // The public test snapshot's Part already has a received ownership field;
    // use it to exercise hidden reads, then a real Fire property alias for writes.
    let result: bool = lua
        .load(
            r#"
        local part = game:FindFirstChild('Part')
        local owner, hidden = gethiddenproperty(part, 'NetworkOwnerV3')
        assert(owner == 7 and hidden == true)
        assert(not pcall(function() return part.NetworkOwnerV3 end))
        assert(setscriptable(part, 'NetworkOwnerV3', true) == false)
        assert(part.NetworkOwnerV3 == 7)
        setscriptable(part, 'NetworkOwnerV3', false)
        assert(not pcall(function() return part.NetworkOwnerV3 end))
        local public, public_hidden = gethiddenproperty(part, 'Anchored')
        assert(public == false and public_hidden == false)
        assert(setscriptable(part, 'missingProperty', true) == nil)
        assert(not pcall(function() gethiddenproperty(part, 'missingProperty') end))
        local fire = Instance.new('Fire')
        local old, hidden = gethiddenproperty(fire, 'size_xml')
        assert(old == 5 and hidden == true)
        assert(sethiddenproperty(fire, 'size_xml', 2) == true)
        local changed, still_hidden = gethiddenproperty(fire, 'size_xml')
        assert(changed == 2 and still_hidden == true and fire.Size == 2)
        assert(setscriptable(fire, 'size_xml', true) == false)
        assert(fire.size_xml == 2)
        setscriptable(fire, 'size_xml', false)
        assert(not pcall(function() return fire.size_xml end))
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(result);
}

#[test]
fn form_factor_raw_scriptability_uses_the_pinned_part_constructor_default() {
    let (lua, _world) = setup();
    // This focused setup installs individual helpers rather than the full
    // rust-host bootstrap, so install the enum catalog needed by the property.
    super::enums::install(&lua).unwrap();
    let result: bool = lua
        .load(
            r#"
        local part = Instance.new('Part')
        assert(isscriptable(part, 'formFactorRaw') == false)
        assert(setscriptable(part, 'siz', true) == nil)
        assert(not pcall(function() return part.siz end))

        assert(setscriptable(part, 'formFactorRaw', true) == false)
        assert(isscriptable(part, 'formFactorRaw') == true)
        assert(part.formFactorRaw == Enum.FormFactor.Brick)

        assert(setscriptable(part, 'formFactorRaw', false) == true)
        assert(isscriptable(part, 'formFactorRaw') == false)
        assert(not pcall(function() return part.formFactorRaw end))
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(result);
}

#[test]
fn valueless_script_property_scriptability_survives_authored_wait() {
    let (lua, _world) = setup();
    super::enums::install(&lua).unwrap();
    let scheduler = Rc::new(RefCell::new(super::scheduler::Scheduler::new()));
    super::scheduler::install(&lua, &scheduler).unwrap();

    lua.load(
        r#"
        task.spawn(function()
            local script = Instance.new('Script')
            assert(isscriptable(script, 'isPlayerScript') == false)
            assert(setscriptable(script, 'isPlayerScript', true) == false)
            task.wait()
            assert(isscriptable(script, 'isPlayerScript') == true)
            -- The native descriptor has no captured value. Scriptability
            -- metadata must not fabricate one when access is enabled.
            assert(not pcall(function() return script.isPlayerScript end))
            assert(setscriptable(script, 'isPlayerScript', false) == true)
            assert(isscriptable(script, 'isPlayerScript') == false)
        end)
    "#,
    )
    .exec()
    .unwrap();

    let first = super::scheduler::step(
        &lua,
        &scheduler,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let resumed = super::scheduler::step(
        &lua,
        &scheduler,
        Instant::now() + Duration::from_secs(1),
        Duration::from_millis(50),
    );
    assert!(resumed.errors.is_empty(), "{:?}", resumed.errors);
}

#[test]
fn scriptability_overrides_do_not_cross_lua_sessions() {
    let (lua, world) = setup();
    lua.load("local p=game:FindFirstChild('Part'); setscriptable(p, 'NetworkOwnerV3', true)")
        .exec()
        .unwrap();
    // Bind a second VM to the same world. This checks VM isolation rather
    // than merely observing two unrelated world snapshots.
    let other = Lua::new();
    signals::install(&other, &world).unwrap();
    world::install(&other, &world).unwrap();
    bindables::install(&other, &world).unwrap();
    remotes::install(&other, &world).unwrap();
    instance_helpers::install(&other, &world).unwrap();
    let hidden: bool = other
        .load("local p=game:FindFirstChild('Part'); return not isscriptable(p, 'NetworkOwnerV3') and not pcall(function() return p.NetworkOwnerV3 end)")
        .eval()
        .unwrap();
    assert!(hidden);
}
