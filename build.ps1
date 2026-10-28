# ==============================================================================
# OpenAnima — Automated Build and Packaging Script
# Produces:
#   1. Portable ZIP (OpenAnima-v0.2.0-portable.zip)
#   2. Standalone Single-File EXE (OpenAnima-v0.2.0-standalone.exe)
#   3. Self-Contained Single-File EXE (OpenAnima-v0.2.0-selfcontained.exe)
# ==============================================================================

param(
    [string]$Version = "0.2.0",
    [switch]$SkipSelfContained = $false
)

$ErrorActionPreference = "Stop"
$ScriptRoot = $PSScriptRoot
if (-not $ScriptRoot) { $ScriptRoot = Get-Location }

# Paths
$Root = (Resolve-Path $ScriptRoot).Path
$CoreDir = Join-Path $Root "OpenAnima\OpenAnima.Core"
$UiDir = Join-Path $Root "OpenAnima\OpenAnima.UI"
$UiProj = Join-Path $UiDir "OpenAnima.UI.csproj"
$OutputDir = Join-Path $Root "releases"
$TempDir = Join-Path $Root "dist"

Write-Host ""
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "       OpenAnima v$Version - Automated Release Builder      " -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host ""

# 1. Check tools
Write-Host "[1/6] Checking build toolchain..." -ForegroundColor Yellow
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "Rust compiler (cargo) not found in PATH. Install Rust from https://rustup.rs"
}
if (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) {
    Write-Error ".NET SDK (dotnet) not found in PATH. Install .NET 8 SDK from https://dotnet.microsoft.com"
}
$cargoVer = (cargo --version).Trim()
$dotnetVer = (dotnet --version).Trim()
Write-Host "  -> cargo: $cargoVer" -ForegroundColor Gray
Write-Host "  -> dotnet: $dotnetVer" -ForegroundColor Gray

# Ensure clean directories
if (Test-Path $OutputDir) { Remove-Item $OutputDir -Recurse -Force }
New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
if (Test-Path $TempDir) { Remove-Item $TempDir -Recurse -Force }
New-Item -ItemType Directory -Path $TempDir -Force | Out-Null

# 2. Build Rust Core (Release)
Write-Host "`n[2/6] Building Rust Microcore (openanima-core.exe)..." -ForegroundColor Yellow
Push-Location $CoreDir
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "Cargo release build failed" }
}
finally {
    Pop-Location
}

$CoreExe = Join-Path $CoreDir "target\release\openanima-core.exe"
if (-not (Test-Path $CoreExe)) {
    Write-Error "openanima-core.exe was not produced by cargo!"
}
$CoreSizeKb = [math]::Round((Get-Item $CoreExe).Length / 1KB, 1)
Write-Host "  -> openanima-core.exe built successfully ($CoreSizeKb KB)" -ForegroundColor Green

# Copy core to UI bin folders for dev/local debug convenience
$DevBinDebug = Join-Path $UiDir "bin\Debug\net8.0-windows"
$DevBinRelease = Join-Path $UiDir "bin\Release\net8.0-windows"
if (Test-Path $DevBinDebug) { Copy-Item $CoreExe -Destination (Join-Path $DevBinDebug "openanima-core.exe") -Force }
if (Test-Path $DevBinRelease) { Copy-Item $CoreExe -Destination (Join-Path $DevBinRelease "openanima-core.exe") -Force }

# 3. Build Portable Folder and ZIP
Write-Host "`n[3/6] Packaging Portable ZIP distribution..." -ForegroundColor Yellow
$PortableFolder = Join-Path $TempDir "OpenAnima-v$Version-portable"
dotnet publish $UiProj -c Release -o $PortableFolder --no-self-contained
if ($LASTEXITCODE -ne 0) { throw "Portable publish failed" }

# Ensure openanima-core.exe and README are in the portable folder
Copy-Item $CoreExe -Destination (Join-Path $PortableFolder "openanima-core.exe") -Force
$ReadmeSrc = Join-Path $Root "OpenAnima\README.md"
$ReadmeRuSrc = Join-Path $Root "OpenAnima\README.ru.md"
if (Test-Path $ReadmeSrc) { Copy-Item $ReadmeSrc -Destination (Join-Path $PortableFolder "README.md") -Force }
if (Test-Path $ReadmeRuSrc) { Copy-Item $ReadmeRuSrc -Destination (Join-Path $PortableFolder "README.ru.md") -Force }

