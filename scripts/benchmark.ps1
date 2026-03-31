param(
    [string]$SourceRoot = "D:\lsp-cli",
    [string]$BenchRoot = "D:\bench\lsp-cli-bench",
    [string]$ProjectRoot = "D:\source-fast\source_fast",
    [string]$TargetDir = "D:\source-fast\source_fast\target-bench",
    [string]$Query = "wait_for_daemon_port",
    [string]$FileQuery = "pyproject",
    [int]$Runs = 5,
    [string[]]$Scenarios = @("cold", "warm", "search-file", "incremental-small", "incremental-large"),
    [string]$OutputPath = "",
    [string]$HarnessLogPath = "",
    [switch]$RebuildBenchRoot,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ([string]::IsNullOrWhiteSpace($HarnessLogPath)) {
    $HarnessLogPath = Join-Path $ProjectRoot "benchmark_harness.log"
}

$harnessLogDir = Split-Path -Parent $HarnessLogPath
if (-not [string]::IsNullOrWhiteSpace($harnessLogDir)) {
    New-Item -ItemType Directory -Path $harnessLogDir -Force | Out-Null
}
Set-Content -LiteralPath $HarnessLogPath -Value ""

function Write-Heartbeat {
    param([string]$Message)

    $timestamp = (Get-Date).ToString("yyyy-MM-dd HH:mm:ss.fff")
    $line = "[$timestamp] $Message"
    Write-Host $line
    Add-Content -LiteralPath $HarnessLogPath -Value $line
}

function Write-Step {
    param([string]$Message)
    Write-Heartbeat "==> $Message"
}

function Write-CommandOutput {
    param(
        [string]$Label,
        [string]$Text
    )

    if ([string]::IsNullOrWhiteSpace($Text)) {
        return
    }

    Add-Content -LiteralPath $HarnessLogPath -Value @(
        "----- $Label -----",
        $Text.TrimEnd(),
        "----- end $Label -----"
    )
}

function Start-HeartbeatSidecar {
    param(
        [int]$ObservedPid,
        [string]$ObservedRoot,
        [string]$ObservedExe,
        [string]$CommandLabel
    )

    $heartbeatScript = Join-Path $ProjectRoot "scripts\benchmark_heartbeat.ps1"
    if (-not (Test-Path -LiteralPath $heartbeatScript)) {
        return $null
    }

    $heartbeatArgs = @(
        "-NoProfile",
        "-ExecutionPolicy", "Bypass",
        "-File", $heartbeatScript,
        "-ObservedPid", $ObservedPid.ToString(),
        "-ObservedRoot", $ObservedRoot,
        "-ObservedExe", $ObservedExe,
        "-HarnessLogPath", $HarnessLogPath,
        "-CommandLabel", $CommandLabel
    )

    return Start-Process -FilePath "powershell.exe" -ArgumentList $heartbeatArgs -NoNewWindow -PassThru
}

function Get-SfExePath {
    param(
        [string]$Root,
        [string]$CargoTargetDir,
        [switch]$NoBuild
    )

    $exePath = Join-Path $CargoTargetDir "debug\sf.exe"
    if ((-not $NoBuild) -and (-not (Test-Path -LiteralPath $exePath))) {
        Write-Step "Building benchmark executable into $CargoTargetDir"
        $env:CARGO_TARGET_DIR = $CargoTargetDir
        try {
            cargo build -q -p source_fast --bin sf | Out-Null
        } finally {
            Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
        }
    }

    if (-not (Test-Path -LiteralPath $exePath)) {
        throw "Benchmark executable not found: $exePath"
    }

    return $exePath
}

function Invoke-Sf {
    param(
        [string]$ExePath,
        [string[]]$Arguments,
        [switch]$IgnoreExitCode,
        [switch]$Quiet
    )

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $ExePath
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.Environment["RUST_LOG"] = "trace"
    $escapedArgs = $Arguments | ForEach-Object {
        '"' + ($_ -replace '\\', '\\' -replace '"', '\"') + '"'
    }
    $psi.Arguments = ($escapedArgs -join " ")

    $displayArgs = $Arguments | ForEach-Object {
        if ($_ -match "\s") { '"' + $_ + '"' } else { $_ }
    }
    $commandLabel = "sf $($displayArgs -join ' ')"
    if (-not $Quiet) {
        Write-Heartbeat "Starting $commandLabel"
    }

    $rootPath = $null
    for ($i = 0; $i -lt $Arguments.Length - 1; $i++) {
        if ($Arguments[$i] -eq "--root") {
            $rootPath = $Arguments[$i + 1]
            break
        }
    }

    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $psi
    [void]$process.Start()
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $heartbeatProcess = $null
    if (-not $Quiet) {
        $heartbeatProcess = Start-HeartbeatSidecar -ObservedPid $process.Id -ObservedRoot $rootPath -ObservedExe $ExePath -CommandLabel $commandLabel
    }
    $process.WaitForExit()
    $stdout = $stdoutTask.GetAwaiter().GetResult()
    $stderr = $stderrTask.GetAwaiter().GetResult()
    $timer.Stop()

    if ($null -ne $heartbeatProcess) {
        [void]$heartbeatProcess.WaitForExit(2000)
        if (-not $heartbeatProcess.HasExited) {
            try {
                $heartbeatProcess.Kill()
            } catch {
            }
        }
    }

    Write-CommandOutput -Label "$commandLabel stdout" -Text $stdout
    Write-CommandOutput -Label "$commandLabel stderr" -Text $stderr
    if (-not $Quiet) {
        Write-Heartbeat "Finished $commandLabel exit=$($process.ExitCode) elapsed_ms=$([math]::Round($timer.Elapsed.TotalMilliseconds, 2))"
    }

    if ((-not $IgnoreExitCode) -and $process.ExitCode -ne 0) {
        throw "sf exited with code $($process.ExitCode). Args: $($Arguments -join ' ')`n$stderr$stdout"
    }

    $stdoutLines = @()
    if (-not [string]::IsNullOrEmpty($stdout)) {
        $stdoutLines = $stdout -split "`r?`n" | Where-Object { $_ -ne "" }
    }

    return [pscustomobject]@{
        ExitCode    = $process.ExitCode
        StdoutText  = $stdout
        StdoutLines = $stdoutLines
        StderrText  = $stderr
        ElapsedMs   = [math]::Round($timer.Elapsed.TotalMilliseconds, 2)
    }
}

function Get-StatusSnapshot {
    param(
        [string]$ExePath,
        [string]$Root
    )

    $status = Invoke-Sf -ExePath $ExePath -Arguments @("status", "--root", $Root) -IgnoreExitCode -Quiet
    if ($status.ExitCode -eq 0 -and -not [string]::IsNullOrWhiteSpace($status.StdoutText)) {
        return (($status.StdoutLines | ForEach-Object { $_.Trim() }) -join " | ")
    }

    if (-not [string]::IsNullOrWhiteSpace($status.StderrText)) {
        return "status_exit=$($status.ExitCode) stderr=$($status.StderrText.Trim())"
    }

    return "status_exit=$($status.ExitCode) no_output"
}

function Stop-BenchDaemon {
    param(
        [string]$ExePath,
        [string]$Root
    )

    $dbPath = Join-Path $Root ".source_fast\index.mdb"
    if (Test-Path -LiteralPath $dbPath) {
        Write-Heartbeat "Stopping benchmark daemon for $Root"
        Invoke-Sf -ExePath $ExePath -Arguments @("stop", "--root", $Root) -IgnoreExitCode | Out-Null
        Start-Sleep -Milliseconds 1500
    }
}

function Ensure-BenchRoot {
    param(
        [string]$ExePath,
        [string]$Source,
        [string]$Root,
        [switch]$Reset
    )

    if ($Reset -or -not (Test-Path -LiteralPath $Root)) {
        Write-Step "Preparing disposable benchmark root at $Root"
        Stop-BenchDaemon -ExePath $ExePath -Root $Root
        Remove-Item -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue
        Copy-Item -LiteralPath $Source -Destination $Root -Recurse -Force
    }
}

function Ensure-DaemonReady {
    param(
        [string]$ExePath,
        [string]$Root,
        [string]$WarmupQuery
    )

    Write-Heartbeat "Ensuring daemon readiness with warmup query '$WarmupQuery'"
    Invoke-Sf -ExePath $ExePath -Arguments @("search", "--root", $Root, $WarmupQuery) | Out-Null

    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    $nextHeartbeatMs = 0
    while ($deadline.Elapsed.TotalSeconds -lt 10) {
        $status = Invoke-Sf -ExePath $ExePath -Arguments @("status", "--root", $Root) -IgnoreExitCode -Quiet
        $statusText = $status.StdoutText
        if ($statusText -match "Leader:\s+pid:" -and $statusText -match "Index status:\s+complete") {
            Write-Heartbeat "Daemon ready after $([math]::Round($deadline.Elapsed.TotalMilliseconds, 2)) ms snapshot=$(Get-StatusSnapshot -ExePath $ExePath -Root $Root)"
            return
        }
        if ($deadline.ElapsedMilliseconds -ge $nextHeartbeatMs) {
            Write-Heartbeat "Waiting for daemon readiness elapsed_ms=$([math]::Round($deadline.Elapsed.TotalMilliseconds, 2)) snapshot=$(Get-StatusSnapshot -ExePath $ExePath -Root $Root)"
            $nextHeartbeatMs += 1000
        }
        Start-Sleep -Milliseconds 250
    }

    throw "Daemon did not become ready for benchmark root $Root. Last snapshot: $(Get-StatusSnapshot -ExePath $ExePath -Root $Root)"
}

function Measure-CommandMilliseconds {
    param([scriptblock]$Script)
    return [Math]::Round((Measure-Command $Script).TotalMilliseconds, 2)
}

function Measure-UntilSearchable {
    param(
        [string]$ExePath,
        [string]$Root,
        [string]$SearchQuery,
        [int]$TimeoutSeconds = 20
    )

    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    $nextHeartbeatMs = 0
    while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        $output = Invoke-Sf -ExePath $ExePath -Arguments @("search", "--root", $Root, $SearchQuery)
        if (($output.StdoutLines | Measure-Object).Count -gt 0) {
            Write-Heartbeat "Query '$SearchQuery' became searchable after command_elapsed_ms=$($output.ElapsedMs) wall_elapsed_ms=$([math]::Round($timer.Elapsed.TotalMilliseconds, 2))"
            return [Math]::Round($timer.Elapsed.TotalMilliseconds, 2)
        }
        if ($timer.ElapsedMilliseconds -ge $nextHeartbeatMs) {
            Write-Heartbeat "Waiting for query '$SearchQuery' elapsed_ms=$([math]::Round($timer.Elapsed.TotalMilliseconds, 2)) snapshot=$(Get-StatusSnapshot -ExePath $ExePath -Root $Root)"
            $nextHeartbeatMs += 1000
        }
        Start-Sleep -Milliseconds 100
    }

    throw "Timed out waiting for query '$SearchQuery' to become searchable in $Root. Last snapshot: $(Get-StatusSnapshot -ExePath $ExePath -Root $Root)"
}

