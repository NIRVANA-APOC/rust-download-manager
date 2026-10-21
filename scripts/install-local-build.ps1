#Requires -Version 7.0
<#
.SYNOPSIS
    构建并把最新版 RDM GUI / native messaging host 安装到实际安装目录。

.DESCRIPTION
    按 AGENTS.md 的约定：「每次修改后都要把新版的 GUI、宿主装到实际安装目录」。
    本脚本把这件事做成一条命令，可反复执行（幂等）：

      1. cargo build -p rdm-native              （默认 target，扩展实际加载的宿主）
      2. cargo build -p rdm-gui --features windows-subsystem
                                                （安装版用 GUI 子系统，双击不弹控制台）
      3. 结束正在运行的旧 rdm-native 进程（无状态，Firefox 下次请求会自动拉起新宿主）
      4. 复制到 %LOCALAPPDATA%\RDM（可用 -InstallDir 覆盖），并对每个文件做 SHA-256 校验
      5. 目标 exe 被占用时：先尝试优雅/强制结束；仍占用则把旧文件重命名为
         *.old-<时间戳> 再写入（Windows 允许重命名运行中的 exe），并提示重启生效
      6. 清理可删除的 *.old-*（仍被旧进程占用的留到下次）
      7. 默认重启 GUI：若发现旧 GUI 在运行，结束它并用新构建重新启动（-NoRestart 可关）

    扩展源码（extension/firefox、extension/chrome）不在本脚本范围内：
    Firefox 在 about:debugging 重新载入即可；Chrome 先跑 scripts/build-chrome-extension.ps1。

.PARAMETER SkipBuild
    跳过 cargo 构建，只安装 target\debug 下已有的产物（用于快速重装）。

.PARAMETER NoRestart
    不重启 GUI（只安装）。脚本仍会尝试结束旧 rdm-native 进程。

.PARAMETER InstallDir
    安装目录，默认 %LOCALAPPDATA%\RDM。

.EXAMPLE
    pwsh -File scripts/install-local-build.ps1

.EXAMPLE
    pwsh -File scripts/install-local-build.ps1 -SkipBuild -NoRestart

.NOTES
    退出码：0 = 全部安装成功且哈希一致；1 = 有文件未安装成功。
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$NoRestart,
    [string]$InstallDir = ""
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

function Write-Step($Message) { Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Ok($Message) { Write-Host "    $Message" -ForegroundColor Green }
function Write-Warn2($Message) { Write-Host "    $Message" -ForegroundColor Yellow }

# ---------------- 前置：cargo / MinGW PATH ----------------
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if ((Test-Path -LiteralPath $cargoBin) -and (($env:PATH -split ';') -notcontains $cargoBin)) {
    $env:PATH = "$cargoBin;$env:PATH"
}
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) { throw "找不到 cargo；请先安装 Rust 并确保 %USERPROFILE%\.cargo\bin 可用。" }

$installDir = if ($InstallDir) { [System.IO.Path]::GetFullPath($InstallDir) } else { Join-Path $env:LOCALAPPDATA "RDM" }
$guiName = "rdm-gui.exe"
$nativeName = "rdm-native.exe"
$targetDir = Join-Path $repoRoot "target\debug"
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"

# ---------------- 构建 ----------------
if (-not $SkipBuild) {
    Push-Location -LiteralPath $repoRoot
    try {
        Write-Step "构建 rdm-native（默认 target）"
        & cargo build -p rdm-native
        if ($LASTEXITCODE -ne 0) { throw "cargo build -p rdm-native 失败（exit=$LASTEXITCODE）" }
        Write-Ok "rdm-native 构建完成"

        Write-Step "构建 rdm-gui（--features windows-subsystem，安装版用 GUI 子系统）"
        & cargo build -p rdm-gui --features windows-subsystem
        if ($LASTEXITCODE -ne 0) { throw "cargo build -p rdm-gui 失败（exit=$LASTEXITCODE）" }
        Write-Ok "rdm-gui 构建完成"
    }
    finally {
        Pop-Location
    }
}

# ---------------- 工具函数 ----------------
function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Test-FileLocked([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $false }
    try {
        $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
        $stream.Close()
        $stream.Dispose()
        return $false
    }
    catch {
        return $true
    }
}

function Stop-ProcessByName([string]$Name, [int]$WaitMs = 3000) {
    $stopped = @()
    foreach ($proc in @(Get-Process -Name $Name -ErrorAction SilentlyContinue)) {
        try {
            $proc.Kill($true)
            if ($proc.WaitForExit($WaitMs)) { $stopped += $proc.Id }
        }
        catch {
            Write-Warn2 "无法结束 $Name (pid=$($proc.Id))：$($_.Exception.Message)"
        }
    }
    return $stopped
}