# Remove pdb files from release
Get-ChildItem $PortableFolder -Filter "*.pdb" | Remove-Item -Force

# Create Portable ZIP
$PortableZip = Join-Path $OutputDir "OpenAnima-v$Version-portable.zip"
if (Test-Path $PortableZip) { Remove-Item $PortableZip -Force }
Compress-Archive -Path "$PortableFolder\*" -DestinationPath $PortableZip -CompressionLevel Optimal
$ZipSizeMb = [math]::Round((Get-Item $PortableZip).Length / 1MB, 2)
Write-Host "  -> Portable ZIP created: $PortableZip ($ZipSizeMb MB)" -ForegroundColor Green

# 4. Build Standalone Single-File EXE (All-in-One Framework-dependent ~9 MB)
Write-Host "`n[4/6] Building Standalone Single-File EXE (embedded core)..." -ForegroundColor Yellow
$StandaloneTemp = Join-Path $TempDir "standalone"
dotnet publish $UiProj -c Release -r win-x64 --self-contained false `
    -p:PublishSingleFile=true `
    -p:IncludeNativeLibrariesForSelfExtract=true `
    -o $StandaloneTemp
if ($LASTEXITCODE -ne 0) { throw "Standalone publish failed" }

$StandaloneExe = Join-Path $OutputDir "OpenAnima-v$Version-standalone.exe"
Copy-Item (Join-Path $StandaloneTemp "OpenAnima.exe") -Destination $StandaloneExe -Force
$StandaloneSizeMb = [math]::Round((Get-Item $StandaloneExe).Length / 1MB, 2)
Write-Host "  -> Standalone Single-File EXE created: $StandaloneExe ($StandaloneSizeMb MB)" -ForegroundColor Green

# 5. Build Self-Contained Single-File EXE (Optional, fully zero-dep ~75 MB)
if (-not $SkipSelfContained) {
    Write-Host "`n[5/6] Building Self-Contained EXE (zero prerequisites)..." -ForegroundColor Yellow
    $SelfContainedTemp = Join-Path $TempDir "selfcontained"
    dotnet publish $UiProj -c Release -r win-x64 --self-contained true `
        -p:PublishSingleFile=true `
        -p:IncludeNativeLibrariesForSelfExtract=true `
        -p:EnableCompressionInSingleFile=true `
        -o $SelfContainedTemp
    if ($LASTEXITCODE -ne 0) { throw "Self-contained publish failed" }

    $SelfContainedExe = Join-Path $OutputDir "OpenAnima-v$Version-selfcontained.exe"
    Copy-Item (Join-Path $SelfContainedTemp "OpenAnima.exe") -Destination $SelfContainedExe -Force
    $SelfContainedSizeMb = [math]::Round((Get-Item $SelfContainedExe).Length / 1MB, 2)
    Write-Host "  -> Self-Contained EXE created: $SelfContainedExe ($SelfContainedSizeMb MB)" -ForegroundColor Green
} else {
    Write-Host "`n[5/6] Skipping Self-Contained build (--SkipSelfContained)" -ForegroundColor Gray
}

# 6. Generate Checksums (SHA256)
Write-Host "`n[6/6] Generating SHA-256 checksums..." -ForegroundColor Yellow
$ChecksumFile = Join-Path $OutputDir "SHA256SUMS.txt"
$Checksums = @()
Get-ChildItem $OutputDir -File | Where-Object { $_.Name -ne "SHA256SUMS.txt" } | ForEach-Object {
    $h = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
    $Checksums += "$h  $($_.Name)"
}
$Checksums | Out-File -FilePath $ChecksumFile -Encoding ascii
Write-Host "  -> Checksums saved to $ChecksumFile" -ForegroundColor Green

# Cleanup temp build directory
Remove-Item $TempDir -Recurse -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "               BUILD COMPLETED SUCCESSFULLY!              " -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "Artifacts location: $OutputDir" -ForegroundColor White
Write-Host ""

Get-ChildItem $OutputDir | Select-Object Name, @{Name="Size (MB)"; Expression={[math]::Round($_.Length / 1MB, 2)}} | Format-Table -AutoSize
