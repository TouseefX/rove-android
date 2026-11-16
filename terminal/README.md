# rove terminal

A Rust dashboard for the Rust client. Game search, a scrolling RAM chart,
real ping/session counters, and a Luau console in one terminal.

The workspace has oversized live readouts on larger terminals, a dedicated
full-page game browser, and an image gallery with bordered photo previews.
Press **F6** or click **expand preview** for the larger gallery, including on
compact terminals. Left/right switches photos there; F6 or Esc returns to the
results without losing your search. Search typing and paste are paused while
that gallery is expanded. Cover photos fill their frames; logos stay uncropped.
The compact layout keeps exact values, units, and keyboard controls readable.

```sh
./terminal/run.sh --credential-file /path/to/roblox-cookie.txt
```

Windows: `./terminal/run.ps1 --credential-file C:\path\roblox-cookie.txt`.
The existing Rust client build prerequisites also apply. The first launch builds
the frontend and official Luau VM; later launches run the last successful build.
Use `./terminal/run.sh --rebuild` (or `run.ps1 --rebuild`) to rebuild the dashboard.
Use `--rebuild-worker` to include engine changes too. Display-only changes reuse
the installed session worker, so they do not rebuild the VM. A worker build
failure leaves the previous worker available. `ROVE_WORKER_BIN` can select an
explicit worker build.

F2 finds games by name, place ID, or a public Roblox game URL. Names search after
a 600 ms typing pause; Enter searches immediately or joins a selected result.
Tab switches to a local fuzzy filter of the returned results, with matched
letters highlighted. Arrow keys select; the adjacent preview shows game details.
Esc leaves the filter before closing search. Place IDs only join when submitted.
F1 lists all controls. Type Luau in the
console and press Enter; pasted multiline code is held until you submit it.
`:run /path/file.luau` loads a script, `:status ID` checks it, and `:save /path/ram.csv`
exports memory samples. Ctrl+D leaves; Ctrl+Q leaves and quits. A new join leaves
the previous session first. Physics is on by default; F3/F4 change physics and
lightweight mode before joining.

Ctrl+K opens the searchable action menu. Alt+1/2/3 switch between overview,
console, and network views; the tabs and workspace sidebar also accept clicks.
Ctrl+T cycles aurora (default), orchid, midnight, ember, and glacier themes.
Aurora pairs violet memory graphs with cyan network charts, mint ready states,
and amber peak readouts. Unavailable measurements stay muted.
F8 switches graph history between 30 seconds,
2 minutes, and 10 minutes. Ctrl+E filters activity to all events, scripts, or
problems. The Luau editor has line numbers and syntax colors; highlighting
never changes the source sent to the VM. Search results show the creator,
player count, approval, and description when Roblox supplies them.

Search previews include game artwork and a clickable screenshot strip.
Ctrl+Left/Right switches photos without editing the search query.
The Rove icon you supplied also appears in the header and empty preview.
Compatible Kitty, iTerm2, and Sixel terminals get pixel images and smooth,
gradient-filled charts. Other terminals get colored adaptive-block artwork and
the filled Braille charts. Images are detected automatically at startup.
`--images blocks` forces the portable fallback; `--images off` disables
artwork downloads. Advanced overrides: `--images kitty`, `iterm2`, or `sixel`.
Multiplexers use blocks without changing any terminal configuration.
Apple Terminal uses the block renderer: 32 samples per character are fitted to
quadrant and fractional blocks, preserving edges down to one-eighth of a cell
in height or one-quarter in width, with two fitted colors per cell. F6 expands the
artwork; a smaller terminal font provides more image detail. Actual pixel images
require a compatible terminal.

Artwork loading and image encoding run in bounded background workers. The
cache retains four games with up to three 768x432 covers and a 128x128 icon per
game. Downloads and decoded image sizes are capped; scripts and account
cookies are never sent to the artwork service. Pixel charts update at most
twice per second, use the same actual samples as the text charts, and leave
missing intervals blank. Graph image IDs are reused to avoid accumulating
images in the terminal. [Artwork attribution](THIRD_PARTY.md).

Try `./terminal/run.sh --offline` without credentials. It uses the official VM
and a small local fixture world, clearly marked as a sandbox. No game is joined.

RAM is measured from the OS, in decimal MB. The graph legend identifies the
client worker and client + dashboard. Peak is sampled, not an exact kernel peak. The graph shows
the selected time window, with filled areas and current/average/maximum readouts.
Downsampling preserves spikes; observation gaps longer than two seconds stay
blank. CSV retains up to an hour at four samples per second.
Network values stay unavailable until the transport reports them. Search is
public and never receives your cookie. The client joins only after selection.

The whole-client RAM budget starts at 128 MB and grows by 10 MB when usage is
within 10 MB of it: 118 MB used raises the budget to 138 MB. In the dashboard,
this covers client + dashboard RSS; standalone live sessions use their own
process RSS. Larger jumps add enough steps in one sample. The current budget
appears below the graph and in the workspace sidebar. This is a soft budget, not an OS
allocation cap or a reservation: it does not force a heavy game below 128 MB.
It is separate from the Luau allocator cap below.

Sessions default to one hour (`--seconds 1..3600`). The Luau memory setting
(`--lua-memory-mib 8..512`) is separate from total process memory. Console
submissions are capped at 1 MiB, output/history are bounded, and the VM stays on
its owner thread. This adds a frontend, not missing engine APIs or a renderer.
Private share links are not yet accepted by this picker; use a public game URL
or place ID. Server/account errors are shown without claiming a connection.

Visual references: [Posting](https://posting.sh/) for the workspace and pane
hierarchy, [btop](https://github.com/aristocratos/btop) for filled monitoring
graphs, and [Television](https://github.com/alexpasmantier/television) for the
search/results/preview layout and local matching.
The layout and widgets here are our own implementation.
