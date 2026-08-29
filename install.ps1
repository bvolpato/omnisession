# Install latest OmniSession Windows preview for current user.
param(
    [scriptblock] $InstallerDownloadFactory,
    [scriptblock] $InstallerPathAction
)

& {
param(
    [scriptblock] $DownloadFactory,
    [scriptblock] $PathAction
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$WarningPreference = "Continue"

$repository = "bvolpato/omnisession"
$archiveName = "omni-windows-x86_64.zip"
$maximumArchiveSize = 64MB
$maximumChecksumSize = 64KB
$maximumBinarySize = 64MB
$ownershipMarker = "omnisession-windows-installer-v1"
$downloadFactoryOverride = $DownloadFactory

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Write-Info {
    param([string] $Message)
    Write-Output $Message
}

function Get-RegularFile {
    param([string] $Path, [string] $Label)

    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer -or (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "$Label must be a regular file: $Path"
    }
    return $item
}

function Assert-NoReparsePoint {
    param([string] $Path)

    $current = [IO.Path]::GetFullPath($Path)
    while (-not [string]::IsNullOrEmpty($current)) {
        if ([IO.File]::Exists($current) -or [IO.Directory]::Exists($current)) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Refusing reparse point in installation path: $current"
            }
        }
        $parent = [IO.Directory]::GetParent($current)
        if ($null -eq $parent) {
            break
        }
        $current = $parent.FullName
    }
}

function Test-FullyQualifiedWindowsPath {
    param([string] $Path)

    if ([string]::IsNullOrWhiteSpace($Path) -or $Path -match '^[\\/]{2}[?.][\\/]') {
        return $false
    }
    return $Path -match '^[A-Za-z]:[\\/]' -or
        $Path -match '^[\\/]{2}[^\\/]+[\\/][^\\/]+(?:[\\/]|$)'
}

function Open-DownloadResponse {
    param([string] $Uri)

    $request = [Net.HttpWebRequest]::CreateHttp($Uri)
    $request.AllowAutoRedirect = $true
    $request.MaximumAutomaticRedirections = 5
    $request.Timeout = 120000
    $request.ReadWriteTimeout = 120000
    $request.UserAgent = "OmniSession-Windows-Installer/1"
    $request.AutomaticDecompression =
        [Net.DecompressionMethods]::GZip -bor [Net.DecompressionMethods]::Deflate

    $response = $null
    try {
        $response = $request.GetResponse()
        $stream = $response.GetResponseStream()
        return @{
            ContentLength = $response.ContentLength
            FinalUri = $response.ResponseUri
            Response = $response
            Stream = $stream
        }
    } catch {
        if ($null -ne $response) {
            $response.Dispose()
        }
        throw
    }
}

