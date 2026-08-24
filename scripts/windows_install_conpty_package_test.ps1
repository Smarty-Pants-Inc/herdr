param(
    [Parameter(Mandatory = $true)]
    [string]$ArchivePath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$installerPath = (Resolve-Path -LiteralPath "$PSScriptRoot\..\website\install.ps1").Path
$bootstrapPath = (Resolve-Path -LiteralPath "$PSScriptRoot\..\website\install.cmd").Path
$bootstrapContent = Get-Content -LiteralPath $bootstrapPath -Raw
foreach ($forbiddenCommand in @("Invoke-RestMethod", "Invoke-WebRequest", "Invoke-Expression", "iex")) {
    if ($bootstrapContent -match "(?i)\b$forbiddenCommand\b") {
        throw "CMD bootstrap uses forbidden PowerShell network execution: $forbiddenCommand"
    }
}
if ($bootstrapContent -notmatch "(?i)\bcurl\.exe\b") {
    throw "CMD bootstrap does not download through curl.exe"
}
$parseErrors = $null
$tokens = $null
$installerAst = [System.Management.Automation.Language.Parser]::ParseFile(
    $installerPath,
    [ref]$tokens,
    [ref]$parseErrors
)
if ($parseErrors.Count -ne 0) {
    throw ($parseErrors | Out-String)
}
$forbiddenPowerShellCommands = @(
    $installerAst.FindAll(
        { param($node) $node -is [System.Management.Automation.Language.CommandAst] },
        $true
    ) |
        ForEach-Object { $_.GetCommandName() } |
        Where-Object {
            $_ -in @("Invoke-RestMethod", "Invoke-WebRequest", "Invoke-Expression", "irm", "iwr", "iex")
        }
)
if ($forbiddenPowerShellCommands.Count -ne 0) {
    throw "installer uses forbidden PowerShell network execution: $($forbiddenPowerShellCommands -join ', ')"
}
foreach ($functionName in @("Prepend-PathEntry", "Update-PathRegistryEntry")) {
    $definition = $installerAst.FindAll(
        {
            param($node)
            $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
                $node.Name -eq $functionName
        },
        $true
    ) | Select-Object -First 1
    if ($null -eq $definition) {
        throw "installer is missing function $functionName"
    }
    Invoke-Expression $definition.Extent.Text
}

$pathTestVariable = "HERDR_INSTALLER_PATH_TEST"
$oldPathTestVariable = [Environment]::GetEnvironmentVariable($pathTestVariable, "Process")
$testRegistryPath = "Software\HerdrInstallerTests-$([Guid]::NewGuid().ToString('N'))"
$testEnvironmentKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($testRegistryPath)
if ($null -eq $testEnvironmentKey) {
    throw "unable to create temporary installer test registry key"
}
try {
    [Environment]::SetEnvironmentVariable($pathTestVariable, "C:\expanded", "Process")
    $testEnvironmentKey.SetValue(
        "Path",
        "%$pathTestVariable%\bin;C:\existing",
        [Microsoft.Win32.RegistryValueKind]::ExpandString
    )
    $pathChanged = Update-PathRegistryEntry -EnvironmentKey $testEnvironmentKey -Entry "C:\Herdr\bin"
    if (-not $pathChanged) {
        throw "installer PATH update reported no change"
    }
    if (Update-PathRegistryEntry -EnvironmentKey $testEnvironmentKey -Entry "C:\Herdr\bin") {
        throw "installer PATH update was not idempotent"
    }

    $options = [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
    $rawPath = $testEnvironmentKey.GetValue("Path", $null, $options)
    $expectedPath = "C:\Herdr\bin;%$pathTestVariable%\bin;C:\existing"
    if ($rawPath -cne $expectedPath) {
        throw "installer changed raw PATH: expected '$expectedPath', got '$rawPath'"
    }
    if ($testEnvironmentKey.GetValueKind("Path") -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) {
        throw "installer changed the PATH registry value kind"
    }
} finally {
    $testEnvironmentKey.Dispose()
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($testRegistryPath, $false)
    [Environment]::SetEnvironmentVariable($pathTestVariable, $oldPathTestVariable, "Process")
}
function Get-ReleaseDirectoryForIdentity {
    param(
        [string]$ReleasesDir,
        [string]$VersionIdentity,
        [string]$ExpectedSha256
    )

    $releaseMatches = @()
    foreach ($releaseDir in @(Get-ChildItem -LiteralPath $ReleasesDir -Directory)) {
        if ($releaseDir.Name -notmatch '^r-[0-9a-f]{16}-x86_64-pc-windows-msvc$') {
            continue
        }
        $metadataPath = Join-Path $releaseDir.FullName ".herdr-release.json"
        if (-not (Test-Path -LiteralPath $metadataPath -PathType Leaf)) {
            continue
        }
        try {
            $metadata = Get-Content -LiteralPath $metadataPath -Raw | ConvertFrom-Json
        } catch {
            continue
        }
        if ([string]$metadata.version_identity -cne $VersionIdentity) {
            continue
        }
        $sha256 = [System.Security.Cryptography.SHA256]::Create()
        try {
            $digest = [System.BitConverter]::ToString(
                $sha256.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($VersionIdentity))
            ).Replace("-", "").ToLowerInvariant()
        } finally {
            $sha256.Dispose()
        }
        if ($releaseDir.Name -cne "r-$($digest.Substring(0, 16))-x86_64-pc-windows-msvc") {
            throw "installer did not derive the short release key from $VersionIdentity"
        }

        if (
            @($metadata.PSObject.Properties).Count -ne 5 -or
            [int]$metadata.schema_version -ne 1 -or
            [string]$metadata.target_triple -cne "x86_64-pc-windows-msvc" -or
            [string]$metadata.format -cne "zip" -or
            [string]$metadata.sha256 -cne $ExpectedSha256
        ) {
            throw "installer wrote invalid release identity metadata for $VersionIdentity"
        }
        $releaseMatches += $releaseDir
    }
    if ($releaseMatches.Count -ne 1) {
        throw "installer did not create exactly one short release directory for $VersionIdentity"
    }
    return $releaseMatches[0]
}


$archive = (Resolve-Path -LiteralPath $ArchivePath).Path
$root = Join-Path $env:RUNNER_TEMP ("herdr-installer-test-" + [Guid]::NewGuid().ToString("N"))
$webRoot = Join-Path $root "web"
$herdrHome = Join-Path $root "home"
$installDir = Join-Path $root "bin"
New-Item -ItemType Directory -Force -Path $webRoot | Out-Null
Copy-Item -LiteralPath $archive -Destination (Join-Path $webRoot "herdr-windows-x86_64.zip")
Copy-Item -LiteralPath $installerPath -Destination (Join-Path $webRoot "install.ps1")
$hash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
$previewParent = ("a" * 40) -join ""
$previewSource = ("b" * 40) -join ""
$previewOmp = ("c" * 40) -join ""
$previewBuildId = "2026-08-11-p$previewParent-r$previewSource-o$previewOmp"
$previewHistoricalParent = ("1" * 40) -join ""
$previewHistoricalSource = ("2" * 40) -join ""
$previewHistoricalOmp = ("3" * 40) -join ""
$previewHistoricalBuildId = "2026-08-10-p$previewHistoricalParent-r$previewHistoricalSource-o$previewHistoricalOmp"
$sha256 = [System.Security.Cryptography.SHA256]::Create()
try {
    $previewBootstrapBuildId = "bootstrap-" + [System.BitConverter]::ToString(
        $sha256.ComputeHash([System.Text.Encoding]::ASCII.GetBytes($previewBuildId))
    ).Replace("-", "").ToLowerInvariant()
} finally {
    $sha256.Dispose()
}
$previewVersionIdentity = "0.0.0-preview.$previewBuildId"
$previewBootstrapVersionIdentity = "0.0.0-preview.$previewBootstrapBuildId"
$localPackageIdentity = "0.0.0-preview.local-package"

$representativeLegacyRoot = "C:\Users\runneradmin\.herdr\packages\standalone\releases"
$canonicalLegacyStaging = "$representativeLegacyRoot\.staging.$previewVersionIdentity-x86_64-pc-windows-msvc.12345\conpty\arm64\OpenConsole.exe"
$bootstrapLegacyStaging = "$representativeLegacyRoot\.staging.$previewBootstrapVersionIdentity-x86_64-pc-windows-msvc.12345\conpty\arm64\OpenConsole.exe"
if ($canonicalLegacyStaging.Length -le 260) {
    throw "full P/R/O identity no longer characterizes the legacy MAX_PATH failure"
}
if ($bootstrapLegacyStaging.Length -ge 260) {
    throw "bootstrap identity does not keep the legacy first hop below MAX_PATH"
}

$legacyFirstHopStaging = Join-Path $root ".staging.$previewBootstrapVersionIdentity-x86_64-pc-windows-msvc.$PID"
Expand-Archive -LiteralPath $archive -DestinationPath $legacyFirstHopStaging
if (-not (Test-Path -LiteralPath (Join-Path $legacyFirstHopStaging "conpty\arm64\OpenConsole.exe") -PathType Leaf)) {
    throw "legacy bootstrap first hop did not extract the complete Windows package"
}
Remove-Item -LiteralPath $legacyFirstHopStaging -Recurse -Force
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$listener.Start()
$port = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
$listener.Stop()


$previewReleaseUrl = "https://github.com/Smarty-Pants-Inc/herdr/releases/download/smarty-preview-$previewBuildId"
$previewWindowsAsset = @{
    url = "$previewReleaseUrl/herdr-windows-x86_64.zip"
    sha256 = $hash
    format = "zip"
}
$previewHerdrAssets = @{
    "linux-x86_64" = @{ url = "$previewReleaseUrl/herdr-linux-x86_64"; sha256 = ("d" * 64) }
    "linux-aarch64" = @{ url = "$previewReleaseUrl/herdr-linux-aarch64"; sha256 = ("d" * 64) }
    "macos-x86_64" = @{ url = "$previewReleaseUrl/herdr-macos-x86_64"; sha256 = ("d" * 64) }
    "macos-aarch64" = @{ url = "$previewReleaseUrl/herdr-macos-aarch64"; sha256 = ("d" * 64) }
    "windows-x86_64" = $previewWindowsAsset
}
$previewOmpAssets = @{
    "linux-x86_64" = @{ url = "$previewReleaseUrl/omp-linux-x86_64"; sha256 = ("e" * 64) }
    "linux-aarch64" = @{ url = "$previewReleaseUrl/omp-linux-aarch64"; sha256 = ("e" * 64) }
    "macos-x86_64" = @{ url = "$previewReleaseUrl/omp-macos-x86_64"; sha256 = ("e" * 64) }
    "macos-aarch64" = @{ url = "$previewReleaseUrl/omp-macos-aarch64"; sha256 = ("e" * 64) }
}
$previewOmpMetadata = @{
    assets = $previewOmpAssets
    build_id = "omp-build-test"
    commit = $previewOmp
    tree = ("f" * 40)
    version = "17.3.0"
}

$previewBuilds = @{}
$previewBuilds[$previewBuildId] = @{
    base_version = "0.0.0"
    commit = $previewSource
    built_at = "2026-08-11T00:00:00Z"
    protocol = 1
    tag = "smarty-preview-$previewBuildId"
    assets = $previewHerdrAssets
    omp = $previewOmpMetadata
}
$previewLegacyBuildId = "2026-08-09-" + ("e" * 12)
$previewLegacyTag = "smarty-preview-$previewLegacyBuildId"
$previewLegacyReleaseUrl = "https://github.com/Smarty-Pants-Inc/herdr/releases/download/$previewLegacyTag"
$previewLegacyAssets = @{
    "linux-x86_64" = @{ url = "$previewLegacyReleaseUrl/herdr-linux-x86_64"; sha256 = ("d" * 64) }
    "linux-aarch64" = @{ url = "$previewLegacyReleaseUrl/herdr-linux-aarch64"; sha256 = ("d" * 64) }
    "macos-x86_64" = @{ url = "$previewLegacyReleaseUrl/herdr-macos-x86_64"; sha256 = ("d" * 64) }
    "macos-aarch64" = @{ url = "$previewLegacyReleaseUrl/herdr-macos-aarch64"; sha256 = ("d" * 64) }
    "windows-x86_64" = @{ url = "$previewLegacyReleaseUrl/herdr-windows-x86_64.zip"; sha256 = ("d" * 64); format = "zip" }
}
$previewBuilds[$previewLegacyBuildId] = @{
    base_version = "0.0.0"
    commit = ("e" * 40)
    built_at = "2026-08-09T23:30:00-04:00"
    protocol = 1
    tag = $previewLegacyTag
    assets = $previewLegacyAssets
}
$previewManifest = @{
    schema_version = 2
    channel = "preview"
    base_version = "0.0.0"
    build_id = $previewBootstrapBuildId
    canonical_build_id = $previewBuildId
    commit = $previewSource
    built_at = "2026-08-11T00:00:00Z"
    protocol = 1
    notes = "Windows bootstrap bridge"
    assets = $previewHerdrAssets
    omp = $previewOmpMetadata
    bootstrap = @{
        schema = "smarty.windows-bootstrap.v1"
        paired_build_id = $previewBuildId
        paired_tag = "smarty-preview-$previewBuildId"
        paired_manifest = "preview.json"
        windows_asset_sha256 = $hash
    }
    builds = $previewBuilds
} | ConvertTo-Json -Depth 8
$canonicalPreviewManifest = @{
    schema_version = 1
    channel = "preview"
    base_version = "0.0.0"
    build_id = $previewBuildId
    commit = $previewSource
    built_at = "2026-08-11T00:00:00Z"
    protocol = 1
    notes = "Windows canonical preview"
    assets = $previewHerdrAssets
    omp = $previewOmpMetadata
    builds = $previewBuilds
} | ConvertTo-Json -Depth 8
$fallbackPreview = $canonicalPreviewManifest | ConvertFrom-Json
$fallbackPreviewUrl = "http://127.0.0.1:$port/herdr-windows-x86_64.zip"
$fallbackPreview.assets."windows-x86_64".url = $fallbackPreviewUrl
$fallbackPreview.builds.PSObject.Properties[$previewBuildId].Value.assets."windows-x86_64".url = $fallbackPreviewUrl
$fallbackPreviewManifest = $fallbackPreview | ConvertTo-Json -Depth 8
$legacyStableManifest = @{
    version = "0.0.0"
    assets = @{}
} | ConvertTo-Json -Depth 5
$stableManifest = @{
    version = "0.0.1"
    assets = @{
        "windows-x86_64" = "http://127.0.0.1:$port/herdr-windows-x86_64.zip"
    }
    sha256 = @{
        "windows-x86_64" = $hash
    }
} | ConvertTo-Json -Depth 5
$previewManifestPath = Join-Path $webRoot "preview.json"
$stableManifestPath = Join-Path $webRoot "latest.json"
$previewManifest | Out-File -LiteralPath $previewManifestPath -Encoding utf8
$legacyStableManifest | Out-File -LiteralPath $stableManifestPath -Encoding utf8

$server = $null
$oldInstallerUrl = $env:HERDR_INSTALLER_URL
$oldHerdrHome = $env:HERDR_HOME
$oldProcessPath = $env:Path
$curlShimDir = Join-Path $root "curl-shim"
New-Item -ItemType Directory -Force -Path $curlShimDir | Out-Null
$curlShimPath = Join-Path $curlShimDir "curl.exe"
$curlShimSource = @'
using System;
using System.IO;

public static class HerdrInstallerCurlShim
{
    public static int Main(string[] args)
    {
        try
        {
            string output = null;
            for (int i = 0; i + 1 < args.Length; i++)
            {
                if (args[i] == "--output")
                {
                    output = args[i + 1];
                    break;
                }
            }
            string uri = args.Length == 0 ? null : args[args.Length - 1];
            if (String.IsNullOrEmpty(output) || String.IsNullOrEmpty(uri))
            {
                return 2;
            }
            string stableUrl = Environment.GetEnvironmentVariable("HERDR_INSTALLER_TEST_STABLE_URL");
            string customPreviewUrl = Environment.GetEnvironmentVariable("HERDR_INSTALLER_TEST_CUSTOM_PREVIEW_URL");
            string trustedPreviewUrl = Environment.GetEnvironmentVariable("HERDR_INSTALLER_TEST_TRUSTED_PREVIEW_URL");
            string localArchiveUrl = Environment.GetEnvironmentVariable("HERDR_INSTALLER_TEST_LOCAL_ARCHIVE_URL");
            string trustedArchiveUrl = Environment.GetEnvironmentVariable("HERDR_INSTALLER_TEST_TRUSTED_ARCHIVE_URL");
            string source;
            if (String.Equals(uri, stableUrl, StringComparison.Ordinal))
            {
                source = Environment.GetEnvironmentVariable("HERDR_INSTALLER_TEST_STABLE_MANIFEST");
            }
            else if (String.Equals(uri, customPreviewUrl, StringComparison.Ordinal) ||
                     String.Equals(uri, trustedPreviewUrl, StringComparison.Ordinal))
            {
                source = Environment.GetEnvironmentVariable("HERDR_INSTALLER_TEST_PREVIEW_MANIFEST");
            }
            else if (String.Equals(uri, localArchiveUrl, StringComparison.Ordinal) ||
                     String.Equals(uri, trustedArchiveUrl, StringComparison.Ordinal))
            {
                source = Environment.GetEnvironmentVariable("HERDR_INSTALLER_TEST_ARCHIVE");
            }
            else
            {
                return 22;
            }
            if (String.IsNullOrEmpty(source) || !File.Exists(source))
            {
                Console.Error.WriteLine("404 Not Found: " + uri);
                return 22;
            }
            File.Copy(source, output, true);
            return 0;
        }
        catch (Exception error)
        {
            Console.Error.WriteLine(error.Message);
            return 1;
        }
    }
}
'@
Add-Type -TypeDefinition $curlShimSource -OutputAssembly $curlShimPath -OutputType ConsoleApplication | Out-Null
$oldCurlStableManifest = [Environment]::GetEnvironmentVariable("HERDR_INSTALLER_TEST_STABLE_MANIFEST", "Process")
$oldCurlPreviewManifest = [Environment]::GetEnvironmentVariable("HERDR_INSTALLER_TEST_PREVIEW_MANIFEST", "Process")
$oldCurlArchive = [Environment]::GetEnvironmentVariable("HERDR_INSTALLER_TEST_ARCHIVE", "Process")
$oldCurlStableUrl = [Environment]::GetEnvironmentVariable("HERDR_INSTALLER_TEST_STABLE_URL", "Process")
$oldCurlCustomPreviewUrl = [Environment]::GetEnvironmentVariable("HERDR_INSTALLER_TEST_CUSTOM_PREVIEW_URL", "Process")
$oldCurlTrustedPreviewUrl = [Environment]::GetEnvironmentVariable("HERDR_INSTALLER_TEST_TRUSTED_PREVIEW_URL", "Process")
$oldCurlLocalArchiveUrl = [Environment]::GetEnvironmentVariable("HERDR_INSTALLER_TEST_LOCAL_ARCHIVE_URL", "Process")
$oldCurlTrustedArchiveUrl = [Environment]::GetEnvironmentVariable("HERDR_INSTALLER_TEST_TRUSTED_ARCHIVE_URL", "Process")
[Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_STABLE_MANIFEST", $stableManifestPath, "Process")
[Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_PREVIEW_MANIFEST", $previewManifestPath, "Process")
[Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_ARCHIVE", $archive, "Process")
$previousGlobalInvokeWebRequest = Get-Item -LiteralPath Function:\global:Invoke-WebRequest -ErrorAction SilentlyContinue
$previousGlobalInvokeRestMethod = Get-Item -LiteralPath Function:\global:Invoke-RestMethod -ErrorAction SilentlyContinue
$previewDownloadShimInstalled = $false
$previewManifestShimInstalled = $false
try {
    $server = Start-Process python -ArgumentList @("-m", "http.server", "$port", "--bind", "127.0.0.1", "--directory", $webRoot) -PassThru -WindowStyle Hidden
    $env:HERDR_HOME = Join-Path $root "unused\..\home"
    $previewManifestUrl = "https://raw.githubusercontent.com/Smarty-Pants-Inc/herdr/smarty-channel/preview.json"
    $stableManifestUrl = "http://127.0.0.1:$port/latest.json"
    $customPreviewUrl = "http://127.0.0.1:$port/preview.json"
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_STABLE_URL", $stableManifestUrl, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_CUSTOM_PREVIEW_URL", $customPreviewUrl, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_TRUSTED_PREVIEW_URL", $previewManifestUrl, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_LOCAL_ARCHIVE_URL", $fallbackPreviewUrl, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_TRUSTED_ARCHIVE_URL", $previewWindowsAsset.url, "Process")
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        try {
            Invoke-WebRequest -Uri $stableManifestUrl -UseBasicParsing | Out-Null
            break
        } catch {
            if ($attempt -eq 19) { throw }
            Start-Sleep -Milliseconds 100
        }
    }
    $global:HerdrInstallerTestPreviewArchive = $archive
    $global:HerdrInstallerTestPreviewManifestPath = $previewManifestPath
    function global:Invoke-RestMethod {
        [CmdletBinding()]
        param(
            [Parameter(Mandatory = $true)]
            [Uri]$Uri
        )

        if ([string]$Uri -ceq $global:HerdrInstallerTestPreviewManifestUrl) {
            return (Get-Content -LiteralPath $global:HerdrInstallerTestPreviewManifestPath -Raw | ConvertFrom-Json)
        }
        Microsoft.PowerShell.Utility\Invoke-RestMethod @PSBoundParameters
    }
    $global:HerdrInstallerTestPreviewManifestUrl = $previewManifestUrl
    $previewManifestShimInstalled = $true
    $global:HerdrInstallerTestPreviewUrl = $previewWindowsAsset.url
    $global:HerdrInstallerTestPreviewDownloadAvailable = $true
    function global:Invoke-WebRequest {
        [CmdletBinding()]
        param(
            [Parameter(Mandatory = $true)]
            [Uri]$Uri,
            [string]$OutFile,
            [switch]$UseBasicParsing
        )

        if ([string]$Uri -ceq $global:HerdrInstallerTestPreviewUrl) {
            if (-not $global:HerdrInstallerTestPreviewDownloadAvailable) {
                throw "404 Not Found: $Uri"
            }
            Copy-Item -LiteralPath $global:HerdrInstallerTestPreviewArchive -Destination $OutFile -Force
            return [PSCustomObject]@{ StatusCode = 200 }
        }
        Microsoft.PowerShell.Utility\Invoke-WebRequest @PSBoundParameters
    }
    $previewDownloadShimInstalled = $true


    $freshStableHome = Join-Path $root "fresh-stable-home"
    $freshStableBin = Join-Path $root "fresh-stable-bin"
    $stableManifest | Out-File -LiteralPath $stableManifestPath -Encoding utf8
    $env:HERDR_HOME = $freshStableHome
    $env:HERDR_INSTALLER_URL = "http://127.0.0.1:$port/install.ps1"
    $env:Path = $oldProcessPath
    & $bootstrapPath `
        -ManifestUrl $stableManifestUrl `
        -InstallDir $freshStableBin
    if ($LASTEXITCODE -ne 0) {
        throw "CMD bootstrap failed with exit code $LASTEXITCODE"
    }
    $env:HERDR_INSTALLER_URL = $oldInstallerUrl
    $freshStableRelease = Get-ReleaseDirectoryForIdentity `
        -ReleasesDir (Join-Path $freshStableHome "packages\standalone\releases") `
        -VersionIdentity "0.0.1" `
        -ExpectedSha256 $hash

    $legacyStableManifest | Out-File -LiteralPath $stableManifestPath -Encoding utf8
    $fallbackPreviewManifest | Out-File -LiteralPath $previewManifestPath -Encoding utf8
    $env:HERDR_HOME = Join-Path $root "unused\..\home"
    $env:Path = "$curlShimDir;$oldProcessPath"
    & $installerPath `
        -ManifestUrl $stableManifestUrl `
        -InstallDir $installDir `
        -ExpectedBuildId $previewBuildId
    $previewManifest | Out-File -LiteralPath $previewManifestPath -Encoding utf8

    # Keep the existing positional web-installer contract, including Retain in slot five.
    & $installerPath "preview" $previewManifestUrl $installDir $previewBootstrapBuildId 3

    function Assert-BridgeManifestRejected {
        param(
            [string]$Name,
            [scriptblock]$Mutate
        )

        $candidate = $previewManifest | ConvertFrom-Json
        & $Mutate $candidate
        $candidate | ConvertTo-Json -Depth 12 | Out-File -LiteralPath $previewManifestPath -Encoding utf8
        $rejected = $false
        try {
            & $installerPath `
                -Channel preview `
                -ManifestUrl $previewManifestUrl `
                -InstallDir $installDir `
                -ExpectedBuildId $previewBootstrapBuildId
        } catch {
            if ($_.Exception.Message -notlike "Preview Phase A bridge*") {
                throw
            }
            $rejected = $true
        }
        if (-not $rejected) {
            throw "installer accepted Phase A bridge mix-and-match: $Name"
        }
    }
    $pairedTimestampMutations = @(
        @{
            Name = "retained-built-at-offset-spelling"
            Mutate = {
                param($m)
                $m.built_at = "2026-08-11T00:00:00+00:00"
                $m.builds.PSObject.Properties[$previewBuildId].Value.built_at = $m.built_at
            }
        },
        @{
            Name = "retained-built-at-offset-cross-day"
            Mutate = {
                param($m)
                $m.built_at = "2026-08-10T19:00:00-05:00"
                $m.builds.PSObject.Properties[$previewBuildId].Value.built_at = $m.built_at
            }
        },
        @{
            Name = "retained-built-at-canonical-cross-day"
            Mutate = {
                param($m)
                $m.built_at = "2026-08-12T00:00:00Z"
                $m.builds.PSObject.Properties[$previewBuildId].Value.built_at = $m.built_at
            }
        }
    )


    $bridgeMutations = @(
        @{ Name = "alias"; Mutate = { param($m) $m.build_id = "bootstrap-" + ("0" * 64) } },
        @{ Name = "missing-canonical-id"; Mutate = { param($m) $m.PSObject.Properties.Remove("canonical_build_id") } },
        @{ Name = "canonical-id"; Mutate = { param($m) $m.canonical_build_id = "not-a-paired-id" } },
        @{ Name = "bridge-schema"; Mutate = { param($m) $m.schema_version = 1 } },
        @{ Name = "missing-bootstrap"; Mutate = { param($m) $m.PSObject.Properties.Remove("bootstrap") } },
        @{ Name = "top-extra-key"; Mutate = { param($m) $m | Add-Member -NotePropertyName unexpected -NotePropertyValue "value" } },
        @{ Name = "top-missing-key"; Mutate = { param($m) $m.PSObject.Properties.Remove("notes") } },
        @{ Name = "bootstrap-schema"; Mutate = { param($m) $m.bootstrap.schema = "other" } },
        @{ Name = "bootstrap-paired-id"; Mutate = { param($m) $m.bootstrap.paired_build_id = "2026-08-11-p" + ("0" * 40) + "-r" + ("1" * 40) + "-o" + ("2" * 40) } },
        @{ Name = "bootstrap-paired-tag"; Mutate = { param($m) $m.bootstrap.paired_tag = "smarty-preview-other" } },
        @{ Name = "bootstrap-paired-manifest"; Mutate = { param($m) $m.bootstrap.paired_manifest = "other.json" } },
        @{ Name = "bootstrap-windows-digest"; Mutate = { param($m) $m.bootstrap.windows_asset_sha256 = "0" * 64 } },
        @{ Name = "bootstrap-extra-key"; Mutate = { param($m) $m.bootstrap | Add-Member -NotePropertyName unexpected -NotePropertyValue "value" } },
        @{ Name = "bootstrap-missing-key"; Mutate = { param($m) $m.bootstrap.PSObject.Properties.Remove("schema") } },
        @{ Name = "malformed-non-current-retained-record"; Mutate = { param($m) $m.builds | Add-Member -NotePropertyName $previewHistoricalBuildId -NotePropertyValue ([PSCustomObject]@{}) } },
        @{ Name = "missing-canonical-record"; Mutate = { param($m) $m.builds.PSObject.Properties.Remove($previewBuildId) } },
        @{ Name = "retained-tag"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewBuildId].Value.tag = "smarty-preview-other" } },
        @{ Name = "retained-commit"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewBuildId].Value.commit = "9" * 40 } },
        @{ Name = "top-base-version"; Mutate = { param($m) $m.base_version = "9.9.9" } },
        @{ Name = "top-built-at"; Mutate = { param($m) $m.built_at = "2026-08-12T00:00:00Z" } },
        @{ Name = "top-commit"; Mutate = { param($m) $m.commit = "9" * 40 } },
        @{ Name = "top-protocol"; Mutate = { param($m) $m.protocol = 2 } },
        @{ Name = "retained-protocol"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewBuildId].Value.protocol = 2 } },
        @{ Name = "top-omp"; Mutate = { param($m) $m.omp.version = "17.3.1" } },
        @{ Name = "retained-omp"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewBuildId].Value.omp.version = "17.3.1" } },
        @{ Name = "retained-omp-commit"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewBuildId].Value.omp.commit = "9" * 40 } },
        @{ Name = "retained-omp-url"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewBuildId].Value.omp.assets."linux-x86_64".url = "https://example.invalid/omp" } },
        @{ Name = "top-windows-digest"; Mutate = { param($m) $m.assets."windows-x86_64".sha256 = "0" * 64 } },
        @{ Name = "top-windows-format"; Mutate = { param($m) $m.assets."windows-x86_64".format = "exe" } },
        @{ Name = "extra-top-asset"; Mutate = { param($m) $m.assets | Add-Member -NotePropertyName "unexpected" -NotePropertyValue @{ url = "https://example.invalid/herdr"; sha256 = ("d" * 64) } } },
        @{ Name = "retained-windows-digest"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewBuildId].Value.assets."windows-x86_64".sha256 = "0" * 64 } },
        @{ Name = "retained-herdr-url"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewBuildId].Value.assets."linux-x86_64".url = "https://example.invalid/herdr" } },
        @{ Name = "missing-top-asset"; Mutate = { param($m) $m.assets.PSObject.Properties.Remove("windows-x86_64") } },
        @{ Name = "missing-retained-asset"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewBuildId].Value.assets.PSObject.Properties.Remove("linux-x86_64") } },
        @{ Name = "legacy-retained-literal-date-mismatch"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewLegacyBuildId].Value.built_at = "2026-08-08T23:30:00-04:00" } },
        @{ Name = "legacy-retained-invalid-date"; Mutate = { param($m) $m.builds.PSObject.Properties[$previewLegacyBuildId].Value.built_at = "2026-02-30T23:30:00-04:00" } }
    ) + $pairedTimestampMutations

    foreach ($mutation in $bridgeMutations) {
        Assert-BridgeManifestRejected -Name $mutation.Name -Mutate $mutation.Mutate
    }

    $downgradeBuildId = "2026-08-11-$($previewSource.Substring(0, 12))"
    $downgrade = $previewManifest | ConvertFrom-Json
    $retained = $downgrade.builds.PSObject.Properties[$previewBuildId].Value
    $downgrade.PSObject.Properties.Remove("canonical_build_id")
    $downgrade.PSObject.Properties.Remove("bootstrap")
    $downgrade.schema_version = 1
    $downgrade.build_id = $downgradeBuildId
    $downgrade.assets = $retained.assets
    $attackerUrl = "http://127.0.0.1:$port/herdr-windows-x86_64.zip"
    $downgrade.assets."windows-x86_64".url = $attackerUrl
    $downgrade.assets."windows-x86_64".sha256 = $hash
    $retained.assets."windows-x86_64".url = $attackerUrl
    $retained.assets."windows-x86_64".sha256 = $hash
    $downgrade | ConvertTo-Json -Depth 12 | Out-File -LiteralPath $previewManifestPath -Encoding utf8
    $downgradeRejected = $false
    try {
        & $installerPath `
            -Channel preview `
            -ManifestUrl $previewManifestUrl `
            -InstallDir $installDir `
            -ExpectedBuildId $downgradeBuildId
    } catch {
        if ($_.Exception.Message -notlike "Canonical preview manifest.build_id must encode*") {
            throw
        }
        $downgradeRejected = $true
    }
    if (-not $downgradeRejected) {
        throw "installer accepted stripped canonical identity with attacker URL and SHA"
    }
    $previewManifest | Out-File -LiteralPath $previewManifestPath -Encoding utf8

    # A caller may have selected the short Phase A alias just before the canonical Phase B promotion.
    $canonicalPreviewManifest | Out-File -LiteralPath $previewManifestPath -Encoding utf8
    & $installerPath `
        -Channel preview `
        -ManifestUrl $previewManifestUrl `
        -InstallDir $installDir `
        -ExpectedBuildId $previewBootstrapBuildId
    $null = Get-ReleaseDirectoryForIdentity `
        -ReleasesDir (Join-Path $herdrHome "packages\standalone\releases") `
        -VersionIdentity $previewVersionIdentity `
        -ExpectedSha256 $hash
    $previewManifest | Out-File -LiteralPath $previewManifestPath -Encoding utf8
    function Assert-CanonicalPreviewManifestRejected {
        param(
            [string]$Name,
            [scriptblock]$Mutate
        )

        $candidate = $canonicalPreviewManifest | ConvertFrom-Json
        & $Mutate $candidate
        $candidate | ConvertTo-Json -Depth 12 | Out-File -LiteralPath $previewManifestPath -Encoding utf8
        $rejected = $false
        try {
            & $installerPath `
                -Channel preview `
                -ManifestUrl $previewManifestUrl `
                -InstallDir $installDir `
                -ExpectedBuildId $previewBootstrapBuildId
        } catch {
            if ($_.Exception.Message -notlike "Canonical preview manifest*") {
                throw
            }
            $rejected = $true
        }
        if (-not $rejected) {
            throw "installer accepted a canonical preview alias without the retained asset binding: $Name"
        }
    }
    Assert-CanonicalPreviewManifestRejected -Name "top-windows-asset" -Mutate {
        param($m)
        $m.assets."windows-x86_64".sha256 = "0" * 64
    }
    Assert-CanonicalPreviewManifestRejected -Name "retained-windows-asset" -Mutate {
        param($m)
        $m.builds.PSObject.Properties[$previewBuildId].Value.assets."windows-x86_64".url = "https://example.invalid/herdr.zip"
    }
    Assert-CanonicalPreviewManifestRejected -Name "malformed-non-current-retained-record" -Mutate {
        param($m)
        $m.builds | Add-Member -NotePropertyName $previewHistoricalBuildId -NotePropertyValue ([PSCustomObject]@{})
    }
    foreach ($mutation in $pairedTimestampMutations) {
        Assert-CanonicalPreviewManifestRejected -Name $mutation.Name -Mutate $mutation.Mutate
    }
    $previewManifest | Out-File -LiteralPath $previewManifestPath -Encoding utf8

    $localInstallDir = Join-Path $root "local-bin"
    $env:HERDR_HOME = Join-Path $root "local-home"
    $partialLocalModeRejected = $false
    try {
        & $installerPath `
            -InstallDir $localInstallDir `
            -LocalPackagePath $archive
    } catch {
        if ($_.Exception.Message -notlike "Local package mode requires*") {
            throw
        }
        $partialLocalModeRejected = $true
    }
    if (-not $partialLocalModeRejected) {
        throw "installer accepted partial local-package inputs"
    }

    $badLocalChecksumRejected = $false
    try {
        & $installerPath `
            -ManifestUrl "$previewManifestUrl/unused" `
            -InstallDir $localInstallDir `
            -LocalPackagePath $archive `
            -LocalPackageFormat "zip" `
            -LocalPackageIdentity $localPackageIdentity `
            -LocalPackageSha256 ("0" * 64)
    } catch {
        if ($_.Exception.Message -notlike "Downloaded Herdr checksum did not match.*") {
            throw
        }
        $badLocalChecksumRejected = $true
    }
    if (-not $badLocalChecksumRejected) {
        throw "installer accepted a local package with the wrong checksum"
    }

    & $installerPath `
        -ManifestUrl "$previewManifestUrl/unused" `
        -InstallDir $localInstallDir `
        -LocalPackagePath $archive `
        -LocalPackageFormat "zip" `
        -LocalPackageIdentity $localPackageIdentity `
        -LocalPackageSha256 $hash
    if (-not (Test-Path -LiteralPath (Join-Path $localInstallDir "herdr.exe") -PathType Leaf)) {
        throw "installer did not activate the verified local package"
    }
    $null = Get-ReleaseDirectoryForIdentity `
        -ReleasesDir (Join-Path (Join-Path $root "local-home") "packages\standalone\releases") `
        -VersionIdentity $localPackageIdentity `
        -ExpectedSha256 $hash
    $env:HERDR_HOME = $herdrHome

    $required = @(
        "herdr.exe",
        "conpty\herdr-conpty.json",
        "conpty\conpty.dll",
        "conpty\x64\OpenConsole.exe",
        "conpty\arm64\OpenConsole.exe",
        "THIRD-PARTY-NOTICES\Microsoft.Windows.Console.ConPTY-LICENSE.txt",
        "THIRD-PARTY-NOTICES\Microsoft.Windows.Console.ConPTY-NOTICE.md"
    )
    foreach ($relative in $required) {
        if (-not (Test-Path -LiteralPath (Join-Path $installDir $relative) -PathType Leaf)) {
            throw "installer did not activate required file $relative"
        }
    }

    $releasesDir = Join-Path $herdrHome "packages\standalone\releases"
    $releaseDir = Get-ReleaseDirectoryForIdentity `
        -ReleasesDir $releasesDir `
        -VersionIdentity $previewVersionIdentity `
        -ExpectedSha256 $hash
    Remove-Item -LiteralPath (Join-Path $releaseDir.FullName "conpty\conpty.dll") -Force

    $global:HerdrInstallerTestPreviewDownloadAvailable = $false
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_ARCHIVE", $null, "Process")

    $downloadFailed = $false
    try {
        & "$PSScriptRoot\..\website\install.ps1" `
            -Channel preview `
            -ManifestUrl $previewManifestUrl `
            -InstallDir $installDir `
            -ExpectedBuildId $previewBuildId
    } catch {
        if ($_.Exception.Message -notmatch "(?i)(404|not found)") {
            throw
        }
        $downloadFailed = $true
    }
    if (-not $downloadFailed) {
        throw "installer repair unexpectedly accepted a missing archive"
    }
    $global:HerdrInstallerTestPreviewDownloadAvailable = $true
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_ARCHIVE", $archive, "Process")

    if (-not (Test-Path -LiteralPath (Join-Path $releaseDir.FullName "herdr.exe") -PathType Leaf)) {
        throw "failed repair removed the existing release"
    }

    $previewManifest | Out-File -LiteralPath $previewManifestPath -Encoding utf8
    $stagedConpty = Join-Path $releasesDir ".staging.$($releaseDir.Name).$PID\conpty\conpty.dll"

    $transientLockState = @{ Handle = $null; Acquired = $false; Released = $false }
    $transientLockTimer = New-Object System.Timers.Timer
    $transientLockTimer.Interval = 300
    $transientLockTimer.AutoReset = $false
    $transientLockSource = "HerdrTransientInstallerLock-$PID"
    $transientLockRelease = Register-ObjectEvent `
        -InputObject $transientLockTimer `
        -EventName Elapsed `
        -SourceIdentifier $transientLockSource `
        -MessageData $transientLockState `
        -Action {
            $state = $event.MessageData
            if ($null -ne $state.Handle) {
                $state.Handle.Dispose()
                $state.Handle = $null
            }
            $state.Released = $true
        }
    $lockStagedFileTransiently = {
        if (-not $transientLockState.Acquired) {
            $transientLockState.Handle = [System.IO.File]::Open(
                $stagedConpty,
                [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::Read,
                [System.IO.FileShare]::Read
            )
            $transientLockState.Acquired = $true
            $transientLockTimer.Start()
        }
    }.GetNewClosure()
    $transientLockBreakpoint = Set-PSBreakpoint -Script $installerPath -Variable "backupDir" -Mode Write -Action $lockStagedFileTransiently
    try {
        & $installerPath `
            -Channel preview `
            -ManifestUrl $previewManifestUrl `
            -InstallDir $installDir `
            -ExpectedBuildId $previewBuildId
        if (-not $transientLockState.Acquired) {
            throw "installer did not acquire the transient staged-file lock"
        }
        if (-not $transientLockState.Released) {
            throw "installer activated the release before the transient lock was released"
        }
        if (-not (Test-Path -LiteralPath (Join-Path $releaseDir.FullName "conpty\conpty.dll") -PathType Leaf)) {
            throw "installer did not repair the release after the transient lock cleared"
        }
    } finally {
        Remove-PSBreakpoint -Breakpoint $transientLockBreakpoint
        $transientLockTimer.Stop()
        Unregister-Event -SourceIdentifier $transientLockSource -ErrorAction SilentlyContinue
        Remove-Job -Id $transientLockRelease.Id -Force -ErrorAction SilentlyContinue
        if ($null -ne $transientLockState.Handle) {
            $transientLockState.Handle.Dispose()
        }
        $transientLockTimer.Dispose()
    }

    Remove-Item -LiteralPath (Join-Path $releaseDir.FullName "conpty\conpty.dll") -Force
    $lockState = @{ Handle = $null }
    $lockStagedFile = {
        if ($null -eq $lockState.Handle) {
            $lockState.Handle = [System.IO.File]::Open(
                $stagedConpty,
                [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::Read,
                [System.IO.FileShare]::Read
            )
        }
    }.GetNewClosure()
    $swapBreakpoint = Set-PSBreakpoint -Script $installerPath -Variable "backupDir" -Mode Write -Action $lockStagedFile
    try {
        $swapFailed = $false
        try {
            & $installerPath `
                -Channel preview `
                -ManifestUrl $previewManifestUrl `
                -InstallDir $installDir `
                -ExpectedBuildId $previewBuildId
        } catch {
            $swapFailed = $true
        }
        if ($null -eq $lockState.Handle) {
            throw "installer did not acquire the staged file handle before the swap"
        }
        if (-not $swapFailed) {
            throw "installer unexpectedly activated a release with a locked staged file"
        }
        if (-not (Test-Path -LiteralPath (Join-Path $releaseDir.FullName "herdr.exe") -PathType Leaf)) {
            throw "failed activation did not restore the prior release"
        }
        if (@(Get-ChildItem -LiteralPath $releasesDir -Force -Directory -Filter ".backup.$($releaseDir.Name).*").Count -ne 0) {
            throw "failed activation stranded a release backup"
        }
        foreach ($junction in @($installDir, (Join-Path $herdrHome "packages\standalone\current"))) {
            if (-not (Test-Path -LiteralPath (Join-Path $junction "herdr.exe") -PathType Leaf)) {
                throw "failed activation left an invalid installer junction at $junction"
            }
        }
    } finally {
        Remove-PSBreakpoint -Breakpoint $swapBreakpoint
        if ($null -ne $lockState.Handle) {
            $lockState.Handle.Dispose()
        }
    }

    & "$PSScriptRoot\..\website\install.ps1" `
        -Channel preview `
        -ManifestUrl $previewManifestUrl `
        -InstallDir $installDir `
        -ExpectedBuildId $previewBuildId
    if (-not (Test-Path -LiteralPath (Join-Path $installDir "conpty\conpty.dll") -PathType Leaf)) {
        throw "installer did not repair an incomplete release"
    }

    $x64HostDir = Join-Path $releaseDir.FullName "conpty\x64"
    $junctionTarget = Join-Path $root "junction-target"
    Move-Item -LiteralPath $x64HostDir -Destination $junctionTarget
    New-Item -ItemType Junction -Path $x64HostDir -Target $junctionTarget | Out-Null
    & "$PSScriptRoot\..\website\install.ps1" `
        -Channel preview `
        -ManifestUrl $previewManifestUrl `
        -InstallDir $installDir `
        -ExpectedBuildId $previewBuildId
    $repairedHostDir = Get-Item -LiteralPath (Join-Path $installDir "conpty\x64") -Force
    if ($repairedHostDir.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "installer accepted a reparse-point ConPTY directory"
    }

    $rejected = $false
    try {
        & "$PSScriptRoot\..\website\install.ps1" `
            -Channel preview `
            -ManifestUrl $previewManifestUrl `
            -InstallDir $installDir `
            -ExpectedBuildId "different-build"
    } catch {
        if ($_.Exception.Message -notlike "Preview manifest changed while updating.*") {
            throw
        }
        $rejected = $true
    }
    if (-not $rejected) {
        throw "installer accepted a manifest that did not match the updater-selected build"
    }

    $stableManifest | Out-File -LiteralPath $stableManifestPath -Encoding utf8
    & "$PSScriptRoot\..\website\install.ps1" `
        -Channel stable `
        -ManifestUrl $stableManifestUrl `
        -InstallDir $installDir
    $stableReleaseDir = Get-ReleaseDirectoryForIdentity `
        -ReleasesDir (Join-Path $herdrHome "packages\standalone\releases") `
        -VersionIdentity "0.0.1" `
        -ExpectedSha256 $hash
    foreach ($relative in $required) {
        if (-not (Test-Path -LiteralPath (Join-Path $installDir $relative) -PathType Leaf)) {
            throw "stable installer did not activate required file $relative"
        }
    }

    $customPreviewManifestPath = Join-Path $webRoot "candidate.json"
    $customPreviewManifest = $canonicalPreviewManifest | ConvertFrom-Json
    $customPreviewManifest.PSObject.Properties.Remove("omp")
    $customPreviewManifest.builds.PSObject.Properties[$previewBuildId].Value.PSObject.Properties.Remove("omp")
    $customPreviewManifest | ConvertTo-Json -Depth 12 | Out-File -LiteralPath $customPreviewManifestPath -Encoding utf8

    $fakeBin = Join-Path $root "fake-existing"
    New-Item -ItemType Directory -Force -Path $fakeBin | Out-Null
    @'
@echo off
if "%1"=="channel" if "%2"=="show" (
  echo preview
  exit /b 0
)
exit /b 1
'@ | Out-File -LiteralPath (Join-Path $fakeBin "herdr.cmd") -Encoding ascii

    $preserveHome = Join-Path $root "preserve-home"
    $preserveBin = Join-Path $root "preserve-bin"
    $preserveManifestUrl = "http://127.0.0.1:$port/candidate.json"
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_CUSTOM_PREVIEW_URL", $preserveManifestUrl, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_PREVIEW_MANIFEST", $customPreviewManifestPath, "Process")
    $env:HERDR_HOME = $preserveHome
    $env:Path = "$fakeBin;$curlShimDir;$oldProcessPath"
    & "$PSScriptRoot\..\website\install.ps1" `
        -ManifestUrl $preserveManifestUrl `
        -InstallDir $preserveBin `
        -ExpectedBuildId $previewBuildId
    $preservedPreview = Get-ReleaseDirectoryForIdentity `
        -ReleasesDir (Join-Path $preserveHome "packages\standalone\releases") `
        -VersionIdentity $previewVersionIdentity `
        -ExpectedSha256 $hash

    & "$PSScriptRoot\..\website\install.ps1" `
        -Channel stable `
        -ManifestUrl $stableManifestUrl `
        -InstallDir $preserveBin
    $explicitStable = Get-ReleaseDirectoryForIdentity `
        -ReleasesDir (Join-Path $preserveHome "packages\standalone\releases") `
        -VersionIdentity "0.0.1" `
        -ExpectedSha256 $hash
} finally {
    $env:HERDR_HOME = $oldHerdrHome
    $env:HERDR_INSTALLER_URL = $oldInstallerUrl
    $env:Path = $oldProcessPath
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_STABLE_MANIFEST", $oldCurlStableManifest, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_PREVIEW_MANIFEST", $oldCurlPreviewManifest, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_ARCHIVE", $oldCurlArchive, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_STABLE_URL", $oldCurlStableUrl, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_CUSTOM_PREVIEW_URL", $oldCurlCustomPreviewUrl, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_TRUSTED_PREVIEW_URL", $oldCurlTrustedPreviewUrl, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_LOCAL_ARCHIVE_URL", $oldCurlLocalArchiveUrl, "Process")
    [Environment]::SetEnvironmentVariable("HERDR_INSTALLER_TEST_TRUSTED_ARCHIVE_URL", $oldCurlTrustedArchiveUrl, "Process")
    if ($previewDownloadShimInstalled) {
        Remove-Item -LiteralPath Function:\global:Invoke-WebRequest -Force
        if ($null -ne $previousGlobalInvokeWebRequest) {
            Set-Item -LiteralPath Function:\global:Invoke-WebRequest -Value $previousGlobalInvokeWebRequest.ScriptBlock
        }
    }
    if ($previewManifestShimInstalled) {
        Remove-Item -LiteralPath Function:\global:Invoke-RestMethod -Force
        if ($null -ne $previousGlobalInvokeRestMethod) {
            Set-Item -LiteralPath Function:\global:Invoke-RestMethod -Value $previousGlobalInvokeRestMethod.ScriptBlock
        }
    }
    Remove-Variable -Name HerdrInstallerTestPreviewArchive -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name HerdrInstallerTestPreviewUrl -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name HerdrInstallerTestPreviewDownloadAvailable -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name HerdrInstallerTestPreviewManifestPath -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name HerdrInstallerTestPreviewManifestUrl -Scope Global -ErrorAction SilentlyContinue
    if ($null -ne $server -and -not $server.HasExited) {
        Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
