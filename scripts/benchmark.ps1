# RDM benchmark: 1 connection vs N connections against the local fixture server.
#
# Reproducible measurement of the engine's segmentation benefit under a
# per-connection-rate-limited server (the realistic reason multi-connection
# downloads help). The fixture server delays each served chunk to simulate a
# server (or CDN edge) that throttles per TCP connection.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts/benchmark.ps1
#   powershell ... -SizeMB 128 -DelayMs 25 -Connections 8 -Runs 3
[CmdletBinding()]
param(
    [int]$SizeMB = 64,
    [int]$DelayMs = 20,
    [int]$Connections = 8,
    [int]$Runs = 2,
    [int]$Port = 18080
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$target = Join-Path $root "target-bench"
$env:CARGO_TARGET_DIR = $target
$env:PATH = "C:\Users\nirvana\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin;C:\Users\nirvana\.cargo\bin;$env:PATH"

Write-Host "==> building rdm-cli and rdm-fixture-server (debug)" -ForegroundColor Cyan
cargo build -p rdm-cli -p rdm-fixture-server --manifest-path (Join-Path $root "Cargo.toml")
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$cli = Join-Path $target "debug\rdm.exe"
$fixture = Join-Path $target "debug\rdm-fixture-server.exe"
$size = $SizeMB * 1MB
$outDir = Join-Path $root "bench-out"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$results = @()
foreach ($c in @(1, $Connections)) {
    for ($run = 1; $run -le $Runs; $run++) {
        Write-Host "==> run: connections=$c ($run/$Runs)" -ForegroundColor Cyan
        $server = Start-Process -FilePath $fixture -ArgumentList @(
            "--port", $Port, "--size", $size, "--delay-ms", $DelayMs
        ) -PassThru -WindowStyle Hidden
        try {
            Start-Sleep -Milliseconds 400
            $file = Join-Path $outDir "bench-c$c-r$run.bin"
            Remove-Item $file -ErrorAction SilentlyContinue
            $json = & $cli download "http://127.0.0.1:$Port/file.bin" `
                --dir $outDir --name ("bench-c$c-r$run.bin") `
                --connections $c --json --quiet 2>&1
            $last = $json | Where-Object { $_ -match '"event"\s*:\s*"completed"' } | Select-Object -Last 1
            if (-not $last) { throw "download failed: $($json | Select-Object -Last 5 | Out-String)" }
            $evt = $last | ConvertFrom-Json
            $results += [pscustomobject]@{
                Connections = $c
                Run         = $run
                Seconds     = [math]::Round($evt.elapsed_ms / 1000.0, 3)
                MBps        = [math]::Round(($evt.total_bytes / 1MB) / ($evt.elapsed_ms / 1000.0), 2)
                Bytes       = $evt.total_bytes
                Path        = $evt.path
            }
            Remove-Item $file -ErrorAction SilentlyContinue
        }
        finally {
            if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
        }
    }
}

$summary = $results | Group-Object Connections | ForEach-Object {
    [pscustomobject]@{
        Connections = $_.Name
        AvgSeconds  = [math]::Round(($_.Group | Measure-Object Seconds -Average).Average, 3)
        AvgMBps     = [math]::Round(($_.Group | Measure-Object MBps -Average).Average, 2)
    }
}

$summary | Format-Table -AutoSize | Out-String | Write-Host
$base = ($summary | Where-Object Connections -eq 1).AvgSeconds
$multi = ($summary | Where-Object Connections -eq $Connections).AvgSeconds
if ($base -and $multi) {
    Write-Host ("speedup: {0:N2}x  (1 conn {1}s -> {2} conns {3}s)" -f ($base / $multi), $base, $Connections, $multi) -ForegroundColor Green
}

$summary | ConvertTo-Json | Set-Content (Join-Path $root "bench-out\summary.json")
Write-Host "raw results: bench-out\summary.json"
