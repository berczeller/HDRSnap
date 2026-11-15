# Build both release packages into dist\:
#   HDRSnap-<version>-portable.zip   unzip anywhere, keeps everything in its folder
#   HDRSnap-<version>-setup.exe      Windows installer (per user, no admin needed)
#
# Needs: Rust (MSVC), Inno Setup 6 (winget install JRSoftware.InnoSetup).
# ffmpeg.exe is taken from vendor\ffmpeg\ (downloaded and checked on first run).

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$FfmpegUrl = "https://github.com/GyanD/codexffmpeg/releases/download/7.1.1/ffmpeg-7.1.1-essentials_build.zip"
$FfmpegSha256 = "04861d3339c5ebe38b56c19a15cf2c0cc97f5de4fa8910e4d47e5e6404e4a2d4"

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
Write-Host "HDRSnap $version"

# --- ffmpeg ---------------------------------------------------------------
$ff = "vendor\ffmpeg\ffmpeg.exe"
if (-not (Test-Path $ff)) {
    $tmp = Join-Path $env:TEMP "hdrsnap-ffmpeg-$([guid]::NewGuid())"
    New-Item -ItemType Directory $tmp | Out-Null
    Write-Host "Downloading $FfmpegUrl"
    Invoke-WebRequest $FfmpegUrl -OutFile "$tmp\ff.zip"
    $hash = (Get-FileHash "$tmp\ff.zip" -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $FfmpegSha256) { throw "ffmpeg download hash mismatch: $hash" }
    Expand-Archive "$tmp\ff.zip" "$tmp\x"
    New-Item -ItemType Directory -Force vendor\ffmpeg | Out-Null
    Copy-Item (Get-ChildItem "$tmp\x\*\bin\ffmpeg.exe").FullName $ff
    Remove-Item -Recurse -Force $tmp
}

# --- build + tests --------------------------------------------------------
cargo test --lib
if ($LASTEXITCODE) { throw "tests failed" }
cargo build --release
if ($LASTEXITCODE) { throw "build failed" }

# --- stage ----------------------------------------------------------------
$stage = "dist\stage"
if (Test-Path dist) { Remove-Item -Recurse -Force dist }
New-Item -ItemType Directory $stage | Out-Null
Copy-Item target\release\HDRSnap.exe, $ff, README.md, LICENSE, FFMPEG-LICENSE.txt $stage

# --- portable zip (top-level folder inside the zip) -----------------------
$portable = "dist\HDRSnap-$version"
Copy-Item -Recurse $stage $portable
Compress-Archive -Path $portable -DestinationPath "dist\HDRSnap-$version-portable.zip"
Remove-Item -Recurse -Force $portable

# --- installer ------------------------------------------------------------
$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found (winget install JRSoftware.InnoSetup)" }
& $iscc /Q "/DAppVersion=$version" "/DStage=$PSScriptRoot\$stage" "/DOutDir=$PSScriptRoot\dist" installer\HDRSnap.iss
if ($LASTEXITCODE) { throw "installer build failed" }

Remove-Item -Recurse -Force $stage
Get-ChildItem dist | ForEach-Object { "{0,-36} {1,8:N1} MB" -f $_.Name, ($_.Length / 1MB) }
