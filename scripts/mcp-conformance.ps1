# RDM agent-interface conformance entry point (independent QA).
#
# Orchestrates:
#   1) build of the agent-facing binaries into the isolated QA target dir
#   2) MCP stdio conformance + real e2e + CLI agent contract (scripts/mcp_e2e.mjs)
#   3) workspace regression: build / test / fmt / clippy
#
# The MCP/CLI harness itself is `scripts/mcp_e2e.mjs` (Node, standard library
# only). This script only orchestrates, records logs, and aggregates PASS/FAIL.
#
# Usage:
#   pwsh -NoProfile -File scripts/mcp-conformance.ps1                 # full run
#   pwsh -NoProfile -File scripts/mcp-conformance.ps1 -SkipBuild     # binaries already built
#   pwsh -NoProfile -File scripts/mcp-conformance.ps1 -OnlyMcp       # MCP+CLI only, no regression
#   pwsh -NoProfile -File scripts/mcp-conformance.ps1 -Only "download|policy" [-KeepTemp]
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$OnlyMcp,
    [switch]$SkipE2E,
    [switch]$SkipRegression,
    [switch]$KeepTemp,
    [string]$Only = "",
    [string]$RdmMcp = "",
    [string]$RdmCli = "",
    [string]$Fixture = "",
    [int]$SizeMb = 32,
    [string]$Out = ""
)

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root

# --- environment -----------------------------------------------------------
$mingw = "C:\Users\nirvana\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin"
if ((Test-Path $mingw) -and ($env:PATH -notlike "*$mingw*")) { $env:PATH = "$mingw;$env:PATH" }
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if ((Test-Path $cargoBin) -and ($env:PATH -notlike "*$cargoBin*")) { $env:PATH = "$cargoBin;$env:PATH" }
$env:CARGO_TARGET_DIR = Join-Path $root "target-qa-agent"
$debugDir = Join-Path $env:CARGO_TARGET_DIR "debug"
$logDir = Join-Path $env:CARGO_TARGET_DIR "qa-agent-logs"
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$summary = [ordered]@{
    generated_at = (Get-Date).ToString("o")
    git_commit   = (& git rev-parse HEAD 2>$null | Out-String).Trim()
    git_dirty    = ((& git status --short 2>$null | Measure-Object).Count -gt 0)
    rustc        = (& rustc -V 2>$null | Out-String).Trim()
    node         = (& node -v 2>$null | Out-String).Trim()
    target_dir   = $env:CARGO_TARGET_DIR
    steps        = @()
    mcp          = $null
    regression   = $null
    pass         = $null
    fail         = $null
}

function Add-Step([string]$Name, [bool]$Pass, [string]$Detail = "") {
    $summary.steps += [pscustomobject]@{ name = $Name; pass = $Pass; detail = $Detail }
    $color = if ($Pass) { "Green" } else { "Red" }
    Write-Host ("[{0}] {1}{2}" -f $(if ($Pass) { "PASS" } else { "FAIL" }), $Name, $(if ($Detail) { "  ($Detail)" } else { "" })) -ForegroundColor $color
}

function Run-Logged([string]$Name, [string]$Exe, [string[]]$CommandArgs, [int]$TimeoutSec = 1800) {
    $log = Join-Path $logDir "$stamp-$Name.log"
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    # Call operator + `*>` is reliable for console apps on Windows; Start-Process
    # argument marshalling silently dropped array args in earlier revisions.
    & $Exe @CommandArgs *> $log
    $code = $LASTEXITCODE
    $sw.Stop()
    $tail = @()
    if (Test-Path $log) { $tail += (Get-Content $log -Tail 4) }
    return [pscustomobject]@{
        name = $Name; exit = $code; seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1)
        log = $log; err_log = ""; tail = ($tail -join " | ")
    }
}

function Resolve-Bin([string]$Explicit, [string]$FileName) {
    if ($Explicit) { return (Resolve-Path -LiteralPath $Explicit -ErrorAction SilentlyContinue)?.Path }
    return (Join-Path $debugDir $FileName)
}

Write-Host "=== RDM agent-interface conformance ===" -ForegroundColor Cyan
Write-Host "repo     : $root"
Write-Host "commit   : $($summary.git_commit) (dirty=$($summary.git_dirty))"
Write-Host "rustc    : $($summary.rustc)"
Write-Host "node     : $($summary.node)"
Write-Host "target   : $($env:CARGO_TARGET_DIR)"
Write-Host "logs     : $logDir"
Write-Host ""