function Save-Download {
    param(
        [string] $Uri,
        [string] $Destination,
        [long] $MaximumSize
    )

    $initialUri = [Uri] $Uri
    if (-not $initialUri.IsAbsoluteUri -or $initialUri.Scheme -ne [Uri]::UriSchemeHttps) {
        throw "Download URL must use HTTPS"
    }

    $download = $null
    $inputStream = $null
    $outputStream = $null
    $response = $null
    $completed = $false
    try {
        $download = if ($null -eq $downloadFactoryOverride) {
            Open-DownloadResponse -Uri $Uri
        } else {
            & $downloadFactoryOverride $Uri
        }
        $inputStream = $download.Stream
        $response = $download.Response
        $finalUri = [Uri] $download.FinalUri
        $contentLength = [long] $download.ContentLength
        if (-not $finalUri.IsAbsoluteUri -or $finalUri.Scheme -ne [Uri]::UriSchemeHttps) {
            throw "Download redirected to a non-HTTPS URL: $finalUri"
        }
        if ($null -eq $inputStream -or -not $inputStream.CanRead) {
            throw "Download response did not provide a readable stream: $Uri"
        }
        if ($contentLength -eq 0 -or $contentLength -gt $MaximumSize) {
            throw "Downloaded release asset has invalid size: $Uri"
        }

        $outputStream = [IO.File]::Open(
            $Destination,
            [IO.FileMode]::CreateNew,
            [IO.FileAccess]::Write,
            [IO.FileShare]::None
        )
        $buffer = New-Object byte[] 16384
        [long] $total = 0
        while ($true) {
            $remaining = $MaximumSize - $total
            $readLimit = [int] [Math]::Min([long] $buffer.Length, $remaining + 1)
            $read = $inputStream.Read($buffer, 0, $readLimit)
            if ($read -eq 0) {
                break
            }
            if ($read -gt $remaining) {
                throw "Downloaded release asset exceeds safe size: $Uri"
            }
            $outputStream.Write($buffer, 0, $read)
            $total += $read
        }
        if ($total -le 0) {
            throw "Downloaded release asset is empty: $Uri"
        }
        $outputStream.Flush()
        $completed = $true
    } finally {
        if ($null -ne $outputStream) {
            $outputStream.Dispose()
        }
        if ($null -ne $inputStream) {
            $inputStream.Dispose()
        }
        if ($null -ne $response) {
            $response.Dispose()
        }
        if (-not $completed -and [IO.File]::Exists($Destination)) {
            [IO.File]::Delete($Destination)
        }
    }

    $download = Get-RegularFile -Path $Destination -Label "Downloaded release asset"
    if ($download.Length -le 0 -or $download.Length -gt $MaximumSize) {
        throw "Downloaded release asset has invalid size: $Uri"
    }
}

function Get-InstalledVersion {
    param([string] $Binary)

    $reported = (& $Binary --version 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $reported -notmatch '^omni [0-9]+\.[0-9]+\.[0-9]+$') {
        throw "Downloaded OmniSession binary reported an invalid version"
    }
    return $reported
}

function Test-SamePath {
    param([string] $Left, [string] $Right)

    try {
        $leftPath = [IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($Left)).TrimEnd([char[]]"\/")
        $rightPath = [IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($Right)).TrimEnd([char[]]"\/")
        return $leftPath.Equals($rightPath, [StringComparison]::OrdinalIgnoreCase)
    } catch {
        return $false
    }
}

function Add-UserPath {
    param([string] $Directory)

    if ([Environment]::GetEnvironmentVariable("OMNI_NO_MODIFY_PATH", "Process") -eq "1") {
        Write-Info "Skipped user PATH setup because OMNI_NO_MODIFY_PATH=1."
        return
    }

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = if ([string]::IsNullOrWhiteSpace($userPath)) { @() } else { @($userPath -split ';') }
    if (-not ($entries | Where-Object { Test-SamePath -Left $_ -Right $Directory })) {
        $updated = if ([string]::IsNullOrWhiteSpace($userPath)) { $Directory } else { "$Directory;$userPath" }
        if ($updated.Length -ge 32767) {
            throw "User PATH is too long to add $Directory"
        }
        [Environment]::SetEnvironmentVariable("Path", $updated, "User")
        Write-Info "Added $Directory to user PATH."
    } else {
        Write-Info "User PATH already contains $Directory."
    }

    $processEntries = if ([string]::IsNullOrWhiteSpace($env:Path)) { @() } else { @($env:Path -split ';') }
    if (-not ($processEntries | Where-Object { Test-SamePath -Left $_ -Right $Directory })) {
        $env:Path = if ([string]::IsNullOrWhiteSpace($env:Path)) { $Directory } else { "$Directory;$env:Path" }
    }
}

if ($env:OS -ne "Windows_NT") {
    throw "install.ps1 supports Windows only"
}
$architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($architecture -ne "X64") {
    throw "Unsupported Windows architecture: $architecture (supported: x86-64)"
}

