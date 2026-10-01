param(
    [Parameter(Mandatory = $true)][ValidateSet('start', 'stop')][string]$Action,
    [Parameter(Mandatory = $true)][string]$EmulatorPath,
    [Parameter(Mandatory = $true)][string]$Avd,
    [int]$ProcessId = 0,
    [Int64]$StartedAt = 0
)

$ErrorActionPreference = 'Stop'
$resolved = (Resolve-Path -LiteralPath $EmulatorPath).ProviderPath
if ($Action -eq 'start') {
    $process = Start-Process -FilePath $resolved -ArgumentList @('-avd', $Avd) -PassThru
    [Console]::Out.WriteLine("{0}|{1}" -f $process.Id, $process.StartTime.ToUniversalTime().Ticks)
    exit 0
}
if ($ProcessId -le 0 -or $StartedAt -le 0) { throw 'ProcessId and StartedAt are required to stop an emulator process.' }
$process = Get-Process -Id $ProcessId -ErrorAction Stop
if ($process.Path -ne $resolved) { throw "Refusing to stop process $ProcessId because it is not $resolved" }
if ($process.StartTime.ToUniversalTime().Ticks -ne $StartedAt) { throw "Refusing to stop process $ProcessId because its identity changed" }
$children = Get-CimInstance Win32_Process | Where-Object { $_.ParentProcessId -eq $ProcessId } | ForEach-Object { $_.ProcessId }
foreach ($child in $children) { Stop-Process -Id $child -ErrorAction SilentlyContinue }
Stop-Process -Id $ProcessId -ErrorAction Stop
