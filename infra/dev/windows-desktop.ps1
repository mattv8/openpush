[CmdletBinding()]
param(
  [ValidateSet('dev', 'build', 'open')]
  [string]$Action,
  [Parameter(Mandatory = $true)]
  [string]$RepoPath,
  [Parameter(Mandatory = $true)]
  [string]$CargoTargetDir
)

$ErrorActionPreference = 'Stop'
$script:StrawberryPerlPath = 'C:\Strawberry\perl\bin\perl.exe'

function Invoke-Checked {
  param([string]$File, [string[]]$Arguments)
  & $File @Arguments
  if ($LASTEXITCODE -ne 0) {
    throw "$File failed with exit code $LASTEXITCODE"
  }
}

function Get-NativeCommand {
  param([string]$Name)
  $command = Get-Command $Name -CommandType Application -ErrorAction Stop
  $path = (Resolve-Path -LiteralPath $command.Source).ProviderPath
  if ($path -match '^(\\\\|//)') {
    throw "$Name must be a native Windows executable, not a UNC or WSL path."
  }
  return $path
}

function Get-NativePerl {
  $perl = $null
  try {
    $perl = Get-NativeCommand 'perl.exe'
  } catch {
    # A normal Windows installation may have Strawberry Perl without PATH setup.
  }
  if (-not $perl -or $perl -match '(?i)\\(?:Git\\usr|msys64\\usr)\\bin\\perl\.exe$') {
    $perl = $script:StrawberryPerlPath
    if (-not (Test-Path -LiteralPath $perl -PathType Leaf)) {
      throw 'Native Strawberry Perl was not found. Install it or place it before Git/MSYS Perl.'
    }
  }
  & $perl -MIPC::Cmd -e '1'
  if ($LASTEXITCODE -ne 0) {
    throw "Native Perl at $perl cannot load IPC::Cmd."
  }
  return (Resolve-Path -LiteralPath $perl).ProviderPath
}

function Get-NativePnpm {
  try {
    return Get-NativeCommand 'pnpm.cmd'
  } catch {
    return Get-NativeCommand 'pnpm.exe'
  }
}

function Assert-VisualStudioBuildTools {
  $candidates = @()
  foreach ($base in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
    if ($base) {
      $candidate = Join-Path $base 'Microsoft Visual Studio\Installer\vswhere.exe'
      if (Test-Path -LiteralPath $candidate -PathType Leaf) { $candidates += $candidate }
    }
  }
  if ($candidates.Count -eq 0) {
    throw 'Visual Studio Build Tools were not found. Install the C++ build tools and Windows SDK.'
  }
  $installation = & $candidates[0] -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
  if ($LASTEXITCODE -ne 0 -or -not $installation) {
    throw 'Visual Studio C++ Build Tools with Microsoft.VisualStudio.Component.VC.Tools.x86.x64 are required.'
  }
  return $installation.Trim()
}

function Assert-NativeToolchain {
  param([string]$Repository)
  $node = Get-NativeCommand 'node.exe'
  $pnpm = Get-NativePnpm
  $cargo = Get-NativeCommand 'cargo.exe'
  $rustc = Get-NativeCommand 'rustc.exe'
  $nodeExpected = (Get-Content -LiteralPath (Join-Path $Repository '.node-version') -Raw).Trim()
  $rustExpected = ((Select-String -LiteralPath (Join-Path $Repository 'rust-toolchain.toml') -Pattern '^channel = "([^"]+)"').Matches[0].Groups[1].Value)
  if ((& $node --version) -ne "v$nodeExpected") { throw "Node $nodeExpected is required." }
  if ((& $pnpm --version) -ne '12.8.1') { throw 'pnpm 12.8.1 is required.' }
  if (-not ((& $rustc --version) -like "rustc $rustExpected *")) { throw "Rust $rustExpected is required." }
  $env:OPENSSL_SRC_PERL = Get-NativePerl
  return @{ Node = $node; Pnpm = $pnpm; Cargo = $cargo; VisualStudio = (Assert-VisualStudioBuildTools) }
}

function Invoke-WindowsDesktop {
  param([string]$RequestedAction, [string]$Repository, [string]$TargetDirectory)
  $Repository = (Resolve-Path -LiteralPath $Repository).ProviderPath
  $TargetDirectory = [System.IO.Path]::GetFullPath($TargetDirectory)
  $app = Join-Path $TargetDirectory 'release\peppy-desktop.exe'
  Push-Location $Repository
  try {
    if ($RequestedAction -eq 'open') {
      if (-not (Test-Path -LiteralPath $app -PathType Leaf)) {
        throw "No current Windows executable at $app; run desktop-bundle first."
      }
      Start-Process -FilePath $app
      return
    }
    $tools = Assert-NativeToolchain -Repository $Repository
    $env:CARGO_TARGET_DIR = $TargetDirectory
    Invoke-Checked -File $tools.Pnpm -Arguments @('install', '--frozen-lockfile')
    if ($RequestedAction -eq 'dev') {
      Invoke-Checked -File $tools.Pnpm -Arguments @('--dir', 'apps/desktop', 'exec', 'tauri', 'dev', '--', '--locked')
    } else {
      Invoke-Checked -File $tools.Pnpm -Arguments @('--dir', 'apps/desktop', 'exec', 'tauri', 'build', '--bundles', 'nsis,msi', '--', '--locked')
    }
  } finally {
    Pop-Location
  }
}

if ($MyInvocation.InvocationName -ne '.') {
  try {
    Invoke-WindowsDesktop -RequestedAction $Action -Repository $RepoPath -TargetDirectory $CargoTargetDir
  } catch {
    [Console]::Error.WriteLine("Windows desktop setup requires native Node/pnpm/Rust, Visual Studio C++ Build Tools with the Windows SDK, WebView2 Runtime, and native Strawberry Perl. $($_.Exception.Message)")
    exit 1
  }
}
