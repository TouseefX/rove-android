# Runtime waits and cancellation

`task.cancel()` now closes the coroutine through the official Luau VM before
removing its scheduler entry. The thread becomes dead and cannot be resumed
manually; stack-only objects can be collected even if a script retains its thread
handle. Previously the host only removed the scheduler entry, leaving the stack
alive and resumable. See the official [task reference](https://create.roblox.com/docs/reference/engine/libraries/task)
and [Luau coroutine.close design](https://rfcs.luau.org/function-coroutine-close.html).

The VM rejects attempts to close a running coroutine or an ancestor that is
currently resuming another coroutine. Rejection leaves scheduler state untouched.
Closing unmanaged or already errored coroutines does not create scheduler tasks
or diagnostic records; tracked tasks still publish their cancellation outcome
once.

The host also checks that a task is still parked before delivering a delayed
result. These guards and registration cleanup remain necessary for abandoned
waits, independently resumed tasks, and releasing pending records promptly.

Cancellation is respected by:

- `WaitForChild` and `InstanceHandle:Wait`, including later arrival and timeout.
- Signal waits, including event delivery and destruction of the signal source.
- `BindableFunction:Invoke`, including callback completion and destruction.
- `RemoteFunction:InvokeServer`, including received results and destruction.
- `require`, including the first caller and concurrent callers waiting on the
  same ModuleScript.

Cancelling a BindableFunction caller does not cancel its independently scheduled
callback. Cancelling a require caller does not cancel shared module execution:
remaining callers still receive the result or error, and the module cache still
records it. Cancelled require waiters are pruned when another caller joins and
discarded at completion; synchronous loading failures clear their wait records.

The shared root/Actor frame loop releases cancelled child waits, their timers,
and pending bindable/remote caller records without needing new world traffic.
Registration also prunes abandoned records before checking capacity. Signal
records are pruned when that signal is used or diagnostics are collected.

Cancelled remote calls still waiting in the local outbound queue are removed.
This cannot undo an operation already sent to a server. A matching reply received
before cleanup is consumed without resuming the cancelled caller; other callers
retain their full return tuples, including nils. Batch validation and receiver/
request-ID matching still happen before any replies are delivered.

These are local runtime and scheduler changes. Their regression tests run real
Luau with controlled callbacks and simulated remote results; they do not establish
fresh live-server acceptance or a Windows execution result.

## Late BindableFunction callbacks

`BindableFunction:Invoke()` now parks until `OnInvoke` is assigned, matching the
documented wait behavior in the official [BindableFunction reference](https://create.roblox.com/docs/reference/engine/classes/BindableFunction).
Arguments are retained as one Luau tuple while waiting and moved into the
callback task on dispatch. Nil arguments and trailing nil returns are preserved.

Clearing `OnInvoke` makes future calls wait again. Replacing or clearing it does
not replace a callback already selected for a dispatched call. Queued calls are
dispatched in invocation order, and cancelled callers are discarded first.
Destroying the function releases active waiting callers with a destruction error.
Callbacks remain private to their world and VM, including Actor VMs.

The existing limit of 512 outstanding calls per VM includes calls waiting for
assignment. Cancelled calls release their retained tuples during registration
cleanup or the frame pump. A single cached wrapper runs callbacks under the
scheduler; assignment does not execute authored callback code inside the setter.

Verification passed 154 focused checks: 139 unit checks and 15 host integration
checks. The new integration fixtures exercise received BindableFunctions across
commands and separate root/Actor callback state. Six late-binding regressions
failed before implementation. These results are local macOS execution, not a
fresh live-server or Windows test.

The subsequent VM-closure fix passed 170 focused checks, including root/Actor
command tests and a weak-reference garbage-collection regression. That regression
proves stack-only objects become collectible; it is not a process RAM benchmark.