function Summarize-Runs {
    param([double[]]$Values)

    $sorted = @($Values | Sort-Object)
    $mean = ($Values | Measure-Object -Average).Average
    $sumSqDiff = ($Values | ForEach-Object { ($_ - $mean) * ($_ - $mean) } | Measure-Object -Sum).Sum
    $stddev = [Math]::Round([Math]::Sqrt($sumSqDiff / $Values.Count), 2)
    return [ordered]@{
        runs = @($Values)
        min = $sorted[0]
        median = $sorted[[int][Math]::Floor($sorted.Count / 2)]
        max = $sorted[-1]
        mean = [Math]::Round($mean, 2)
        stddev = $stddev
    }
}

function Run-ColdScenario {
    param(
        [string]$ExePath,
        [string]$Root,
        [string]$SearchQuery,
        [int]$Count
    )

    $runs = New-Object System.Collections.Generic.List[Double]
    for ($i = 0; $i -lt $Count; $i++) {
        Write-Heartbeat "Cold scenario run $($i + 1)/$Count starting"
        Stop-BenchDaemon -ExePath $ExePath -Root $Root
        Remove-Item -LiteralPath (Join-Path $Root ".source_fast") -Recurse -Force -ErrorAction SilentlyContinue
        $ms = Measure-CommandMilliseconds { Invoke-Sf -ExePath $ExePath -Arguments @("search", "--root", $Root, "--wait", $SearchQuery) | Out-Null }
        Write-Heartbeat "Cold scenario run $($i + 1)/$Count finished elapsed_ms=$ms"
        $runs.Add($ms)
    }
    return (Summarize-Runs -Values $runs.ToArray())
}

