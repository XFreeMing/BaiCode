# Install a locally built BaiCode CLI for the current user.
param([Parameter(Mandatory=$true, Position=0)][string]$Binary)
$ErrorActionPreference = "Stop"
$source = (Get-Item -LiteralPath $Binary).FullName
$version = & $source --version
if ($LASTEXITCODE -ne 0 -or "$version" -notmatch '^baic ') {
    throw "The supplied binary did not identify itself as baic."
}
$dir = if ($env:BAICODE_INSTALL_DIR) { $env:BAICODE_INSTALL_DIR } else {
    Join-Path $env:LOCALAPPDATA "Programs\BaiCode\bin"
}
[IO.Directory]::CreateDirectory($dir) | Out-Null
$dest = Join-Path $dir "baic.exe"
$tmp = Join-Path $dir ([guid]::NewGuid().ToString() + ".tmp")
try {
    [IO.File]::Copy($source, $tmp)
    if ([IO.File]::Exists($dest)) {
        [IO.File]::Replace($tmp, $dest, $null)
    } else {
        [IO.File]::Move($tmp, $dest)
    }
} finally {
    if ([IO.File]::Exists($tmp)) { [IO.File]::Delete($tmp) }
}
Write-Output "Installed $version at $dest"
