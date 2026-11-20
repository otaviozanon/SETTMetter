# Release build wrapper: shows each step clearly instead of raw cargo/PowerShell noise.
# Usage: powershell -ExecutionPolicy Bypass -File scripts\build.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Step($n, $total, $text) {
    Write-Host "[$n/$total] $text" -ForegroundColor Cyan
}

$total = 4

Step 1 $total "Stopping running instance (if any)..."
Get-Process SETTMetter -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

Step 2 $total "Regenerating icon..."
powershell -ExecutionPolicy Bypass -File "$PSScriptRoot\make-icon.ps1" -OutDir "$root\assets" | Out-Null

Step 3 $total "Compiling (cargo build --release)..."
$env:PATH += ";$env:USERPROFILE\.cargo\bin"
$log = & cargo build --release --quiet --message-format=short 2>&1
$exit = $LASTEXITCODE

$warnings = $log | Where-Object { $_ -match '^warning:' }
$errors = $log | Where-Object { $_ -match '^error' }

if ($exit -ne 0) {
    Step 4 $total "Build FAILED" 
    Write-Host ""
    $log | ForEach-Object { Write-Host $_ }
    exit 1
}

Step 4 $total "Build OK"
if ($warnings.Count -gt 0) {
    Write-Host "  $($warnings.Count) warning(s):" -ForegroundColor Yellow
    $warnings | ForEach-Object { Write-Host "  - $_" -ForegroundColor Yellow }
} else {
    Write-Host "  No warnings." -ForegroundColor Green
}

$exe = Join-Path $root "target\release\SETTMetter.exe"
if (Test-Path $exe) {
    $size = [math]::Round((Get-Item $exe).Length / 1MB, 2)
    Write-Host "  $exe ($size MB)" -ForegroundColor Green
}
