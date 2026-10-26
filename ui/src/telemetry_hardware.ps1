[Console]::OutputEncoding = [Text.UTF8Encoding]::new()
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$gpu = Get-CimInstance Win32_VideoController | Select-Object -First 1
$system = Get-CimInstance Win32_ComputerSystem
$gpuName = $gpu.Name
$gpuDriver = $gpu.DriverVersion
$vram = $null
$smi = Get-Command nvidia-smi -ErrorAction SilentlyContinue
if ($smi) {
    $line = & $smi.Source '--query-gpu=name,memory.total,driver_version' '--format=csv,noheader,nounits' 2>$null | Select-Object -First 1
    $fields = $line -split ','
    if ($fields.Length -eq 3 -and $fields[1].Trim() -match '^\d+$') {
        $gpuName = $fields[0].Trim()
        $vram = [int]$fields[1]
        $gpuDriver = $fields[2].Trim()
    }
}
@{
    cpu_name = $cpu.Name
    cpu_logical = [int]$system.NumberOfLogicalProcessors
    gpu_name = $gpuName
    gpu_driver = $gpuDriver
    gpu_vram_mb = $vram
    ram_gb = [int][math]::Round($system.TotalPhysicalMemory / 1GB)
    os_build = [Environment]::OSVersion.Version.Build
} | ConvertTo-Json -Compress
