// Layout-sensitive closure operations on the pinned local Luau VM.
// This is compiled into Luau.VM beside the exact headers for this checkout.
#include "lua.h"
#include "lualib.h"
#include "lapi.h"
#include "lstate.h"
#include "lfunc.h"
#include "lgc.h"
#include "lmem.h"
#include <Luau/Bytecode.h>
#include <cstdint>
#include <cstring>
#include <utility>

extern "C" void rove_vm_closures_mark(lua_State* L, int index);

namespace {
char executor_prototype;
constexpr const char* kOwned = "Rove.ExecutorClosures";
constexpr const char* kHooks = "Rove.ClosureHooks";
constexpr const char* kRoutes = "Rove.ClosureRoutes";
constexpr const char* kWrapped = "Rove.WrappedClosures";
constexpr const char* kNativeWrapped = "Rove.NativeWrappedClosures";

void registry_table(lua_State* L, const char* name) {
    lua_rawgetfield(L, LUA_REGISTRYINDEX, name);
    if (lua_istable(L, -1)) return;
    lua_pop(L, 1);
    lua_newtable(L);
    lua_newtable(L);
    lua_pushliteral(L, "k");
    lua_setfield(L, -2, "__mode");
    lua_setmetatable(L, -2);
    lua_pushvalue(L, -1);
    lua_rawsetfield(L, LUA_REGISTRYINDEX, name);
}

void copy_entry(lua_State* L, const char* name, int from, int to) {
    from = lua_absindex(L, from);
    to = lua_absindex(L, to);
    registry_table(L, name);
    lua_pushvalue(L, to);
    lua_pushvalue(L, from);
    lua_rawget(L, -3);
    lua_rawset(L, -3);
    lua_pop(L, 1);
}

void mark_prototype(Proto* p) {
    p->userdata = &executor_prototype;
    for (int i = 0; i < p->sizep; ++i) mark_prototype(p->p[i]);
}

int continuation(lua_State* L, int status) {
    if (status != LUA_OK) {
        lua_error(L);
        return 0;
    }
    return lua_gettop(L);
}

int wrapped_continuation(lua_State* L, int status) { return continuation(L, status); }

int call_wrapped(lua_State* L) {
    const int argc = lua_gettop(L);
    registry_table(L, kWrapped);
    luaA_pushvalue(L, L->ci->func);
    lua_rawget(L, -2);
    if (!lua_isfunction(L, -1)) {
        lua_pop(L, 1);
        lua_pop(L, 1);
        registry_table(L, kNativeWrapped);
        luaA_pushvalue(L, L->ci->func);
        lua_rawget(L, -2);
    }
    lua_remove(L, -2);
    if (!lua_isfunction(L, -1)) {
        lua_pushliteral(L, "wrapped function is unavailable");
        lua_error(L);
        return 0;
    }
    lua_insert(L, 1);
    return lua_callyieldable(L, argc, LUA_MULTRET);
}

void copy_body(lua_State* L, Closure* to, Closure* from) {
    to->inactive = from->inactive;
    to->env = from->env;
    to->stacksize = from->stacksize;
    to->preload = from->preload;
    luaC_objbarrier(L, to, to->env);
    if (to->isC) {
        to->c.f = from->c.f;
        to->c.cont = from->c.cont;
        to->c.debugname = from->c.debugname;
        if (to->c.debugname) luaC_objbarrier(L, to, to->c.debugname);
    } else {
        to->l.p = from->l.p;
        luaC_objbarrier(L, to, to->l.p);
    }
    TValue* dst = to->isC ? to->c.upvals : to->l.uprefs;
    TValue* src = from->isC ? from->c.upvals : from->l.uprefs;
    for (int i = 0; i < from->nupvalues; ++i) {
        setobj(L, &dst[i], &src[i]);
        luaC_barrier(L, to, &dst[i]);
    }
    for (int i = from->nupvalues; i < to->nupvalues; ++i) setnilvalue(&dst[i]);
}

void push_clone(lua_State* L, int index) {
    index = lua_absindex(L, index);
    Closure* original = clvalue(luaA_toobject(L, index));
    if (!original->isC) {
        lua_clonefunction(L, index);
        lua_getfenv(L, index);
        lua_setfenv(L, -2);
        if (original->l.p->userdata == &executor_prototype)
            mark_prototype(clvalue(luaA_toobject(L, -1))->l.p);
        return;
    }
    Closure* clone = luaF_newCclosure(L, original->nupvalues, original->env);
    TValue value;
    setclvalue(L, &value, clone);
    luaA_pushvalue(L, &value);
    copy_body(L, clone, original);
    copy_entry(L, kRoutes, index, -1);
    copy_entry(L, kWrapped, index, -1);
    copy_entry(L, kNativeWrapped, index, -1);
    copy_entry(L, kOwned, index, -1);
}

int clonefunction(lua_State* L) {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    push_clone(L, 1);
    return 1;
}

int hook_target(lua_State* L) {
    const int argc = lua_gettop(L);
    registry_table(L, kRoutes);
    luaA_pushvalue(L, L->ci->func);
    lua_rawget(L, -2);
    lua_remove(L, -2);
    if (!lua_isfunction(L, -1)) {
        lua_pushliteral(L, "hook target is unavailable");
        lua_error(L);
        return 0;
    }
    lua_insert(L, 1);
    return lua_callyieldable(L, argc, LUA_MULTRET);
}

// For a Lua target replaced with a C function, use a small real Luau prototype
// that calls that function with the original argument list.
Proto* forwarding_prototype(lua_State* L, int replacement, Proto* original) {
    Proto* p = luaF_newproto(L);
    p->is_vararg = 1;
    p->maxstacksize = 2;
    p->code = luaM_newarray(L, 5, Instruction, p->memcat);
    p->sizecode = 5;
    p->code[0] = LOP_PREPVARARGS;
    p->code[1] = LOP_LOADK;
    p->code[2] = LOP_GETVARARGS | (1u << 8);
    p->code[3] = LOP_CALL;
    p->code[4] = LOP_RETURN;
    p->codeentry = p->code;
    p->k = luaM_newarray(L, 1, TValue, p->memcat);
    p->sizek = 1;
    setobj(L, &p->k[0], luaA_toobject(L, replacement));
    luaC_barrier(L, p, &p->k[0]);
    p->source = original->source;
    p->debugname = original->debugname;
    p->linedefined = original->linedefined;
    p->userdata = original->userdata;
    return p;
}

int hookfunction(lua_State* L) {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    luaL_checktype(L, 2, LUA_TFUNCTION);
    Closure* target = clvalue(luaA_toobject(L, 1));
    Closure* replacement = clvalue(luaA_toobject(L, 2));
    if (target->inactive || replacement->inactive) {
        lua_pushliteral(L, "inactive prototypes cannot be hooked");
        lua_error(L);
        return 0;
    }
    push_clone(L, 1);
    const int previous = lua_gettop(L);
    registry_table(L, kHooks);
    lua_pushvalue(L, 1);
    lua_rawget(L, -2);
    const bool first = lua_isnil(L, -1);
    lua_pop(L, 1);
    if (first) {
        lua_pushvalue(L, 1);
        push_clone(L, previous);
        lua_rawset(L, -3);
    }
    lua_pop(L, 1);

    if (target->isC) {
        push_clone(L, 2);
        const int route = lua_gettop(L);
        registry_table(L, kRoutes);
        lua_pushvalue(L, 1);
        lua_pushvalue(L, route);
        lua_rawset(L, -3);
        lua_pop(L, 2);
        target->c.f = hook_target;
        target->c.cont = continuation;
    } else if (!replacement->isC && replacement->nupvalues <= target->nupvalues) {
        copy_body(L, target, replacement);
    } else {
        if (!replacement->isC) {
            push_clone(L, 2);
            target->l.p = forwarding_prototype(L, -1, target->l.p);
            lua_pop(L, 1);
            target->env = replacement->env;
            luaC_objbarrier(L, target, target->env);
        } else {
            target->l.p = forwarding_prototype(L, 2, target->l.p);
        }
        target->stacksize = target->l.p->maxstacksize;
        target->preload = 0;
        luaC_objbarrier(L, target, target->l.p);
        for (int i = 0; i < target->nupvalues; ++i) setnilvalue(&target->l.uprefs[i]);
    }
    lua_setsafeenv(L, LUA_GLOBALSINDEX, false);
    lua_pushvalue(L, previous);
    return 1;
}

int restorefunction(lua_State* L) {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    registry_table(L, kHooks);
    lua_pushvalue(L, 1);
    lua_rawget(L, -2);
    if (!lua_isfunction(L, -1)) {
        lua_pushliteral(L, "function has not been hooked");
        lua_error(L);
        return 0;
    }
    Closure* target = clvalue(luaA_toobject(L, 1));
    Closure* original = clvalue(luaA_toobject(L, -1));
    copy_body(L, target, original);
    copy_entry(L, kRoutes, -1, 1);
    lua_pop(L, 1);
    lua_pushvalue(L, 1);
    lua_pushnil(L);
    lua_rawset(L, -3);
    return 0;
}

int newcclosure(lua_State* L) {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    const char* debug_name = luaL_optstring(L, 2, "newcclosure");
    if (lua_iscfunction(L, 1)) {
        lua_pushvalue(L, 1);
        return 1;
    }
    lua_pushcclosurek(L, call_wrapped, debug_name, 0, wrapped_continuation);
    registry_table(L, kWrapped);
    lua_pushvalue(L, -2);
    lua_pushvalue(L, 1);
    lua_rawset(L, -3);
    lua_pop(L, 1);
    lua_getfenv(L, 1);
    lua_setfenv(L, -2);
    rove_vm_closures_mark(L, -1);
    return 1;
}

int isnewcclosure(lua_State* L) {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    registry_table(L, kWrapped);
    lua_pushvalue(L, 1);
    lua_rawget(L, -2);
    const bool result = lua_isfunction(L, -1);
    lua_pop(L, 2);
    lua_pushboolean(L, result);
    return 1;
}

int iscclosure(lua_State* L) {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    lua_pushboolean(L, lua_iscfunction(L, 1));
    return 1;
}

int islclosure(lua_State* L) {
    luaL_checktype(L, 1, LUA_TFUNCTION);
    lua_pushboolean(L, lua_isLfunction(L, 1));
    return 1;
}

int instance_namecall(lua_State* L) {
    const int argc = lua_gettop(L);
    const char* method = lua_namecallatom(L, nullptr);
    if (!method) {
        lua_pushliteral(L, "namecall method is unavailable");
        lua_error(L);
        return 0;
    }
    lua_pushstring(L, method);
    lua_gettable(L, 1);
    if (!lua_isfunction(L, -1)) {
        lua_pushfstring(L, "attempt to call missing method '%s'", method);
        lua_error(L);
        return 0;
    }
    lua_insert(L, 1);
    return lua_callyieldable(L, argc, LUA_MULTRET);
}

int isexecutorclosure(lua_State* L) {
    if (!lua_isfunction(L, 1)) {
        lua_pushboolean(L, 0);
        return 1;
    }
    Closure* closure = clvalue(luaA_toobject(L, 1));
    if (!closure->isC) {
        lua_pushboolean(L, closure->l.p->userdata == &executor_prototype);
        return 1;
    }
    registry_table(L, kOwned);
    lua_pushvalue(L, 1);
    lua_rawget(L, -2);
    const bool result = lua_toboolean(L, -1);
    lua_pop(L, 2);
    lua_pushboolean(L, result);
    return 1;
}

int hookmetamethod(lua_State* L) {
    luaL_checkany(L, 1);
    luaL_checktype(L, 2, LUA_TSTRING);
    luaL_checktype(L, 3, LUA_TFUNCTION);
    if (!lua_getmetatable(L, 1)) {
        lua_pushliteral(L, "target has no metatable");
        lua_error(L);
        return 0;
    }
    // hookmetamethod is an executor primitive and must be able to update a
    // protected metatable, while leaving its readonly state intact for Lua.
    const int metatable = lua_absindex(L, -1);
    const bool was_readonly = lua_getreadonly(L, metatable) != 0;
    if (was_readonly) {
        lua_setreadonly(L, metatable, 0);
    }
    lua_pushvalue(L, 2);
    lua_rawget(L, -2);
    if (lua_isnil(L, -1) && lua_type(L, 1) == LUA_TUSERDATA &&
        std::strcmp(lua_tostring(L, 2), "__namecall") == 0) {
        lua_pop(L, 1);
        lua_pushcclosurek(L, instance_namecall, "Rove.__namecall", 0, continuation);
        rove_vm_closures_mark(L, -1);
        lua_pushvalue(L, 2);
        lua_pushvalue(L, -2);
        lua_rawset(L, -4);
        // The inserted forwarding closure is the previous namecall callback.
        // The common replacement code below returns it to the hooking script.
    }
    // Native clients commonly add a first __namecall hook to a userdata
    // metatable where no Lua field existed yet; nil is a valid old value.
    lua_pushvalue(L, 2);
    lua_pushvalue(L, 3);
    lua_rawset(L, -4);
    if (was_readonly) {
        lua_setreadonly(L, metatable, 1);
    }
    // The VM caches absent metamethods in the table flags. Reassigning the
    // same metatable invalidates those negative-cache bits after the raw write.
    lua_pushvalue(L, -2);
    lua_setmetatable(L, 1);
    return 1;
}

int getnamecallmethod(lua_State* L) {
    const char* name = lua_namecallatom(L, nullptr);
    if (name) lua_pushstring(L, name);
    else lua_pushnil(L);
    return 1;
}

void wrap_coroutine_entry(lua_State* L, int coroutine, const char* name) {
    coroutine = lua_absindex(L, coroutine);
    lua_getfield(L, coroutine, name);
    if (!lua_isfunction(L, -1)) {
        lua_pop(L, 1);
        return;
    }
    const int original = lua_absindex(L, -1);
    lua_pushcclosurek(L, call_wrapped, name, 0, wrapped_continuation);
    const int wrapper = lua_absindex(L, -1);
    registry_table(L, kNativeWrapped);
    lua_pushvalue(L, wrapper);
    lua_pushvalue(L, original);
    lua_rawset(L, -3);
    lua_pop(L, 1);
    lua_pushvalue(L, wrapper);
    lua_setfield(L, coroutine, name);
    lua_pop(L, 2);
}

void install_coroutine_c_wrappers(lua_State* L) {
    lua_getglobal(L, "coroutine");
    if (!lua_istable(L, -1)) {
        lua_pop(L, 1);
        return;
    }
    const int coroutine = lua_absindex(L, -1);
    const bool was_readonly = lua_getreadonly(L, coroutine) != 0;
    if (was_readonly) lua_setreadonly(L, coroutine, 0);
    for (const char* name : {"create", "resume", "wrap"})
        wrap_coroutine_entry(L, coroutine, name);
    if (was_readonly) lua_setreadonly(L, coroutine, 1);
    lua_pop(L, 1);
}

} // namespace

