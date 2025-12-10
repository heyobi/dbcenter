$ErrorActionPreference = "Stop"

function Check-Command($cmd) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        Write-Error "$cmd could not be found. Please ensure it is installed and in your PATH."
        exit 1
    }
}

Check-Command "cargo"
Check-Command "go"
Check-Command "zig"
# Check-Command "python" # We might use a specific python

function Find-Maturin {
    if (Get-Command "maturin" -ErrorAction SilentlyContinue) {
        return "maturin"
    }
    
    # Try common locations from FULLBuild.md
    $candidates = @(
        "$env:USERPROFILE\.conda\envs\gis\Scripts\maturin.exe",
        "$env:USERPROFILE\miniconda3\Scripts\maturin.exe",
        "C:\ProgramData\miniconda3\Scripts\maturin.exe"
    )
    
    foreach ($c in $candidates) {
        if (Test-Path $c) {
            return $c
        }
    }
    
    Write-Error "Could not find 'maturin'. Please install it or add it to PATH."
    exit 1
}

$MaturinExe = Find-Maturin
Write-Host "Using Maturin: $MaturinExe" -ForegroundColor Gray

# --- Configuration ---
$ProjectRoot = $PSScriptRoot
$GoDir = Join-Path $ProjectRoot "dbcenter-go"
$PythonDir = Join-Path $ProjectRoot "dbcenter-python"
$DistDir = Join-Path $ProjectRoot "dist"

$RemoteUser = "ibox"
$RemoteHost = "maltepe"
$RemoteBuildDir = "/tmp/dbcenter_build"

# Create Dist Directory
if (Test-Path $DistDir) { Remove-Item -Path $DistDir -Recurse -Force }
New-Item -ItemType Directory -Path $DistDir | Out-Null
New-Item -ItemType Directory -Path "$DistDir/windows" | Out-Null
New-Item -ItemType Directory -Path "$DistDir/linux" | Out-Null

Write-Host "Build started..." -ForegroundColor Green

# ==========================================
# 1. PYTHON BUILD (Maturin)
# ==========================================
Write-Host "`n[Python] Building for Windows..." -ForegroundColor Cyan
Push-Location $PythonDir
try {
    # Windows Build
    & $MaturinExe build --release --out "$DistDir/windows"
}
finally {
    Pop-Location
}

# ==========================================
# 2. GO BUILD (Windows)
# ==========================================
Write-Host "`n[Go] Building for Windows..." -ForegroundColor Cyan
Push-Location $GoDir
try {
    go build -o "$DistDir/windows/dbcenter-go.exe" .
    if ($LASTEXITCODE -ne 0) { throw "Go Windows Build Failed" }
}
finally {
    Pop-Location
}

# ==========================================
# 3. LINUX BUILD (Remote)
# ==========================================
Write-Host "`n[Linux] Building on Remote Host ($RemoteHost)..." -ForegroundColor Cyan

# 1. Prepare Remote
Write-Host "Cleaning remote directory..."
ssh $RemoteUser@$RemoteHost "rm -rf $RemoteBuildDir && mkdir -p $RemoteBuildDir"

# 2. Transfer Source (whitelist to avoid target/)
Write-Host "Transferring source code..."
$ItemsToCopy = @("dbcenter-core", "dbcenter-ffi", "dbcenter-python", "dbcenter-go", "dbcenter-watcher", "dbcenter-node", "dbcenter-java", "examples", "Cargo.toml")
if (Test-Path "$ProjectRoot/Cargo.lock") { $ItemsToCopy += "Cargo.lock" }

foreach ($item in $ItemsToCopy) {
    $localPath = Join-Path $ProjectRoot $item
    if (Test-Path $localPath) {
        # Use -r for directories
        scp -r $localPath $RemoteUser@$RemoteHost`:$RemoteBuildDir/
    }
}

# Copy build script
scp "$ProjectRoot/build_remote.sh" $RemoteUser@$RemoteHost`:$RemoteBuildDir/

# 3. Execute Remote Build
Write-Host "Executing remote build script..."
ssh $RemoteUser@$RemoteHost "chmod +x $RemoteBuildDir/build_remote.sh && $RemoteBuildDir/build_remote.sh"

if ($LASTEXITCODE -ne 0) {
    Write-Error "Remote build failed!"
    exit 1
}

# 4. Retrieve Artifacts
Write-Host "Retrieving Linux artifacts..."
scp $RemoteUser@$RemoteHost`:$RemoteBuildDir/dist/linux/* "$DistDir/linux/"


Write-Host "`nBuild Complete!" -ForegroundColor Green
Write-Host "Artifacts in: $DistDir"
