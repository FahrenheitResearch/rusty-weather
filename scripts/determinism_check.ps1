# Determinism / identity gate for store ingest (see docs/DETERMINISM.md).
#
# Proves that the working tree's rw_ingest writes hour files structurally
# identical (writer.build excluded) to a baseline build's MAJORITY outcome,
# with N runs per side so a scheduling-dependent flake on either side is
# visible instead of silently passing or failing the gate.
#
# Guard rails this script enforces (both grew out of a real incident where
# a "main" baseline binary had actually been built from the branch):
#   * capture-time stamp: the baseline exe is copied next to a
#     BUILD_SHA.txt recording `git rev-parse --short=12 HEAD` of the CLEAN
#     baseline worktree at the moment of capture;
#   * use-time assertion: every run.json a binary produces is checked
#     against the stamp (rw_store_diff assert-build) BEFORE any comparison
#     is trusted. A dirty or mislabeled build fails the assert.
#
# Usage (from the repo root, Windows PowerShell 5.1):
#   powershell -File scripts\determinism_check.ps1 `
#     -BaselineWorktree C:\Users\drew\rw-main-review -Runs 3
#
# Exit code 0 = gate passed; 1 = gate failed; 2 = setup/assertion error.

param(
    [Parameter(Mandatory = $true)][string]$BaselineWorktree,
    [int]$Runs = 3,
    [string]$OutDir = "out\determinism_check",
    [string]$Model = "hrrr",
    [string]$Date = "20260608",
    [int]$Cycle = 0,
    [string]$Hour = "6",
    [string]$CacheDir = "out\smoke_direct\cache",
    [string]$IngestProfile = "full",
    [switch]$NoHeavy,
    [switch]$SkipBaselineBuild,
    [switch]$SkipBranchBuild
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

function Fail([string]$message, [int]$code) {
    Write-Host "FAIL: $message" -ForegroundColor Red
    exit $code
}

function Get-CleanSha([string]$worktree) {
    $status = git -C $worktree status --porcelain
    if ($LASTEXITCODE -ne 0) { Fail "git status failed in $worktree" 2 }
    if ($status) { Fail "worktree $worktree is dirty; the build stamp would be -dirty and unprovable" 2 }
    $sha = (git -C $worktree rev-parse --short=12 HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $sha) { Fail "git rev-parse failed in $worktree" 2 }
    return $sha
}

function Invoke-Ingest([string]$exe, [string]$storeRoot, [string]$logPath) {
    $ingestArgs = @(
        "--model", $Model, "--date", $Date, "--cycle", $Cycle, "--hours", $Hour,
        "--store-root", $storeRoot, "--cache-dir", $CacheDir, "--profile", $IngestProfile
    )
    if ($NoHeavy) { $ingestArgs += "--no-heavy" }
    if (Test-Path $storeRoot) { Remove-Item -Recurse -Force $storeRoot }
    & $exe @ingestArgs *> $logPath
    if ($LASTEXITCODE -ne 0) { Fail "ingest failed ($exe -> $storeRoot); see $logPath" 2 }
}

Set-Location $repoRoot
$runDirName = "{0}_{1:00}z" -f $Date, $Cycle
$hourFileName = "f{0:000}.rws" -f [int]$Hour

# --- capture: baseline binary + stamp ---------------------------------------
$baselineSha = Get-CleanSha $BaselineWorktree
$branchSha = Get-CleanSha $repoRoot
Write-Host "baseline $BaselineWorktree @ $baselineSha | branch $repoRoot @ $branchSha"

if (-not $SkipBaselineBuild) {
    Write-Host "building baseline rw_ingest in $BaselineWorktree ..."
    cargo build --release --manifest-path (Join-Path $BaselineWorktree "Cargo.toml") --bin rw_ingest
    if ($LASTEXITCODE -ne 0) { Fail "baseline build failed" 2 }
}
if (-not $SkipBranchBuild) {
    Write-Host "building branch rw_ingest + rw_store_diff ..."
    cargo build --release --bin rw_ingest --bin rw_store_diff
    if ($LASTEXITCODE -ne 0) { Fail "branch build failed" 2 }
}

$binBaseline = Join-Path $OutDir "bin_baseline"
New-Item -ItemType Directory -Force $binBaseline | Out-Null
Copy-Item (Join-Path $BaselineWorktree "target\release\rw_ingest.exe") (Join-Path $binBaseline "rw_ingest.exe") -Force
Set-Content -Path (Join-Path $binBaseline "BUILD_SHA.txt") -Value $baselineSha -Encoding ascii
$storeDiff = Join-Path $repoRoot "target\release\rw_store_diff.exe"
$baselineExe = Join-Path $binBaseline "rw_ingest.exe"
$branchExe = Join-Path $repoRoot "target\release\rw_ingest.exe"

# --- N runs per side ---------------------------------------------------------
$baselineHours = @()
$branchHours = @()
foreach ($side in @("baseline", "branch")) {
    for ($i = 1; $i -le $Runs; $i++) {
        $storeRoot = Join-Path $OutDir "${side}_run$i"
        $log = Join-Path $OutDir "${side}_run$i.log"
        if ($side -eq "baseline") { $exe = $baselineExe } else { $exe = $branchExe }
        Write-Host "ingest ${side} run $i/$Runs ..."
        Invoke-Ingest $exe $storeRoot $log
        $runJson = Join-Path $storeRoot "$Model\$runDirName\run.json"
        $hourFile = Join-Path $storeRoot "$Model\$runDirName\$hourFileName"
        # use-time assertion: the artifact must carry the stamped sha.
        if ($side -eq "baseline") {
            $expected = (Get-Content (Join-Path $binBaseline "BUILD_SHA.txt")).Trim()
        } else {
            $expected = $branchSha
        }
        & $storeDiff assert-build $expected $runJson $hourFile
        if ($LASTEXITCODE -ne 0) { Fail "${side} run $i artifacts are not from build $expected (mislabeled or dirty binary)" 2 }
        if ($side -eq "baseline") { $baselineHours += $hourFile } else { $branchHours += $hourFile }
    }
}

# --- self-consistency per side ----------------------------------------------
$baselineConsistent = $true
Write-Host "`n== baseline self-consistency ($Runs runs) =="
& $storeDiff @baselineHours
if ($LASTEXITCODE -eq 2) { Fail "baseline self-consistency comparison errored" 2 }
if ($LASTEXITCODE -ne 0) {
    $baselineConsistent = $false
    Write-Host "baseline is NOT run-to-run deterministic; gating on its majority outcome" -ForegroundColor Yellow
}
Write-Host "`n== branch self-consistency ($Runs runs) =="
& $storeDiff @branchHours
if ($LASTEXITCODE -eq 2) { Fail "branch self-consistency comparison errored" 2 }
if ($LASTEXITCODE -ne 0) { Fail "branch is not run-to-run self-consistent: fix this before gating against the baseline" 1 }

# --- identity gate: branch vs baseline majority outcome ----------------------
Write-Host "`n== identity gate: branch vs baseline majority =="
$matchCount = 0
foreach ($baselineHour in $baselineHours) {
    & $storeDiff $branchHours[0] $baselineHour | Out-Null
    if ($LASTEXITCODE -eq 2) { Fail "gate comparison errored" 2 }
    if ($LASTEXITCODE -eq 0) { $matchCount++ }
}
if ($matchCount -eq $Runs) {
    Write-Host "PASS: branch output matches all $Runs baseline runs (byte-identical, writer.build excluded)" -ForegroundColor Green
    exit 0
}
if ($matchCount * 2 -gt $Runs) {
    Write-Host "PASS (majority): branch output matches $matchCount/$Runs baseline runs; baseline self-consistent=$baselineConsistent" -ForegroundColor Yellow
    Write-Host "the baseline flake is the known main-side nondeterminism: see docs/DETERMINISM.md"
    exit 0
}
Fail "branch output matches only $matchCount/$Runs baseline runs (no majority match)" 1