function Run-WarmScenario {
    param(
        [string]$ExePath,
        [string]$Root,
        [string]$SearchQuery,
        [int]$Count
    )

    Ensure-DaemonReady -ExePath $ExePath -Root $Root -WarmupQuery $SearchQuery
    $runs = New-Object System.Collections.Generic.List[Double]
    for ($i = 0; $i -lt $Count; $i++) {
        Write-Heartbeat "Warm scenario run $($i + 1)/$Count starting"
        $ms = Measure-CommandMilliseconds { Invoke-Sf -ExePath $ExePath -Arguments @("search", "--root", $Root, $SearchQuery) | Out-Null }
        Write-Heartbeat "Warm scenario run $($i + 1)/$Count finished elapsed_ms=$ms"
        $runs.Add($ms)
    }
    return (Summarize-Runs -Values $runs.ToArray())
}

function Run-SearchFileScenario {
    param(
        [string]$ExePath,
        [string]$Root,
        [string]$Pattern,
        [int]$Count
    )

    Ensure-DaemonReady -ExePath $ExePath -Root $Root -WarmupQuery $Query
    $runs = New-Object System.Collections.Generic.List[Double]
    for ($i = 0; $i -lt $Count; $i++) {
        Write-Heartbeat "Search-file scenario run $($i + 1)/$Count starting"
        $ms = Measure-CommandMilliseconds { Invoke-Sf -ExePath $ExePath -Arguments @("search-file", "--root", $Root, $Pattern) | Out-Null }
        Write-Heartbeat "Search-file scenario run $($i + 1)/$Count finished elapsed_ms=$ms"
        $runs.Add($ms)
    }
    return (Summarize-Runs -Values $runs.ToArray())
}

