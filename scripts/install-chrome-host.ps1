#Requires -Version 5.1
<#
.SYNOPSIS
    安装 RDM Chrome 原生消息宿主（当前用户，无需管理员权限）。

.DESCRIPTION
    1. 定位（或使用 -Build 构建）rdm-native.exe，复制到
       %LOCALAPPDATA%\RDM\rdm-native.exe（有 rdm-gui.exe 时一并复制，便于自动启动）；
    2. 生成/更新 %LOCALAPPDATA%\RDM\com.rdm.download.json：
       allowed_origins 使用 chrome-extension://<id>/；
       若该文件同时被 Firefox 使用，会保留已有的 allowed_extensions（反之亦然），
       因此 Firefox 与 Chrome 可以共用同一个宿主 manifest；
    3. 注册 HKCU\Software\Google\Chrome\NativeMessagingHosts\com.rdm.download
       默认值为 manifest 绝对路径。

    Chrome 扩展 ID 的获取：
      * 未打包加载（dist\rdm-chrome）时，ID 由扩展目录路径派生；在
        chrome://extensions 里复制，然后用 -ExtensionId 传入；
      * 若 extension/chrome/manifest.json 含 "key"，本脚本会自动推导（推荐，
        见 extension/chrome/README.md）。
    只写 HKCU 与当前用户目录，不需要管理员权限。

.PARAMETER ExtensionId
    Chrome 扩展 ID（32 位 a-p 字母），也接受 chrome-extension://<id>/ 形式。

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

.PARAMETER DryRun
    只解析扩展 ID / 查找宿主 exe 并打印计划，不复制文件、不写注册表（用于自检）。

.PARAMETER Force
    保留参数以兼容脚本化调用；安装始终覆盖旧文件。

.EXAMPLE
    pwsh -File scripts/install-chrome-host.ps1 -ExtensionId abcdefghijklmnopabcdefghijklmnop

.EXAMPLE
    pwsh -File scripts/install-chrome-host.ps1 -Build -Configuration Release -ExtensionId <id>
