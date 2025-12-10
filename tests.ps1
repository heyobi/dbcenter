$ErrorActionPreference = "Stop"

$ProjectRoot = $PSScriptRoot
$DistDir = Join-Path $ProjectRoot "dist"
$DistWin = Join-Path $DistDir "windows"
$DistLin = Join-Path $DistDir "linux"

$RemoteUser = "ibox"
$RemoteHost = "maltepe"
$RemoteDir = "/tmp/dbcenter_tests"

# ==========================================
# 1. WINDOWS TESTS (Local)
# ==========================================
Write-Host "--- Windows Tests ---" -ForegroundColor Cyan

# A. Go Tests
Write-Host "[Go] Running local tests..." -ForegroundColor Green
$GoDir = Join-Path $ProjectRoot "dbcenter-go"
Push-Location $GoDir
try {
    # We assume 'go' is in PATH
    go test -v ./...
    if ($LASTEXITCODE -ne 0) { throw "Go tests failed on Windows" }
}
finally {
    Pop-Location
}

# B. Python Tests
Write-Host "`n[Python] Running local tests..." -ForegroundColor Green
# We need to install the wheel we just built to test IT.
# Get the latest wheel
$wheel = Get-ChildItem "$DistWin/*.whl" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $wheel) { throw "No Windows wheel found in $DistWin" }

Write-Host "Installing $wheel..."
pip install --force-reinstall $wheel
if ($LASTEXITCODE -ne 0) { throw "Failed to install Python wheel" }

# Run test script
$PyTestFile = Join-Path $ProjectRoot "python_tests/test_final.py"
python $PyTestFile
if ($LASTEXITCODE -ne 0) { throw "Python tests failed on Windows" }

# ==========================================
# 2. LINUX TESTS (Remote)
# ==========================================
Write-Host "`n--- Linux Tests (Remote: $RemoteHost) ---" -ForegroundColor Cyan

# A. Prepare Remote
Write-Host "Cleaning remote directory..."
# Set 777 permissions on host directory to allow any Docker user to write
ssh $RemoteUser@$RemoteHost "rm -rf $RemoteDir && mkdir -p $RemoteDir && chmod 777 $RemoteDir"

# B. Transfer Artifacts
Write-Host "Transferring artifacts..."
# We need:
# 1. Linux Wheel
# 2. Go Test Binary (test_go_linux)
# 3. Rust .so (libdbcenter_ffi.so)
# 4. Python Test Script (test_final.py)

$LinWheel = Get-ChildItem "$DistLin/*.whl" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $LinWheel) { throw "No Linux wheel found in $DistLin" }

# Copy items
scp $LinWheel $RemoteUser@$RemoteHost`:$RemoteDir/
scp "$DistLin/test_go_linux" $RemoteUser@$RemoteHost`:$RemoteDir/
scp "$DistLin/test_c_linux" $RemoteUser@$RemoteHost`:$RemoteDir/
scp "$DistLin/test_rust_linux" $RemoteUser@$RemoteHost`:$RemoteDir/
scp "$DistLin/libdbcenter_ffi.so" $RemoteUser@$RemoteHost`:$RemoteDir/
scp "$DistLin/dbcenter-watcher" $RemoteUser@$RemoteHost`:$RemoteDir/
scp "$ProjectRoot/python_tests/test_final.py" $RemoteUser@$RemoteHost`:$RemoteDir/

# C. Run Remote Tests (Docker)
Write-Host "Running Remote Tests in Docker..."

# Define the remote command script
$RemoteScript = @"
set -e
echo '[Remote] Starting Docker container for tests...'

# 1. Run Python Tests
# We rely on host dir being 777 permissions.
docker run --rm -v ${RemoteDir}:/work python:3.11-bookworm bash -c '
    cd /work
    pip install *.whl
    useradd -ms /bin/bash testuser
    su testuser -c "python test_final.py"
'

# 2. Run Go Tests
# Using debian:bookworm-slim for glibc compatibility
docker run --rm -v ${RemoteDir}:/work -e LD_LIBRARY_PATH=/work debian:bookworm-slim bash -c '
    cd /work
    apt-get update && apt-get install -y postgresql-common
    chmod +x test_go_linux dbcenter-watcher test_c_linux test_rust_linux
    useradd -ms /bin/bash testuser
    su testuser -c "./test_go_linux"
'

# 3. Run C Tests
docker run --rm -v ${RemoteDir}:/work -e LD_LIBRARY_PATH=/work debian:bookworm-slim bash -c '
    cd /work
    chmod +x test_c_linux dbcenter-watcher
    useradd -ms /bin/bash testuser
    su testuser -c "./test_c_linux"
'

# 4. Run Rust Tests
docker run --rm -v ${RemoteDir}:/work -e LD_LIBRARY_PATH=/work debian:bookworm-slim bash -c '
    cd /work
    chmod +x test_rust_linux
    useradd -ms /bin/bash testuser
    su testuser -c "./test_rust_linux"
'
"@

# Execute via SSH
# Execute via SSH
# We pass the script to ssh, removing \r for Linux compatibility
ssh $RemoteUser@$RemoteHost ($RemoteScript -replace "`r", "")

if ($LASTEXITCODE -ne 0) { 
    Write-Error "Remote/Linux tests failed!" 
    exit 1
}

Write-Host "`nAll Tests Passed!" -ForegroundColor Green
