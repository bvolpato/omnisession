[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $BinaryPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$projectRoot = Split-Path -Parent $PSScriptRoot
$binary = (Get-Item -LiteralPath $BinaryPath -Force).FullName
$reported = (& $binary --version 2>$null | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $reported -notmatch '^omni ([0-9]+\.[0-9]+\.[0-9]+)$') {
    throw "Fixture binary did not report a valid OmniSession version"
}
$version = $Matches[1]

$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("omni-windows-install-test-" + [guid]::NewGuid().ToString("N"))
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
$originalInstallDirectory = [Environment]::GetEnvironmentVariable("OMNI_INSTALL_DIR", "Process")
$originalNoModifyPath = [Environment]::GetEnvironmentVariable("OMNI_NO_MODIFY_PATH", "Process")
$originalProcessPath = $env:Path
$originalUserPath = [Environment]::GetEnvironmentVariable("Path", "User")

function Write-ChecksumFixture {
    param(
        [string] $Archive,
        [string] $Destination,
        [int] $Copies = 1
    )

    $hash = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
    $lines = for ($index = 0; $index -lt $Copies; $index += 1) {
        "$hash  omni-windows-x86_64.zip"
    }
    [IO.File]::WriteAllLines($Destination, $lines)
}

$fixtureState = @{
    Archive = $null
    Checksums = $null
    ContentLengthOverride = $null
    FinalUri = $null
}

$downloadFactory = {
    param([string] $Uri)

    $source = if ($Uri.EndsWith("/SHA256SUMS", [StringComparison]::Ordinal)) {
        $fixtureState.Checksums
    } elseif ($Uri.EndsWith("/omni-windows-x86_64.zip", [StringComparison]::Ordinal)) {
        $fixtureState.Archive
    } else {
        throw "Unexpected installer URL: $Uri"
    }
    $stream = [IO.File]::OpenRead($source)
    $contentLength = if ($null -eq $fixtureState.ContentLengthOverride) {
        $stream.Length
    } else {
        $fixtureState.ContentLengthOverride
    }
    $finalUri = if ($null -eq $fixtureState.FinalUri) { [Uri] $Uri } else { [Uri] $fixtureState.FinalUri }
    return @{
        ContentLength = $contentLength
        FinalUri = $finalUri
        Response = $null
        Stream = $stream
    }
}

function Invoke-Installer {
    param(
        [string] $InstallDirectory,
        [string] $Archive,
        [string] $Checksums,
        [scriptblock] $PathAction = $null,
        [switch] $PassThru
    )

    $fixtureState.Archive = $Archive
    $fixtureState.Checksums = $Checksums
    [Environment]::SetEnvironmentVariable("OMNI_INSTALL_DIR", $InstallDirectory, "Process")
    [Environment]::SetEnvironmentVariable("OMNI_NO_MODIFY_PATH", "1", "Process")
    $arguments = @{
        InstallerDownloadFactory = $downloadFactory
    }
    if ($null -ne $PathAction) {
        $arguments.InstallerPathAction = $PathAction
    }
    $output = & (Join-Path $projectRoot "install.ps1") @arguments
    if ($PassThru) {
        return $output
    }
}

function Assert-Failure {
    param(
        [scriptblock] $Action,
        [string] $Label,
        [string] $ExpectedMessage = $null
    )

    try {
        & $Action
    } catch {
        if ($null -ne $ExpectedMessage -and -not $_.Exception.Message.Contains($ExpectedMessage)) {
            throw "Expected failure '$ExpectedMessage' for ${Label}; got: $($_.Exception.Message)"
        }
        return
    }
    throw "Expected failure: $Label"
}

try {
    $fixtureDirectory = Join-Path $temporaryDirectory "fixture"
    [IO.Directory]::CreateDirectory($fixtureDirectory) | Out-Null
    $goodArchive = Join-Path $fixtureDirectory "omni-windows-x86_64.zip"
    & (Join-Path $projectRoot "scripts\package-windows.ps1") `
        -BinaryPath $binary `
        -LicensePath (Join-Path $projectRoot "LICENSE") `
        -OutputPath $goodArchive `
        -ExpectedVersion $version | Out-Null
    $goodChecksums = Join-Path $fixtureDirectory "SHA256SUMS"
    Write-ChecksumFixture -Archive $goodArchive -Destination $goodChecksums

    $installDirectory = Join-Path $temporaryDirectory "installed"
    Invoke-Installer -InstallDirectory $installDirectory -Archive $goodArchive -Checksums $goodChecksums
    $installed = Join-Path $installDirectory "omni.exe"
    if (-not [IO.File]::Exists($installed)) {
        throw "Installer did not publish omni.exe"
    }
    $installedVersion = (& $installed --version 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $installedVersion -ne "omni $version") {
        throw "Installed binary reported an unexpected version"
    }
    if ([IO.File]::ReadAllText((Join-Path $installDirectory ".omnisession-installer")).Trim() -ne
        "omnisession-windows-installer-v1") {
        throw "Installer did not write ownership marker"
    }

    Invoke-Installer -InstallDirectory $installDirectory -Archive $goodArchive -Checksums $goodChecksums
    $stagedFiles = @(Get-ChildItem -LiteralPath $installDirectory -Force -Filter ".omni-*.exe")
    if ($stagedFiles.Count -ne 0) {
        throw "Installer left staged or backup executables"
    }

    $foreignDirectory = Join-Path $temporaryDirectory "foreign"
    [IO.Directory]::CreateDirectory($foreignDirectory) | Out-Null
    [IO.File]::Copy($binary, (Join-Path $foreignDirectory "omni.exe"))
    Assert-Failure -Label "foreign existing binary" -Action {
        Invoke-Installer -InstallDirectory $foreignDirectory -Archive $goodArchive -Checksums $goodChecksums
    }

    Assert-Failure -Label "relative install directory" -ExpectedMessage "must be an absolute path" -Action {
        Invoke-Installer -InstallDirectory "relative\bin" -Archive $goodArchive -Checksums $goodChecksums
    }
    Assert-Failure -Label "drive-relative install directory" -ExpectedMessage "must be an absolute path" -Action {
        Invoke-Installer -InstallDirectory "C:relative\bin" -Archive $goodArchive -Checksums $goodChecksums
    }
    Assert-Failure -Label "current-drive-relative install directory" -ExpectedMessage "must be an absolute path" -Action {
        Invoke-Installer -InstallDirectory "\relative\bin" -Archive $goodArchive -Checksums $goodChecksums
    }

    $junctionTarget = Join-Path $temporaryDirectory "junction-target"
    $junction = Join-Path $temporaryDirectory "junction"
    [IO.Directory]::CreateDirectory($junctionTarget) | Out-Null
    New-Item -ItemType Junction -Path $junction -Target $junctionTarget | Out-Null
    Assert-Failure -Label "reparse point install directory" -Action {
        Invoke-Installer -InstallDirectory (Join-Path $junction "bin") -Archive $goodArchive -Checksums $goodChecksums
    }

    $fixtureState.FinalUri = "http://fixtures.invalid/omni-windows-x86_64.zip"
    try {
        $redirectDirectory = Join-Path $temporaryDirectory "insecure-redirect"
        Assert-Failure -Label "non-HTTPS redirect" -ExpectedMessage "redirected to a non-HTTPS URL" -Action {
            Invoke-Installer -InstallDirectory $redirectDirectory -Archive $goodArchive -Checksums $goodChecksums
        }
    } finally {
        $fixtureState.FinalUri = $null
    }

    $badChecksums = Join-Path $fixtureDirectory "BAD-SHA256SUMS"
    [IO.File]::WriteAllText($badChecksums, ("0" * 64) + "  omni-windows-x86_64.zip`n")
    $badDirectory = Join-Path $temporaryDirectory "bad-checksum"
    Assert-Failure -Label "wrong checksum" -Action {
        Invoke-Installer -InstallDirectory $badDirectory -Archive $goodArchive -Checksums $badChecksums
    }
    if ([IO.File]::Exists((Join-Path $badDirectory "omni.exe"))) {
        throw "Installer wrote omni.exe before checksum verification"
    }

    $duplicateChecksums = Join-Path $fixtureDirectory "DUPLICATE-SHA256SUMS"
    Write-ChecksumFixture -Archive $goodArchive -Destination $duplicateChecksums -Copies 2
    $duplicateDirectory = Join-Path $temporaryDirectory "duplicate-checksum"
    Assert-Failure -Label "duplicate checksum" -Action {
        Invoke-Installer -InstallDirectory $duplicateDirectory -Archive $goodArchive -Checksums $duplicateChecksums
    }
    if ([IO.File]::Exists((Join-Path $duplicateDirectory "omni.exe"))) {
        throw "Installer accepted duplicate checksums"
    }

    $unexpectedArchive = Join-Path $fixtureDirectory "unexpected.zip"
    [IO.File]::Copy($goodArchive, $unexpectedArchive)
    $archive = [IO.Compression.ZipFile]::Open($unexpectedArchive, [IO.Compression.ZipArchiveMode]::Update)
    try {
        $entry = $archive.CreateEntry("unexpected.txt")
        $writer = New-Object IO.StreamWriter($entry.Open())
        try {
            $writer.Write("unexpected")
        } finally {
            $writer.Dispose()
        }
    } finally {
        $archive.Dispose()
    }
    $unexpectedChecksums = Join-Path $fixtureDirectory "UNEXPECTED-SHA256SUMS"
    Write-ChecksumFixture -Archive $unexpectedArchive -Destination $unexpectedChecksums
    $unexpectedDirectory = Join-Path $temporaryDirectory "unexpected-layout"
    Assert-Failure -Label "unexpected archive layout" -Action {
        Invoke-Installer -InstallDirectory $unexpectedDirectory -Archive $unexpectedArchive -Checksums $unexpectedChecksums
    }
    if ([IO.File]::Exists((Join-Path $unexpectedDirectory "omni.exe"))) {
        throw "Installer accepted unexpected archive entries"
    }

    $oversizedChecksums = Join-Path $fixtureDirectory "OVERSIZED-SHA256SUMS"
    [IO.File]::WriteAllBytes($oversizedChecksums, (New-Object byte[] (64KB + 1)))
    $oversizedDirectory = Join-Path $temporaryDirectory "oversized-checksums"
    $fixtureState.ContentLengthOverride = -1
    try {
        Assert-Failure -Label "streamed oversized checksum document" -ExpectedMessage "exceeds safe size" -Action {
            Invoke-Installer -InstallDirectory $oversizedDirectory -Archive $goodArchive -Checksums $oversizedChecksums
        }
    } finally {
        $fixtureState.ContentLengthOverride = $null
    }

    $pathFailureDirectory = Join-Path $temporaryDirectory "path-setup-failure"
    $pathFailureOutput = Invoke-Installer `
        -InstallDirectory $pathFailureDirectory `
        -Archive $goodArchive `
        -Checksums $goodChecksums `
        -PathAction { param([string] $Directory); throw "fixture PATH failure for $Directory" } `
        -PassThru 3>&1 | Out-String
    if (-not [IO.File]::Exists((Join-Path $pathFailureDirectory "omni.exe"))) {
        throw "Installer rolled back a valid binary after optional PATH setup failed"
    }
    if ($pathFailureOutput -notmatch "user PATH setup failed: fixture PATH failure" -or
        $pathFailureOutput -notmatch "Add .* to user PATH manually") {
        throw "Installer did not report PATH setup failure and manual recovery"
    }

    if ($env:Path -ne $originalProcessPath -or
        [Environment]::GetEnvironmentVariable("Path", "User") -ne $originalUserPath) {
        throw "Installer changed PATH despite OMNI_NO_MODIFY_PATH=1"
    }

    Write-Output "install.ps1 smoke test passed"
} finally {
    [Environment]::SetEnvironmentVariable("OMNI_INSTALL_DIR", $originalInstallDirectory, "Process")
    [Environment]::SetEnvironmentVariable("OMNI_NO_MODIFY_PATH", $originalNoModifyPath, "Process")
    $env:Path = $originalProcessPath
    if ([IO.Directory]::Exists($temporaryDirectory)) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}
