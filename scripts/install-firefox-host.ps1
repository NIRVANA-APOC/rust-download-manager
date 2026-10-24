#Requires -Version 5.1
<#
.SYNOPSIS
    安装 RDM Firefox 原生消息宿主（当前用户，无需管理员权限）。

.DESCRIPTION
    1. 定位（或使用 -Build 构建）rdm-native.exe，复制到
       %LOCALAPPDATA%\RDM\rdm-native.exe（有 rdm-gui.exe 时一并复制，便于自动启动）；
    2. 生成 %LOCALAPPDATA%\RDM\com.rdm.download.json，
       allowed_extensions 限定为扩展 ID rdm@rdm.download；
    3. 注册 HKCU\Software\Mozilla\NativeMessagingHosts\com.rdm.download
       默认值为 manifest 绝对路径。

    只写 HKCU 与当前用户目录，不需要管理员权限，也不修改系统级 PATH。

.PARAMETER Configuration
    Auto（默认，优先 release）、Release 或 Debug。选择 target 目录下的构建配置。

.PARAMETER NativeExe
    显式指定 rdm-native.exe 路径（优先于 target 目录搜索）。

.PARAMETER GuiExe
    显式指定 rdm-gui.exe 路径；省略时自动在构建目录中查找并复制。

.PARAMETER Build
    先执行 `cargo build -p rdm-native`（Auto 配置为 release），再安装。

.PARAMETER NoGui
    不复制 rdm-gui.exe（只安装原生宿主）。

.PARAMETER Force
    保留参数以兼容脚本化调用；安装始终覆盖旧文件。

.EXAMPLE
    pwsh -File scripts/install-firefox-host.ps1
    pwsh -File scripts/install-firefox-host.ps1 -Configuration Debug
    pwsh -File scripts/install-firefox-host.ps1 -Build -Configuration Release
