# Quick benchmark script for fast iteration (3 runs)
# Usage: .\scripts\quick-bench.ps1 [repo_path]
# Default: C:\git\wtg\Glow

param(
    [string]$RepoPath = "C:\git\GitHub\WiseTechGlobal\Glow",
    [int]$Runs = 3
)

$RepoName = Split-Path $RepoPath -Leaf
$IndexPattern = "$env:LOCALAPPDATA\fxi\indexes\$RepoName-*"

function Clear-Index {
    Remove-Item -Recurse -Force $IndexPattern -ErrorAction SilentlyContinue
}

Write-Host "Quick Benchmark: $RepoName ($Runs runs)" -ForegroundColor Cyan
Write-Host "Repository: $RepoPath" -ForegroundColor Gray
Write-Host ""

$times = @()

for ($i = 1; $i -le $Runs; $i++) {
    Write-Host "Run $i/$Runs... " -NoNewline
    
    Clear-Index
    
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    & vfp9 index $RepoPath 2>&1 | Out-Null
    $stopwatch.Stop()
    
    $elapsed = $stopwatch.Elapsed.TotalSeconds
    $times += $elapsed
    
    Write-Host ("{0:F2}s" -f $elapsed) -ForegroundColor Green
}

Write-Host ""
Write-Host "Results:" -ForegroundColor Cyan
Write-Host ("  Min:     {0:F2}s" -f ($times | Measure-Object -Minimum).Minimum)
Write-Host ("  Max:     {0:F2}s" -f ($times | Measure-Object -Maximum).Maximum)
Write-Host ("  Average: {0:F2}s" -f ($times | Measure-Object -Average).Average) -ForegroundColor Yellow
