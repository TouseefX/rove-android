$ErrorActionPreference = 'Stop'
$target = if ($env:ROVE_TERMINAL_TARGET) { $env:ROVE_TERMINAL_TARGET } else { Join-Path $env:LOCALAPPDATA 'rove-terminal\target' }
$binary = Join-Path $target 'release\rove-terminal.exe'
$worker = Join-Path $target 'release\rove-session-worker.exe'
$roveArgs = @($args)
$rebuildWorker = $roveArgs.Count -gt 0 -and $roveArgs[0] -eq '--rebuild-worker'
$rebuild = $rebuildWorker -or ($roveArgs.Count -gt 0 -and $roveArgs[0] -eq '--rebuild')
if ($rebuild) {
    $roveArgs = if ($roveArgs.Count -gt 1) { @($roveArgs[1..($roveArgs.Count - 1)]) } else { @() }
}
if ((Test-Path $binary) -and (Test-Path $worker) -and -not $rebuild) {
    & $binary @roveArgs
    exit $LASTEXITCODE
}
if ($rebuildWorker -or -not (Test-Path $worker)) {
    & cargo build --locked --release --features embedded-worker --manifest-path (Join-Path $PSScriptRoot 'Cargo.toml') --target-dir $target
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Copy-Item $binary ($worker + '.new') -Force
    Move-Item ($worker + '.new') $worker -Force
}
& cargo build --locked --release --manifest-path (Join-Path $PSScriptRoot 'Cargo.toml') --target-dir $target
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& $binary @roveArgs
exit $LASTEXITCODE