#>
[CmdletBinding()]
param(
    [ValidateSet("Auto", "Debug", "Release")]
    [string]$Configuration = "Auto",
    [string]$NativeExe = "",
    [string]$GuiExe = "",
    [switch]$Build,
    [switch]$NoGui,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$HostName = "com.rdm.download"
$ExtensionId = "rdm@rdm.download"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$InstallDir = Join-Path $env:LOCALAPPDATA "RDM"
$InstalledExe = Join-Path $InstallDir "rdm-native.exe"
$ManifestPath = Join-Path $InstallDir "$HostName.json"
$RegistryKey = "HKCU:\Software\Mozilla\NativeMessagingHosts\$HostName"

function Write-Step($Message) { Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Ok($Message) { Write-Host "    $Message" -ForegroundColor Green }
function Write-Warn2($Message) { Write-Host "    $Message" -ForegroundColor Yellow }

# 读取 PE 子系统（2 = GUI，3 = 控制台）；用于提醒"双击会弹命令行窗口"的 debug 构建。
function Get-PeSubsystem([string]$Path) {
    try {
        $stream = [System.IO.File]::OpenRead($Path)
        try {
            $header = New-Object byte[] 1024
            $read = $stream.Read($header, 0, $header.Length)
            if ($read -lt 0x40) { return $null }
            $pe = [BitConverter]::ToInt32($header, 0x3C)
            if ($pe -le 0 -or ($pe + 0x5E) -gt $read) { return $null }
            return [BitConverter]::ToUInt16($header, $pe + 0x5C)
        } finally { $stream.Dispose() }
    } catch { return $null }
}

function Add-ToolchainPath {
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    if ((Test-Path -LiteralPath $cargoBin -PathType Container) -and ($env:PATH -notlike "*$cargoBin*")) {
        $env:PATH = "$cargoBin;$env:PATH"
    }
    if (-not (Get-Command gcc -ErrorAction SilentlyContinue)) {
        $wingetBase = Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages"
        if (Test-Path -LiteralPath $wingetBase -PathType Container) {
            $mingwBin = Get-ChildItem -LiteralPath $wingetBase -Directory -Filter "BrechtSanders.WinLibs.POSIX*" -ErrorAction SilentlyContinue |
                ForEach-Object { Join-Path $_.FullName "mingw64\bin" } |
                Where-Object { Test-Path -LiteralPath $_ -PathType Container } |
                Select-Object -First 1
            if ($mingwBin) { $env:PATH = "$mingwBin;$env:PATH" }
        }
    }
}

function Get-ConfigDirs {
    switch ($Configuration) {
        "Release" { return @("release") }
        "Debug" { return @("debug") }
        default { return @("release", "debug") }
    }
}

function Find-BuiltBinary([string]$Name, [string]$ExplicitPath) {
    if ($ExplicitPath) {
        if (Test-Path -LiteralPath $ExplicitPath -PathType Leaf) {
            return (Resolve-Path -LiteralPath $ExplicitPath).Path
        }
        throw "指定的文件不存在：$ExplicitPath"
    }
    $configDirs = Get-ConfigDirs
    $targetDirs = New-Object System.Collections.Generic.List[string]
    foreach ($configDir in $configDirs) {
        $targetDirs.Add((Join-Path $RepoRoot "target-native\$configDir"))
        $targetDirs.Add((Join-Path $RepoRoot "target\$configDir"))
        Get-ChildItem -LiteralPath $RepoRoot -Directory -Filter "target*" -ErrorAction SilentlyContinue |
            ForEach-Object { $targetDirs.Add((Join-Path $_.FullName $configDir)) }
    }
    foreach ($targetDir in $targetDirs) {
        $candidate = Join-Path $targetDir $Name
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    return $null
}

function Get-MingwBinDir {
    $gcc = Get-Command gcc -ErrorAction SilentlyContinue
    if ($gcc) { return (Split-Path -Parent $gcc.Source) }
    return $null
}

function Copy-MingwRuntimeDlls([string]$Destination, [string]$MingwBin) {
    if (-not $MingwBin) { return }
    $runtimeDlls = @("libgcc_s_seh-1.dll", "libwinpthread-1.dll", "libgcc_s_dw2-1.dll")
    foreach ($dll in $runtimeDlls) {
        $source = Join-Path $MingwBin $dll
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Copy-Item -LiteralPath $source -Destination (Join-Path $Destination $dll) -Force
            Write-Ok "复制运行库 $dll"
        }
    }
}

function Assert-ExtensionId {
    $extensionManifest = Join-Path $RepoRoot "extension\firefox\manifest.json"
    if (-not (Test-Path -LiteralPath $extensionManifest -PathType Leaf)) { return }
    try {
        $json = Get-Content -LiteralPath $extensionManifest -Raw -Encoding UTF8 | ConvertFrom-Json
        $id = $json.browser_specific_settings.gecko.id
        if ($id -and $id -ne $ExtensionId) {
            Write-Warn2 "警告：扩展 manifest 的 gecko.id=$id，与宿主 allowed_extensions 的 $ExtensionId 不一致"
        }
    } catch {
        Write-Warn2 "无法解析扩展 manifest.json：$($_.Exception.Message)"
    }
}

try {
    Write-Step "RDM Firefox 原生宿主安装（用户级，无需管理员）"
    Write-Host "    仓库：$RepoRoot"
    Write-Host "    安装目录：$InstallDir"

    if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot "Cargo.toml") -PathType Leaf)) {
        throw "未找到 Cargo.toml，请在 RDM 仓库内运行本脚本（脚本应位于 scripts/ 目录）。"
    }

    Add-ToolchainPath

    if ($Build) {
        Write-Step "构建 rdm-native（cargo build -p rdm-native）"
        $cargo = Get-Command cargo -ErrorAction SilentlyContinue
        if (-not $cargo) { throw "未找到 cargo，请先安装 Rust，或去掉 -Build 使用已构建的 exe。" }
        $env:CARGO_TARGET_DIR = Join-Path $RepoRoot "target-native"
        $cargoArgs = @("build", "-p", "rdm-native")
        if ((Get-ConfigDirs)[0] -eq "release") { $cargoArgs += "--release" }
        Push-Location $RepoRoot
        try {
            & $cargo.Source @cargoArgs
            if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（退出码 $LASTEXITCODE）" }
        } finally {
            Pop-Location
        }
        Write-Ok "构建完成 -> $env:CARGO_TARGET_DIR"
    }

    $nativeSource = Find-BuiltBinary -Name "rdm-native.exe" -ExplicitPath $NativeExe
    if (-not $nativeSource) {
        throw "未找到 rdm-native.exe。请先构建（cargo build -p rdm-native 或加 -Build），或用 -NativeExe 指定路径。"
    }
    Write-Ok "使用宿主：$nativeSource"

    if (-not (Test-Path -LiteralPath $InstallDir -PathType Container)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }

    Copy-Item -LiteralPath $nativeSource -Destination $InstalledExe -Force
    Write-Ok "已安装：$InstalledExe"

    # 复制 GUI（若已构建），让宿主能在同目录自动启动 RDM。
    if (-not $NoGui) {
        $guiSource = Find-BuiltBinary -Name "rdm-gui.exe" -ExplicitPath $GuiExe
        if ($guiSource) {
            $guiDest = Join-Path $InstallDir "rdm-gui.exe"
            Copy-Item -LiteralPath $guiSource -Destination $guiDest -Force
            Write-Ok "已复制 GUI：$guiSource -> $guiDest"
            if ((Get-PeSubsystem $guiDest) -eq 3) {
                Write-Warn2 "注意：rdm-gui.exe 是控制台子系统（debug 默认），双击会弹出命令行窗口。"
                Write-Warn2 "安装无控制台版本：cargo build -p rdm-gui --features windows-subsystem，再重新运行本脚本。"
                Write-Warn2 "无控制台构建的日志写到 %LOCALAPPDATA%\\RDM\\logs\\gui.log。"
            }
        } else {
            Write-Warn2 "未找到 rdm-gui.exe（GUI 可能尚未构建）：宿主会在需要时尝试启动它。"
        }
    }

    # 写 Firefox 原生宿主 manifest（路径必须是绝对路径；ConvertTo-Json 负责反斜杠转义）。
    #
    # 重要：Firefox 用扩展 schema 校验 native manifest，**不允许未知字段**
    # （Firefox 158.0.1 实测：manifest 里出现 Chrome 的 `allowed_origins` 会让 Firefox
    # 报 "Unexpected property ... allowed_origins"，connectNative 表现为
    # "No such native application"）。因此 Firefox 与 Chrome/Edge 必须使用两个独立
    # manifest 文件：Firefox 只用 allowed_extensions（本文件），Chrome/Edge 用
    # allowed_origins（com.rdm.download.chrome.json，见 install-chrome-host.ps1）。
    $manifest = [ordered]@{
        name               = $HostName
        description        = "RDM native messaging host (Firefox)"
        path               = $InstalledExe
        type               = "stdio"
        allowed_extensions = @($ExtensionId)
    }
    # 迁移旧版共用 manifest：若历史文件里带 allowed_origins（v0.18.0 安装器写入），
    # 先把它迁到独立的 Chrome/Edge manifest，避免 Firefox 继续解析失败。
    $chromeManifestPath = Join-Path $InstallDir "$HostName.chrome.json"
    if (Test-Path -LiteralPath $ManifestPath -PathType Leaf) {
        try {
            $existing = Get-Content -LiteralPath $ManifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
            $origins = @($existing.allowed_origins | Where-Object { $_ })
            if ($origins.Count -gt 0) {
                $chromeManifest = [ordered]@{
                    name            = $HostName
                    description     = "RDM native messaging host (Chrome/Edge)"
                    path            = $InstalledExe
                    type            = "stdio"
                    allowed_origins = $origins
                }
                $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
                [System.IO.File]::WriteAllText($chromeManifestPath, ($chromeManifest | ConvertTo-Json -Depth 5), $utf8NoBom)
                Write-Ok "已将旧 manifest 的 Chrome allowed_origins 迁移到：$chromeManifestPath"
                foreach ($chromeKey in @(
                    "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName",
                    "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\$HostName"
                )) {
                    if (-not (Test-Path -LiteralPath $chromeKey)) { New-Item -Path $chromeKey -Force | Out-Null }
                    Set-Item -Path $chromeKey -Value $chromeManifestPath
                }
                Write-Ok "已把 Chrome/Edge 注册表改指向独立 manifest"
                $backupPath = "$ManifestPath.bak-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
                Copy-Item -LiteralPath $ManifestPath -Destination $backupPath -Force
                Write-Ok "旧 manifest 已备份：$backupPath"
            }
        } catch {
            Write-Warn2 "已有宿主 manifest 无法解析，将重新写入：$($_.Exception.Message)"
        }
    }
    $manifestJson = $manifest | ConvertTo-Json -Depth 5
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($ManifestPath, $manifestJson, $utf8NoBom)
    Write-Ok "已写入 Firefox 宿主 manifest：$ManifestPath"

    # 注册表：Firefox 只读取 manifest 的绝对路径。
    if (-not (Test-Path -LiteralPath $RegistryKey)) {
        New-Item -Path $RegistryKey -Force | Out-Null
    }
    Set-Item -Path $RegistryKey -Value $ManifestPath
    $registered = (Get-Item -LiteralPath $RegistryKey).GetValue("")
    if ($registered -ne $ManifestPath) {
        throw "注册表写入校验失败：期望 $ManifestPath，实际 $registered"
    }
    Write-Ok "已注册：$RegistryKey"
    Write-Ok "          默认值 = $registered"

    Assert-ExtensionId

    # 冒烟检查：能运行说明 exe 及其 DLL 依赖在当前目录可解析。
    Write-Step "冒烟检查原生宿主"
    $versionOutput = & $InstalledExe --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        # GNU 工具链产物偶尔依赖 MinGW 运行库；仅在需要时补到安装目录。
        Write-Warn2 "宿主无法直接运行，尝试补齐 MinGW 运行库…"
        Copy-MingwRuntimeDlls -Destination $InstallDir -MingwBin (Get-MingwBinDir)
        $versionOutput = & $InstalledExe --version 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "rdm-native.exe 无法运行（退出码 $LASTEXITCODE）：$versionOutput"
        }
    }
    Write-Ok "版本输出：$versionOutput"

    $firefox = Get-Process -Name "firefox" -ErrorAction SilentlyContinue
    if ($firefox) {
        Write-Warn2 "检测到 Firefox 正在运行：建议重启 Firefox 后再在 about:debugging 载入扩展。"
    }

    Write-Step "安装完成"
    Write-Host @"

下一步：
  1. 打开 Firefox -> about:debugging#/runtime/this-firefox
  2. 点击「临时载入附加组件…」，选择：
       $(Join-Path $RepoRoot "extension\firefox\manifest.json")
  3. 点击工具栏 RDM 图标，确认「原生宿主：已连接」；
     若显示未安装，重启 Firefox 后重试。

卸载：
  pwsh -File scripts/uninstall-firefox-host.ps1
"@
}
catch {
    Write-Host ""
    Write-Host "安装失败：$($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