#>
[CmdletBinding()]
param(
    [string]$ExtensionId = "",
    [ValidateSet("Auto", "Debug", "Release")]
    [string]$Configuration = "Auto",
    [string]$NativeExe = "",
    [string]$GuiExe = "",
    [switch]$Build,
    [switch]$NoGui,
    [switch]$DryRun,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$HostName = "com.rdm.download"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$InstallDir = Join-Path $env:LOCALAPPDATA "RDM"
$InstalledExe = Join-Path $InstallDir "rdm-native.exe"
$ManifestPath = Join-Path $InstallDir "$HostName.json"
$RegistryKey = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName"
$ChromeExtensionManifest = Join-Path $RepoRoot "extension\chrome\manifest.json"

function Write-Step($Message) { Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Ok($Message) { Write-Host "    $Message" -ForegroundColor Green }
function Write-Warn2($Message) { Write-Host "    $Message" -ForegroundColor Yellow }

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

function Get-ExtensionIdFromKey([string]$Base64Key) {
    try {
        $der = [Convert]::FromBase64String($Base64Key.Trim())
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try {
            $hash = $sha.ComputeHash($der)
        } finally {
            $sha.Dispose()
        }
        $chars = New-Object System.Text.StringBuilder
        for ($i = 0; $i -lt 16; $i++) {
            $byte = $hash[$i]
            [void]$chars.Append([char]([int][char]'a' + ($byte -shr 4)))
            [void]$chars.Append([char]([int][char]'a' + ($byte -band 0x0F)))
        }
        return $chars.ToString()
    } catch {
        Write-Warn2 "无法从 manifest key 推导扩展 ID：$($_.Exception.Message)"
        return ""
    }
}

function Resolve-ExtensionId([string]$Raw) {
    $candidate = ""
    if ($Raw) {
        $candidate = $Raw.Trim()
        $prefix = "chrome-extension://"
        if ($candidate.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
            $candidate = $candidate.Substring($prefix.Length)
        }
        $candidate = ($candidate.Trim('/').Split('/')[0]).Trim().ToLowerInvariant()
    }
    if (-not $candidate -and (Test-Path -LiteralPath $ChromeExtensionManifest -PathType Leaf)) {
        try {
            $json = Get-Content -LiteralPath $ChromeExtensionManifest -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($json.key) {
                $candidate = Get-ExtensionIdFromKey ([string]$json.key)
                if ($candidate) {
                    Write-Ok "从 extension/chrome/manifest.json 的 key 推导扩展 ID：$candidate"
                }
            }
        } catch {
            Write-Warn2 "无法解析 extension/chrome/manifest.json：$($_.Exception.Message)"
        }
    }
    if (-not $candidate) {
        throw @"
未提供 Chrome 扩展 ID。请先：
  1. 构建扩展：pwsh -File scripts/build-chrome-extension.ps1
  2. 打开 chrome://extensions，开启「开发者模式」；
  3. 「加载已解压的扩展程序」选择 dist\rdm-chrome；
  4. 复制该扩展的 32 位 ID（a-p 字母），然后重新运行：
       pwsh -File scripts/install-chrome-host.ps1 -ExtensionId <id>
  或者把扩展的 "key" 写入 extension/chrome/manifest.json（稳定 ID，推荐）。
"@
    }
    if ($candidate -notmatch '^[a-p]{32}$') {
        throw "Chrome 扩展 ID 格式非法（应为 32 位 a-p 字母）：$candidate"
    }
    return $candidate
}

try {
    Write-Step "RDM Chrome 原生宿主安装（用户级，无需管理员）"
    Write-Host "    仓库：$RepoRoot"
    Write-Host "    安装目录：$InstallDir"

    if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot "Cargo.toml") -PathType Leaf)) {
        throw "未找到 Cargo.toml，请在 RDM 仓库内运行本脚本（脚本应位于 scripts/ 目录）。"
    }

    Add-ToolchainPath
    $resolvedExtensionId = Resolve-ExtensionId $ExtensionId
    Write-Ok "Chrome 扩展 ID：$resolvedExtensionId"
    if ($DryRun) {
        Write-Step "DryRun：只做检查，不复制文件、不写注册表"
    }

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
        if (-not $DryRun) {
            New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        }
    }

    if ($DryRun) {
        Write-Ok "DryRun：将安装 $nativeSource -> $InstalledExe"
    } else {
        Copy-Item -LiteralPath $nativeSource -Destination $InstalledExe -Force
        Write-Ok "已安装：$InstalledExe"
    }

    if (-not $NoGui) {
        $guiSource = Find-BuiltBinary -Name "rdm-gui.exe" -ExplicitPath $GuiExe
        if ($guiSource) {
            if ($DryRun) {
                Write-Ok "DryRun：将复制 GUI $guiSource -> $InstallDir"
            } else {
                Copy-Item -LiteralPath $guiSource -Destination (Join-Path $InstallDir "rdm-gui.exe") -Force
                Write-Ok "已复制 GUI：$guiSource"
            }
        } else {
            Write-Warn2 "未找到 rdm-gui.exe（GUI 可能尚未构建）：宿主会在需要时尝试启动它。"
        }
    }

    # 与 Firefox 共用同一个宿主 manifest 文件：尽量同时保留 allowed_origins
    # 与 allowed_extensions，避免后装的浏览器覆盖先装的配置。
    $origins = New-Object System.Collections.Generic.List[string]
    $origins.Add("chrome-extension://$resolvedExtensionId/")
    $extensions = New-Object System.Collections.Generic.List[string]
    if (Test-Path -LiteralPath $ManifestPath -PathType Leaf) {
        try {
            $existing = Get-Content -LiteralPath $ManifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
            foreach ($origin in @($existing.allowed_origins | Where-Object { $_ })) {
                if (-not $origins.Contains([string]$origin)) { $origins.Add([string]$origin) }
            }
            foreach ($extension in @($existing.allowed_extensions | Where-Object { $_ })) {
                if (-not $extensions.Contains([string]$extension)) { $extensions.Add([string]$extension) }
            }
        } catch {
            Write-Warn2 "已有宿主 manifest 无法解析，将重新写入：$($_.Exception.Message)"
        }
    }
    $manifest = [ordered]@{
        name            = $HostName
        description     = "RDM native messaging host"
        path            = $InstalledExe
        type            = "stdio"
        allowed_origins = @($origins)
    }
    if ($extensions.Count -gt 0) {
        $manifest.allowed_extensions = @($extensions)
        Write-Ok "保留 Firefox allowed_extensions：$($extensions -join ', ')"
    }
    if ($DryRun) {
        Write-Ok "DryRun：将写入宿主 manifest：$ManifestPath"
        Write-Ok "DryRun：allowed_origins = $($origins -join ', ')"
        if ($extensions.Count -gt 0) {
            Write-Ok "DryRun：保留 allowed_extensions = $($extensions -join ', ')"
        }
        Write-Ok "DryRun：将注册 $RegistryKey"
    } else {
        $manifestJson = $manifest | ConvertTo-Json -Depth 5
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText($ManifestPath, $manifestJson, $utf8NoBom)
        Write-Ok "已写入宿主 manifest：$ManifestPath"

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

        Write-Step "冒烟检查原生宿主"
        $versionOutput = & $InstalledExe --version 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Warn2 "宿主无法直接运行，尝试补齐 MinGW 运行库…"
            Copy-MingwRuntimeDlls -Destination $InstallDir -MingwBin (Get-MingwBinDir)
            $versionOutput = & $InstalledExe --version 2>&1
            if ($LASTEXITCODE -ne 0) {
                throw "rdm-native.exe 无法运行（退出码 $LASTEXITCODE）：$versionOutput"
            }
        }
        Write-Ok "版本输出：$versionOutput"
    }

    $chrome = Get-Process -Name "chrome" -ErrorAction SilentlyContinue
    if ($chrome) {
        Write-Warn2 "检测到 Chrome 正在运行：如扩展仍显示宿主未安装，请完全退出并重启 Chrome。"
    }

    Write-Step "安装完成"
    Write-Host @"

下一步：
  1. 打开 Chrome -> chrome://extensions，开启「开发者模式」；
  2. 「加载已解压的扩展程序」选择：
       $(Join-Path $RepoRoot "dist\rdm-chrome")
     （若尚未构建：pwsh -File scripts/build-chrome-extension.ps1）
  3. 确认扩展 ID 与本宿主 allowed_origins 一致（当前注册）：
       $(($origins)[0])
  4. 点击工具栏 RDM 图标，状态应显示「原生宿主：已连接」。
  5. Chrome 商店版需要稳定扩展 ID：把 chrome://extensions 中的 "key"
     写入 extension/chrome/manifest.json 后重新构建（见 extension/chrome/README.md）。

卸载（手动）：
  Remove-Item -LiteralPath "$RegistryKey" -Recurse -Force
  Remove-Item -LiteralPath "$ManifestPath" -Force
  # 同时使用 Firefox 宿主时请勿删除 rdm-native.exe / allowed_extensions
"@
} catch {
    Write-Host ""
    Write-Host "安装失败：$($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
