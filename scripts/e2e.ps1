# RDM GUI end-to-end acceptance test (independent QA).
#
# Covers: GUI start with isolated state dir -> IPC ping/status -> enqueue real
# fixture download (with expected SHA-256) -> progress -> completed -> target
# file hash + downloads.json -> screenshot -> pause/resume -> single-instance
# URL delivery -> cancel. No source files are modified.
#
# Isolation: the GUI is started with LOCALAPPDATA pointed at a throwaway tree,
# so %LOCALAPPDATA%\RDM\ipc.json / downloads.json / settings.json of the
# developer's real (or another agent's) instance are never touched.
#
# Usage:
#   pwsh -NoProfile -File scripts/e2e.ps1
#   pwsh -NoProfile -File scripts/e2e.ps1 -RdmGuiExe target-qa\debug\rdm-gui.exe -Port 18131
[CmdletBinding()]
param(
    [string]$RdmGuiExe = "target-qa\debug\rdm-gui.exe",
    [string]$FixtureExe = "target-qa\release\rdm-fixture-server.exe",
    [string]$RdmCliExe = "target-qa\release\rdm.exe",
    [string]$Screenshot = "docs\evidence\rdm-gui-final.png",
    [int]$Port = 18131,
    [int]$SizeMB = 32,
    [switch]$KeepArtifacts
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root

function Resolve-PathFromRoot([string]$Path) {
    if ([System.IO.Path]::IsPathRooted($Path)) { return $Path }
    return (Join-Path $root $Path)
}
$RdmGuiExe = Resolve-PathFromRoot $RdmGuiExe
$FixtureExe = Resolve-PathFromRoot $FixtureExe
$RdmCliExe = Resolve-PathFromRoot $RdmCliExe
$Screenshot = Resolve-PathFromRoot $Screenshot

foreach ($exe in @($RdmGuiExe, $FixtureExe, $RdmCliExe)) {
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "missing executable: $exe" }
}

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$artifacts = Join-Path $root "target-qa\gui-e2e-artifacts"
New-Item -ItemType Directory -Force -Path $artifacts | Out-Null
$isoLocal = Join-Path $env:TEMP "rdm-qa-localappdata"
$dlDir = Join-Path $env:TEMP "rdm-qa-gui-downloads"
$refDir = Join-Path $env:TEMP "rdm-qa-gui-reference"
$guiOut = Join-Path $artifacts "gui-stdout-$stamp.log"
$guiErr = Join-Path $artifacts "gui-stderr-$stamp.log"

$checks = New-Object System.Collections.Generic.List[object]
function Add-Check([string]$Name, [bool]$Pass, [string]$Detail = "") {
    $checks.Add([pscustomobject]@{ name = $Name; pass = $Pass; detail = $Detail })
    $color = if ($Pass) { "Green" } else { "Red" }
    Write-Host ("[{0}] {1}{2}" -f $(if ($Pass) { "PASS" } else { "FAIL" }), $Name, $(if ($Detail) { "  ($Detail)" } else { "" })) -ForegroundColor $color
}

function Wait-Port([int]$Port, [int]$TimeoutMs = 15000) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        try {
            $c = New-Object System.Net.Sockets.TcpClient
            $iar = $c.BeginConnect("127.0.0.1", $Port, $null, $null)
            if ($iar.AsyncWaitHandle.WaitOne(200) -and $c.Connected) { $c.Close(); return $true }
            $c.Close()
        } catch { }
        Start-Sleep -Milliseconds 100
    }
    return $false
}

$script:IpcSeq = 100
function Send-RdmIpc {
    param([int]$IpcPort, [string]$Token, [hashtable]$Request, [int]$TimeoutMs = 15000, [string]$RawLine = "")
    if ($RawLine) {
        # Control methods (pause/resume/cancel/open_folder) use the duplicate-key
        # wire form documented in docs/extension-protocol.md §2.2: the *first*
        # numeric "id" is the envelope correlation id, the *last* string "id" is
        # the download id. A hashtable cannot express duplicate keys, so the line
        # is assembled by hand.
        $json = $RawLine
    } else {
        $script:IpcSeq++
        $Request["id"] = $script:IpcSeq
        $Request["token"] = $Token
        $json = $Request | ConvertTo-Json -Depth 12 -Compress
    }
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $client.ReceiveTimeout = $TimeoutMs
        $client.SendTimeout = $TimeoutMs
        $client.Connect("127.0.0.1", $IpcPort)
        $stream = $client.GetStream()
        $writer = New-Object System.IO.StreamWriter($stream, [System.Text.UTF8Encoding]::new($false))
        $writer.NewLine = "`n"; $writer.AutoFlush = $true
        $reader = New-Object System.IO.StreamReader($stream, [System.Text.UTF8Encoding]::new($false))
        $writer.WriteLine($json)
        $line = $reader.ReadLine()
        if ([string]::IsNullOrWhiteSpace($line)) { throw "empty IPC response" }
        return ($line | ConvertFrom-Json)
    }
    finally {
        $client.Close()
    }
}

