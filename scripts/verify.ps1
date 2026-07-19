param(
    [switch]$FixFormat
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
Push-Location $repoRoot
try {
    if ($FixFormat) {
        cargo fmt --all
    } else {
        cargo fmt --all -- --check
    }
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed" }

    cargo check --locked --all-targets
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed" }

    cargo test --locked --all-targets
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }

    cargo clippy --locked --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed" }

    $npm = Get-Command npm -ErrorAction SilentlyContinue
    if ($null -eq $npm) { throw "npm is required to validate the consumer package" }
    & $npm.Source --prefix npm run check
    if ($LASTEXITCODE -ne 0) { throw "NPM launcher check failed" }
    & $npm.Source pack --dry-run ./npm
    if ($LASTEXITCODE -ne 0) { throw "NPM package dry run failed" }

    $bash = Get-Command bash -ErrorAction SilentlyContinue
    if ($null -eq $bash) { throw "bash is required to parse tracked shell scripts" }
    Get-ChildItem -LiteralPath $PSScriptRoot -Filter *.sh | ForEach-Object {
        & $bash.Source -n "scripts/$($_.Name)"
        if ($LASTEXITCODE -ne 0) { throw "Bash parse failed for $($_.Name)" }
    }

    $parseErrors = @()
    Get-ChildItem -LiteralPath $PSScriptRoot -Filter *.ps1 | ForEach-Object {
        $fileErrors = $null
        [System.Management.Automation.Language.Parser]::ParseFile(
            $_.FullName,
            [ref]$null,
            [ref]$fileErrors
        ) | Out-Null
        $parseErrors += $fileErrors
    }
    if ($parseErrors.Count -gt 0) {
        $parseErrors | ForEach-Object { Write-Error $_ }
        exit 1
    }

    Write-Output "PolyTread verification passed."
}
finally {
    Pop-Location
}
