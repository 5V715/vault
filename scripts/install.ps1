# SPDX-License-Identifier: MIT
<#
.SYNOPSIS
Installs vaultctl, vault-dev and vault-relay from a signed GitHub release of 5V715/vault (Windows).

.DESCRIPTION
Downloads over HTTPS from github.com (no login, no GitHub CLI), then verifies before
installing:
  1. SHA256SUMS.sig is a valid `ssh-keygen -Y` signature over SHA256SUMS by the release
     key embedded below (principal release@vault, namespace vault-release);
  2. the archive's SHA-256 matches its single entry in SHA256SUMS.
Only then is the archive extracted (in a temporary directory) and the three binaries
installed to -Dir (default %LOCALAPPDATA%\vault\bin), which is added to the user PATH
once. Any failure installs nothing; the temporary directory is always removed.
Needs ssh-keygen.exe (OpenSSH 8.2 or later: the Windows 10/11 OpenSSH client or Git for
Windows). Downloads go only to github.com and GitHub's release-asset hosts, over TLS 1.2+
only; any other redirect is refused. Without -Version, one or two unauthenticated GitHub
API requests find the latest stable release. Works in Windows PowerShell 5.1 and
PowerShell 7. See docs/install.md.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\install.ps1
.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Version v1.2.3 -Dir C:\tools\vault
#>
[CmdletBinding()]
param(
    # A release tag vX.Y.Z (or vX.Y.Z-suffix); default: the latest stable release.
    [string]$Version,
    # Install directory; default: %LOCALAPPDATA%\vault\bin.
    [string]$Dir
)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = '5V715/vault'
$Target = 'x86_64-pc-windows-msvc'
$Binaries = @('vaultctl.exe', 'vault-dev.exe', 'vault-relay.exe')
# \z, not $: .NET's $ also matches before a trailing newline.
$VersionPattern = '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?\z'
$StablePattern = '^v[0-9]+\.[0-9]+\.[0-9]+\z'
$Web = 'https://github.com'
$Api = 'https://api.github.com'
# The hosts a download may be served from: github.com redirects release assets to these.
$AssetHosts = @('github.com', 'objects.githubusercontent.com', 'release-assets.githubusercontent.com')

# The release trust root: must equal keys/allowed_signers (scripts/test/trust_root_test.sh
# enforces it). Replace it only together with keys/release.pub and keys/allowed_signers.
# BEGIN VAULT RELEASE ALLOWED SIGNERS
$EmbeddedSigners = 'PLACEHOLDER — owner must replace'
# END VAULT RELEASE ALLOWED SIGNERS

function Fail([string]$Message) {
    throw "install.ps1: $Message"
}

