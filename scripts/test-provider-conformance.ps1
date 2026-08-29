param(
    [ValidateSet("matrix", "adapters", "codex", "opencode", "grok")]
    [string]$Mode = "matrix"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$TemporaryBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$Temporary = Join-Path $TemporaryBase ("omni provider conformance Ω {0}" -f [Guid]::NewGuid())
$OriginalTemporary = @{
    TEMP = $env:TEMP
    TMP = $env:TMP
    TMPDIR = $env:TMPDIR
}

function Invoke-Cargo {
    param([string[]]$Arguments)

    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo exited with status $LASTEXITCODE"
    }
}

function Require-Binary {
    param([string]$Variable)

    $Path = [Environment]::GetEnvironmentVariable($Variable)
    if (-not $Path -or -not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Mode conformance requires $Variable"
    }
}

New-Item -ItemType Directory -Path $Temporary | Out-Null
try {
    $env:TEMP = $Temporary
    $env:TMP = $Temporary
    $env:TMPDIR = $Temporary
    foreach ($Variable in @(
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "XAI_API_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "OPENROUTER_API_KEY"
    )) {
        Remove-Item "Env:$Variable" -ErrorAction SilentlyContinue
    }

    Push-Location $ProjectRoot
    try {
        switch ($Mode) {
            "matrix" {
                Invoke-Cargo @(
                    "test", "--locked", "--package", "omnisession-cli",
                    "conversion_matrix_tests::every_provider_pair_builder_matches_synthetic_oracle",
                    "--", "--exact"
                )
            }
            "adapters" {
                Invoke-Cargo @("test", "--locked", "--package", "omnisession-adapters", "--tests")
            }
            "codex" {
                Require-Binary "OMNI_TEST_CODEX_BIN"
                Invoke-Cargo @(
                    "test", "--locked", "--package", "omnisession-cli", "--test", "native_conformance",
                    "installed_codex_round_trips_isolated_synthetic_history",
                    "--", "--ignored", "--exact", "--nocapture"
                )
            }
            "opencode" {
                Require-Binary "OMNI_TEST_OPENCODE_BIN"
                Invoke-Cargo @(
                    "test", "--locked", "--package", "omnisession-cli",
                    "opencode_import::tests::installed_opencode_round_trips_isolated_bounded_history",
                    "--", "--ignored", "--exact", "--nocapture"
                )
            }
            "grok" {
                Require-Binary "OMNI_TEST_GROK_BIN"
                Invoke-Cargo @(
                    "test", "--locked", "--package", "omnisession-cli", "--test", "grok_conformance",
                    "installed_grok_round_trips_isolated_synthetic_history",
                    "--", "--ignored", "--exact", "--nocapture"
                )
            }
        }
    } finally {
        Pop-Location
    }
} finally {
    foreach ($Name in $OriginalTemporary.Keys) {
        $Value = $OriginalTemporary[$Name]
        if ($null -eq $Value) {
            Remove-Item "Env:$Name" -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable($Name, $Value)
        }
    }
    Remove-Item -LiteralPath $Temporary -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Windows provider conformance passed: $Mode"
