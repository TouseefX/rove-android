# Rove Stellar · Roblox Player integration for Windows x64

Use this package to add a reviewed Luau probe to a test experience your team can publish. The Windows executable packages the probe and reads its results. The installed Roblox Player executes the probe when a tester joins your published copy. The CLI runs locally and requires no Node.js installation.

## First run

Extract the entire ZIP and open PowerShell in its folder. Run `.\rove-stellar.exe` to display the integration commands. If you drag the executable into PowerShell and it inserts a quoted path, type the call operator `&` followed by a space before the path, then press Enter.

1. In your editable test place, set `Workspace.SandboxedInstanceMode` to `Experimental`. Roblox describes script capabilities as a client beta. [Roblox script capabilities](https://create.roblox.com/docs/scripting/capabilities)
2. Add the included `StellarProbe.rbxmx` under `StarterPlayer > StarterPlayerScripts`. Publish that copy through your normal release process. For a sample game, Roblox provides an editable version of The Mystery of Duvall Drive. Use the place ID assigned to your copy. [Official sample](https://create.roblox.com/docs/resources/the-mystery-of-duvall-drive)
3. Join the published copy in Roblox Player. The model checks the runtime and sandbox before calling the packaged probe. Open the client Developer Console with F9 to inspect the report. [Developer Console](https://create.roblox.com/docs/studio/developer-console)

The included model already contains `example.luau`. To package your team's own probe, use a new output filename and put that model in the place for your next test release.

```powershell
$digest = .\rove-stellar.exe pack .\my-probe.luau .\MyProbe.rbxmx
```

`pack` accepts up to 64 KiB of UTF-8 Luau and prints the SHA-256 of the input bytes. The place’s creator controls which probe ships. Use reviewed code for this workflow. A script that loops indefinitely or allocates excessive memory can still stall the tester’s Player.

## Read results in your logger

The client prints `ROVE_STELLAR_PLAYER_V1 ` followed by JSON. For the desktop logger, give the executable the tester’s local Player log. It writes one JSON object per recognized report to stdout. Pass `-` in place of the filename to read a copied report line from stdin.

```powershell
$log = Get-ChildItem "$env:LOCALAPPDATA\Roblox\logs\*_Player_*.log" |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (!$log) { throw "No Roblox Player log found" }
.\rove-stellar.exe read-report $log.FullName
```

The path above is for the desktop Win32 Player. For the Microsoft Store version, Roblox documents a separate logs folder. Keep the full log on the tester’s machine and pass only parsed reports to the logger. [Roblox log locations](https://en.help.roblox.com/hc/en-us/articles/360016022492-How-to-Retrieve-Roblox-Studio-Logs)

An in-game logger can read the StringValue at `PlayerScripts/RoveStellarClient/Sandbox/RoveStellarClient/ReportJson` on the local player. Subscribe to `GetPropertyChangedSignal("Value")` and check its current value for a report that has already arrived. Decode nonempty values with `HttpService:JSONDecode`. This value holds the complete report even if the local log truncates a long line.

| Field | Meaning |
| --- | --- |
| `protocol`, `runtime`, `status` | Wire version, engine context, and execution outcome. Consume results from successful Player reports. |
| `placeId`, `sourceSha256` | The published place ID as a decimal string and the packaged source digest. Match both to your test. |
| `prints` | Ordered captured messages with `level` and `text`. |
| `returns` | Ordered typed Luau values. A `nil` entry retains its position. |

The report closes when the probe returns or raises an error. It includes calls to the probe's supplied `print` and `warn` functions up to that point. Read `status` before consuming results. A successful run reports `runtime: "roblox-player"` and `sandbox: true`.

`logger-example.mjs` exports `buildProbe(sourcePath, modelPath)` and `readProbeReports(logPath, digest, placeId)` for teams using Node.js. A logger in another language can invoke the same two CLI commands. Treat reports as client-supplied diagnostic data. The digest identifies the packaged probe.
