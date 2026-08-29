[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $BinaryPath,

    [Parameter(Mandatory = $true)]
    [string] $LicensePath,

    [Parameter(Mandatory = $true)]
    [string] $OutputPath,

    [Parameter(Mandatory = $true)]
    [string] $ExpectedVersion
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Get-RegularFile {
    param([string] $Path, [string] $Label)

    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer -or (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "$Label must be a regular file: $Path"
    }
    return $item
}

function Add-ZipFile {
    param(
        [IO.Compression.ZipArchive] $Archive,
        [string] $Source,
        [string] $Name
    )

    $entry = $Archive.CreateEntry($Name, [IO.Compression.CompressionLevel]::Optimal)
    $sourceStream = [IO.File]::OpenRead($Source)
    $output = $entry.Open()
    try {
        $sourceStream.CopyTo($output)
    } finally {
        $output.Dispose()
        $sourceStream.Dispose()
    }
}

$binary = Get-RegularFile -Path $BinaryPath -Label "Windows release binary"
$license = Get-RegularFile -Path $LicensePath -Label "License"
$output = [IO.Path]::GetFullPath($OutputPath)
$outputDirectory = [IO.Path]::GetDirectoryName($output)
if ([string]::IsNullOrWhiteSpace($outputDirectory)) {
    throw "OutputPath must include a parent directory"
}
[IO.Directory]::CreateDirectory($outputDirectory) | Out-Null
if ([IO.File]::Exists($output) -or [IO.Directory]::Exists($output)) {
    throw "OutputPath already exists: $output"
}

$reported = (& $binary.FullName --version 2>$null | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $reported -ne "omni $ExpectedVersion") {
    throw "Windows release binary did not report omni $ExpectedVersion"
}

$stream = [IO.File]::Open($output, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
$archive = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $false)
try {
    Add-ZipFile -Archive $archive -Source $binary.FullName -Name "omni.exe"
    Add-ZipFile -Archive $archive -Source $license.FullName -Name "LICENSE"
} finally {
    $archive.Dispose()
    $stream.Dispose()
}

$smokeDirectory = Join-Path $outputDirectory ("windows-package-smoke-" + [guid]::NewGuid().ToString("N"))
[IO.Directory]::CreateDirectory($smokeDirectory) | Out-Null
try {
    $archive = [IO.Compression.ZipFile]::OpenRead($output)
    try {
        $entries = @($archive.Entries)
        $names = @($entries | ForEach-Object { $_.FullName } | Sort-Object)
        if ($entries.Count -ne 2 -or ($names -join "`n") -ne "LICENSE`nomni.exe") {
            throw "Windows release archive must contain exactly omni.exe and LICENSE"
        }
        foreach ($entry in $entries) {
            if ($entry.Length -le 0) {
                throw "Windows release archive contains an empty entry: $($entry.FullName)"
            }
            $destination = Join-Path $smokeDirectory $entry.FullName
            $sourceStream = $entry.Open()
            $outputFile = [IO.File]::Open($destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try {
                $sourceStream.CopyTo($outputFile)
            } finally {
                $outputFile.Dispose()
                $sourceStream.Dispose()
            }
        }
    } finally {
        $archive.Dispose()
    }

    $smokeBinary = Join-Path $smokeDirectory "omni.exe"
    $reported = (& $smokeBinary --version 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $reported -ne "omni $ExpectedVersion") {
        throw "Packaged Windows binary did not report omni $ExpectedVersion"
    }
} finally {
    if ([IO.Directory]::Exists($smokeDirectory)) {
        Remove-Item -LiteralPath $smokeDirectory -Recurse -Force
    }
}

Write-Output "Packaged $output"