function Run-IncrementalSmallScenario {
    param(
        [string]$ExePath,
        [string]$Root,
        [int]$Count
    )

    Ensure-DaemonReady -ExePath $ExePath -Root $Root -WarmupQuery $Query
    $targetFile = Join-Path $Root "pyproject.toml"
    $runs = New-Object System.Collections.Generic.List[Double]

    for ($i = 1; $i -le $Count; $i++) {
        $token = "sf_small_bench_${i}_$([guid]::NewGuid().ToString('N'))"
        Write-Heartbeat "Incremental-small run $i/$Count mutating $targetFile token=$token"
        Add-Content -LiteralPath $targetFile -Value "`n# $token"
        $ms = Measure-UntilSearchable -ExePath $ExePath -Root $Root -SearchQuery $token
        Write-Heartbeat "Incremental-small run $i/$Count finished elapsed_ms=$ms"
        $runs.Add($ms)
    }

    return (Summarize-Runs -Values $runs.ToArray())
}

function Run-IncrementalLargeScenario {
    param(
        [string]$ExePath,
        [string]$Root,
        [int]$Count
    )

    Ensure-DaemonReady -ExePath $ExePath -Root $Root -WarmupQuery $Query
    $runs = New-Object System.Collections.Generic.List[Double]

    for ($i = 1; $i -le $Count; $i++) {
        $token = "sf_large_bench_${i}_$([guid]::NewGuid().ToString('N'))"
        $largeFile = Join-Path $Root "sf_large_bench_$i.py"
        $content = @("# $token")
        $content += 1..4000 | ForEach-Object { "value_$($_) = '$token filler line $_'" }
        Write-Heartbeat "Incremental-large run $i/$Count writing $largeFile token=$token"
        Set-Content -LiteralPath $largeFile -Value $content
        $ms = Measure-UntilSearchable -ExePath $ExePath -Root $Root -SearchQuery $token
        Write-Heartbeat "Incremental-large run $i/$Count finished elapsed_ms=$ms"
        $runs.Add($ms)
    }

    return (Summarize-Runs -Values $runs.ToArray())
}

$scenarioSet = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
foreach ($scenario in $Scenarios) {
    foreach ($part in ($scenario -split ",")) {
        $trimmed = $part.Trim()
        if ($trimmed.Length -gt 0) {
            [void]$scenarioSet.Add($trimmed)
        }
    }
}

$exe = Get-SfExePath -Root $ProjectRoot -CargoTargetDir $TargetDir -NoBuild:$SkipBuild
Write-Step "Harness log path: $HarnessLogPath"
Write-Step "Using RUST_LOG=trace for benchmark child processes"
Ensure-BenchRoot -ExePath $exe -Source $SourceRoot -Root $BenchRoot -Reset:$RebuildBenchRoot

$result = [ordered]@{
    benchmark_root = $BenchRoot
    source_repo = $SourceRoot
    query = $Query
    file_query = $FileQuery
    scenarios = @($scenarioSet)
    generated_at = (Get-Date).ToString("o")
}

if ($scenarioSet.Contains("cold")) {
    Write-Step "Running cold benchmark"
    $result.cold = Run-ColdScenario -ExePath $exe -Root $BenchRoot -SearchQuery $Query -Count $Runs
}

if ($scenarioSet.Contains("warm")) {
    Write-Step "Running warm search benchmark"
    $result.warm = Run-WarmScenario -ExePath $exe -Root $BenchRoot -SearchQuery $Query -Count $Runs
}

if ($scenarioSet.Contains("search-file")) {
    Write-Step "Running search-file benchmark"
    $result.search_file = Run-SearchFileScenario -ExePath $exe -Root $BenchRoot -Pattern $FileQuery -Count $Runs
}

if ($scenarioSet.Contains("incremental-small")) {
    Write-Step "Running incremental-small benchmark"
    $result.incremental_small = Run-IncrementalSmallScenario -ExePath $exe -Root $BenchRoot -Count $Runs
}

if ($scenarioSet.Contains("incremental-large")) {
    Write-Step "Running incremental-large benchmark"
    $result.incremental_large = Run-IncrementalLargeScenario -ExePath $exe -Root $BenchRoot -Count $Runs
}

$json = $result | ConvertTo-Json -Depth 8

if ($OutputPath) {
    $json | Set-Content -LiteralPath $OutputPath
    Write-Step "Wrote benchmark results to $OutputPath"
} else {
    $json
}