# 安装一个 exe：处理占用 → 复制 → 哈希校验；返回是否成功。
function Install-Binary([string]$Source, [string]$Destination, [string]$ProcessName) {
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        Write-Warn2 "缺少构建产物：$Source（先去掉 -SkipBuild 构建）"
        return $false
    }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Destination) | Out-Null

    if (Test-FileLocked $Destination) {
        Write-Warn2 "$(Split-Path -Leaf $Destination) 正被占用，先尝试结束进程 $ProcessName"
        $stopped = Stop-ProcessByName $ProcessName
        if ($stopped.Count -gt 0) { Write-Ok "已结束进程：$($stopped -join ', ')" }
        Start-Sleep -Milliseconds 400
    }

    # 仍被占用（例如浏览器拉起的宿主进程权限更高）：重命名旧文件后写新文件。
    if (Test-FileLocked $Destination) {
        $backup = "$Destination.old-$stamp"
        try {
            Rename-Item -LiteralPath $Destination -NewName (Split-Path -Leaf $backup) -Force -ErrorAction Stop
            Write-Warn2 "旧文件被占用，已重命名为 $(Split-Path -Leaf $backup)（重启对应进程后生效并可删除）"
        }
        catch {
            Write-Warn2 "旧文件无法重命名：$($_.Exception.Message)"
        }
    }

    try {
        Copy-Item -LiteralPath $Source -Destination $Destination -Force -ErrorAction Stop
    }
    catch {
        Write-Warn2 "复制失败：$($_.Exception.Message)"
        return $false
    }

    $sourceHash = Get-Sha256 $Source
    $installedHash = Get-Sha256 $Destination
    if ($sourceHash -ne $installedHash) {
        Write-Warn2 "哈希不一致：source=$sourceHash installed=$installedHash"
        return $false
    }
    Write-Ok "$(Split-Path -Leaf $Destination) 已安装并校验（SHA-256 ${sourceHash}，$(Get-Item -LiteralPath $Destination | Select-Object -ExpandProperty Length) 字节）"
    return $true
}

function Remove-StaleOldFiles([string]$Directory) {
    foreach ($old in @(Get-ChildItem -LiteralPath $Directory -Filter "*.old-*" -File -ErrorAction SilentlyContinue)) {
        if (Test-FileLocked $old.FullName) { continue }
        try {
            Remove-Item -LiteralPath $old.FullName -Force -ErrorAction Stop
            Write-Ok "已清理旧文件 $($old.Name)"
        }
        catch { }
    }
}

# ---------------- 安装 ----------------
Write-Step "安装到 $installDir"
$guiWasRunning = @(Get-Process -Name "rdm-gui" -ErrorAction SilentlyContinue).Count -gt 0

# 宿主无状态：先结束，保证复制不被占用、且 Firefox 下次请求拉起新版本。
$nativeStopped = Stop-ProcessByName "rdm-native"
if ($nativeStopped.Count -gt 0) { Write-Ok "已结束旧 rdm-native：$($nativeStopped -join ', ')" }

$results = [ordered]@{
    $guiName    = Install-Binary (Join-Path $targetDir $guiName) (Join-Path $installDir $guiName) "rdm-gui"
    $nativeName = Install-Binary (Join-Path $targetDir $nativeName) (Join-Path $installDir $nativeName) "rdm-native"
}

# ---------------- 重启 GUI ----------------
$restarted = $false
if (-not $NoRestart) {
    $guiProcess = Get-Process -Name "rdm-gui" -ErrorAction SilentlyContinue
    if ($guiProcess) {
        $stopped = Stop-ProcessByName "rdm-gui"
        Write-Ok "已结束旧 rdm-gui：$($stopped -join ', ')"
        Start-Sleep -Milliseconds 500
    }
    if ($results[$guiName] -or $guiWasRunning) {
        $guiPath = Join-Path $installDir $guiName
        if (Test-Path -LiteralPath $guiPath) {
            # 必须用 ShellExecute（Start-Process 默认）而不是 ProcessStartInfo+UseShellExecute=false：
            # 后者会让 GUI 子进程继承本脚本的 stdout 句柄，调用方（管道/agent）会一直等 EOF，
            # 看起来像脚本挂死。
            $null = Start-Process -FilePath $guiPath -WorkingDirectory $installDir
            $restarted = $true
            Write-Ok "已用新构建启动 GUI：$guiPath"
        }
    }
}
elseif ($guiWasRunning) {
    Write-Warn2 "GUI 仍在运行旧构建（-NoRestart）：请手动重启 RDM 后新行为才生效"
}

Remove-StaleOldFiles $installDir

# ---------------- 汇总 ----------------
Write-Host ""
$failed = @($results.GetEnumerator() | Where-Object { -not $_.Value }).Count
$table = $results.GetEnumerator() | ForEach-Object {
    [pscustomobject]@{
        文件      = $_.Key
        结果      = if ($_.Value) { "OK" } else { "失败" }
        安装位置  = Join-Path $installDir $_.Key
        修改时间  = (Get-Item -LiteralPath (Join-Path $installDir $_.Key) -ErrorAction SilentlyContinue).LastWriteTime
    }
}
$table | Format-Table -AutoSize | Out-String | Write-Host

Write-Host "提示：" -NoNewline
Write-Host "Firefox 若仍持有旧宿主进程，浏览器会在下次请求时自动重连新宿主；如仍未生效请重启浏览器。" -ForegroundColor Yellow
if ($failed -gt 0) {
    Write-Host "结果：$failed 个文件安装失败。" -ForegroundColor Red
    exit 1
}
Write-Host "结果：GUI 与原生宿主均已安装（哈希校验通过）$(if ($restarted) { '，GUI 已重启' })。" -ForegroundColor Green
exit 0