# --- 1) build agent binaries -------------------------------------------------
$agentBins = @(
    @{ pkg = "rdm-mcp"; exe = "rdm-mcp.exe" },
    @{ pkg = "rdm-cli"; exe = "rdm.exe" },
    @{ pkg = "rdm-fixture-server"; exe = "rdm-fixture-server.exe" }
)
if (-not $SkipBuild) {
    Write-Host "==> cargo build -p rdm-mcp -p rdm-cli -p rdm-fixture-server" -ForegroundColor Cyan
    $build = Run-Logged -Name "build-agent" -Exe "cargo" -CommandArgs @("build", "-p", "rdm-mcp", "-p", "rdm-cli", "-p", "rdm-fixture-server")
    Add-Step "build.agent_binaries" ($build.exit -eq 0) "exit=$($build.exit) $($build.seconds)s log=$($build.log)"
    if ($build.exit -ne 0) { Write-Host ($build.tail) -ForegroundColor Red }
} else {
    Add-Step "build.agent_binaries" $true "skipped (-SkipBuild)"
}

$mcpExe = Resolve-Bin $RdmMcp "rdm-mcp.exe"
$cliExe = Resolve-Bin $RdmCli "rdm.exe"
$fixtureExe = Resolve-Bin $Fixture "rdm-fixture-server.exe"
$missingBins = @()
foreach ($pair in @(@("rdm-mcp", $mcpExe), @("rdm", $cliExe), @("fixture", $fixtureExe))) {
    if (-not (Test-Path -LiteralPath $pair[1])) { $missingBins += "$($pair[0])=$($pair[1])" }
}
if ($SkipE2E) {
    Add-Step "binaries.present" $true "skipped (-SkipE2E)"
} else {
    Add-Step "binaries.present" ($missingBins.Count -eq 0) $(if ($missingBins.Count) { $missingBins -join "; " } else { "rdm-mcp/rdm/fixture ok" })
}