function ConvertTo-Argument([string]$Value) {
    if ($Value -eq '') { return '""' }
    if ($Value -notmatch '[\s"]') { return $Value }
    return '"' + ($Value -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
}

# Runs a program without the shell and captures its output. With -StdinFile, stdin is
# the file itself (Start-Process opens it as the child's handle): Windows PowerShell 5.1
# re-encodes piped text, and .NET Framework's redirected StandardInput writer appends
# its encoding preamble on close, either of which changes the signed bytes.
function Invoke-Native([string]$File, [string[]]$Arguments, [string]$StdinFile) {
    $argLine = (@($Arguments) | ForEach-Object { ConvertTo-Argument $_ }) -join ' '
    if ($StdinFile) {
        $dir = Split-Path -Parent $StdinFile
        $o = Join-Path $dir '.native-stdout'
        $e = Join-Path $dir '.native-stderr'
        try {
            $p = Start-Process -FilePath $File -ArgumentList $argLine -NoNewWindow -Wait -PassThru `
                -RedirectStandardInput $StdinFile -RedirectStandardOutput $o -RedirectStandardError $e
            return [pscustomobject]@{ ExitCode = $p.ExitCode; Out = [IO.File]::ReadAllText($o); Err = [IO.File]::ReadAllText($e) }
        } finally {
            Remove-Item -LiteralPath $o, $e -Force -ErrorAction SilentlyContinue
        }
    }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $File
    $psi.Arguments = $argLine
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    $p.WaitForExit()
    return [pscustomobject]@{ ExitCode = $p.ExitCode; Out = $out.Result; Err = $err.Result }
}

function Find-Program([string]$Name) {
    # -CommandType Application: `gh` is also a PowerShell alias of Get-Help.
    $c = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $c) { Fail "$Name is required but not installed" }
    return $c.Source
}

function Get-NormalizedPath([string]$Path) {
    return [Environment]::ExpandEnvironmentVariables($Path).Trim().TrimEnd('\')
}

function Test-PathListContains([string]$List, [string]$Entry) {
    $want = Get-NormalizedPath $Entry
    foreach ($e in ($List -split ';')) {
        if ($e.Trim() -ne '' -and (Get-NormalizedPath $e) -ieq $want) { return $true }
    }
    return $false
}

# Adds $Entry to the user PATH (HKCU\Environment) unless an equivalent entry is there;
# keeps the other entries unexpanded and the value's registry kind.
function Add-UserPath([string]$Entry) {
    $keyName = 'Environment'
    $testKey = $false
    if ($env:VAULT_INSTALL_TEST -ceq '1' -and $env:VAULT_INSTALL_TEST_PATH_KEY) {
        $keyName = $env:VAULT_INSTALL_TEST_PATH_KEY
        $testKey = $true
    }
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($keyName)
    try {
        $current = ''
        $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        if ($key.GetValueNames() -contains 'Path') {
            $current = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            if ($key.GetValueKind('Path') -eq [Microsoft.Win32.RegistryValueKind]::String) { $kind = [Microsoft.Win32.RegistryValueKind]::String }
        }
        if (Test-PathListContains $current $Entry) { return }
        if ($current.Trim() -eq '') { $new = $Entry } else { $new = $current.TrimEnd(';') + ';' + $Entry }
        $key.SetValue('Path', $new, $kind)
        Write-Host "added $Entry to your user PATH (new terminals pick it up)"
    } finally {
        $key.Close()
    }
    if (-not $testKey) {
        # Tell Explorer (and so new terminals) that the environment changed. Best effort.
        try {
            if (-not ('VaultInstall.Native' -as [type])) {
                Add-Type -Namespace VaultInstall -Name Native -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
            }
            $result = [UIntPtr]::Zero
            [void][VaultInstall.Native]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
        } catch {
            Write-Host 'note: could not announce the PATH change; sign out and in if new terminals do not see it'
        }
    }
}

# --- arguments and trust root (nothing is downloaded before these pass)

if ($PSBoundParameters.ContainsKey('Version') -and $Version -cnotmatch $VersionPattern) {
    Fail "invalid version '$Version' (expected vX.Y.Z)"
}

# The test override is honoured only in test mode (VAULT_INSTALL_TEST=1).
$Signers = $EmbeddedSigners
if ($env:VAULT_INSTALL_TEST -ceq '1' -and $env:VAULT_INSTALL_TEST_ALLOWED_SIGNERS) {
    $Signers = $env:VAULT_INSTALL_TEST_ALLOWED_SIGNERS
}
if ($Signers -cnotmatch '^release@vault namespaces="vault-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5[A-Za-z0-9+/]+=*\z') {
    Fail 'release key not configured: this copy of install.ps1 has no release key (the owner has not created one yet, see docs/install.md). Nothing was installed.'
}

$onWindows = ($PSVersionTable.PSEdition -eq 'Desktop') -or ((Test-Path variable:IsWindows) -and $IsWindows)
if (-not $onWindows) { Fail 'install.ps1 is for Windows; use install.sh on Linux and macOS' }
$arch = $env:PROCESSOR_ARCHITEW6432
if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64') { Fail "unsupported platform Windows/$arch (prebuilt: Windows x86_64; build from source instead)" }

$SshKeygen = Find-Program 'ssh-keygen'

# Test mode only: a local HTTPS stand-in for github.com (scripts/test/fake-github).
if ($env:VAULT_INSTALL_TEST -ceq '1' -and $env:VAULT_INSTALL_TEST_BASE_URL) {
    if ($env:VAULT_INSTALL_TEST_BASE_URL -cnotmatch '^https://[^/@]+(/[A-Za-z0-9._-]+)*\z') { Fail 'VAULT_INSTALL_TEST_BASE_URL must be an https URL' }
    $Web = $env:VAULT_INSTALL_TEST_BASE_URL
    $Api = "$Web/api"
    $AssetHosts = @('localhost', '127.0.0.1')
    if ($env:VAULT_INSTALL_TEST_ASSET_HOSTS) { $AssetHosts = @($env:VAULT_INSTALL_TEST_ASSET_HOSTS -split ' ') }
}

# HTTPS: System.Net.Http with redirects followed by hand (each hop is checked), the same in
# Windows PowerShell 5.1 and PowerShell 7.
Add-Type -AssemblyName System.Net.Http
if ($PSVersionTable.PSEdition -eq 'Desktop') {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
}
$HttpHandler = New-Object System.Net.Http.HttpClientHandler
$HttpHandler.AllowAutoRedirect = $false
$Http = New-Object System.Net.Http.HttpClient($HttpHandler)
$Http.Timeout = [TimeSpan]::FromMinutes(10)

function Test-AllowedUrl([Uri]$Uri) {
    if ($Uri.Scheme -cne 'https' -or $Uri.UserInfo) { return $false }
    if ($AssetHosts -contains $Uri.Host) { return $true }
    return $Uri.AbsoluteUri.StartsWith("$Web/", [StringComparison]::Ordinal) -or $Uri.AbsoluteUri.StartsWith("$Api/", [StringComparison]::Ordinal)
}

# GET over TLS into $OutFile, following at most 3 redirects, each of which must stay https
# and on $AssetHosts. Returns the final status (200 = written) and whether a 403/429 was the
# API rate limit. Connection and TLS failures stop the install.
function Get-Https([string]$Url, [string]$OutFile, [switch]$ApiRequest) {
    $uri = [Uri]$Url
    for ($hop = 0; ; $hop++) {
        if (-not (Test-AllowedUrl $uri)) { Fail "refusing to download from $uri (not GitHub or not https); nothing was installed" }
        $req = New-Object System.Net.Http.HttpRequestMessage([System.Net.Http.HttpMethod]::Get, $uri)
        [void]$req.Headers.TryAddWithoutValidation('User-Agent', 'vault-install')
        if ($ApiRequest) {
            [void]$req.Headers.TryAddWithoutValidation('Accept', 'application/vnd.github+json')
            [void]$req.Headers.TryAddWithoutValidation('X-GitHub-Api-Version', '2022-11-28')
        }
        try {
            $resp = $Http.SendAsync($req, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
        } catch {
            Fail "could not connect to $($uri.GetLeftPart([UriPartial]::Path)): $($_.Exception.GetBaseException().Message)"
        }
        try {
            $code = [int]$resp.StatusCode
            if ($code -eq 200) {
                $in = $resp.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
                $fs = [IO.File]::Create($OutFile)
                try { $in.CopyTo($fs) } finally { $fs.Dispose(); $in.Dispose() }
                return [pscustomobject]@{ Status = 200; RateLimited = $false }
            }
            if (@(301, 302, 303, 307, 308) -contains $code) {
                if ($hop -ge 3) { Fail "too many redirects for $($uri.GetLeftPart([UriPartial]::Path))" }
                $loc = $resp.Headers.Location
                if (-not $loc) { Fail "redirect without a location for $($uri.GetLeftPart([UriPartial]::Path))" }
                if (-not $loc.IsAbsoluteUri) { $loc = New-Object Uri($uri, $loc) }
                $uri = $loc
                continue
            }
            $limited = $false
            $vals = $null
            if (($code -eq 403 -or $code -eq 429) -and $resp.Headers.TryGetValues('X-RateLimit-Remaining', [ref]$vals)) {
                $limited = @($vals) -contains '0'
            }
            return [pscustomobject]@{ Status = $code; RateLimited = $limited }
        } finally {
            $resp.Dispose()
        }
    }
}

# An unauthenticated GitHub API request: the JSON text, or $null for 404. (The caller
# parses it: a function returning an array would unroll it.)
function Get-Api([string]$Path, [string]$OutFile) {
    $r = Get-Https "$Api/repos/$Repo/$Path" $OutFile -ApiRequest
    if ($r.Status -eq 200) { return [IO.File]::ReadAllText($OutFile) }
    if ($r.RateLimited) {
        Fail 'the GitHub API rate limit for your address is used up (60 requests per hour without login); wait for it to reset, or name the release: -Version vX.Y.Z (needs no API request)'
    }
    if ($r.Status -eq 404) { return $null }
    Fail "could not look up the releases of $Repo (HTTP $($r.Status))"
}

function Test-StableRelease($Release) {
    if ($null -eq $Release) { return $false }
    $names = @($Release.PSObject.Properties | ForEach-Object Name)
    if (-not ($names -contains 'tag_name' -and $names -contains 'draft' -and $names -contains 'prerelease')) { return $false }
    return ([string]$Release.tag_name -cmatch $StablePattern) -and ($Release.draft -eq $false) -and ($Release.prerelease -eq $false)
}

if (-not $Dir) {
    if (-not $env:LOCALAPPDATA) { Fail 'LOCALAPPDATA is not set; pass -Dir' }
    $Dir = Join-Path $env:LOCALAPPDATA 'vault\bin'
}
$Dir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Dir)

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ('vault-install-' + [guid]::NewGuid().ToString('N'))
$Staged = @()
$Backups = @()     # (destination, moved-aside previous version) pairs
$Placed = @()      # destinations this run renamed a new binary into
$Installed = $false
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    $Dl = Join-Path $Tmp 'dl'
    $X = Join-Path $Tmp 'x'
    $HttpDir = Join-Path $Tmp 'http'
    New-Item -ItemType Directory -Path $Dl, $X, $HttpDir | Out-Null

    if (-not $PSBoundParameters.ContainsKey('Version')) {
        # The latest stable release: published (no drafts), not marked as a pre-release, and
        # a plain vX.Y.Z tag (an unmarked -rc tag is skipped too). The one GitHub marks
        # latest if it is one; otherwise the newest such release. Pre-releases install only
        # with -Version.
        $Version = $null
        $text = Get-Api 'releases/latest' (Join-Path $HttpDir 'latest.json')
        if ($null -ne $text) {
            $latest = ConvertFrom-Json $text
            if (Test-StableRelease $latest) { $Version = [string]$latest.tag_name }
        }
        if (-not $Version) {
            $text = Get-Api 'releases?per_page=100' (Join-Path $HttpDir 'releases.json')
            if ($null -eq $text) { Fail "could not list the releases of $Repo (HTTP 404)" }
            # Windows PowerShell 5.1 returns a JSON array as one object; foreach over the
            # variable enumerates it in both editions.
            $list = ConvertFrom-Json $text
            foreach ($rel in $list) {
                if (Test-StableRelease $rel) { $Version = [string]$rel.tag_name; break }
            }
        }
        if (-not $Version) { Fail "no published stable release of $Repo found (pre-releases install only with -Version)" }
    }
    $Archive = "vault-$($Version.Substring(1))-$Target.zip"
    function Get-Asset([string]$Name) {
        Get-Https "$Web/$Repo/releases/download/$Version/$Name" (Join-Path $Dl $Name)
    }
    Write-Host "vault $Version for $Target"

    # 1. The signature over SHA256SUMS, before anything else is downloaded.
    $Sums = Join-Path $Dl 'SHA256SUMS'
    $Sig = Join-Path $Dl 'SHA256SUMS.sig'
    $r = Get-Asset 'SHA256SUMS'
    if ($r.Status -eq 404) { Fail "release $Version not found (or it has no SHA256SUMS)" }
    if ($r.Status -ne 200) { Fail "could not download SHA256SUMS of release $Version (HTTP $($r.Status))" }
    $r = Get-Asset 'SHA256SUMS.sig'
    if ($r.Status -eq 404) { Fail "release $Version is not signed (no SHA256SUMS.sig); nothing was installed" }
    if ($r.Status -ne 200) { Fail "could not download SHA256SUMS.sig of release $Version (HTTP $($r.Status))" }
    $SignersFile = Join-Path $Tmp 'allowed_signers'
    [IO.File]::WriteAllText($SignersFile, $Signers + "`n")
    $r = Invoke-Native $SshKeygen @('-Y', 'verify', '-f', $SignersFile, '-I', 'release@vault', '-n', 'vault-release', '-s', $Sig) $Sums
    if ($r.ExitCode -ne 0) {
        Write-Host (($r.Out + $r.Err).Trim())
        Fail "SHA256SUMS.sig signature check failed: release $Version was not signed by the vault release key; nothing was installed"
    }
    Write-Host 'signature: ok (release@vault)'

    # 2. The archive's hash: exactly one SHA256SUMS line must name it.
    $expected = $null
    $count = 0
    foreach ($line in ([IO.File]::ReadAllText($Sums) -split "`n")) {
        $line = $line.TrimEnd("`r")
        if ($line -eq '') { continue }
        if (-not ($line -cmatch '^([0-9a-fA-F]{64}) [ *](.+)\z')) { Fail "SHA256SUMS has a malformed line: $line" }
        if ($Matches[2] -ceq $Archive) {
            $expected = $Matches[1].ToLowerInvariant()
            $count++
        }
    }
    if ($count -eq 0) { Fail "SHA256SUMS does not list $Archive (no build for $Target in $($Version)?)" }
    if ($count -ne 1) { Fail "SHA256SUMS lists $Archive more than once; refusing an ambiguous release" }
    $Zip = Join-Path $Dl $Archive
    $r = Get-Asset $Archive
    if ($r.Status -ne 200 -or -not (Test-Path -LiteralPath $Zip -PathType Leaf)) {
        Fail "could not download $Archive (HTTP $($r.Status))"
    }
    $actual = (Get-FileHash -LiteralPath $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -cne $expected) {
        Fail "$Archive does not match SHA256SUMS (got $actual, signed $expected); nothing was installed"
    }
    Write-Host "sha256: ok ($Archive)"

    # 3. Extract (verified content only) and check the three binaries are there.
    Expand-Archive -LiteralPath $Zip -DestinationPath $X
    foreach ($b in $Binaries) {
        $p = Join-Path $X $b
        if (-not (Test-Path -LiteralPath $p -PathType Leaf) -or
            ((Get-Item -LiteralPath $p -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            Fail "$Archive is missing $b; nothing was installed"
        }
    }

    # 4. Install, all or nothing: stage every binary next to its destination, move every
    #    previous version aside (a running .exe can usually be renamed, not overwritten),
    #    rename every new one into place; on any failure `finally` removes what was placed
    #    and restores the previous versions.
    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    foreach ($b in $Binaries) {
        $d = Join-Path $Dir $b
        if ((Test-Path -LiteralPath $d) -and -not (Test-Path -LiteralPath $d -PathType Leaf)) {
            Fail "$d exists and is not a regular file; nothing was installed"
        }
    }
    $Suffix = '.vault-install-' + [guid]::NewGuid().ToString('N')
    foreach ($b in $Binaries) {
        $s = Join-Path $Dir ".$b$Suffix"
        $Staged += $s
        [IO.File]::Copy((Join-Path $X $b), $s, $false)
    }
    foreach ($b in $Binaries) {
        $d = Join-Path $Dir $b
        if (Test-Path -LiteralPath $d) {
            $old = Join-Path $Dir ".$b.vault-old$Suffix"
            try {
                [IO.File]::Move($d, $old)
            } catch {
                Fail "could not move $d aside ($($_.Exception.Message)); if it is running, stop it and retry. Nothing was installed"
            }
            $Backups += , @($d, $old)
        }
    }
    foreach ($b in $Binaries) {
        $d = Join-Path $Dir $b
        if ($env:VAULT_INSTALL_TEST -ceq '1' -and $env:VAULT_INSTALL_TEST_FAIL_RENAME -and "$($env:VAULT_INSTALL_TEST_FAIL_RENAME).exe" -ceq $b) {
            Fail "could not install $d (test hook); the previous version was restored"
        }
        [IO.File]::Move((Join-Path $Dir ".$b$Suffix"), $d)
        $Placed += $d
    }
    $Installed = $true
    $Staged = @()
    foreach ($pair in $Backups) {
        try { [IO.File]::Delete($pair[1]) } catch {
            Write-Host "note: the previous $($pair[0]) is still running; delete $($pair[1]) once it has exited"
        }
    }
    Write-Host "installed $($Binaries -join ' ') ($Version) to $Dir"
} finally {
    if (-not $Installed) {
        # Roll back: remove what this run placed, then put the previous versions back.
        foreach ($p in $Placed) { Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue }
        foreach ($pair in $Backups) {
            try { [IO.File]::Move($pair[1], $pair[0]) } catch {
                Write-Host "install.ps1: error: could not restore $($pair[0]) from $($pair[1])"
            }
        }
    }
    foreach ($s in $Staged) { Remove-Item -LiteralPath $s -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Add-UserPath $Dir
if (-not (Test-PathListContains $env:PATH $Dir)) { $env:PATH = $env:PATH.TrimEnd(';') + ';' + $Dir }