$configuredInstallDirectory = [Environment]::GetEnvironmentVariable("OMNI_INSTALL_DIR", "Process")
if ([string]::IsNullOrWhiteSpace($configuredInstallDirectory)) {
    if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        throw "LOCALAPPDATA must be set when OMNI_INSTALL_DIR is not provided"
    }
    $configuredInstallDirectory = Join-Path $env:LOCALAPPDATA "OmniSession\bin"
}
if (-not (Test-FullyQualifiedWindowsPath -Path $configuredInstallDirectory)) {
    throw "OMNI_INSTALL_DIR must be an absolute path"
}
$installDirectory = [IO.Path]::GetFullPath($configuredInstallDirectory)
$root = [IO.Path]::GetPathRoot($installDirectory)
if ($installDirectory.TrimEnd([char[]]"\/").Equals($root.TrimEnd([char[]]"\/"), [StringComparison]::OrdinalIgnoreCase)) {
    throw "OMNI_INSTALL_DIR must not be a filesystem root"
}

Assert-NoReparsePoint -Path $installDirectory
if ([IO.File]::Exists($installDirectory)) {
    throw "Installation directory is a file: $installDirectory"
}
[IO.Directory]::CreateDirectory($installDirectory) | Out-Null
Assert-NoReparsePoint -Path $installDirectory

$target = Join-Path $installDirectory "omni.exe"
$marker = Join-Path $installDirectory ".omnisession-installer"
if ([IO.Directory]::Exists($target)) {
    throw "Installation target is a directory: $target"
}
if ([IO.Directory]::Exists($marker)) {
    throw "Installation ownership marker is a directory: $marker"
}
if ([IO.File]::Exists($target)) {
    Get-RegularFile -Path $target -Label "Existing installation target" | Out-Null
    if (-not [IO.File]::Exists($marker)) {
        throw "Refusing to replace unowned existing binary: $target"
    }
    $markerFile = Get-RegularFile -Path $marker -Label "Installation ownership marker"
    if ([IO.File]::ReadAllText($markerFile.FullName).Trim() -ne $ownershipMarker) {
        throw "Refusing to replace binary with an invalid ownership marker: $target"
    }
} elseif ([IO.File]::Exists($marker)) {
    $markerFile = Get-RegularFile -Path $marker -Label "Installation ownership marker"
    if ([IO.File]::ReadAllText($markerFile.FullName).Trim() -ne $ownershipMarker) {
        throw "Installation ownership marker is invalid: $marker"
    }
}

[Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("omni-install-" + [guid]::NewGuid().ToString("N"))
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
$staged = Join-Path $installDirectory (".omni-" + [guid]::NewGuid().ToString("N") + ".exe")
$backup = Join-Path $installDirectory (".omni-backup-" + [guid]::NewGuid().ToString("N") + ".exe")
$hadTarget = [IO.File]::Exists($target)
$published = $false
$createdMarker = $false
$keepBackup = $false

try {
    $releaseUrl = "https://github.com/$repository/releases/latest/download"
    $archivePath = Join-Path $temporaryDirectory $archiveName
    $checksumsPath = Join-Path $temporaryDirectory "SHA256SUMS"

    Write-Info "Downloading OmniSession for windows/x86_64..."
    Save-Download -Uri "$releaseUrl/$archiveName" -Destination $archivePath -MaximumSize $maximumArchiveSize
    Save-Download -Uri "$releaseUrl/SHA256SUMS" -Destination $checksumsPath -MaximumSize $maximumChecksumSize

    $checksumMatches = New-Object 'System.Collections.Generic.List[string]'
    foreach ($line in [IO.File]::ReadAllLines($checksumsPath)) {
        $fields = @($line.Trim() -split '\s+')
        if ($fields.Count -eq 2 -and $fields[1].TrimStart([char]'*') -eq $archiveName) {
            $checksumMatches.Add($fields[0])
        }
    }
    if ($checksumMatches.Count -ne 1 -or $checksumMatches[0] -notmatch '^[0-9a-fA-F]{64}$') {
        throw "SHA256SUMS does not contain exactly one valid checksum for $archiveName"
    }
    $actualChecksum = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
    if (-not $actualChecksum.Equals($checksumMatches[0], [StringComparison]::OrdinalIgnoreCase)) {
        throw "SHA-256 verification failed for $archiveName"
    }

    $archive = [IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        $entries = @($archive.Entries)
        $names = @($entries | ForEach-Object { $_.FullName } | Sort-Object)
        if ($entries.Count -ne 2 -or ($names -join "`n") -ne "LICENSE`nomni.exe") {
            throw "$archiveName must contain exactly omni.exe and LICENSE"
        }
        $binaryEntry = @($entries | Where-Object { $_.FullName -eq "omni.exe" })[0]
        $licenseEntry = @($entries | Where-Object { $_.FullName -eq "LICENSE" })[0]
        if ($binaryEntry.Length -le 0 -or $binaryEntry.Length -gt $maximumBinarySize -or
            $licenseEntry.Length -le 0 -or $licenseEntry.Length -gt 1MB) {
            throw "$archiveName contains an entry with invalid size"
        }

        $candidate = Join-Path $temporaryDirectory "omni.exe"
        $sourceStream = $binaryEntry.Open()
        $output = [IO.File]::Open($candidate, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try {
            $sourceStream.CopyTo($output)
            $output.Flush()
        } finally {
            $output.Dispose()
            $sourceStream.Dispose()
        }
    } finally {
        $archive.Dispose()
    }

    Get-RegularFile -Path $candidate -Label "Extracted OmniSession binary" | Out-Null
    $candidateVersion = Get-InstalledVersion -Binary $candidate
    [IO.File]::Copy($candidate, $staged, $false)
    Get-RegularFile -Path $staged -Label "Staged OmniSession binary" | Out-Null

    if ($hadTarget) {
        [IO.File]::Replace($staged, $target, $backup, $true)
    } else {
        [IO.File]::Move($staged, $target)
    }
    $published = $true

    $installedVersion = Get-InstalledVersion -Binary $target
    if ($installedVersion -ne $candidateVersion) {
        throw "Installed OmniSession binary failed read-back verification"
    }
    if (-not [IO.File]::Exists($marker)) {
        $markerBytes = [Text.Encoding]::UTF8.GetBytes($ownershipMarker + [Environment]::NewLine)
        $markerStream = [IO.File]::Open($marker, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $createdMarker = $true
        try {
            $markerStream.Write($markerBytes, 0, $markerBytes.Length)
            $markerStream.Flush()
        } finally {
            $markerStream.Dispose()
        }
    }

    if ([IO.File]::Exists($backup)) {
        Remove-Item -LiteralPath $backup -Force
    }
    $published = $false
    try {
        if ($null -eq $PathAction) {
            Add-UserPath -Directory $installDirectory
        } else {
            & $PathAction $installDirectory
        }
    } catch {
        Write-Warning "Installed $candidateVersion, but user PATH setup failed: $($_.Exception.Message)"
        Write-Info "Add $installDirectory to user PATH manually."
    }
    Write-Info "Installed $candidateVersion to $target"
} catch {
    if ($published) {
        try {
            if ($hadTarget -and [IO.File]::Exists($backup)) {
                if ([IO.File]::Exists($target)) {
                    [IO.File]::Replace($backup, $target, $null, $true)
                } else {
                    [IO.File]::Move($backup, $target)
                }
            } elseif (-not $hadTarget -and [IO.File]::Exists($target)) {
                Remove-Item -LiteralPath $target -Force
                if ($createdMarker -and [IO.File]::Exists($marker)) {
                    Remove-Item -LiteralPath $marker -Force
                }
            }
        } catch {
            $keepBackup = $true
            throw "Installation failed and rollback also failed; backup preserved at ${backup}: $($_.Exception.Message)"
        }
    }
    throw
} finally {
    if ([IO.File]::Exists($staged)) {
        Remove-Item -LiteralPath $staged -Force
    }
    if (-not $keepBackup -and [IO.File]::Exists($backup)) {
        Remove-Item -LiteralPath $backup -Force
    }
    if ([IO.Directory]::Exists($temporaryDirectory)) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}
} $InstallerDownloadFactory $InstallerPathAction