extern "C" void rove_vm_closures_mark(lua_State* L, int index) {
    index = lua_absindex(L, index);
    Closure* closure = clvalue(luaA_toobject(L, index));
    if (!closure->isC) {
        mark_prototype(closure->l.p);
        return;
    }
    registry_table(L, kOwned);
    lua_pushvalue(L, index);
    lua_pushboolean(L, 1);
    lua_rawset(L, -3);
    lua_pop(L, 1);
}

extern "C" void rove_vm_coroutines_wrap(lua_State* L) {
    install_coroutine_c_wrappers(L);
}

extern "C" void rove_vm_closures_install(lua_State* L) {
    for (const auto& item : {
             std::pair<const char*, lua_CFunction>{"hookfunction", hookfunction},
             {"replaceclosure", hookfunction}, {"restorefunction", restorefunction},
             {"hookmetamethod", hookmetamethod}, {"clonefunction", clonefunction},
             {"clonefunc", clonefunction}, {"newcclosure", newcclosure},
             {"isnewcclosure", isnewcclosure}, {"iscclosure", iscclosure},
             {"islclosure", islclosure}, {"isexecutorclosure", isexecutorclosure},
             {"getnamecallmethod", getnamecallmethod}}) {
        lua_pushcfunction(L, item.second, item.first);
        rove_vm_closures_mark(L, -1);
        lua_setglobal(L, item.first);
    }
    install_coroutine_c_wrappers(L);
}
