param(
    [int]$ObservedPid,
    [string]$ObservedRoot = "",
    [string]$ObservedExe = "",
    [string]$HarnessLogPath = "",
    [string]$CommandLabel = ""
)

$ErrorActionPreference = "SilentlyContinue"
Set-StrictMode -Version Latest

$timer = [System.Diagnostics.Stopwatch]::StartNew()

while ($true) {
    $proc = Get-Process -Id $ObservedPid -ErrorAction SilentlyContinue
    if ($null -eq $proc) {
        break
    }

    $timestamp = (Get-Date).ToString("yyyy-MM-dd HH:mm:ss.fff")
    $line = "[$timestamp] [hb] Still running $CommandLabel elapsed_ms=$([math]::Round($timer.Elapsed.TotalMilliseconds, 2))"

    if ((-not [string]::IsNullOrWhiteSpace($ObservedRoot)) -and (-not [string]::IsNullOrWhiteSpace($ObservedExe))) {
        $statusOutput = & $ObservedExe status --root $ObservedRoot 2>&1
        $statusExit = $LASTEXITCODE
        $statusLines = @(
            $statusOutput |
                ForEach-Object { $_.ToString().Trim() } |
                Where-Object { $_ -ne "" }
        )
        if ($statusLines.Count -gt 0) {
            $line += " snapshot=$($statusLines -join ' | ')"
        } else {
            $line += " snapshot=status_exit=$statusExit no_output"
        }
    }

    Write-Host $line
    if (-not [string]::IsNullOrWhiteSpace($HarnessLogPath)) {
        Add-Content -LiteralPath $HarnessLogPath -Value $line -ErrorAction SilentlyContinue
    }

    Start-Sleep -Seconds 1
}
