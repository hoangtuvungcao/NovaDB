[CmdletBinding()]
param(
    [string]$Repository = $env:NOVADB_GITHUB_REPOSITORY,
    [string]$Version = "latest",
    [string]$InstallDir = $env:NOVADB_INSTALL_DIR,
    [switch]$NoPathUpdate
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Repository) -or $Repository -notmatch '^[^/]+/[^/]+$') {
    throw "Pass -Repository OWNER/REPOSITORY (this source tree has no canonical release repository yet)."
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        $InstallDir = Join-Path $HOME ".local\bin"
    } else {
        $InstallDir = Join-Path $env:LOCALAPPDATA "NovaDB\bin"
    }
}

$architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
switch ($architecture) {
    "X64" { $platform = "windows-x86_64" }
    "Arm64" { $platform = "windows-aarch64" }
    default { throw "Unsupported CPU architecture: $architecture" }
}

$archive = "novadb-$platform.zip"
if ($Version -eq "latest") {
    $releaseBase = "https://github.com/$Repository/releases/latest/download"
} else {
    $releaseBase = "https://github.com/$Repository/releases/download/$Version"
}

$tempDir = Join-Path ([IO.Path]::GetTempPath()) ("novadb-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tempDir | Out-Null

try {
    $archivePath = Join-Path $tempDir $archive
    $checksumsPath = Join-Path $tempDir "SHA256SUMS"
    Write-Host "Downloading $archive ..."
    Invoke-WebRequest -Uri "$releaseBase/$archive" -OutFile $archivePath
    Invoke-WebRequest -Uri "$releaseBase/SHA256SUMS" -OutFile $checksumsPath

    $escapedArchive = [regex]::Escape($archive)
    $checksumPattern = "^[0-9a-fA-F]{64}\s+\*?${escapedArchive}$"
    $checksumLine = Get-Content $checksumsPath | Where-Object { $_ -match $checksumPattern } | Select-Object -First 1
    if (-not $checksumLine) {
        throw "Checksum for $archive is missing."
    }
    $expected = ($checksumLine -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "Checksum verification failed."
    }

    Expand-Archive -Path $archivePath -DestinationPath $tempDir
    $packageDir = Join-Path $tempDir "novadb-$platform"
    $client = Join-Path $packageDir "novadb.exe"
    $server = Join-Path $packageDir "novadbd.exe"
    if (-not (Test-Path $client) -or -not (Test-Path $server)) {
        throw "Archive does not contain novadb.exe and novadbd.exe."
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Force $client $InstallDir
    Copy-Item -Force $server $InstallDir

    if (-not $NoPathUpdate) {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $pathEntries = @($userPath -split ';' | Where-Object { $_ })
        if ($pathEntries -notcontains $InstallDir) {
            $newPath = (@($pathEntries) + $InstallDir) -join ';'
            [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
            Write-Host "Added $InstallDir to your user PATH; open a new terminal to use it."
        }
    }

    Write-Host "Installed novadb.exe and novadbd.exe into $InstallDir"
} finally {
    if (Test-Path $tempDir) {
        Remove-Item -Recurse -Force $tempDir
    }
}
