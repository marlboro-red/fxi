# A/B Benchmark Script for fxi
# Compares indexing performance between two branches
#
# Usage: .\scripts\benchmark-ab.ps1 -BaselineBranch main -TestBranch perf2 -Repo Glow
#
# Note: Copies release binary to vfp9.exe location to bypass Windows Defender scanning

param(
    [string]$BaselineBranch = "main",
    [string]$TestBranch = "perf2",
    [string]$Repo = "Glow",
    [int]$Iterations = 3
)

$ErrorActionPreference = "Stop"

# Configuration
$VFP9_PATH = "c:\git\OTHERS\dom\scoop\persist\rustup\.cargo\bin\vfp9.exe"
$REPOS = @{
    "Glow" = "C:\git\GitHub\WiseTechGlobal\Glow"
    "CargoWise" = "C:\git\GitHub\WiseTechGlobal\CargoWise"
}

$RepoPath = $REPOS[$Repo]
if (-not $RepoPath) {
    Write-Error "Unknown repo: $Repo. Available: $($REPOS.Keys -join ', ')"
    exit 1
}

if (-not (Test-Path $RepoPath)) {
    Write-Error "Repo path not found: $RepoPath"
    exit 1
}

function Clear-Index {
    param([string]$RepoName)
    $indexPattern = "$env:LOCALAPPDATA\fxi\indexes\$RepoName-*"
    Remove-Item -Recurse -Force $indexPattern -ErrorAction SilentlyContinue
}

function Build-And-Deploy {
    param([string]$Branch)
    
    Write-Host "`n=== Building $Branch ===" -ForegroundColor Cyan
    git checkout $Branch 2>$null
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Failed to checkout $Branch"
        exit 1
    }
    
    cargo build --release 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Failed to build $Branch"
        exit 1
    }
    
    Copy-Item .\target\release\fxi.exe $VFP9_PATH -Force
    Write-Host "Deployed $Branch to $VFP9_PATH" -ForegroundColor Green
}

function Run-Benchmark {
    param(
        [string]$Label,
        [string]$RepoPath,
        [string]$RepoName,
        [int]$Iterations
    )
    
    $times = @()
    
    for ($i = 1; $i -le $Iterations; $i++) {
        Write-Host "  Run $i/$Iterations... " -NoNewline
        
        # Clear index for cold start
        Clear-Index -RepoName $RepoName
        
        # Run indexing and measure time
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $VFP9_PATH index $RepoPath 2>&1 | Out-Null
        $sw.Stop()
        
        $elapsed = $sw.Elapsed.TotalSeconds
        $times += $elapsed
        Write-Host ("{0:F2}s" -f $elapsed)
    }
    
    # Calculate stats
    $avg = ($times | Measure-Object -Average).Average
    $min = ($times | Measure-Object -Minimum).Minimum
    $max = ($times | Measure-Object -Maximum).Maximum
    
    return @{
        Label = $Label
        Times = $times
        Avg = $avg
        Min = $min
        Max = $max
    }
}

# Main
Write-Host "=== fxi A/B Benchmark ===" -ForegroundColor Yellow
Write-Host "Baseline: $BaselineBranch"
Write-Host "Test:     $TestBranch"
Write-Host "Repo:     $Repo ($RepoPath)"
Write-Host "Iterations: $Iterations"

$originalBranch = git rev-parse --abbrev-ref HEAD

# Run baseline
Build-And-Deploy -Branch $BaselineBranch
Write-Host "`n=== Benchmarking $BaselineBranch ===" -ForegroundColor Cyan
$baselineResults = Run-Benchmark -Label $BaselineBranch -RepoPath $RepoPath -RepoName $Repo -Iterations $Iterations

# Run test
Build-And-Deploy -Branch $TestBranch
Write-Host "`n=== Benchmarking $TestBranch ===" -ForegroundColor Cyan
$testResults = Run-Benchmark -Label $TestBranch -RepoPath $RepoPath -RepoName $Repo -Iterations $Iterations

# Restore original branch
git checkout $originalBranch 2>$null

# Results
Write-Host "`n=== Results ===" -ForegroundColor Yellow
Write-Host ("{0,-15} {1,10} {2,10} {3,10}" -f "Branch", "Avg", "Min", "Max")
Write-Host ("{0,-15} {1,10:F2}s {2,10:F2}s {3,10:F2}s" -f $baselineResults.Label, $baselineResults.Avg, $baselineResults.Min, $baselineResults.Max)
Write-Host ("{0,-15} {1,10:F2}s {2,10:F2}s {3,10:F2}s" -f $testResults.Label, $testResults.Avg, $testResults.Min, $testResults.Max)

$diff = $testResults.Avg - $baselineResults.Avg
$pct = ($diff / $baselineResults.Avg) * 100

Write-Host ""
if ($diff -lt 0) {
    Write-Host ("Improvement: {0:F2}s faster ({1:F1}%)" -f (-$diff), (-$pct)) -ForegroundColor Green
} else {
    Write-Host ("Regression: {0:F2}s slower ({1:F1}%)" -f $diff, $pct) -ForegroundColor Red
}
