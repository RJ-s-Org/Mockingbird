# Installs the mockingbird binary (Windows).
#
#   irm https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.ps1 | iex
#
# Environment:
#   MOCKINGBIRD_VERSION      release tag to install (default: the latest release)
#   MOCKINGBIRD_INSTALL_DIR  where the binary goes (default: ~\.mockingbird\bin)
#
# Downloads the release zip, checks its SHA-256, installs mockingbird.exe and
# adds the directory to the user PATH. Falls back to `cargo install` when no
# release fits and cargo is available.
$ErrorActionPreference = 'Stop'
$Repo = 'rj-s-org/mockingbird'
$InstallDir = if ($env:MOCKINGBIRD_INSTALL_DIR) { $env:MOCKINGBIRD_INSTALL_DIR } else { Join-Path $HOME '.mockingbird\bin' }
$Target = 'x86_64-pc-windows-msvc'

function Say($msg) { Write-Host "mockingbird-install: $msg" }

function Install-Release {
    $tag = $env:MOCKINGBIRD_VERSION
    if (-not $tag) {
        try { $tag = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest").tag_name } catch { $tag = $null }
    }
    if (-not $tag) {
        # No final release yet: the newest prerelease (releases/latest skips them).
        try { $tag = @(Invoke-RestMethod "https://api.github.com/repos/$Repo/releases?per_page=1")[0].tag_name } catch { $tag = $null }
    }
    if (-not $tag) { Say 'no published release found'; return $false }
    $version = $tag.TrimStart('v')
    $name = "mockingbird-$version-$Target"
    $base = "https://github.com/$Repo/releases/download/$tag"
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Say "downloading $name ($tag)"
        Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip" -UseBasicParsing
        try {
            Invoke-WebRequest "$base/$name.zip.sha256" -OutFile "$tmp\sum" -UseBasicParsing
            $want = (Get-Content "$tmp\sum" -Raw).Split(' ')[0].Trim().ToLower()
            $got = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
            if ($want -ne $got) { throw "checksum mismatch for $name.zip" }
            Say 'checksum ok'
        } catch [System.Net.WebException] { }
        Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        Copy-Item "$tmp\$name\mockingbird.exe" (Join-Path $InstallDir 'mockingbird.exe') -Force
        return $true
    } catch {
        Say "release install failed: $_"
        return $false
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Install-Source {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { return $false }
    Say 'building from source with cargo (a few minutes)'
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid())
    $cargoArgs = @('install', '--locked', '--git', "https://github.com/$Repo", '--root', $tmp, 'mockingbird-cli')
    if ($env:MOCKINGBIRD_VERSION) { $cargoArgs += @('--tag', $env:MOCKINGBIRD_VERSION) }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { return $false }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item "$tmp\bin\mockingbird.exe" (Join-Path $InstallDir 'mockingbird.exe') -Force
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    return $true
}

if (-not (Install-Release)) {
    if (-not (Install-Source)) {
        throw "could not install: no release binary and no cargo. Install Rust (https://rustup.rs) and rerun, or use Docker."
    }
}

$exe = Join-Path $InstallDir 'mockingbird.exe'
& $exe --version
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable('Path', "$InstallDir;$userPath", 'User')
    Say "added $InstallDir to your user PATH (open a new terminal)"
}
Say 'next: mockingbird agent install <claude-code|codex|cursor|gemini|kiro|vscode|windsurf|claude-desktop>'
Write-Output $exe
