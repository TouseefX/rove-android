// Safe ABI callbacks for introspection of this pinned, in-process Luau VM.
// The VM heap traversal uses Luau's own GC visitor; it does not read another
// process and never returns objects already marked dead by the collector.
#include "lua.h"
#include "lualib.h"
#include "lapi.h"
#include "lstate.h"
#include "lobject.h"
#include "lgc.h"
#include "lmem.h"
#include "lfunc.h"
#include "ltable.h"
#include <Luau/Bytecode.h>
#include <Luau/BytecodeUtils.h>
#include <algorithm>
#include <cmath>
#include <cstdint>
#include <limits>
#include <string>
#include <vector>

namespace {
int checked_index(lua_State* L, int index, int minimum) {
    const double value = luaL_checknumber(L, index);
    if (!std::isfinite(value) || std::trunc(value) != value || value < minimum || value > INT32_MAX) {
        luaL_error(L, "debug index or level out of range");
        return minimum;
    }
    return static_cast<int>(value);
}

Closure* checked_lclosure(lua_State* L, int index) {
    if (lua_type(L, index) == LUA_TNUMBER) {
        const int level = checked_index(L, index, 0);
        if (level >= L->ci - L->base_ci) {
            luaL_error(L, "stack level out of range");
            return nullptr;
        }
        CallInfo* frame = L->ci - level;
        if (!isLfunction(frame->func)) {
            luaL_error(L, "Luau closure required");
            return nullptr;
        }
        return clvalue(frame->func);
    }
    luaL_checktype(L, index, LUA_TFUNCTION);
    if (!lua_isLfunction(L, index)) {
        luaL_error(L, "Luau closure required");
        return nullptr;
    }
    return clvalue(luaA_toobject(L, index));
}

struct GcSnapshot {
    lua_State* state;
    Proto* prototype = nullptr;
    bool include_tables = false;
    GCObject* excluded = nullptr;
    int count = 0;
    bool write_results = false;
};

void push_gc_object(lua_State* L, GCObject* object);

bool visit_gc(void* context, lua_Page*, GCObject* object) {
    GcSnapshot& snapshot = *static_cast<GcSnapshot*>(context);
    if (isdead(snapshot.state->global, object))
        return false;

    const bool include = snapshot.prototype
        ? object->gch.tt == LUA_TFUNCTION && !object->cl.isC && !object->cl.inactive &&
              !object->cl.preload && object->cl.l.p == snapshot.prototype
        : object->gch.tt == LUA_TFUNCTION || object->gch.tt == LUA_TUSERDATA ||
              (snapshot.include_tables && object->gch.tt == LUA_TTABLE);
    if (!include || object == snapshot.excluded)
        return false;

    if (snapshot.write_results) {
        // The exact result array capacity is allocated before this pass, so
        // inserting an integer slot cannot resize the table or trigger GC.
        push_gc_object(snapshot.state, object);
        lua_rawseti(snapshot.state, -2, ++snapshot.count);
    } else {
        ++snapshot.count;
    }
    return false;
}

// Drop unreachable objects before counting the snapshot. A second pass fills
// the result table directly, avoiding a native pointer vector and avoiding
// allocating a large Luau table while the collector is stopped.
void collect_before_snapshot(lua_State* L) {
    if (lua_gc(L, LUA_GCISRUNNING, 0) != 0)
        lua_gc(L, LUA_GCCOLLECT, 0);
}

// Prototype inspection must not collect the very closures it is inspecting.
// Pause only for the two-pass snapshot and its exactly sized result array.
// The pinned VM uses C++ exceptions, so the threshold is restored on OOM too.
struct PrototypeSnapshotGuard {
    global_State* global;
    size_t threshold;
    explicit PrototypeSnapshotGuard(lua_State* L)
        : global(L->global), threshold(global->GCthreshold) {
        global->GCthreshold = SIZE_MAX;
    }
    ~PrototypeSnapshotGuard() { global->GCthreshold = threshold; }
};

void push_gc_object(lua_State* L, GCObject* object) {
    TValue value;
    value.value.gc = object;
    value.tt = object->gch.tt;
    luaA_pushvalue(L, &value);
}

void append_u32(std::string& output, uint32_t value) {
    for (int i = 0; i < 4; ++i)
        output.push_back(char(value >> (i * 8)));
}

void canonical_proto(lua_State* L, std::string& output, const Proto* proto, int depth = 0) {
    if (depth > 128 || output.size() > 64 * 1024 * 1024)
        throw std::runtime_error("function prototype traversal limit exceeded");
    output.push_back(char(proto->numparams));
    output.push_back(char(proto->is_vararg));
    output.push_back(char(proto->nups));
    append_u32(output, static_cast<uint32_t>(proto->sizek));
    std::vector<bool> imports(static_cast<size_t>(proto->sizek), false);
    for (int pc = 0; pc < proto->sizecode;) {
        const auto op = LuauOpcode(LUAU_INSN_OP(proto->code[pc]));
        if (op == LOP_GETIMPORT) {
            const int index = LUAU_INSN_D(proto->code[pc]);
            if (index >= 0 && index < proto->sizek)
                imports[static_cast<size_t>(index)] = true;
        }
        pc += Luau::getOpLength(op);
    }
    for (int i = 0; i < proto->sizek; ++i) {
        const TValue& value = proto->k[i];
        if (imports[static_cast<size_t>(i)]) {
            output.push_back('I');
            continue;
        }
        output.push_back(char(ttype(&value)));
        if (ttisstring(&value)) {
            const TString* string = tsvalue(&value);
            append_u32(output, string->len);
            output.append(getstr(string), string->len);
        } else if (ttisnumber(&value)) {
            uint64_t bits;
            const double number = nvalue(&value);
            std::memcpy(&bits, &number, sizeof(bits));
            for (int j = 0; j < 8; ++j)
                output.push_back(char(bits >> (j * 8)));
        } else if (ttisboolean(&value)) {
            output.push_back(char(bvalue(&value)));
        } else if (ttisvector(&value)) {
            for (int j = 0; j < LUA_VECTOR_SIZE; ++j) {
                uint32_t bits;
                const float number = vvalue(&value)[j];
                std::memcpy(&bits, &number, sizeof(bits));
                append_u32(output, bits);
            }
        } else if (ttistable(&value)) {
            const LuaTable* table = hvalue(&value);
            std::vector<std::string> keys;
            for (int j = 0; j < sizenode(table); ++j) {
                TValue key_value;
                getnodekey(L, &key_value, &table->node[j]);
                if (ttisstring(&key_value)) {
                    const TString* key = tsvalue(&key_value);
                    keys.emplace_back(getstr(key), key->len);
                }
            }
            std::sort(keys.begin(), keys.end());
            append_u32(output, static_cast<uint32_t>(keys.size()));
            for (const auto& key : keys) {
                append_u32(output, static_cast<uint32_t>(key.size()));
                output.append(key);
            }
        } else if (isLfunction(&value)) {
            canonical_proto(L, output, clvalue(&value)->l.p, depth + 1);
        }
    }
    append_u32(output, static_cast<uint32_t>(proto->sizecode));
    for (int pc = 0; pc < proto->sizecode;) {
        Instruction instruction = proto->code[pc];
        auto op = LuauOpcode(LUAU_INSN_OP(instruction));
        if (op == LOP_BREAK && proto->debuginsn) {
            op = LuauOpcode(proto->debuginsn[pc]);
            instruction = (instruction & ~Instruction(255)) | Instruction(op);
        }
        if (op == LOP_GETGLOBAL || op == LOP_SETGLOBAL || op == LOP_GETTABLEKS ||
            op == LOP_SETTABLEKS || op == LOP_NAMECALL)
            instruction &= 0x00ffffffu;
        append_u32(output, instruction);
        const int length = Luau::getOpLength(op);
        for (int j = 1; j < length; ++j)
            append_u32(output, proto->code[pc + j]);
        pc += length;
    }
    append_u32(output, static_cast<uint32_t>(proto->sizep));
    for (int i = 0; i < proto->sizep; ++i)
        canonical_proto(L, output, proto->p[i], depth + 1);
}
} // namespace

