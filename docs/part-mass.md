# Part and assembly mass reads

`Mass` and `GetMass()` use the same resolver. A local geometry or density edit
recomputes both, so an old snapshot Mass cannot override the new method result.
Received mass observations remain usable until a newer physical input or local
edit makes them stale, including observations for meshes whose geometry is
unavailable. New material inputs similarly expire old effective density data.
Roblox documents these two APIs as equivalent in the
[BasePart reference](https://create.roblox.com/docs/reference/engine/classes/BasePart#GetMass).

When a received primitive has no Mass observation, the host can derive it from
known size, shape, and density. Custom physical properties take priority;
otherwise an available effective CurrentPhysicalProperties observation supplies
the density. Material alone is insufficient for a received part because custom
materials and material overrides can affect it. Local primitives retain the
existing standard-material defaults. Unknown custom material density produces
an error.

AssemblyMass and AssemblyCenterOfMass can use these part calculations when the
assembly observations are absent. The existing rigid graph, anchoring, massless
and root-selection rules still apply. Local weld edits invalidate old assembly
observations through both local and shared received endpoints, including the
assembly a retargeted joint detached from. Missing assembly data remains an error.
Primitive centers are derived when absent, and received centers accept finite
negative and zero coordinates. Point-velocity queries benefit from the assembly
center fallback.

`CurrentPhysicalProperties` uses that effective-property resolver too. It returns
PhysicalProperties userdata, gives per-part custom values priority, and rejects
writes. CustomPhysicalProperties accepts PhysicalProperties or nil. Legacy
five-component received values decode into the six-component runtime value with
the existing zero acoustic default; malformed observed ranges remain errors.

`PhysicalProperties.new(Enum.Material)` and local material calculations share
43 mechanical defaults from Roblox's [published material table](https://create.roblox.com/docs/parts/materials#default-physical-properties).
The five documented components are captured in a source-attributed test fixture.
This also supplies the previously missing Slate density. The table omits acoustic
absorption; the implementation retains the numeric constructor's zero default,
which is not a verified native material acoustic value. Air/Water defaults are not implemented. Received parts can also resolve explicitly observed material configuration. An
incomplete received world does not establish the absence of material overrides.

Calculations run on demand without a new world cache. Existing primitive volume
models are retained; mesh/union mass is not approximated from a bounding box.
A bounded dependency history expires received Mass/CurrentPhysicalProperties
reports after relevant catalog changes, including rename and removal. These changes do not
add a force solver.

`Changed` (with `Mass`) and `GetPropertyChangedSignal("Mass")` now notify
subscribers when a known resolved mass changes. Local size/density edits,
model scaling, received property batches and Actor replay use the same resolver
as mass reads. Direct received part-property batches preserve intermediate changes, even if
their final mass equals the initial mass; callbacks read the committed world.
Material catalog changes notify parts whose mass is derived from the catalog
after the committed batch; their
intermediate dependency changes can coalesce. Repeated
equal values do not notify. Unknown mass does not produce a fabricated value or
notification. No derived `AssemblyMass` change signal is added, matching the
[documented distinction](https://create.roblox.com/docs/reference/engine/classes/Object#Changed).

Local observers retain only weak world references and one mass value per
subscribed part, with a 4096-entry limit per VM. Disconnected/destroyed observers
are pruned on refresh. Received comparisons are temporary and limited to touched,
observed parts. Only the fixed motion and mass dependency fields acquire new
revision metadata; unrelated properties do not.

Tests use the official Luau VM on macOS, including ordinary and shared storage,
Actor reads, received updates, local edits, known observations and invalid data.
There is no fresh live-server, Windows, native mass comparison or RSS result.

## Material service bindings

`GetMaterialVariant`, `GetBaseMaterialOverride`, and `SetBaseMaterialOverride`
operate on the current world. Lookup matches name and BaseMaterial beneath the
service, including nested folders, and follows rename/reparent/destruction.
Duplicate matches use received order; native duplicate selection is not pinned.
The 40 override fields listed in the [MaterialService reference](https://create.roblox.com/docs/reference/engine/classes/MaterialService)
are accessed through methods; direct script reads and writes remain unavailable.
An explicitly missing received override field remains unknown, while a new local
service begins without overrides.

For authored parts, physical resolution now follows per-part custom properties,
a named variant's custom properties, the base-material override's custom
properties, then the built-in defaults. Nil variant properties inherit. Catalog
changes update reads and derived Mass notifications, including Actor replay and
received material changes. Received parts without a usable effective-property
report can now follow the same catalog when their Material, MaterialVariant,
and nil CustomPhysicalProperties are explicitly known. A matching named variant
with custom data does not require a base override observation. Falling through
to the base material requires a known override field, even if GetService created
a local service. Missing named variants remain unknown until initial replication
is marked complete. Known empty overrides or known nil variant properties permit
built-in defaults. Reads resolve current data on demand without another cache.

Received Mass/CurrentPhysicalProperties reports retain priority until a relevant
catalog change supersedes them. Dependency stamps distinguish base overrides
from named variants. An explicit named custom value does not depend on the base
override; per-part custom values bypass catalog dependencies. Unrelated materials
keep their reports, including meshes whose geometry cannot be derived. When
material inputs are unknown, report freshness cannot be established after a
catalog change.

Received edits carry the sequence of their actual material or hierarchy
inputs, so a newer report can become usable again. Local edits remain dirty
until corresponding received configuration replaces them. Rename, reparent,
creation and removal retain invalidation history even after the old entry is
gone. Failed received transactions do not commit history. Actor forks share the
history until a change requires a copy; synchronization preserves active local
differences. Duplicate variants retain the existing received-order selection.

History retains at most 4096 keys and 256 KiB of variant-name bytes per distinct
world history, not copies of part data. Temporary catalog comparisons use
fingerprints, preserve packed unrelated world data, and run only for potentially
relevant edits. Overflow conservatively expires older reports; local overflow
keeps catalog-dependent reports unavailable for the rest of that world lifetime.
These are resource limits, not Roblox limits. Catalog queries can still derive
known supported physical properties within their own bounds. No new peak-RAM
measurement is claimed.

For a batch containing both part and material inputs, only parts with a planned
Mass notification have their observer baseline silently synchronized. An equal
Size receipt therefore cannot hide a material-driven Mass notification. Catalog
notifications use the committed result; intermediate catalog edits can coalesce.
The runtime does not yet retain constructor provenance to distinguish all native
non-explicit PhysicalProperties values from numeric authored values.

Lookup retains no second material catalog payload; the separate bounded history
retains only dependency keys and change stamps. It scans headers and reads candidate
variants, with limits of 4096 variants and 1024 ancestors per query. These checks
are resource limits, not native engine limits. No new live-server, Windows or
native material comparison is claimed.


## Assembly observation freshness

AssemblyMass and AssemblyCenterOfMass now compare their observation revision
with the physical inputs of each current rigid member and that member's
material dependency history. Received size, density, mass, massless, anchoring,
root-priority and root-name changes expire older observations. Center reads also
track local-center and pose changes; a pose change alone does not expire the
assembly's mass observation. Newer observations regain priority. Queries still
use the current rigid graph and the existing root/anchored/massless rules.

CenterOfMass observations expire after geometry changes (Size or Shape), but
remain valid across uniform density or material edits. This preserves a known
mesh center without inventing mesh geometry. Unknown changed geometry remains
an error. Point-velocity queries use the refreshed assembly center.

Mass, CenterOfMass, AssemblyMass and AssemblyCenterOfMass reject script writes,
matching their read-only API definitions in the [BasePart reference](https://create.roblox.com/docs/reference/engine/classes/BasePart).
Assembly observations reject invalid vectors and negative/nonfinite masses;
positive infinite assembly mass remains supported for anchored assemblies.

The shared store stamps only the additional assembly/center and root-selection
fields needed for these comparisons. No new per-part cache is retained. Offline
coverage includes ordinary/shared storage, Actor reads, mixed old/new reports,
unrelated assemblies, mesh centers, point velocity and metadata rollback.
Received rigid-joint and hierarchy changes now use the topology history described
below to expire observations for former as well as current members. No fresh native, live-server, Windows or peak-RAM result is claimed.


## Received assembly topology

Before and after a relevant received transaction, the host uses the same rigid
graph builder as script queries. It compares connected membership, retains
revision stamps for affected part IDs, and covers both former and new components.
Retargeting, enabling/disabling, creation/removal, ancestry changes and anchored
splits can therefore expire old AssemblyMass/AssemblyCenterOfMass observations.
A redundant edge removal that leaves membership unchanged preserves reports.
Independent components use their own relevant receipt sequences; a later unrelated
change in the same batch does not expire a newer report for another component.

Server confirmation of a local detach keeps the old observation stale even when
the effective graph already looked detached before the receipt. Confirmed joint
removals are distinguished from local Destroy operations so fresh reports can be
used again. Both topology stamps and removal confirmations roll back on a failed
received transaction. Actor worlds share the committed history; current local
joint edits retain the existing authored-state checks.

The graph can read a borrowed world directly, avoiding a cloned world just for
comparison. History uses shared IDs and copy-on-write storage, retaining at most
8192 combined part stamps/removal confirmations and 512 KiB of ID bytes. Transient
capture is limited to 4096 joint candidates, 8192 referenced parts and a bounded
identity-byte preflight. Overflow conservatively expires older reports globally;
newer reports remain usable. These are implementation limits, not native limits.
Regular pose traffic does not build topology snapshots.

Offline tests cover ordinary/shared storage, Actor reads, retargeted chains,
ancestor moves, deletion, local confirmation, redundant cycles, independent batch
ordering, rollback, packed unrelated parts and bounded history. Native/live,
Windows and peak-RAM verification remain outstanding.