function Get-RdmStatus([int]$IpcPort, [string]$Token) {
    $resp = Send-RdmIpc -IpcPort $IpcPort -Token $Token -Request @{ method = "status" }
    if (-not $resp.ok) { throw "status rejected: $($resp.error)" }
    return $resp.data
}

function Find-Download($Data, [string]$Id) {
    foreach ($d in $Data.downloads) { if ($d.id -eq $Id) { return $d } }
    return $null
}

$guiProc = $null
$fixtureProc = $null
$secondProc = $null
$oldLocalAppData = $env:LOCALAPPDATA
$oldRustLog = $env:RUST_LOG
$report = [ordered]@{
    generated_at   = (Get-Date).ToString("o")
    git_commit     = (& git rev-parse HEAD 2>$null | Out-String).Trim()
    git_dirty      = ((& git status --short 2>$null | Measure-Object).Count -gt 0)
    gui_exe        = $RdmGuiExe
    fixture_exe    = $FixtureExe
    isolated_localappdata = $isoLocal
    checks         = @()
}

try {
    # ---- 0. environment isolation ----------------------------------------
    if (Get-Process -Name "rdm-gui" -ErrorAction SilentlyContinue) {
        throw "a real rdm-gui is already running; refusing to run e2e (would collide with ipc.json / screenshots)"
    }
    Remove-Item -Recurse -Force $isoLocal -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $dlDir -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $refDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path (Join-Path $isoLocal "RDM") | Out-Null
    New-Item -ItemType Directory -Force -Path $dlDir | Out-Null
    New-Item -ItemType Directory -Force -Path $refDir | Out-Null
    # Pre-seed settings so the GUI's defaults point into the throwaway tree.
    $settings = [ordered]@{
        default_connections = 8
        default_dir         = $dlDir
        speed_limit_bps     = $null
        verify_sha256       = $true
        theme               = "dark"
        auto_start          = $true
    }
    $settings | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $isoLocal "RDM\settings.json") -Encoding UTF8
    $env:LOCALAPPDATA = $isoLocal
    $env:RUST_LOG = "info"

    # ---- 1. fixture server ------------------------------------------------
    Write-Host "==> starting fixture server on port $Port" -ForegroundColor Cyan
    $fixtureProc = Start-Process -FilePath $FixtureExe -ArgumentList @(
        "--port", "$Port", "--size", ($SizeMB * 1MB), "--delay-ms", "50"
    ) -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $artifacts "fixture-$stamp.log") -RedirectStandardError (Join-Path $artifacts "fixture-$stamp.err.log")
    if (-not (Wait-Port -Port $Port)) { throw "fixture server did not start" }
    $fileUrl = "http://127.0.0.1:$Port/file"

    # ---- 2. reference payload (independent hash source) -------------------
    Write-Host "==> reference download via CLI ($SizeMB MiB)" -ForegroundColor Cyan
    $refName = "reference-$SizeMB.bin"
    $refLines = & $RdmCliExe download $fileUrl -d $refDir --name $refName -c 8 --json --quiet 2>&1
    if ($LASTEXITCODE -ne 0) { throw "reference download failed: $($refLines | Select-Object -Last 5 | Out-String)" }
    $refHash = (Get-FileHash -LiteralPath (Join-Path $refDir $refName) -Algorithm SHA256).Hash
    $refCompleted = $refLines | Where-Object { $_ -match '"event"\s*:\s*"completed"' } | Select-Object -Last 1
    $refEvent = $refCompleted | ConvertFrom-Json
    Write-Host "    reference sha256=$refHash size=$($refEvent.total_bytes)" -ForegroundColor DarkGray

    # ---- 3. start GUI -----------------------------------------------------
    Write-Host "==> starting GUI: $RdmGuiExe" -ForegroundColor Cyan
    $guiProc = Start-Process -FilePath $RdmGuiExe -PassThru `
        -RedirectStandardOutput $guiOut -RedirectStandardError $guiErr
    $ipcPath = Join-Path $isoLocal "RDM\ipc.json"
    $deadline = (Get-Date).AddSeconds(25)
    while ((Get-Date) -lt $deadline -and -not (Test-Path -LiteralPath $ipcPath)) {
        if ($guiProc.HasExited) { throw "GUI exited early (code $($guiProc.ExitCode)); stderr: $(Get-Content $guiErr -Raw -ErrorAction SilentlyContinue)" }
        Start-Sleep -Milliseconds 150
    }
    $ipcOk = Test-Path -LiteralPath $ipcPath
    $ipc = $null
    if ($ipcOk) {
        $ipc = Get-Content -LiteralPath $ipcPath -Raw | ConvertFrom-Json
        $ipcOk = ($ipc.protocol -eq 1) -and $ipc.port -and $ipc.token -and $ipc.pid -eq $guiProc.Id
    }
    Add-Check "gui.publishes_ipc_json_own_pid" $ipcOk $(if ($ipc) { "port=$($ipc.port) pid=$($ipc.pid) token_len=$($ipc.token.Length)" } else { "no ipc.json" })
    if (-not $ipcOk) { throw "GUI did not publish a valid isolated ipc.json" }
    $ipcPort = [int]$ipc.port
    $token = [string]$ipc.token

    # ---- 4. ping / initial status ----------------------------------------
    $pong = Send-RdmIpc -IpcPort $ipcPort -Token $token -Request @{ method = "ping" }
    Add-Check "ipc.ping" ($pong.ok -and $pong.data.gui_running -eq $true -and $pong.data.protocol -eq 1) "version=$($pong.data.version) protocol=$($pong.data.protocol)"
    $initial = Get-RdmStatus -IpcPort $ipcPort -Token $token
    Add-Check "ipc.status_initial_empty" (@($initial.downloads).Count -eq 0) "downloads=$(@($initial.downloads).Count)"

    # unauthorized must be rejected
    $badReq = @{ method = "ping"; id = 999; token = "0000000000000000000000000000000000000000000000000000000000000000" }
    $bad = $null
    try { $bad = Send-RdmIpc -IpcPort $ipcPort -Token "0000000000000000000000000000000000000000000000000000000000000000" -Request @{ method = "ping" } } catch { }
    Add-Check "ipc.rejects_bad_token" ($null -ne $bad -and $bad.ok -eq $false) $(if ($bad) { "error=$($bad.error)" } else { "no response" })

    # ---- 5. enqueue download #1 with expected SHA-256 ---------------------
    Write-Host "==> enqueue download #1 ($SizeMB MiB, checksum verified)" -ForegroundColor Cyan
    $target1 = Join-Path $dlDir "gui-e2e-1.bin"
    $enq1 = Send-RdmIpc -IpcPort $ipcPort -Token $token -Request @{
        method = "enqueue"
        item   = @{
            download    = @{
                url             = $fileUrl
                output_dir      = $dlDir
                file_name       = "gui-e2e-1.bin"
                connections     = 8
                expected_sha256 = $refHash
            }
            source      = "qa-e2e"
            source_page = "http://127.0.0.1:$Port/file"
        }
    }
    Add-Check "ipc.enqueue_accepted" ($enq1.ok -eq $true) $(if ($enq1.ok) { "id=$($enq1.data.download_id)" } else { "error=$($enq1.error)" })
    if (-not $enq1.ok) { throw "enqueue #1 rejected" }
    $id1 = [string]$enq1.data.download_id

    # ---- 6. progress -> completed ----------------------------------------
    $sawDownloading = $false
    $sawProgress = $false
    $maxDownloaded = 0
    $final1 = $null
    $deadline = (Get-Date).AddSeconds(120)
    while ((Get-Date) -lt $deadline) {
        $st = Get-RdmStatus -IpcPort $ipcPort -Token $token
        $item = Find-Download $st $id1
        if ($null -eq $item) { Start-Sleep -Milliseconds 100; continue }
        if ($item.state -eq "downloading") { $sawDownloading = $true }
        if ([int64]$item.downloaded -gt 0) { $sawProgress = $true }
        if ([int64]$item.downloaded -gt $maxDownloaded) { $maxDownloaded = [int64]$item.downloaded }
        if ($item.state -in @("completed", "failed", "cancelled")) { $final1 = $item; break }
        Start-Sleep -Milliseconds 100
    }
    Add-Check "gui.download1_reaches_completed" ($null -ne $final1 -and $final1.state -eq "completed") $(if ($final1) { "state=$($final1.state) downloaded=$($final1.downloaded)/$($final1.total) speed=$([math]::Round($final1.speed_bps,0))" } else { "timeout" })
    Add-Check "gui.download1_shows_progress" ($sawDownloading -or $sawProgress) "saw_downloading=$sawDownloading max_downloaded=$maxDownloaded"
    Add-Check "gui.download1_total_bytes" ($null -ne $final1 -and [int64]$final1.total -eq [int64]($SizeMB * 1MB)) $(if ($final1) { "$($final1.total)" } else { "n/a" })

    # ---- 7. target file + downloads.json ---------------------------------
    Start-Sleep -Milliseconds 800
    $file1Ok = Test-Path -LiteralPath $target1
    $hash1 = if ($file1Ok) { (Get-FileHash -LiteralPath $target1 -Algorithm SHA256).Hash } else { "" }
    Add-Check "gui.download1_file_hash_matches_payload" ($file1Ok -and $hash1 -eq $refHash) "sha256=$hash1"
    $dlJsonPath = Join-Path $isoLocal "RDM\downloads.json"
    $persisted = $null
    if (Test-Path -LiteralPath $dlJsonPath) { $persisted = Get-Content -LiteralPath $dlJsonPath -Raw | ConvertFrom-Json }
    $persistedItem = $null
    if ($persisted) { $persistedItem = @($persisted.items) | Where-Object { $_.id -eq $id1 } | Select-Object -First 1 }
    Add-Check "gui.downloads_json_has_completed_item" ($null -ne $persistedItem -and $persistedItem.state -eq "completed" -and [int64]$persistedItem.downloaded -eq [int64]($SizeMB * 1MB)) $(if ($persistedItem) { "state=$($persistedItem.state) downloaded=$($persistedItem.downloaded) sha=$($persistedItem.sha256)" } else { "not found" })
    Add-Check "gui.downloads_json_sha256" ($null -ne $persistedItem -and $persistedItem.sha256 -eq $refHash) $(if ($persistedItem) { "$($persistedItem.sha256)" } else { "n/a" })

    # ---- 8. pause -> screenshot -> resume (64 MiB with 50 ms latency) -----
    Write-Host "==> enqueue download #2 ($SizeMB MiB) for pause/resume" -ForegroundColor Cyan
    $target2 = Join-Path $dlDir "gui-e2e-2.bin"

    $enq2 = Send-RdmIpc -IpcPort $ipcPort -Token $token -Request @{
        method = "enqueue"
        item   = @{
            download    = @{ url = $fileUrl; output_dir = $dlDir; file_name = "gui-e2e-2.bin"; connections = 8 }
            source      = "qa-e2e"
            source_page = $null
        }
    }
    Add-Check "ipc.enqueue_download2" ($enq2.ok -eq $true) $(if ($enq2.ok) { "id=$($enq2.data.download_id)" } else { "error=$($enq2.error)" })
    if (-not $enq2.ok) { throw "enqueue #2 rejected" }
    $id2 = [string]$enq2.data.download_id

    $pausedOk = $false
    $pausedDownloaded = 0
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        $st = Get-RdmStatus -IpcPort $ipcPort -Token $token
        $item = Find-Download $st $id2
        if ($null -eq $item) { Start-Sleep -Milliseconds 80; continue }
        if ($item.state -eq "downloading" -and [int64]$item.downloaded -ge (2MB)) {
            $script:IpcSeq++
            $pauseLine = "{`"id`":$($script:IpcSeq),`"token`":`"$token`",`"method`":`"pause`",`"id`":`"$id2`"}"
            $pauseResp = Send-RdmIpc -IpcPort $ipcPort -Token $token -Request @{} -RawLine $pauseLine
            if (-not $pauseResp.ok) { Write-Warning "pause rejected: $($pauseResp.error)" }
            # wait for the state machine to settle on paused
            $pdeadline = (Get-Date).AddSeconds(10)
            while ((Get-Date) -lt $pdeadline) {
                Start-Sleep -Milliseconds 120
                $st2 = Get-RdmStatus -IpcPort $ipcPort -Token $token
                $it2 = Find-Download $st2 $id2
                if ($it2.state -eq "paused") { $pausedOk = $true; $pausedDownloaded = [int64]$it2.downloaded; break }
                if ($it2.state -in @("completed", "failed")) { break }
            }
            break
        }
        Start-Sleep -Milliseconds 80
    }
    Add-Check "gui.download2_pauses_midway" ($pausedOk -and $pausedDownloaded -gt 0 -and $pausedDownloaded -lt ($SizeMB * 1MB)) "paused_at=$pausedDownloaded / $($SizeMB * 1MB)"

    # persist check: downloads.json must record paused state before resume
    if ($pausedOk) {
        Start-Sleep -Milliseconds 700
        $persisted2 = Get-Content -LiteralPath $dlJsonPath -Raw | ConvertFrom-Json
        $pItem2 = @($persisted2.items) | Where-Object { $_.id -eq $id2 } | Select-Object -First 1
        Add-Check "gui.downloads_json_has_paused_item" ($null -ne $pItem2 -and $pItem2.state -eq "paused") $(if ($pItem2) { "state=$($pItem2.state) downloaded=$($pItem2.downloaded)" } else { "not found" })
    }

    # screenshot with completed + paused rows visible
    Write-Host "==> capturing window screenshot" -ForegroundColor Cyan
    if (Test-Path -LiteralPath (Join-Path $root "scripts\capture-window.ps1")) {
        & pwsh -NoProfile -File (Join-Path $root "scripts\capture-window.ps1") -ProcessName "rdm-gui" -OutFile $Screenshot 2>&1 | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
        $shotOk = Test-Path -LiteralPath $Screenshot
        $shotSize = if ($shotOk) { (Get-Item -LiteralPath $Screenshot).Length } else { 0 }
        Add-Check "gui.screenshot_saved" ($shotOk -and $shotSize -gt 10000) "path=$Screenshot bytes=$shotSize"
    } else {
        Add-Check "gui.screenshot_saved" $false "scripts/capture-window.ps1 missing"
    }

    # resume -> completed
    $script:IpcSeq++
    $resumeLine = "{`"id`":$($script:IpcSeq),`"token`":`"$token`",`"method`":`"resume`",`"id`":`"$id2`"}"
    $resumeResp = Send-RdmIpc -IpcPort $ipcPort -Token $token -Request @{} -RawLine $resumeLine
    Add-Check "ipc.resume_accepted" ($resumeResp.ok -eq $true) $(if ($resumeResp.ok) { "ok" } else { "error=$($resumeResp.error)" })
    $final2 = $null
    $deadline = (Get-Date).AddSeconds(180)
    while ((Get-Date) -lt $deadline) {
        $st = Get-RdmStatus -IpcPort $ipcPort -Token $token
        $item = Find-Download $st $id2
        if ($item -and $item.state -in @("completed", "failed", "cancelled")) { $final2 = $item; break }
        Start-Sleep -Milliseconds 150
    }
    Add-Check "gui.download2_resume_completes" ($null -ne $final2 -and $final2.state -eq "completed") $(if ($final2) { "state=$($final2.state) downloaded=$($final2.downloaded)/$($final2.total)" } else { "timeout" })
    Start-Sleep -Milliseconds 500
    $file2Ok = Test-Path -LiteralPath $target2
    $hash2 = if ($file2Ok) { (Get-FileHash -LiteralPath $target2 -Algorithm SHA256).Hash } else { "" }
    Add-Check "gui.download2_resumed_file_hash" ($file2Ok -and $hash2 -eq $refHash) "sha256=$hash2"

    # ---- 9. single instance: `rdm-gui.exe <URL>` while running ----------
    Write-Host "==> single-instance URL delivery" -ForegroundColor Cyan
    $singleOut = Join-Path $artifacts "single-instance-$stamp.log"
    $singleErr = Join-Path $artifacts "single-instance-$stamp.err.log"
    $secondProc = Start-Process -FilePath $RdmGuiExe -ArgumentList @("$fileUrl") -PassThru `
        -RedirectStandardOutput $singleOut -RedirectStandardError $singleErr
    $exited = $secondProc.WaitForExit(20000)
    $secondCode = if ($exited) { $secondProc.ExitCode } else { $null }
    $singleText = if (Test-Path -LiteralPath $singleOut) { Get-Content -LiteralPath $singleOut -Raw } else { "" }
    Add-Check "gui.single_instance_delivers_url" ($exited -and $secondCode -eq 0 -and $singleText -match "已投递") $(if ($exited) { "exit=$secondCode out=$($singleText.Trim())" } else { "second instance did not exit" })
    if (-not $exited) { try { $secondProc.Kill() } catch { } }

    $stAfter = Get-RdmStatus -IpcPort $ipcPort -Token $token
    $newItem = @($stAfter.downloads) | Where-Object { $_.id -notin @($id1, $id2) } | Select-Object -First 1
    Add-Check "gui.single_instance_item_appears" ($null -ne $newItem) $(if ($newItem) { "id=$($newItem.id) state=$($newItem.state)" } else { "no new item" })

    # cancel the single-instance item (covers cancel path too)
    if ($newItem) {
        $script:IpcSeq++
        $cancelLine = "{`"id`":$($script:IpcSeq),`"token`":`"$token`",`"method`":`"cancel`",`"id`":`"$($newItem.id)`"}"
        $cancelResp = Send-RdmIpc -IpcPort $ipcPort -Token $token -Request @{} -RawLine $cancelLine
        Start-Sleep -Milliseconds 800
        $stCancel = Get-RdmStatus -IpcPort $ipcPort -Token $token
        $cancelled = Find-Download $stCancel $newItem.id
        $cancelOk = ($cancelled.state -eq "cancelled")
        Add-Check "ipc.cancel_sets_cancelled" ($cancelResp.ok -and $cancelOk) $(if ($cancelled) { "state=$($cancelled.state)" } else { "gone" })
    }

    # ---- 10. GUI still alive / one instance only ------------------------
    $guiProcs = @(Get-Process -Name "rdm-gui" -ErrorAction SilentlyContinue)
    $settle = 0
    while ($guiProcs.Count -ne 1 -and $settle -lt 20) { Start-Sleep -Milliseconds 250; $settle++; $guiProcs = @(Get-Process -Name "rdm-gui" -ErrorAction SilentlyContinue) }
    Add-Check "gui.single_owned_instance_alive" (-not $guiProc.HasExited -and $guiProcs.Count -eq 1) "instances=$($guiProcs.Count)"
}
catch {
    Add-Check "e2e.harness" $false $_.Exception.Message
    Write-Host $_.ScriptStackTrace -ForegroundColor DarkRed
}
finally {
    Write-Host "==> cleanup" -ForegroundColor Cyan
    try {
    foreach ($proc in @($secondProc, $guiProc, $fixtureProc)) {
        if ($proc -and -not $proc.HasExited) {
            try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch { }
            Start-Sleep -Milliseconds 300
        }
    }
    # preserve diagnostics before deleting the isolated state
    try {
        if (Test-Path -LiteralPath (Join-Path $isoLocal "RDM\downloads.json")) {
            Copy-Item -LiteralPath (Join-Path $isoLocal "RDM\downloads.json") -Destination (Join-Path $artifacts "downloads-$stamp.json") -Force
        }
        if (Test-Path -LiteralPath $guiErr) {
            $remaining = Get-Content -LiteralPath $guiErr -Raw -ErrorAction SilentlyContinue
            Set-Content -LiteralPath (Join-Path $artifacts "gui-stderr-$stamp.log") -Value $remaining -Encoding UTF8
        }
    } catch { }
    if (-not $KeepArtifacts) {
        Remove-Item -Recurse -Force $isoLocal -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $dlDir -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $refDir -ErrorAction SilentlyContinue
    }
    $env:LOCALAPPDATA = $oldLocalAppData
    $env:RUST_LOG = $oldRustLog
    } catch {
        Write-Warning ("cleanup warning: {0}" -f $_.Exception.Message)
        Write-Warning $_.InvocationInfo.PositionMessage
    }
    $passed = @($checks | Where-Object pass).Count
    $failed = @($checks | Where-Object { -not $_.pass }).Count
    $report.checks = $checks
    $report.pass_count = $passed
    $report.fail_count = $failed
    $report.screenshot = $Screenshot
    $reportPath = Join-Path $root "target-qa\gui-e2e-results.json"
    try {
        $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $reportPath -Encoding UTF8
        Write-Host ""
        Write-Host ("== GUI E2E SUMMARY == {0} passed, {1} failed -> {2}" -f $passed, $failed, $reportPath) -ForegroundColor $(if ($failed -eq 0) { "Green" } else { "Red" })
        Write-Host ("artifacts: {0}" -f $artifacts) -ForegroundColor DarkGray
    } catch {
        Write-Warning ("report write failed: {0}" -f $_.Exception.Message)
    }
    if ($failed -gt 0) { exit 1 }
    exit 0
}




