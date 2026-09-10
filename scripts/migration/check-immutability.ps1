$ErrorActionPreference = "Stop"

$repoRoot = (git rev-parse --show-toplevel)
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
Set-Location $repoRoot

$migrationDir = Join-Path $repoRoot "crates/aionui-db/migrations"
$duplicateVersions = Get-ChildItem -LiteralPath $migrationDir -File -Filter "*.sql" |
    ForEach-Object {
        if ($_.Name -match '^([0-9]+)_') {
            [PSCustomObject]@{ Version = [int64]$Matches[1]; Name = $_.Name }
        }
    } |
    Group-Object Version |
    Where-Object { $_.Count -gt 1 } |
    Sort-Object Name

if ($duplicateVersions) {
    [Console]::Error.WriteLine("Duplicate database migration versions are not allowed.")
    [Console]::Error.WriteLine("")
    [Console]::Error.WriteLine("Rename the later migration to the next unused numeric prefix.")
    [Console]::Error.WriteLine("")
    [Console]::Error.WriteLine("Duplicate versions:")
    foreach ($duplicate in $duplicateVersions) {
        $names = ($duplicate.Group | ForEach-Object { $_.Name }) -join ", "
        [Console]::Error.WriteLine("$($duplicate.Name): $names")
    }
    exit 1
}

if ($env:AIONCORE_ALLOW_MAIN_MIGRATION_EDIT -eq "1") {
    Write-Output "AIONCORE_ALLOW_MAIN_MIGRATION_EDIT=1; skipping migration immutability check"
    exit 0
}

$baseRef = $env:AIONCORE_MIGRATION_BASE_REF
if ([string]::IsNullOrWhiteSpace($baseRef)) {
    $mainUpstreamOutput = git rev-parse --abbrev-ref --symbolic-full-name 'main@{upstream}' 2>$null
    $mainUpstreamStatus = $LASTEXITCODE
    $mainUpstream = ($mainUpstreamOutput | Select-Object -First 1)
    if ($mainUpstreamStatus -eq 0 -and -not [string]::IsNullOrWhiteSpace($mainUpstream)) {
        $baseRef = $mainUpstream.Trim()
    } else {
        git rev-parse --verify --quiet origin/main | Out-Null
        if ($LASTEXITCODE -eq 0) {
            $baseRef = "origin/main"
        } else {
            git rev-parse --verify --quiet main | Out-Null
            if ($LASTEXITCODE -eq 0) {
                $baseRef = "main"
            } else {
                Write-Output "No configured main upstream, origin/main, or main ref found; skipping migration immutability check"
                exit 0
            }
        }
    }
}

git rev-parse --verify --quiet $baseRef | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Error "Migration immutability base ref not found: $baseRef"
    exit 1
}

$baseCommit = git merge-base HEAD $baseRef
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$addedMigrations = @(
    git diff --name-only --diff-filter=A $baseCommit -- "crates/aionui-db/migrations/*.sql"
    git ls-files --others --exclude-standard -- "crates/aionui-db/migrations/*.sql"
) | Sort-Object -Unique
$invalidNewMigrations = @(
    $addedMigrations |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
        Where-Object { [System.IO.Path]::GetFileName($_) -notmatch '^20[0-9]{12}_[a-z0-9]+(_[a-z0-9]+)*[.]sql$' }
)
if ($invalidNewMigrations.Count -gt 0) {
    [Console]::Error.WriteLine("New database migrations must use a 14-digit UTC timestamp prefix.")
    [Console]::Error.WriteLine("")
    [Console]::Error.WriteLine("Create new files as YYYYMMDDHHMMSS_descriptive_name.sql. Existing shipped migrations keep their original names.")
    [Console]::Error.WriteLine("")
    [Console]::Error.WriteLine("Invalid new migrations:")
    [Console]::Error.WriteLine(($invalidNewMigrations -join "`n"))
    exit 1
}

$changed = git diff --name-status --diff-filter=DMR $baseCommit -- "crates/aionui-db/migrations/*.sql"
if (-not [string]::IsNullOrWhiteSpace(($changed -join "`n"))) {
    [Console]::Error.WriteLine("Existing migration files from main must not be modified or deleted.")
    [Console]::Error.WriteLine("")
    [Console]::Error.WriteLine("Fix this by reverting changes to existing migration files and adding a new UTC timestamp-prefixed migration instead.")
    [Console]::Error.WriteLine("If this is an intentional high-risk exception, rerun with AIONCORE_ALLOW_MAIN_MIGRATION_EDIT=1.")
    [Console]::Error.WriteLine("")
    [Console]::Error.WriteLine("Changed existing migrations:")
    [Console]::Error.WriteLine(($changed -join "`n"))
    exit 1
}

Write-Output "Migration immutability check passed"