extern "C" {

int rove_vm_getgc(lua_State* L) {
    const bool include_tables = lua_toboolean(L, 1) != 0;
    collect_before_snapshot(L);
    GcSnapshot count{L, nullptr, include_tables};
    luaM_visitgco(L, &count, visit_gc);
    lua_createtable(L, count.count, 0);
    GcSnapshot fill{L, nullptr, include_tables, gcvalue(luaA_toobject(L, -1)), 0, true};
    luaM_visitgco(L, &fill, visit_gc);
    return 1;
}

int rove_vm_getconstants(lua_State* L) {
    Proto* proto = checked_lclosure(L, 1)->l.p;
    lua_createtable(L, proto->sizek, 0);
    for (int index = 0; index < proto->sizek; ++index) {
        // A GETIMPORT cache is an implementation detail, not a literal constant.
        if (ttisfunction(&proto->k[index]))
            lua_pushnil(L);
        else
            luaA_pushvalue(L, &proto->k[index]);
        lua_rawseti(L, -2, index + 1);
    }
    return 1;
}

int rove_vm_getconstant(lua_State* L) {
    Proto* proto = checked_lclosure(L, 1)->l.p;
    const int index = checked_index(L, 2, 1);
    if (index < 1 || index > proto->sizek) {
        luaL_error(L, "constant index out of range");
        return 0;
    }
    if (ttisfunction(&proto->k[index - 1]))
        lua_pushnil(L);
    else
        luaA_pushvalue(L, &proto->k[index - 1]);
    return 1;
}

int rove_vm_setconstant(lua_State* L) {
    Proto* proto = checked_lclosure(L, 1)->l.p;
    const int index = checked_index(L, 2, 1);
    luaL_checkany(L, 3);
    if (index < 1 || index > proto->sizek) {
        luaL_error(L, "constant index out of range");
        return 0;
    }

    const TValue* replacement = luaA_toobject(L, 3);
    TValue* slot = &proto->k[index - 1];
    if (ttype(replacement) != ttype(slot) ||
        (!ttisnil(replacement) && !ttisboolean(replacement) && !ttisnumber(replacement) &&
         !ttisstring(replacement))) {
        luaL_error(L, "constant replacement must preserve its primitive type");
        return 0;
    }

    // Imported-function cache entries are function TValue slots, so the
    // primitive type check above rejects them. Keep the write barrier because
    // replacing a primitive can still change the Proto's collector edges.
    setobj(L, slot, replacement);
    luaC_barrier(L, proto, replacement);
    lua_setsafeenv(L, LUA_GLOBALSINDEX, 0);
    return 0;
}

int rove_vm_functiondata(lua_State* L) {
    Closure* closure = checked_lclosure(L, 1);
    try {
        std::string bytes;
        canonical_proto(L, bytes, closure->l.p);
        lua_pushlstring(L, bytes.data(), bytes.size());
        return 1;
    } catch (const std::exception& error) {
        luaL_error(L, "getfunctionhash: %s", error.what());
        return 0;
    }
}

void rove_vm_pushproto(lua_State* L, Closure* parent, Proto* proto, int active) {
    if (active) {
        PrototypeSnapshotGuard guard(L);
        GcSnapshot count{L, proto, false};
        luaM_visitgco(L, &count, visit_gc);
        lua_createtable(L, count.count, 0);
        GcSnapshot fill{L, proto, false, gcvalue(luaA_toobject(L, -1)), 0, true};
        luaM_visitgco(L, &fill, visit_gc);
    } else {
        Closure* closure = luaF_newLclosure(L, proto->nups, parent->env, proto);
        closure->inactive = 1;
        TValue value;
        setclvalue(L, &value, closure);
        luaA_pushvalue(L, &value);
    }
}

int rove_vm_getproto(lua_State* L) {
    Closure* closure = checked_lclosure(L, 1);
    const int index = checked_index(L, 2, 1);
    if (index > closure->l.p->sizep) {
        luaL_error(L, "prototype index out of range");
        return 0;
    }
    rove_vm_pushproto(L, closure, closure->l.p->p[index - 1], lua_toboolean(L, 3));
    return 1;
}

int rove_vm_getprotos(lua_State* L) {
    Closure* closure = checked_lclosure(L, 1);
    lua_createtable(L, closure->l.p->sizep, 0);
    for (int index = 0; index < closure->l.p->sizep; ++index) {
        rove_vm_pushproto(L, closure, closure->l.p->p[index], 0);
        lua_rawseti(L, -2, index + 1);
    }
    return 1;
}

// Debug stack access is defined over the VM register slots in a running Luau
// frame, including temporary registers without source-level local names.
CallInfo* checked_stack_frame(lua_State* L, int level_argument) {
    const int level = checked_index(L, level_argument, 1);
    if (level >= L->ci - L->base_ci) {
        luaL_error(L, "stack level out of range");
        return nullptr;
    }

    CallInfo* frame = L->ci - level;
    if (!isLua(frame) || (frame->flags & LUA_CALLINFO_NATIVE)) {
        luaL_error(L, "interpreted Luau frame required");
        return nullptr;
    }
    return frame;
}

void reserve_debug_stack_output(lua_State* L) {
    StkId protected_top = L->top;
    // The current C frame's top is reserved capacity, not caller registers.
    // lua_checkstack expands it, so it must not define this boundary.
    for (CallInfo* frame = L->base_ci; frame < L->ci; ++frame)
        if (frame->top > protected_top)
            protected_top = frame->top;
    const ptrdiff_t protected_offset = protected_top - L->stack;
    const ptrdiff_t gap = protected_top - L->top;
    if (gap > INT32_MAX - 2 || !lua_checkstack(L, static_cast<int>(gap) + 2)) {
        luaL_error(L, "stack space unavailable");
        return;
    }

    // Stack growth rebases pointers without collecting. Extend the live range
    // without lua_settop, which would replace caller registers with nil. The
    // pinned collector clears unmarked slots before reclaiming their objects.
    // The result and its temporary value can now be pushed above every caller.
    luaC_threadbarrier(L);
    L->top = L->stack + protected_offset;
}

int rove_vm_getstack(lua_State* L) {
    CallInfo* frame = checked_stack_frame(L, 1);
    const int level = checked_index(L, 1, 1);
    const ptrdiff_t register_count = frame->top - frame->base;
    if (register_count < 0 || register_count > INT32_MAX) {
        luaL_error(L, "stack frame is unavailable");
        return 0;
    }

    if (!lua_isnoneornil(L, 2)) {
        const int index = checked_index(L, 2, 1);
        if (index > register_count) {
            luaL_error(L, "stack index out of range");
            return 0;
        }
        reserve_debug_stack_output(L);
        frame = L->ci - level;
        luaC_threadbarrier(L);
        luaA_pushvalue(L, frame->base + (index - 1));
        return 1;
    }

    reserve_debug_stack_output(L);
    frame = L->ci - level;
    lua_createtable(L, static_cast<int>(register_count), 0);
    // Table creation can run GC and move the CallInfo array, so resolve the
    // source frame again before iterating its registers.
    frame = L->ci - level;
    const int result = lua_absindex(L, -1);
    // Access frame->base afresh: table allocation or GC may relocate the VM
    // value stack and update the CallInfo pointers.
    for (int index = 1; index <= register_count; ++index) {
        luaC_threadbarrier(L);
        luaA_pushvalue(L, frame->base + (index - 1));
        lua_rawseti(L, result, index);
    }
    return 1;
}

int rove_vm_setstack(lua_State* L) {
    CallInfo* frame = checked_stack_frame(L, 1);
    const int index = checked_index(L, 2, 1);
    const ptrdiff_t register_count = frame->top - frame->base;
    if (register_count < 0 || index > register_count) {
        luaL_error(L, "stack index out of range");
        return 0;
    }

    // Write the actual active register so ordinary VM reads observe the new
    // value; lua_setlocal only addresses slots with debug local metadata.
    luaC_threadbarrier(L);
    setobj2s(L, frame->base + (index - 1), luaA_toobject(L, 3));
    return 0;
}

} // extern "C"
