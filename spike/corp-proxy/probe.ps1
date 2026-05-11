# Corp-proxy probe.
#
# Run from a real Baloise / corp-network Windows machine:
#
#     .\probe.ps1 | Tee-Object -FilePath probe-results.txt
#
# Reports which candidate auto-update delivery channels actually pass
# through the corp proxy. No credentials, no internal hostnames — safe
# to share the output.

$ErrorActionPreference = 'Continue'
$ProgressPreference    = 'SilentlyContinue'

# Test URLs. Each represents one delivery channel; see README.md for
# what each tells us. The byte sizes here are deliberately small so the
# probe finishes quickly even on slow links.
$cases = @(
    @{ Name = '1. GH Release binary (.tar.gz)';
       Url  = 'https://github.com/cli/cli/releases/download/v2.50.0/gh_2.50.0_windows_amd64.zip';
       ExpectBinary = $true },
    @{ Name = '2. GH API (JSON)';
       Url  = 'https://api.github.com/repos/cli/cli/releases/latest';
       ExpectBinary = $false },
    @{ Name = '3. raw.githubusercontent.com (text)';
       Url  = 'https://raw.githubusercontent.com/cli/cli/trunk/README.md';
       ExpectBinary = $false },
    @{ Name = '4. objects.githubusercontent.com (release blob CDN)';
       Url  = 'https://objects.githubusercontent.com/';
       ExpectBinary = $false },
    @{ Name = '5. workers.dev (CF Worker)';
       Url  = 'https://workers.cloudflare.com/';
       ExpectBinary = $false },
    @{ Name = '6. example.com (sanity)';
       Url  = 'https://example.com/';
       ExpectBinary = $false }
)

function Probe([string]$name, [string]$url, [bool]$expectBinary) {
    Write-Output "=== $name ==="
    Write-Output "URL: $url"
    $start = Get-Date
    try {
        $resp = Invoke-WebRequest -Uri $url -MaximumRedirection 10 `
            -UseBasicParsing -ErrorAction Stop -TimeoutSec 30
        $elapsed = ((Get-Date) - $start).TotalSeconds
        Write-Output ("status:        {0}" -f $resp.StatusCode)
        Write-Output ("final-url:     {0}" -f $resp.BaseResponse.ResponseUri)
        Write-Output ("content-type:  {0}" -f $resp.Headers['Content-Type'])
        Write-Output ("content-length:{0}" -f $resp.RawContentLength)
        Write-Output ("elapsed-sec:   {0:N2}" -f $elapsed)
        $head = $resp.Content
        if ($head -is [byte[]]) {
            $hex = ($head | Select-Object -First 16 | ForEach-Object { '{0:x2}' -f $_ }) -join ' '
            Write-Output ("first-16-bytes: $hex")
        } elseif ($head -is [string]) {
            $snippet = $head.Substring(0, [Math]::Min(120, $head.Length)) -replace "`r|`n", ' '
            Write-Output "first-120-chars: $snippet"
            if ($expectBinary -and $snippet -match '<html|<HTML|<\?xml') {
                Write-Output 'VERDICT: BLOCKED — got HTML where binary was expected (proxy interstitial?).'
            }
        }
        Write-Output 'VERDICT: PASS'
    } catch {
        Write-Output ("error:         {0}" -f $_.Exception.Message)
        if ($_.Exception.Response) {
            Write-Output ("status:        {0}" -f $_.Exception.Response.StatusCode.value__)
        }
        Write-Output 'VERDICT: FAIL'
    }
    Write-Output ''
}

Write-Output ('CatCast corp-proxy probe — {0}' -f (Get-Date -Format o))
Write-Output ('Host:                       {0}' -f $env:COMPUTERNAME)
Write-Output ('Proxy (HTTP_PROXY env):     {0}' -f $env:HTTP_PROXY)
Write-Output ('Proxy (HTTPS_PROXY env):    {0}' -f $env:HTTPS_PROXY)
Write-Output ('WinINet ProxyEnable / Server:')
try {
    $reg = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings'
    Write-Output ('  ProxyEnable={0}  ProxyServer={1}  AutoConfigURL={2}' `
        -f $reg.ProxyEnable, $reg.ProxyServer, $reg.AutoConfigURL)
} catch {
    Write-Output '  (could not read)'
}
Write-Output ''

foreach ($c in $cases) {
    Probe -name $c.Name -url $c.Url -expectBinary $c.ExpectBinary
}
