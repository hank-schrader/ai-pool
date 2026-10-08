# Install ai-pool (pool-miner, pool-server, clef-token-count) from a GitHub release
# for the current user. Windows x86_64.
#
#   irm https://raw.githubusercontent.com/hank-schrader/ai-pool/main/scripts/install.ps1 | iex
#   .\install.ps1 -Version v0.1.0
#
# Installs into %LOCALAPPDATA%\ai-pool\<version> and adds it to the user PATH.
# Nothing is installed system-wide; GPU drivers are not touched.
param(
    [string]$Version = "latest",
    [string]$Dir = (Join-Path $env:LOCALAPPDATA "ai-pool")
)
$ErrorActionPreference = "Stop"
$repo = "hank-schrader/ai-pool"

if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") { throw "unsupported architecture $env:PROCESSOR_ARCHITECTURE (supported: x86_64)" }
$target = "x86_64-pc-windows-msvc"

if ($Version -eq "latest") {
    $Version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
$name = "ai-pool-$Version-$target"
$base = "https://github.com/$repo/releases/download/$Version"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    Write-Host "downloading $name"
    Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip"
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
    $expected = (Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match " $([regex]::Escape("$name.zip"))$" }) -split " " | Select-Object -First 1
    $actual = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
    if (-not $expected -or $expected -ne $actual) { throw "checksum mismatch for $name.zip" }

    $dest = Join-Path $Dir $Version
    New-Item -ItemType Directory -Force $dest | Out-Null
    Expand-Archive "$tmp\$name.zip" -DestinationPath $dest -Force
    $installDir = Join-Path $dest $name

    $path = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($path -split ";") -notcontains $installDir) {
        [Environment]::SetEnvironmentVariable("Path", "$installDir;$path", "User")
        Write-Host "added $installDir to your user PATH (open a new terminal)"
    }
    Write-Host "installed $Version to $installDir"
    Write-Host "next: pool-miner --pool http://<pool-host>:8080"
} finally {
    Remove-Item -Recurse -Force $tmp
}