# --- 2) MCP + CLI conformance harness ---------------------------------------
if (-not $SkipE2E -and $missingBins.Count -eq 0) {
    Write-Host "`n==> node scripts/mcp_e2e.mjs (MCP protocol + real e2e + CLI contract)" -ForegroundColor Cyan
    $mjsArgs = @("scripts/mcp_e2e.mjs", "--rdm-mcp", $mcpExe, "--rdm", $cliExe, "--fixture", $fixtureExe, "--size-mb", "$SizeMb")
    if ($Only) { $mjsArgs += @("--only", $Only) }
    if ($KeepTemp) { $mjsArgs += "--keep-temp" }
    if ($Out) { $mjsArgs += @("--out", $Out) }
    $log = Join-Path $logDir "$stamp-mcp-e2e.log"
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $mjs = Start-Process -FilePath "node" -ArgumentList $mjsArgs -NoNewWindow -PassThru -Wait `
        -RedirectStandardOutput $log -RedirectStandardError "$log.err" -ErrorAction Stop
    $sw.Stop()
    $mjsExit = $mjs.ExitCode
    $resultsPath = if ($Out) { $Out } else { Join-Path $env:CARGO_TARGET_DIR "mcp-e2e-results.json" }
    $mcpSummary = $null
    if (Test-Path $resultsPath) {
        try { $mcpSummary = Get-Content -LiteralPath $resultsPath -Raw | ConvertFrom-Json } catch { }
    }
    $summary.mcp = [ordered]@{
        exit      = $mjsExit
        seconds   = [math]::Round($sw.Elapsed.TotalSeconds, 1)
        results   = $resultsPath
        log       = $log
        pass      = if ($mcpSummary) { $mcpSummary.summary.pass } else { $null }
        fail      = if ($mcpSummary) { $mcpSummary.summary.fail } else { $null }
        sections  = if ($mcpSummary) { @($mcpSummary.summary.sections) } else { @() }
        failures  = if ($mcpSummary) { @($mcpSummary.checks | Where-Object { -not $_.pass } | ForEach-Object { $_.name }) } else { @() }
    }
    Add-Step "mcp_e2e.run" ($mjsExit -eq 0) "exit=$mjsExit $($summary.mcp.seconds)s pass=$($summary.mcp.pass) fail=$($summary.mcp.fail) log=$log"
    if ($mcpSummary) {
        Write-Host ("    MCP/CLI checks: pass={0} fail={1}" -f $mcpSummary.summary.pass, $mcpSummary.summary.fail)
        foreach ($sec in @($mcpSummary.summary.sections)) {
            $secColor = if ($sec.failed -eq 0) { "Green" } else { "Red" }
            Write-Host ("      - {0,-22} {1,3} checks, {2} failed" -f $sec.name, $sec.checks, $sec.failed) -ForegroundColor $secColor
        }
        foreach ($f in @($summary.mcp.failures)) { Write-Host "      FAILED: $f" -ForegroundColor Red }
        Write-Host "    full transcript: $resultsPath"
    }
} else {
    if ($SkipE2E) {
        Add-Step "mcp_e2e.run" $true "skipped (-SkipE2E)"
    } else {
        Add-Step "mcp_e2e.run" $false "binaries missing"
    }
}

# --- 3) regression -----------------------------------------------------------
if (-not $SkipRegression -and -not $OnlyMcp) {
    Write-Host "`n==> regression: build / test / fmt / clippy" -ForegroundColor Cyan
    $reg = [ordered]@{ build = $null; test = $null; fmt = $null; clippy = $null; test_summary = $null }

    $b = Run-Logged -Name "regression-build-workspace" -Exe "cargo" -CommandArgs @("build", "--workspace")
    $reg.build = $b
    Add-Step "regression.build_workspace" ($b.exit -eq 0) "exit=$($b.exit) $($b.seconds)s log=$($b.log)"

    $t = Run-Logged -Name "regression-test-workspace" -Exe "cargo" -CommandArgs @("test", "--workspace", "--no-fail-fast")
    $reg.test = $t
    $testSummary = $null
    if (Test-Path $t.log) {
        $resultLines = @(Get-Content $t.log | Select-String -Pattern "test result:")
        $passed = 0; $failedTests = 0
        foreach ($line in $resultLines) {
            if ($line.Line -match "(\d+) passed; (\d+) failed") {
                $passed += [int]$Matches[1]; $failedTests += [int]$Matches[2]
            }
        }
        $testSummary = [pscustomobject]@{ suites = $resultLines.Count; passed = $passed; failed = $failedTests }
        $reg.test_summary = $testSummary
    }
    Add-Step "regression.test_workspace" ($t.exit -eq 0) "exit=$($t.exit) suites=$($testSummary.suites) passed=$($testSummary.passed) failed=$($testSummary.failed) $($t.seconds)s log=$($t.log)"

    $f = Run-Logged -Name "regression-fmt" -Exe "cargo" -CommandArgs @("fmt", "--all", "--check")
    $reg.fmt = $f
    Add-Step "regression.fmt_check" ($f.exit -eq 0) "exit=$($f.exit) log=$($f.log)"

    $cl = Run-Logged -Name "regression-clippy" -Exe "cargo" -CommandArgs @("clippy", "-p", "rdm-core", "-p", "rdm-cli", "-p", "rdm-agent", "-p", "rdm-mcp", "--all-targets")
    $reg.clippy = $cl
    $clippyWarnings = 0
    if (Test-Path $cl.log) {
        $clippyWarnings = @(Get-Content $cl.log | Select-String -Pattern "^warning: " | Where-Object { $_.Line -notmatch "generated|future version" }).Count
    }
    Add-Step "regression.clippy" ($cl.exit -eq 0 -and $clippyWarnings -eq 0) "exit=$($cl.exit) warnings=$clippyWarnings log=$($cl.log)"
    $summary.regression = $reg
} else {
    Add-Step "regression" $true "skipped"
}

# --- summary -----------------------------------------------------------------
$failedSteps = @($summary.steps | Where-Object { -not $_.pass })
$summary.pass = @($summary.steps | Where-Object { $_.pass }).Count
$summary.fail = $failedSteps.Count
$outPath = Join-Path $env:CARGO_TARGET_DIR "mcp-conformance-summary.json"
$summary | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $outPath -Encoding UTF8

Write-Host ""
Write-Host "=== CONFORMANCE SUMMARY ===" -ForegroundColor Cyan
$summary.steps | Format-Table name, pass -AutoSize | Out-String | Write-Host
Write-Host ("steps: {0} passed, {1} failed -> {2}" -f $summary.pass, $summary.fail, $outPath) -ForegroundColor $(if ($summary.fail -eq 0) { "Green" } else { "Red" })
if ($summary.fail -gt 0) {
    Write-Host "failed steps:" -ForegroundColor Red
    foreach ($s in $failedSteps) { Write-Host ("  - {0}: {1}" -f $s.name, $s.detail) -ForegroundColor Red }
    exit 1
}
exit 0


