[CmdletBinding()]
param(
    [string]$Channel = $env:HERDR_CHANNEL,
    [string]$ManifestUrl = $env:HERDR_MANIFEST_URL,
    [string]$InstallDir = $env:HERDR_INSTALL_DIR,
    [string]$ExpectedBuildId = $env:HERDR_EXPECTED_BUILD_ID,
    [int]$Retain = 3,
    [string]$LocalPackagePath,
    [string]$LocalPackageFormat,
    [string]$LocalPackageIdentity,
    [string]$LocalPackageSha256
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$channelWasExplicit = -not [string]::IsNullOrWhiteSpace($Channel)
if ($channelWasExplicit -and $Channel -notin @("stable", "preview")) {
    Write-Error "Invalid Herdr channel '$Channel'. Use 'stable' or 'preview'."
    exit 1
}

$localPackageValueCount = @(
    $LocalPackagePath,
    $LocalPackageFormat,
    $LocalPackageIdentity,
    $LocalPackageSha256 |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
).Count
if ($localPackageValueCount -notin @(0, 4)) {
    throw "Local package mode requires path, format, identity, and SHA-256."
}
$useLocalPackage = $localPackageValueCount -eq 4
if ($useLocalPackage -and $LocalPackageFormat -notin @("zip", "exe")) {
    throw "Local Herdr package has unsupported format '$LocalPackageFormat'."
}

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message"
}

function Write-WarningStep {
    param([string]$Message)
    Write-Warning $Message
}

function Get-HerdrCommandSource {
    $existing = Get-Command herdr -ErrorAction SilentlyContinue
    if ($null -eq $existing) {
        return $null
    }

    return $existing.Source
}

function Test-PathStartsWith {
    param(
        [string]$Path,
        [string]$Prefix
    )

    if ([string]::IsNullOrWhiteSpace($Path) -or [string]::IsNullOrWhiteSpace($Prefix)) {
        return $false
    }

    try {
        $normalizedPath = [System.IO.Path]::GetFullPath($Path)
        $normalizedPrefix = [System.IO.Path]::GetFullPath($Prefix).TrimEnd("\") + "\"
        return $normalizedPath.StartsWith($normalizedPrefix, [System.StringComparison]::OrdinalIgnoreCase)
    } catch {
        return $false
    }
}

function Path-Contains {
    param(
        [string]$PathValue,
        [string]$Entry
    )

    if ([string]::IsNullOrWhiteSpace($PathValue)) {
        return $false
    }

    $needle = $Entry.TrimEnd("\")
    foreach ($segment in $PathValue.Split(";", [System.StringSplitOptions]::RemoveEmptyEntries)) {
        if ($segment.TrimEnd("\") -ieq $needle) {
            return $true
        }
    }

    return $false
}

function Prepend-PathEntry {
    param(
        [string]$PathValue,
        [string]$Entry
    )

    $needle = $Entry.TrimEnd("\")
    $segments = @($Entry)
    if (-not [string]::IsNullOrWhiteSpace($PathValue)) {
        $segments += $PathValue.Split(";", [System.StringSplitOptions]::RemoveEmptyEntries) |
            Where-Object { $_.TrimEnd("\") -ine $needle }
    }

    return ($segments -join ";")
}

function Update-PathRegistryEntry {
    param(
        [Microsoft.Win32.RegistryKey]$EnvironmentKey,
        [string]$Entry
    )

    $options = [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
    $value = $EnvironmentKey.GetValue("Path", $null, $options)
    $kind = if ($null -eq $value) {
        [Microsoft.Win32.RegistryValueKind]::String
    } else {
        $EnvironmentKey.GetValueKind("Path")
    }
    $newValue = Prepend-PathEntry -PathValue $value -Entry $Entry
    if ($newValue -ceq $value) {
        return $false
    }

    $EnvironmentKey.SetValue("Path", $newValue, $kind)
    return $true
}

function Publish-EnvironmentChange {
    if (-not ("HerdrInstaller.EnvironmentNativeMethods" -as [type])) {
        Add-Type -Namespace HerdrInstaller -Name EnvironmentNativeMethods -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr SendMessageTimeout(
    System.IntPtr hWnd,
    uint message,
    System.UIntPtr wParam,
    string lParam,
    uint flags,
    uint timeout,
    out System.UIntPtr result);
'@
    }

    $result = [UIntPtr]::Zero
    [HerdrInstaller.EnvironmentNativeMethods]::SendMessageTimeout(
        [IntPtr]0xffff,
        0x1a,
        [UIntPtr]::Zero,
        "Environment",
        0x0002,
        1000,
        [ref]$result
    ) | Out-Null
}

function Get-ManifestAsset {
    param(
        [object]$Manifest,
        [string]$Target
    )

    $property = $Manifest.assets.PSObject.Properties[$Target]
    if ($null -eq $property) {
        throw "Release manifest does not include a binary for $Target."
    }

    $sha256 = $null
    $shaMapProperty = $Manifest.PSObject.Properties["sha256"]
    if ($null -ne $shaMapProperty -and $null -ne $shaMapProperty.Value) {
        $targetShaProperty = $shaMapProperty.Value.PSObject.Properties[$Target]
        if ($null -ne $targetShaProperty -and -not [string]::IsNullOrWhiteSpace([string]$targetShaProperty.Value)) {
            $sha256 = [string]$targetShaProperty.Value
        }
    }

    $asset = $property.Value
    if ($asset -is [string]) {
        $url = [string]$asset
        return [PSCustomObject]@{
            Url = $url
            Sha256 = $sha256
            Format = if ($url.EndsWith(".zip", [System.StringComparison]::OrdinalIgnoreCase)) { "zip" } else { "exe" }
        }
    }

    $urlProperty = $asset.PSObject.Properties["url"]
    if ($null -eq $urlProperty -or [string]::IsNullOrWhiteSpace([string]$urlProperty.Value)) {
        throw "Release manifest asset $Target is missing a URL."
    }

    $url = [string]$urlProperty.Value
    $formatProperty = $asset.PSObject.Properties["format"]
    $format = if ($null -eq $formatProperty -or [string]::IsNullOrWhiteSpace([string]$formatProperty.Value)) {
        if ($url.EndsWith(".zip", [System.StringComparison]::OrdinalIgnoreCase)) { "zip" } else { "exe" }
    } else {
        [string]$formatProperty.Value
    }
    if ($format -notin @("zip", "exe")) {
        throw "Release manifest asset $Target has unsupported format '$format'."
    }
    $shaProperty = $asset.PSObject.Properties["sha256"]
    if ($null -ne $shaProperty -and -not [string]::IsNullOrWhiteSpace([string]$shaProperty.Value)) {
        $sha256 = [string]$shaProperty.Value
    }

    return [PSCustomObject]@{
        Url = $url
        Sha256 = $sha256
        Format = $format
    }
}
function Invoke-CurlDownload {
    param(
        [string]$Uri,
        [string]$Destination
    )

    $parsedUri = $null
    if (-not [System.Uri]::TryCreate($Uri, [System.UriKind]::Absolute, [ref]$parsedUri) -or
        $parsedUri.Scheme -notin @("http", "https")) {
        throw "Herdr download URL must use HTTP or HTTPS: $Uri"
    }

    $curl = Get-Command curl.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $curl) {
        throw "Herdr installation requires curl.exe, which is included with supported Windows versions."
    }

    $arguments = @(
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--connect-timeout", "30",
        "--speed-limit", "1024",
        "--speed-time", "30"
    )
    if ($parsedUri.Scheme -eq "https") {
        $arguments += @("--proto", "=https", "--tlsv1.2")
    }
    $arguments += @("--output", $Destination, "--", $Uri)

    $curlOutput = & $curl.Source @arguments 2>&1
    $curlExitCode = $LASTEXITCODE
    if ($curlExitCode -ne 0) {
        $detail = ($curlOutput | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
        $message = "Failed to download $Uri (curl exit code $curlExitCode)."
        if (-not [string]::IsNullOrWhiteSpace($detail)) {
            $message += " $detail"
        }
        throw $message
    }
}


function Assert-ManifestObject {
    param(
        [object]$Value,
        [string]$Label
    )

    if ($null -eq $Value -or $Value -is [string] -or $Value -is [System.Collections.IEnumerable]) {
        throw "$Label must be a JSON object."
    }
}

function Assert-ExactManifestProperties {
    param(
        [object]$Value,
        [string[]]$Expected,
        [string]$Label
    )

    Assert-ManifestObject -Value $Value -Label $Label
    $actual = @($Value.PSObject.Properties | ForEach-Object { $_.Name })
    if ($actual.Count -ne $Expected.Count -or @($Expected | Where-Object { $actual -cnotcontains $_ }).Count -ne 0) {
        throw "$Label has unexpected fields."
    }
}

function Get-RequiredManifestProperty {
    param(
        [object]$Value,
        [string]$Name,
        [string]$Label
    )

    Assert-ManifestObject -Value $Value -Label $Label
    $property = $Value.PSObject.Properties[$Name]
    if ($null -eq $property) {
        throw "$Label is missing $Name."
    }
    return $property.Value
}

function Get-RequiredManifestString {
    param(
        [object]$Value,
        [string]$Label
    )

    if ($Value -isnot [string] -or
        [string]::IsNullOrWhiteSpace($Value) -or
        $Value -cne $Value.Trim() -or
        $Value.Contains("`n") -or
        $Value.Contains("`r")) {
        throw "$Label must be a nonempty one-line string."
    }
    return $Value
}

function Get-RequiredManifestGitObject {
    param(
        [object]$Value,
        [string]$Label
    )

    $value = Get-RequiredManifestString -Value $Value -Label $Label
    if ($value -notmatch '^[0-9a-f]{40}$') {
        throw "$Label must be a lowercase 40-character Git object ID."
    }
    return $value
}

function Get-RequiredManifestSha256 {
    param(
        [object]$Value,
        [string]$Label
    )

    $value = Get-RequiredManifestString -Value $Value -Label $Label
    if ($value -notmatch '^[0-9a-f]{64}$') {
        throw "$Label must be a lowercase SHA-256 digest."
    }
    return $value
}
function Get-RequiredManifestTimestamp {
    param(
        [object]$Value,
        [string]$Label
    )

    $timestamp = Get-RequiredManifestString -Value $Value -Label $Label
    if ($timestamp -notmatch '^\d{4}-\d{2}-\d{2}T.+(?:Z|[+-]\d{2}:\d{2})$') {
        throw "$Label must be an ISO-8601 timestamp with a timezone."
    }
    $parsed = [DateTimeOffset]::MinValue
    if (-not [DateTimeOffset]::TryParse(
            $timestamp,
            [System.Globalization.CultureInfo]::InvariantCulture,
            [System.Globalization.DateTimeStyles]::None,
            [ref]$parsed
        )) {
        throw "$Label must be an ISO-8601 timestamp with a timezone."
    }
    return $timestamp
}
function Get-RequiredManifestVersion {
    param(
        [object]$Value,
        [string]$Label
    )

    $version = Get-RequiredManifestString -Value $Value -Label $Label
    if ($version -notmatch '^v?[0-9]+\.[0-9]+\.[0-9]+$') {
        throw "$Label must be a semantic version."
    }
    return $version.TrimStart('v')
}



function Get-RequiredManifestPositiveInteger {
    param(
        [object]$Value,
        [string]$Label
    )

    if ($Value -is [bool] -or -not (
            $Value -is [byte] -or $Value -is [int16] -or $Value -is [uint16] -or
            $Value -is [int] -or $Value -is [uint32] -or $Value -is [long] -or
            $Value -is [uint64]
        ) -or [decimal]$Value -lt 1) {
        throw "$Label must be a positive integer."
    }
    return [decimal]$Value
}

function Get-Sha256Hex {
    param([string]$Value)

    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::ASCII.GetBytes($Value)
        return [System.BitConverter]::ToString($sha256.ComputeHash($bytes)).Replace("-", "").ToLowerInvariant()
    } finally {
        $sha256.Dispose()
    }
}
function Get-BridgeReleaseAssetUrl {
    param(
        [object]$Canonical,
        [string]$AssetName
    )

    return "https://github.com/Smarty-Pants-Inc/herdr/releases/download/smarty-preview-$($Canonical.BuildId)/$AssetName"
}
function Get-UpstreamPreviewReleaseAssetUrl {
    param(
        [string]$BuildId,
        [string]$AssetName
    )

    return "https://github.com/herdrdev/herdr/releases/download/preview-$BuildId/$AssetName"
}

function Get-UpstreamHerdrAssets {
    param(
        [object]$Value,
        [object]$Identity,
        [string]$Label
    )

    Assert-ManifestObject -Value $Value -Label $Label
    $shapes = @(
        [PSCustomObject]@{
            Targets = @("linux-x86_64", "linux-aarch64", "macos-x86_64", "macos-aarch64", "windows-x86_64")
            Names = @{
                "linux-x86_64" = "herdr-linux-x86_64"
                "linux-aarch64" = "herdr-linux-aarch64"
                "macos-x86_64" = "herdr-macos-x86_64"
                "macos-aarch64" = "herdr-macos-aarch64"
                "windows-x86_64" = "herdr-windows-x86_64.zip"
            }
            Formatted = @("windows-x86_64")
            WindowsFormatted = $true
        }
        [PSCustomObject]@{
            Targets = @("linux-x86_64", "linux-aarch64", "macos-x86_64", "macos-aarch64", "windows-x86_64")
            Names = @{
                "linux-x86_64" = "herdr-linux-x86_64"
                "linux-aarch64" = "herdr-linux-aarch64"
                "macos-x86_64" = "herdr-macos-x86_64"
                "macos-aarch64" = "herdr-macos-aarch64"
                "windows-x86_64" = "herdr-windows-x86_64.exe"
            }
            Formatted = @()
            WindowsFormatted = $false
        }
        [PSCustomObject]@{
            Targets = @("linux-x86_64", "linux-aarch64", "macos-x86_64", "macos-aarch64")
            Names = @{
                "linux-x86_64" = "herdr-linux-x86_64"
                "linux-aarch64" = "herdr-linux-aarch64"
                "macos-x86_64" = "herdr-macos-x86_64"
                "macos-aarch64" = "herdr-macos-aarch64"
            }
            Formatted = @()
            WindowsFormatted = $null
        }
    )
    $actualTargets = @($Value.PSObject.Properties | ForEach-Object { $_.Name })
    $windowsProperty = $Value.PSObject.Properties["windows-x86_64"]
    $windowsHasFormat = $null -ne $windowsProperty -and $null -ne $windowsProperty.Value.PSObject.Properties["format"]
    $selected = $null
    foreach ($shape in $shapes) {
        $windowsShapeMatches = if ($null -eq $shape.WindowsFormatted) {
            $null -eq $windowsProperty
        } else {
            $null -ne $windowsProperty -and $windowsHasFormat -eq $shape.WindowsFormatted
        }
        if ($windowsShapeMatches -and
            $actualTargets.Count -eq $shape.Targets.Count -and
            @($shape.Targets | Where-Object { $actualTargets -cnotcontains $_ }).Count -eq 0) {
            $selected = $shape
            break
        }
    }
    if ($null -eq $selected) {
        throw "$Label must use one of the authenticated upstream asset shapes."
    }
    $assets = @{}
    foreach ($targetName in $selected.Targets) {
        $withFormat = $selected.Formatted -contains $targetName
        $asset = Get-BridgeAsset `
            -Value (Get-RequiredManifestProperty -Value $Value -Name $targetName -Label $Label) `
            -Label "$Label.$targetName" `
            -ExpectedUrl (Get-UpstreamPreviewReleaseAssetUrl -BuildId $Identity.BuildId -AssetName $selected.Names[$targetName]) `
            -WithFormat:$withFormat
        if ($withFormat -and $asset.Format -cne "zip") {
            throw "$Label.windows-x86_64.format must be zip."
        }
        if ($targetName -eq "windows-x86_64" -and -not $withFormat) {
            $asset.Format = "exe"
        }
        $assets[$targetName] = $asset
    }
    return $assets
}

function Get-CustomPreviewAssets {
    param(
        [object]$Value,
        [string]$Label
    )

    Assert-ManifestObject -Value $Value -Label $Label
    $assets = @{}
    foreach ($property in $Value.PSObject.Properties) {
        $asset = Get-ManifestAsset -Manifest ([PSCustomObject]@{ assets = $Value }) -Target $property.Name
        $null = Get-RequiredManifestSha256 -Value $asset.Sha256 -Label "$Label.$($property.Name).sha256"
        if ($asset.Format -notin @("zip", "exe")) {
            throw "$Label.$($property.Name) has unsupported format '$($asset.Format)'."
        }
        $assets[$property.Name] = $asset
    }
    if ($assets.Count -eq 0) {
        throw "$Label must not be empty."
    }
    return $assets
}


function Get-PairedBuildId {
    param(
        [object]$Value,
        [string]$Label
    )

    $buildId = Get-RequiredManifestString -Value $Value -Label $Label
    $match = [regex]::Match(
        $buildId,
        '^(?<day>[0-9]{4}-[0-9]{2}-[0-9]{2})-p(?<parent>[0-9a-f]{40})-r(?<herdr>[0-9a-f]{40})-o(?<omp>[0-9a-f]{40})$'
    )
    if (-not $match.Success) {
        throw "$Label must encode the full exact P/R/O tuple."
    }
    try {
        $null = [DateTime]::ParseExact(
            $match.Groups["day"].Value,
            "yyyy-MM-dd",
            [System.Globalization.CultureInfo]::InvariantCulture,
            [System.Globalization.DateTimeStyles]::None
        )
    } catch {
        throw "$Label has an invalid build date."
    }
    return [PSCustomObject]@{
        BuildId = $buildId
        Kind = "paired"
        Day = $match.Groups["day"].Value
        Parent = $match.Groups["parent"].Value
        Herdr = $match.Groups["herdr"].Value
        Omp = $match.Groups["omp"].Value
        CommitPrefix = $null
    }
}

function Get-LegacyBuildId {
    param(
        [object]$Value,
        [string]$Label
    )

    $buildId = Get-RequiredManifestString -Value $Value -Label $Label
    $match = [regex]::Match(
        $buildId,
        '^(?<day>[0-9]{4}-[0-9]{2}-[0-9]{2})-(?<commit>[0-9a-f]{12})$'
    )
    if (-not $match.Success) {
        throw "$Label must use YYYY-MM-DD-<12 lowercase hex>."
    }
    try {
        $null = [DateTime]::ParseExact(
            $match.Groups["day"].Value,
            "yyyy-MM-dd",
            [System.Globalization.CultureInfo]::InvariantCulture,
            [System.Globalization.DateTimeStyles]::None
        )
    } catch {
        throw "$Label has an invalid build date."
    }
    return [PSCustomObject]@{
        BuildId = $buildId
        Kind = "legacy"
        Day = $match.Groups["day"].Value
        Parent = $null
        Herdr = $null
        Omp = $null
        CommitPrefix = $match.Groups["commit"].Value
    }
}

function Get-RetainedBuildId {
    param(
        [object]$Value,
        [string]$Label
    )

    $buildId = Get-RequiredManifestString -Value $Value -Label $Label
    if ($buildId -match '-p[0-9a-f]{40}-r[0-9a-f]{40}-o[0-9a-f]{40}$') {
        return Get-PairedBuildId -Value $buildId -Label $Label
    }
    return Get-LegacyBuildId -Value $buildId -Label $Label
}

function Get-BridgeAsset {
    param(
        [object]$Value,
        [string]$Label,
        [string]$ExpectedUrl,
        [switch]$WithFormat
    )

    $expected = if ($WithFormat) { @("url", "sha256", "format") } else { @("url", "sha256") }
    Assert-ExactManifestProperties -Value $Value -Expected $expected -Label $Label
    $url = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Value -Name "url" -Label $Label) -Label "$Label.url"
    if ($url -cne $ExpectedUrl) {
        throw "$Label.url does not bind the canonical release tag."
    }
    $sha256 = Get-RequiredManifestSha256 -Value (Get-RequiredManifestProperty -Value $Value -Name "sha256" -Label $Label) -Label "$Label.sha256"
    $format = $null
    if ($WithFormat) {
        $format = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Value -Name "format" -Label $Label) -Label "$Label.format"
        if ($format -notin @("zip", "exe")) {
            throw "$Label.format is unsupported."
        }
    }
    return [PSCustomObject]@{ Url = $url; Sha256 = $sha256; Format = $format }
}


function Assert-BridgeAssetMatch {
    param(
        [object]$Actual,
        [object]$Expected,
        [string]$Label
    )

    if ($Actual.Url -cne $Expected.Url -or
        $Actual.Sha256 -cne $Expected.Sha256 -or
        $Actual.Format -cne $Expected.Format) {
        throw "$Label does not match the retained canonical asset."
    }
    return $Expected
}

function Get-BridgeOmp {
    param(
        [object]$Value,
        [object]$Canonical,
        [string]$Label
    )

    Assert-ExactManifestProperties -Value $Value -Expected @("assets", "build_id", "commit", "tree", "version") -Label $Label
    $buildId = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Value -Name "build_id" -Label $Label) -Label "$Label.build_id"
    $commit = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Value -Name "commit" -Label $Label) -Label "$Label.commit"
    if ($null -ne $Canonical.Omp -and $commit -cne $Canonical.Omp) {
        throw "$Label.commit does not match the canonical P/R/O build ID."
    }
    $tree = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Value -Name "tree" -Label $Label) -Label "$Label.tree"
    $version = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Value -Name "version" -Label $Label) -Label "$Label.version"
    $assets = Get-RequiredManifestProperty -Value $Value -Name "assets" -Label $Label
    $targets = @("linux-x86_64", "linux-aarch64", "macos-x86_64", "macos-aarch64")
    Assert-ExactManifestProperties -Value $assets -Expected $targets -Label "$Label.assets"
    $parsedAssets = @{}
    foreach ($targetName in $targets) {
        $parsedAssets[$targetName] = Get-BridgeAsset -Value (Get-RequiredManifestProperty -Value $assets -Name $targetName -Label "$Label.assets") -Label "$Label.assets.$targetName" -ExpectedUrl (Get-BridgeReleaseAssetUrl -Canonical $Canonical -AssetName "omp-$targetName")

    }
    return [PSCustomObject]@{
        BuildId = $buildId
        Commit = $commit
        Tree = $tree
        Version = $version
        Assets = $parsedAssets
    }
}

function Assert-BridgeOmpMatch {
    param(
        [object]$Actual,
        [object]$Expected,
        [object]$Canonical,
        [string]$Label
    )

    $actualOmp = Get-BridgeOmp -Value $Actual -Canonical $Canonical -Label "$Label top-level OMP"
    $expectedOmp = $Expected
    foreach ($field in @("BuildId", "Commit", "Tree", "Version")) {
        if ($actualOmp.$field -cne $expectedOmp.$field) {
            throw "$Label OMP scalar binding mismatch."
        }
    }
    foreach ($targetName in $expectedOmp.Assets.Keys) {
        Assert-BridgeAssetMatch -Actual $actualOmp.Assets[$targetName] -Expected $expectedOmp.Assets[$targetName] -Label "$Label OMP asset $targetName" | Out-Null
    }
}

function Get-BridgeHerdrAssets {
    param(
        [object]$Value,
        [object]$Canonical,
        [string]$Label
    )

    $names = @{
        "linux-x86_64" = "herdr-linux-x86_64"
        "linux-aarch64" = "herdr-linux-aarch64"
        "macos-x86_64" = "herdr-macos-x86_64"
        "macos-aarch64" = "herdr-macos-aarch64"
        "windows-x86_64" = "herdr-windows-x86_64.zip"
    }
    $targets = @($names.Keys)
    Assert-ExactManifestProperties -Value $Value -Expected $targets -Label $Label
    $assets = @{}
    foreach ($targetName in $targets) {
        $assets[$targetName] = Get-BridgeAsset -Value (Get-RequiredManifestProperty -Value $Value -Name $targetName -Label $Label) -Label "$Label.$targetName" -ExpectedUrl (Get-BridgeReleaseAssetUrl -Canonical $Canonical -AssetName $names[$targetName]) -WithFormat:($targetName -eq "windows-x86_64")
    }
    if ($assets["windows-x86_64"].Format -cne "zip") {
        throw "$Label.windows-x86_64.format must be zip."
    }
    return $assets
}

function Get-UpstreamRetainedPreviewBuild {
    param(
        [string]$BuildId,
        [object]$Build,
        [string]$Label
    )

    $identity = Get-LegacyBuildId -Value $BuildId -Label $Label
    Assert-ManifestObject -Value $Build -Label $Label
    Assert-ExactManifestProperties -Value $Build -Expected @("assets", "base_version", "built_at", "commit", "protocol", "tag") -Label $Label
    $tag = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Build -Name "tag" -Label $Label) -Label "$Label.tag"
    if ($tag -cne "preview-$($identity.BuildId)") {
        throw "$Label.tag does not bind the retained build ID."
    }
    $commit = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Build -Name "commit" -Label $Label) -Label "$Label.commit"
    if (-not $commit.StartsWith($identity.CommitPrefix, [System.StringComparison]::Ordinal)) {
        throw "$Label.commit does not match the retained legacy build ID."
    }
    $builtAt = Get-RequiredManifestTimestamp -Value (Get-RequiredManifestProperty -Value $Build -Name "built_at" -Label $Label) -Label "$Label.built_at"
    return [PSCustomObject]@{
        Identity = $identity
        BaseVersion = Get-RequiredManifestVersion -Value (Get-RequiredManifestProperty -Value $Build -Name "base_version" -Label $Label) -Label "$Label.base_version"
        BuiltAt = $builtAt
        Commit = $commit
        Protocol = Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Build -Name "protocol" -Label $Label) -Label "$Label.protocol"
        Assets = Get-UpstreamHerdrAssets -Value (Get-RequiredManifestProperty -Value $Build -Name "assets" -Label $Label) -Identity $identity -Label "$Label.assets"
        Omp = $null
    }
}

function Get-RetainedPreviewBuild {
    param(
        [string]$BuildId,
        [object]$Build,
        [string]$Label
    )

    $identity = Get-RetainedBuildId -Value $BuildId -Label $Label
    Assert-ManifestObject -Value $Build -Label $Label
    $fields = @("assets", "base_version", "built_at", "commit", "protocol", "tag")
    $ompProperty = $Build.PSObject.Properties["omp"]
    if ($null -ne $ompProperty) {
        $fields += "omp"
    }
    Assert-ExactManifestProperties -Value $Build -Expected $fields -Label $Label

    $tag = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Build -Name "tag" -Label $Label) -Label "$Label.tag"
    if ($tag -cne "smarty-preview-$($identity.BuildId)") {
        throw "$Label.tag does not bind the retained build ID."
    }
    $commit = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Build -Name "commit" -Label $Label) -Label "$Label.commit"
    if ($identity.Kind -eq "paired") {
        if ($commit -cne $identity.Herdr) {
            throw "$Label.commit does not match the retained P/R/O build ID."
        }
        if ($null -eq $ompProperty) {
            throw "$Label has no paired OMP metadata."
        }
    } elseif (-not $commit.StartsWith($identity.CommitPrefix, [System.StringComparison]::Ordinal)) {
        throw "$Label.commit does not match the retained legacy build ID."
    }

    $sourceBuiltAt = Get-RequiredManifestProperty -Value $Build -Name "built_at" -Label "$Label.built_at"
    $builtAt = Get-RequiredManifestTimestamp -Value $sourceBuiltAt -Label "$Label.built_at"
    if ($identity.Kind -eq "legacy") {
        if ($identity.Day -cne $sourceBuiltAt.Substring(0, 10)) {
            throw "$Label.built_at date does not match the retained legacy build ID."
        }
    } else {
        if ($builtAt -cnotmatch '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$') {
            throw "$Label.built_at must be a canonical second-precision UTC timestamp."
        }
        if ($identity.Day -cne $builtAt.Substring(0, 10)) {
            throw "$Label.built_at date does not match the retained P/R/O build ID."
        }
    }

    $omp = if ($null -ne $ompProperty) {
        Get-BridgeOmp -Value $ompProperty.Value -Canonical $identity -Label "$Label.omp"
    } else {
        $null
    }
    return [PSCustomObject]@{
        Identity = $identity
        BaseVersion = Get-RequiredManifestVersion -Value (Get-RequiredManifestProperty -Value $Build -Name "base_version" -Label $Label) -Label "$Label.base_version"
        BuiltAt = $builtAt
        Commit = $commit
        Protocol = Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Build -Name "protocol" -Label $Label) -Label "$Label.protocol"
        Assets = Get-BridgeHerdrAssets -Value (Get-RequiredManifestProperty -Value $Build -Name "assets" -Label $Label) -Canonical $identity -Label "$Label.assets"
        Omp = $omp
    }
}

function Get-RetainedPreviewBuilds {
    param(
        [object]$Builds,
        [string]$Label,
        [ValidateSet("Smarty", "Upstream")]
        [string]$Contract = "Smarty"
    )

    Assert-ManifestObject -Value $Builds -Label $Label
    $records = @{}
    foreach ($property in $Builds.PSObject.Properties) {
        $recordLabel = "$Label.$($property.Name)"
        if ($Contract -eq "Upstream") {
            $records[$property.Name] = Get-UpstreamRetainedPreviewBuild -BuildId $property.Name -Build $property.Value -Label $recordLabel
        } else {
            $records[$property.Name] = Get-RetainedPreviewBuild -BuildId $property.Name -Build $property.Value -Label $recordLabel
        }
    }
    if ($records.Count -eq 0) {
        throw "$Label must not be empty."
    }
    return $records
}

function Get-CustomRetainedPreviewBuild {
    param(
        [string]$BuildId,
        [object]$Build,
        [string]$Label
    )

    $identity = Get-RetainedBuildId -Value $BuildId -Label $Label
    Assert-ManifestObject -Value $Build -Label $Label
    $fields = @("assets", "base_version", "built_at", "commit", "protocol", "tag")
    $ompProperty = $Build.PSObject.Properties["omp"]
    if ($null -ne $ompProperty) {
        $fields += "omp"
    }
    Assert-ExactManifestProperties -Value $Build -Expected $fields -Label $Label
    $tag = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Build -Name "tag" -Label $Label) -Label "$Label.tag"
    if (-not $tag.EndsWith($identity.BuildId, [System.StringComparison]::Ordinal)) {
        throw "$Label.tag does not bind the retained build ID."
    }
    $commit = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Build -Name "commit" -Label $Label) -Label "$Label.commit"
    if ($identity.Kind -eq "paired" -and $commit -cne $identity.Herdr) {
        throw "$Label.commit does not match the retained P/R/O build ID."
    }
    if ($identity.Kind -eq "legacy" -and -not $commit.StartsWith($identity.CommitPrefix, [System.StringComparison]::Ordinal)) {
        throw "$Label.commit does not match the retained legacy build ID."
    }
    $builtAt = Get-RequiredManifestTimestamp -Value (Get-RequiredManifestProperty -Value $Build -Name "built_at" -Label $Label) -Label "$Label.built_at"
    if ($identity.Day -cne $builtAt.Substring(0, 10)) {
        throw "$Label.built_at date does not match the retained build ID."
    }
    return [PSCustomObject]@{
        Identity = $identity
        BaseVersion = Get-RequiredManifestVersion -Value (Get-RequiredManifestProperty -Value $Build -Name "base_version" -Label $Label) -Label "$Label.base_version"
        BuiltAt = $builtAt
        Commit = $commit
        Protocol = Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Build -Name "protocol" -Label $Label) -Label "$Label.protocol"
        Assets = Get-CustomPreviewAssets -Value (Get-RequiredManifestProperty -Value $Build -Name "assets" -Label $Label) -Label "$Label.assets"
        Omp = if ($null -eq $ompProperty) { $null } else { Get-CustomPreviewOmp -Value $ompProperty.Value -Identity $identity -Label "$Label.omp" }
    }
}

function Get-CustomPreviewOmp {
    param(
        [object]$Value,
        [object]$Identity,
        [string]$Label
    )

    Assert-ExactManifestProperties -Value $Value -Expected @("assets", "build_id", "commit", "tree", "version") -Label $Label
    $commit = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Value -Name "commit" -Label $Label) -Label "$Label.commit"
    if ($Identity.Kind -eq "paired" -and $commit -cne $Identity.Omp) {
        throw "$Label.commit does not match the custom P/R/O build ID."
    }
    $tree = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Value -Name "tree" -Label $Label) -Label "$Label.tree"
    $buildId = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Value -Name "build_id" -Label $Label) -Label "$Label.build_id"
    $version = Get-RequiredManifestVersion -Value (Get-RequiredManifestProperty -Value $Value -Name "version" -Label $Label) -Label "$Label.version"
    $assets = Get-CustomPreviewAssets -Value (Get-RequiredManifestProperty -Value $Value -Name "assets" -Label $Label) -Label "$Label.assets"
    return [PSCustomObject]@{ BuildId = $buildId; Commit = $commit; Tree = $tree; Version = $version; Assets = $assets }
}

function Assert-CustomOmpMatch {
    param(
        [object]$Actual,
        [object]$Expected,
        [string]$Label
    )

    if (($null -eq $Actual) -ne ($null -eq $Expected)) {
        throw "$Label presence differs from the retained current build."
    }
    if ($null -eq $Actual) {
        return
    }
    foreach ($field in @("BuildId", "Commit", "Tree", "Version")) {
        if ($Actual.$field -cne $Expected.$field) {
            throw "$Label scalar metadata differs from the retained current build."
        }
    }
    if ($Actual.Assets.Count -ne $Expected.Assets.Count) {
        throw "$Label asset allow-list differs from the retained current build."
    }
    foreach ($targetName in $Expected.Assets.Keys) {
        if ($null -eq $Actual.Assets[$targetName] -or
            $Actual.Assets[$targetName].Url -cne $Expected.Assets[$targetName].Url -or
            $Actual.Assets[$targetName].Sha256 -cne $Expected.Assets[$targetName].Sha256 -or
            $Actual.Assets[$targetName].Format -cne $Expected.Assets[$targetName].Format) {
            throw "$Label asset $targetName differs from the retained current build."
        }
    }
}

function Resolve-UpstreamPreviewManifest {
    param(
        [object]$Manifest,
        [string]$Target
    )
    $topLevelFields = @("assets", "base_version", "build_id", "builds", "built_at", "channel", "commit", "notes", "protocol", "schema_version")
    Assert-ExactManifestProperties -Value $Manifest -Expected $topLevelFields -Label "Upstream preview manifest"
    if ((Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Manifest -Name "schema_version" -Label "Upstream preview manifest") -Label "Upstream preview manifest.schema_version") -ne 1 -or
        (Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Manifest -Name "channel" -Label "Upstream preview manifest") -Label "Upstream preview manifest.channel") -cne "preview") {
        throw "Upstream preview manifest schema or channel mismatch."
    }
    $currentId = Get-LegacyBuildId -Value (Get-RequiredManifestProperty -Value $Manifest -Name "build_id" -Label "Upstream preview manifest") -Label "Upstream preview manifest.build_id"
    if ($null -ne $Manifest.PSObject.Properties["omp"]) {
        throw "Upstream preview manifest must not claim paired OMP metadata."
    }
    $builds = Get-RetainedPreviewBuilds -Builds (Get-RequiredManifestProperty -Value $Manifest -Name "builds" -Label "Upstream preview manifest") -Label "Upstream preview manifest.builds" -Contract Upstream
    if (-not $builds.ContainsKey($currentId.BuildId)) {
        throw "Upstream preview manifest is missing its current retained build."
    }
    $retained = $builds[$currentId.BuildId]
    if (-not $retained.Assets.ContainsKey($target)) {
        throw "Upstream preview manifest has no binary for $target."
    }
    $topAssets = Get-UpstreamHerdrAssets -Value (Get-RequiredManifestProperty -Value $Manifest -Name "assets" -Label "Upstream preview manifest") -Identity $currentId -Label "Upstream preview manifest.assets"
    $baseVersion = Get-RequiredManifestVersion -Value (Get-RequiredManifestProperty -Value $Manifest -Name "base_version" -Label "Upstream preview manifest") -Label "Upstream preview manifest.base_version"
    $builtAt = Get-RequiredManifestTimestamp -Value (Get-RequiredManifestProperty -Value $Manifest -Name "built_at" -Label "Upstream preview manifest") -Label "Upstream preview manifest.built_at"
    $commit = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Manifest -Name "commit" -Label "Upstream preview manifest") -Label "Upstream preview manifest.commit"
    $protocol = Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Manifest -Name "protocol" -Label "Upstream preview manifest") -Label "Upstream preview manifest.protocol"
    if ($baseVersion -cne $retained.BaseVersion -or $builtAt -cne $retained.BuiltAt -or $commit -cne $retained.Commit -or $protocol -ne $retained.Protocol) {
        throw "Upstream preview manifest top-level/current archive mismatch."
    }
    if ($topAssets.Count -ne $retained.Assets.Count) {
        throw "Upstream preview manifest top-level/current asset allow-list mismatch."
    }
    foreach ($targetName in $retained.Assets.Keys) {
        Assert-BridgeAssetMatch -Actual $topAssets[$targetName] -Expected $retained.Assets[$targetName] -Label "Upstream preview manifest asset $targetName" | Out-Null
    }
    if (-not $commit.StartsWith($currentId.CommitPrefix, [System.StringComparison]::Ordinal)) {
        throw "Upstream preview manifest commit does not match its legacy build ID."
    }
    $notes = Get-RequiredManifestProperty -Value $Manifest -Name "notes" -Label "Upstream preview manifest"
    if ($notes -isnot [string] -or [string]::IsNullOrWhiteSpace($notes)) {
        throw "Upstream preview manifest.notes must be a nonempty string."
    }
    return [PSCustomObject]@{
        Asset = $retained.Assets[$target]
        VersionIdentity = "$($retained.BaseVersion)-preview.$($currentId.BuildId)"
        AcceptedBuildIds = @($currentId.BuildId)
    }
}

function Resolve-CustomPreviewManifest {
    param(
        [object]$Manifest,
        [string]$Target
    )

    $topLevelFields = @("assets", "base_version", "build_id", "builds", "built_at", "channel", "commit", "notes", "protocol", "schema_version")
    $ompProperty = $Manifest.PSObject.Properties["omp"]
    if ($null -ne $ompProperty) {
        $topLevelFields += "omp"
    }
    Assert-ExactManifestProperties -Value $Manifest -Expected $topLevelFields -Label "Custom preview manifest"
    if ((Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Manifest -Name "schema_version" -Label "Custom preview manifest") -Label "Custom preview manifest.schema_version") -ne 1 -or
        (Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Manifest -Name "channel" -Label "Custom preview manifest") -Label "Custom preview manifest.channel") -cne "preview") {
        throw "Custom preview manifest schema or channel mismatch."
    }
    $currentId = Get-RetainedBuildId -Value (Get-RequiredManifestProperty -Value $Manifest -Name "build_id" -Label "Custom preview manifest") -Label "Custom preview manifest.build_id"
    $buildsValue = Get-RequiredManifestProperty -Value $Manifest -Name "builds" -Label "Custom preview manifest"
    Assert-ManifestObject -Value $buildsValue -Label "Custom preview manifest.builds"
    if (@($buildsValue.PSObject.Properties).Count -eq 0) {
        throw "Custom preview manifest.builds must not be empty."
    }
    $currentProperty = $buildsValue.PSObject.Properties[$currentId.BuildId]
    if ($null -eq $currentProperty) {
        throw "Custom preview manifest is missing its current retained build."
    }
    $retained = Get-CustomRetainedPreviewBuild -BuildId $currentId.BuildId -Build $currentProperty.Value -Label "Custom preview manifest.builds.$($currentId.BuildId)"
    $topOmp = if ($null -eq $ompProperty) { $null } else { Get-CustomPreviewOmp -Value $ompProperty.Value -Identity $currentId -Label "Custom preview manifest.omp" }
    $topAssets = Get-CustomPreviewAssets -Value (Get-RequiredManifestProperty -Value $Manifest -Name "assets" -Label "Custom preview manifest") -Label "Custom preview manifest.assets"
    $baseVersion = Get-RequiredManifestVersion -Value (Get-RequiredManifestProperty -Value $Manifest -Name "base_version" -Label "Custom preview manifest") -Label "Custom preview manifest.base_version"
    $builtAt = Get-RequiredManifestTimestamp -Value (Get-RequiredManifestProperty -Value $Manifest -Name "built_at" -Label "Custom preview manifest") -Label "Custom preview manifest.built_at"
    $commit = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Manifest -Name "commit" -Label "Custom preview manifest") -Label "Custom preview manifest.commit"
    $protocol = Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Manifest -Name "protocol" -Label "Custom preview manifest") -Label "Custom preview manifest.protocol"
    if ($baseVersion -cne $retained.BaseVersion -or $builtAt -cne $retained.BuiltAt -or $commit -cne $retained.Commit -or $protocol -ne $retained.Protocol) {
        throw "Custom preview manifest top-level/current archive mismatch."
    }
    Assert-CustomOmpMatch -Actual $topOmp -Expected $retained.Omp -Label "Custom preview manifest.omp"
    if ($topAssets.Count -ne $retained.Assets.Count -or $null -eq $retained.Assets[$Target]) {
        throw "Custom preview manifest asset allow-list differs from the retained current build."
    }
    foreach ($targetName in $retained.Assets.Keys) {
        if ($null -eq $topAssets[$targetName] -or
            $topAssets[$targetName].Url -cne $retained.Assets[$targetName].Url -or
            $topAssets[$targetName].Sha256 -cne $retained.Assets[$targetName].Sha256 -or
            $topAssets[$targetName].Format -cne $retained.Assets[$targetName].Format) {
            throw "Custom preview manifest asset $targetName does not match the retained current build."
        }
    }
    $notes = Get-RequiredManifestProperty -Value $Manifest -Name "notes" -Label "Custom preview manifest"
    if ($notes -isnot [string] -or [string]::IsNullOrWhiteSpace($notes)) {
        throw "Custom preview manifest.notes must be a nonempty string."
    }
    return [PSCustomObject]@{
        Asset = $retained.Assets[$Target]
        VersionIdentity = "$($retained.BaseVersion)-preview.$($currentId.BuildId)"
        AcceptedBuildIds = @($currentId.BuildId)
    }
}

function Get-PhaseARetainedBuild {
    param(
        [object]$Builds,
        [object]$Canonical,
        [string]$Label
    )

    $retainedBuilds = Get-RetainedPreviewBuilds -Builds $Builds -Label $Label
    if (-not $retainedBuilds.ContainsKey($Canonical.BuildId)) {
        throw "$Label is missing $($Canonical.BuildId)."
    }
    return $retainedBuilds[$Canonical.BuildId]
}

function Resolve-PhaseABridgeManifest {
    param([object]$Manifest)

    $topLevelFields = @("assets", "base_version", "bootstrap", "build_id", "builds", "built_at", "canonical_build_id", "channel", "commit", "notes", "omp", "protocol", "schema_version")
    Assert-ExactManifestProperties -Value $Manifest -Expected $topLevelFields -Label "Preview Phase A bridge"
    if ((Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Manifest -Name "schema_version" -Label "Preview Phase A bridge") -Label "Preview Phase A bridge.schema_version") -ne 2) {
        throw "Preview Phase A bridge schema mismatch."
    }
    if ((Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Manifest -Name "channel" -Label "Preview Phase A bridge") -Label "Preview Phase A bridge.channel") -cne "preview") {
        throw "Preview Phase A bridge channel mismatch."
    }
    $canonical = Get-PairedBuildId -Value (Get-RequiredManifestProperty -Value $Manifest -Name "canonical_build_id" -Label "Preview Phase A bridge") -Label "Preview Phase A bridge.canonical_build_id"
    $alias = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Manifest -Name "build_id" -Label "Preview Phase A bridge") -Label "Preview Phase A bridge.build_id"
    if ($alias -cne "bootstrap-$(Get-Sha256Hex -Value $canonical.BuildId)") {
        throw "Preview Phase A bridge build_id does not bind the canonical build ID."
    }
    $retained = Get-PhaseARetainedBuild -Builds (Get-RequiredManifestProperty -Value $Manifest -Name "builds" -Label "Preview Phase A bridge") -Canonical $canonical -Label "Preview Phase A bridge.builds"
    $baseVersion = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Manifest -Name "base_version" -Label "Preview Phase A bridge") -Label "Preview Phase A bridge.base_version"
    $builtAt = Get-RequiredManifestTimestamp -Value (Get-RequiredManifestProperty -Value $Manifest -Name "built_at" -Label "Preview Phase A bridge") -Label "Preview Phase A bridge.built_at"

    $commit = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Manifest -Name "commit" -Label "Preview Phase A bridge") -Label "Preview Phase A bridge.commit"
    $protocol = Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Manifest -Name "protocol" -Label "Preview Phase A bridge") -Label "Preview Phase A bridge.protocol"
    if ($commit -cne $canonical.Herdr -or $baseVersion -cne $retained.BaseVersion -or $builtAt -cne $retained.BuiltAt -or $commit -cne $retained.Commit -or $protocol -ne $retained.Protocol) {
        throw "Preview Phase A bridge scalar binding mismatch."
    }
    $notes = Get-RequiredManifestProperty -Value $Manifest -Name "notes" -Label "Preview Phase A bridge"
    if ($notes -isnot [string]) {
        throw "Preview Phase A bridge.notes must be a string."
    }
    Assert-BridgeOmpMatch -Actual (Get-RequiredManifestProperty -Value $Manifest -Name "omp" -Label "Preview Phase A bridge") -Expected $retained.Omp -Canonical $canonical -Label "Preview Phase A bridge"
    $topAssets = Get-BridgeHerdrAssets -Value (Get-RequiredManifestProperty -Value $Manifest -Name "assets" -Label "Preview Phase A bridge") -Canonical $canonical -Label "Preview Phase A bridge.assets"
    foreach ($targetName in $retained.Assets.Keys) {
        Assert-BridgeAssetMatch -Actual $topAssets[$targetName] -Expected $retained.Assets[$targetName] -Label "Preview Phase A bridge asset $targetName" | Out-Null
    }
    $asset = $topAssets["windows-x86_64"]
    $bootstrap = Get-RequiredManifestProperty -Value $Manifest -Name "bootstrap" -Label "Preview Phase A bridge"
    Assert-ExactManifestProperties -Value $bootstrap -Expected @("schema", "paired_build_id", "paired_tag", "paired_manifest", "windows_asset_sha256") -Label "Preview Phase A bridge.bootstrap"
    if ((Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $bootstrap -Name "schema" -Label "Preview Phase A bridge.bootstrap") -Label "Preview Phase A bridge.bootstrap.schema") -cne "smarty.windows-bootstrap.v1" -or
        (Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $bootstrap -Name "paired_build_id" -Label "Preview Phase A bridge.bootstrap") -Label "Preview Phase A bridge.bootstrap.paired_build_id") -cne $canonical.BuildId -or
        (Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $bootstrap -Name "paired_tag" -Label "Preview Phase A bridge.bootstrap") -Label "Preview Phase A bridge.bootstrap.paired_tag") -cne "smarty-preview-$($canonical.BuildId)" -or
        (Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $bootstrap -Name "paired_manifest" -Label "Preview Phase A bridge.bootstrap") -Label "Preview Phase A bridge.bootstrap.paired_manifest") -cne "preview.json" -or
        (Get-RequiredManifestSha256 -Value (Get-RequiredManifestProperty -Value $bootstrap -Name "windows_asset_sha256" -Label "Preview Phase A bridge.bootstrap") -Label "Preview Phase A bridge.bootstrap.windows_asset_sha256") -cne $asset.Sha256) {
        throw "Preview Phase A bridge bootstrap binding mismatch."
    }
    return [PSCustomObject]@{
        Asset = $asset
        VersionIdentity = "$($retained.BaseVersion)-preview.$($canonical.BuildId)"
        AcceptedBuildIds = @($alias, $canonical.BuildId)
    }
}

function Resolve-CanonicalPreviewManifest {
    param([object]$Manifest)

    $topLevelFields = @("assets", "base_version", "build_id", "builds", "built_at", "channel", "commit", "notes", "omp", "protocol", "schema_version")
    Assert-ExactManifestProperties -Value $Manifest -Expected $topLevelFields -Label "Canonical preview manifest"
    if ((Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Manifest -Name "schema_version" -Label "Canonical preview manifest") -Label "Canonical preview manifest.schema_version") -ne 1 -or
        (Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Manifest -Name "channel" -Label "Canonical preview manifest") -Label "Canonical preview manifest.channel") -cne "preview") {
        throw "Canonical preview manifest schema or channel mismatch."
    }
    $canonical = Get-PairedBuildId -Value (Get-RequiredManifestProperty -Value $Manifest -Name "build_id" -Label "Canonical preview manifest") -Label "Canonical preview manifest.build_id"
    $retained = Get-PhaseARetainedBuild -Builds (Get-RequiredManifestProperty -Value $Manifest -Name "builds" -Label "Canonical preview manifest") -Canonical $canonical -Label "Canonical preview manifest.builds"
    $baseVersion = Get-RequiredManifestString -Value (Get-RequiredManifestProperty -Value $Manifest -Name "base_version" -Label "Canonical preview manifest") -Label "Canonical preview manifest.base_version"
    $builtAt = Get-RequiredManifestTimestamp -Value (Get-RequiredManifestProperty -Value $Manifest -Name "built_at" -Label "Canonical preview manifest") -Label "Canonical preview manifest.built_at"

    $commit = Get-RequiredManifestGitObject -Value (Get-RequiredManifestProperty -Value $Manifest -Name "commit" -Label "Canonical preview manifest") -Label "Canonical preview manifest.commit"
    $protocol = Get-RequiredManifestPositiveInteger -Value (Get-RequiredManifestProperty -Value $Manifest -Name "protocol" -Label "Canonical preview manifest") -Label "Canonical preview manifest.protocol"
    if ($commit -cne $canonical.Herdr -or $baseVersion -cne $retained.BaseVersion -or $builtAt -cne $retained.BuiltAt -or $commit -cne $retained.Commit -or $protocol -ne $retained.Protocol) {
        throw "Canonical preview manifest scalar binding mismatch."
    }
    $notes = Get-RequiredManifestProperty -Value $Manifest -Name "notes" -Label "Canonical preview manifest"
    if ($notes -isnot [string]) {
        throw "Canonical preview manifest.notes must be a string."
    }
    Assert-BridgeOmpMatch -Actual (Get-RequiredManifestProperty -Value $Manifest -Name "omp" -Label "Canonical preview manifest") -Expected $retained.Omp -Canonical $canonical -Label "Canonical preview manifest"
    $topAssets = Get-BridgeHerdrAssets -Value (Get-RequiredManifestProperty -Value $Manifest -Name "assets" -Label "Canonical preview manifest") -Canonical $canonical -Label "Canonical preview manifest.assets"
    foreach ($targetName in $retained.Assets.Keys) {
        Assert-BridgeAssetMatch -Actual $topAssets[$targetName] -Expected $retained.Assets[$targetName] -Label "Canonical preview manifest asset $targetName" | Out-Null
    }
    $alias = "bootstrap-$(Get-Sha256Hex -Value $canonical.BuildId)"
    return [PSCustomObject]@{
        Asset = $retained.Assets["windows-x86_64"]
        VersionIdentity = "$($retained.BaseVersion)-preview.$($canonical.BuildId)"
        AcceptedBuildIds = @($canonical.BuildId, $alias)
    }
}
function Test-PhaseABridgeManifestCandidate {
    param([object]$Manifest)

    Assert-ManifestObject -Value $Manifest -Label "Preview manifest"
    $buildId = $Manifest.PSObject.Properties["build_id"]
    if ($null -ne $Manifest.PSObject.Properties["canonical_build_id"] -or
        $null -ne $Manifest.PSObject.Properties["bootstrap"] -or
        ($null -ne $buildId -and $buildId.Value -is [string] -and $buildId.Value.StartsWith("bootstrap-", [System.StringComparison]::Ordinal))) {
        return $true
    }
    $schemaVersion = $Manifest.PSObject.Properties["schema_version"]
    return $null -ne $schemaVersion -and [string]$schemaVersion.Value -ceq "2"
}


function Resolve-PreviewManifest {
    param(
        [object]$Manifest,
        [string]$Target,
        [string]$ManifestUrl
    )

    Assert-ManifestObject -Value $Manifest -Label "Preview manifest"
    $smartyManifestUrl = "https://raw.githubusercontent.com/Smarty-Pants-Inc/herdr/smarty-channel/preview.json"
    $isBridge = Test-PhaseABridgeManifestCandidate -Manifest $Manifest
    if ($ManifestUrl -ceq $smartyManifestUrl) {
        if ($isBridge) {
            return Resolve-PhaseABridgeManifest -Manifest $Manifest
        }
        return Resolve-CanonicalPreviewManifest -Manifest $Manifest
    }
    if ($isBridge) {
        throw "Smarty Phase A bridge manifests require the exact Smarty channel manifest URL."
    }
    if ([string]::IsNullOrWhiteSpace($ManifestUrl) -or $ManifestUrl -ceq "https://herdr.dev/preview.json") {
        return Resolve-UpstreamPreviewManifest -Manifest $Manifest -Target $Target
    }
    return Resolve-CustomPreviewManifest -Manifest $Manifest -Target $Target
}

function ConvertTo-ManifestObject {
    param([object]$Manifest)

    if ($Manifest -isnot [string]) {
        return $Manifest
    }

    $json = $Manifest.TrimStart([char]0xFEFF)
    $utf8BomDecodedAsLatin1 = [string]::Concat([char]0x00EF, [char]0x00BB, [char]0x00BF)
    if ($json.StartsWith($utf8BomDecodedAsLatin1)) {
        $json = $json.Substring(3)
    }

    return $json | ConvertFrom-Json
}
function Get-RemoteManifest {
    param([string]$Uri)

    $manifestPath = Join-Path ([System.IO.Path]::GetTempPath()) ("herdr-manifest-" + [System.Guid]::NewGuid().ToString("N") + ".json")
    try {
        Invoke-CurlDownload -Uri $Uri -Destination $manifestPath
        return ConvertTo-ManifestObject -Manifest ([System.IO.File]::ReadAllText($manifestPath))
    } finally {
        Remove-Item -LiteralPath $manifestPath -Force -ErrorAction SilentlyContinue
    }
}


function Test-FileDigest {
    param(
        [string]$Path,
        [string]$ExpectedDigest
    )

    if ([string]::IsNullOrWhiteSpace($ExpectedDigest)) {
        throw "A SHA-256 checksum is required for $Path."
    }
    if ($ExpectedDigest -notmatch '^[0-9a-fA-F]{64}$') {
        throw "Invalid SHA-256 checksum for $Path."
    }

    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.IO.File]::ReadAllBytes($Path)
        $actual = [System.BitConverter]::ToString($sha256.ComputeHash($bytes)).Replace("-", "").ToLowerInvariant()
    } finally {
        $sha256.Dispose()
    }
    if ($actual -ne $ExpectedDigest.ToLowerInvariant()) {
        throw "Downloaded Herdr checksum did not match. Expected $ExpectedDigest but got $actual."
    }
}

function Test-RegularFile {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $false
    }
    $item = Get-Item -LiteralPath $Path -Force
    return -not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)
}

function Test-RegularDirectory {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        return $false
    }
    $item = Get-Item -LiteralPath $Path -Force
    return -not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)
}

function Get-ReleaseStorageKey {
    param([string]$VersionIdentity)

    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($VersionIdentity)
        $digest = [System.BitConverter]::ToString($sha256.ComputeHash($bytes)).Replace("-", "").ToLowerInvariant()
        return "r-" + $digest.Substring(0, 16)
    } finally {
        $sha256.Dispose()
    }
}

function Write-ReleaseIdentityMetadata {
    param(
        [string]$ReleaseDir,
        [string]$VersionIdentity,
        [string]$TargetTriple,
        [string]$Format,
        [string]$Sha256
    )

    @{ schema_version = 1; version_identity = $VersionIdentity; target_triple = $TargetTriple; format = $Format; sha256 = $Sha256 } |
        ConvertTo-Json -Compress |
        Set-Content -LiteralPath (Join-Path $ReleaseDir ".herdr-release.json") -Encoding utf8 -NoNewline
}

function Test-HerdrReleaseComplete {
    param(
        [string]$ReleaseDir,
        [string]$Format,
        [string]$VersionIdentity,
        [string]$TargetTriple,
        [string]$ExpectedSha256
    )

    if (-not (Test-RegularDirectory -Path $ReleaseDir)) {
        return $false
    }
    $identityPath = Join-Path $ReleaseDir ".herdr-release.json"
    if (-not (Test-RegularFile -Path $identityPath)) {
        return $false
    }
    try {
        $identity = ConvertTo-ManifestObject -Manifest (Get-Content -LiteralPath $identityPath -Raw)
        if (@($identity.PSObject.Properties).Count -ne 5 -or
            $null -eq $identity.PSObject.Properties["schema_version"] -or [int]$identity.schema_version -ne 1 -or
            $null -eq $identity.PSObject.Properties["version_identity"] -or [string]$identity.version_identity -cne $VersionIdentity -or
            $null -eq $identity.PSObject.Properties["target_triple"] -or [string]$identity.target_triple -cne $TargetTriple -or
            $null -eq $identity.PSObject.Properties["format"] -or [string]$identity.format -cne $Format -or
            $null -eq $identity.PSObject.Properties["sha256"] -or [string]$identity.sha256 -cne $ExpectedSha256) {
            return $false
        }
    } catch {
        return $false
    }
    $herdrExe = Join-Path $ReleaseDir "herdr.exe"
    if (-not (Test-RegularFile -Path $herdrExe)) {
        return $false
    }
    if ($Format -eq "exe") {
        return $true
    }

    $conptyRoot = Join-Path $ReleaseDir "conpty"
    if (-not (Test-RegularDirectory -Path $conptyRoot) -or
        -not (Test-RegularDirectory -Path (Join-Path $conptyRoot "x64")) -or
        -not (Test-RegularDirectory -Path (Join-Path $conptyRoot "arm64"))) {
        return $false
    }
    $markerPath = Join-Path $conptyRoot "herdr-conpty.json"
    $required = @(
        "conpty/conpty.dll",
        "conpty/x64/OpenConsole.exe",
        "conpty/arm64/OpenConsole.exe",
        "THIRD-PARTY-NOTICES/Microsoft.Windows.Console.ConPTY-LICENSE.txt",
        "THIRD-PARTY-NOTICES/Microsoft.Windows.Console.ConPTY-NOTICE.md"
    )
    foreach ($relative in $required) {
        if (-not (Test-RegularFile -Path (Join-Path $ReleaseDir ($relative -replace '/', '\')))) {
            return $false
        }
    }
    if (-not (Test-RegularFile -Path $markerPath)) {
        return $false
    }

    try {
        $marker = ConvertTo-ManifestObject -Manifest (Get-Content -LiteralPath $markerPath -Raw)
        $schemaProperty = $marker.PSObject.Properties["schema_version"]
        $packageProperty = $marker.PSObject.Properties["package"]
        $versionProperty = $marker.PSObject.Properties["version"]
        $architectureProperty = $marker.PSObject.Properties["architecture"]
        $filesProperty = $marker.PSObject.Properties["files"]
        if ($null -eq $schemaProperty -or [int]$schemaProperty.Value -ne 1 -or
            $null -eq $packageProperty -or [string]$packageProperty.Value -ne "Microsoft.Windows.Console.ConPTY" -or
            $null -eq $versionProperty -or [string]::IsNullOrWhiteSpace([string]$versionProperty.Value) -or
            $null -eq $architectureProperty -or [string]$architectureProperty.Value -ne "x86_64" -or
            $null -eq $filesProperty) {
            return $false
        }

        $expectedConptyFiles = @(
            "conpty/conpty.dll",
            "conpty/x64/OpenConsole.exe",
            "conpty/arm64/OpenConsole.exe"
        )
        $markerFileNames = @($filesProperty.Value.PSObject.Properties | ForEach-Object { $_.Name })
        if (@(Compare-Object $expectedConptyFiles $markerFileNames).Count -ne 0) {
            return $false
        }

        $bundleEntries = @(Get-ChildItem -LiteralPath $conptyRoot -Force -Recurse)
        if (@($bundleEntries | Where-Object {
            $_.Attributes -band [IO.FileAttributes]::ReparsePoint
        }).Count -ne 0) {
            return $false
        }
        $releaseRoot = [System.IO.Path]::GetFullPath($ReleaseDir).TrimEnd('\')
        $actualBundleFiles = @($bundleEntries | Where-Object { -not $_.PSIsContainer } | ForEach-Object {
            $_.FullName.Substring($releaseRoot.Length + 1).Replace('\', '/')
        })
        $expectedBundleFiles = @($expectedConptyFiles) + "conpty/herdr-conpty.json"
        if (@(Compare-Object $expectedBundleFiles $actualBundleFiles).Count -ne 0) {
            return $false
        }
        foreach ($relative in $expectedConptyFiles) {
            $digestProperty = $filesProperty.Value.PSObject.Properties[$relative]
            if ($null -eq $digestProperty) {
                return $false
            }
            Test-FileDigest -Path (Join-Path $ReleaseDir ($relative -replace '/', '\')) -ExpectedDigest ([string]$digestProperty.Value)
        }
    } catch {
        return $false
    }
    return $true
}

function Move-DirectoryWithRetry {
    param(
        [string]$Source,
        [string]$Destination,
        [int]$TimeoutMilliseconds = 5000
    )

    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMilliseconds)
    while ($true) {
        try {
            [System.IO.Directory]::Move($Source, $Destination)
            return
        } catch {
            $retryable = $false
            $exception = $_.Exception
            while ($null -ne $exception) {
                if ($exception -is [System.IO.IOException] -or
                    $exception -is [System.UnauthorizedAccessException]) {
                    $retryable = $true
                    break
                }
                $exception = $exception.InnerException
            }
            if (-not $retryable -or
                [DateTime]::UtcNow -ge $deadline -or
                -not (Test-Path -LiteralPath $Source -PathType Container) -or
                (Test-Path -LiteralPath $Destination)) {
                throw
            }
            Start-Sleep -Milliseconds 100
        }
    }
}

function Remove-DirectoryWithRetry {
    param(
        [string]$Path,
        [int]$TimeoutMilliseconds = 5000
    )

    $fullPath = [System.IO.Path]::GetFullPath($Path)
    $extendedPath = if ($fullPath.StartsWith("\\")) {
        "\\?\UNC\" + $fullPath.TrimStart([char]'\')
    } else {
        "\\?\" + $fullPath
    }
    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMilliseconds)
    while (Test-Path -LiteralPath $Path) {
        try {
            [System.IO.Directory]::Delete($extendedPath, $true)
            return
        } catch {
            if ([DateTime]::UtcNow -ge $deadline) {
                Write-WarningStep "Herdr installed successfully but could not remove a temporary release backup at $Path."
                return
            }
            Start-Sleep -Milliseconds 100
        }
    }
}

function Invoke-WithInstallLock {
    param(
        [string]$LockPath,
        [scriptblock]$Script
    )

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $LockPath) | Out-Null
    $lock = $null
    while ($null -eq $lock) {
        try {
            $lock = [System.IO.File]::Open(
                $LockPath,
                [System.IO.FileMode]::OpenOrCreate,
                [System.IO.FileAccess]::ReadWrite,
                [System.IO.FileShare]::None
            )
        } catch [System.IO.IOException] {
            Start-Sleep -Milliseconds 250
        }
    }

    try {
        & $Script
    } finally {
        $lock.Dispose()
    }
}

function Test-IsJunction {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) {
        return $false
    }

    $item = Get-Item -LiteralPath $Path -Force
    return ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -and $item.LinkType -eq "Junction"
}

function Set-ManagedJunction {
    param(
        [string]$LinkPath,
        [string]$TargetPath,
        [string]$ManagedTargetPrefix,
        [bool]$AllowLegacyHerdrBinMigration = $false
    )

    if (Test-Path -LiteralPath $LinkPath) {
        $item = Get-Item -LiteralPath $LinkPath -Force
        if (Test-IsJunction -Path $LinkPath) {
            $existingTarget = [string]$item.Target
            if (-not [string]::IsNullOrWhiteSpace($ManagedTargetPrefix)) {
                $ownedPrefix = $ManagedTargetPrefix.TrimEnd("\")
                if (-not $existingTarget.StartsWith($ownedPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
                    throw "Refusing to retarget junction at $LinkPath because it is not managed by this installer."
                }
            }
            if ($existingTarget.Equals($TargetPath, [System.StringComparison]::OrdinalIgnoreCase)) {
                return
            }
            Remove-Item -LiteralPath $LinkPath -Recurse -Force
        } elseif ($item.PSIsContainer) {
            if ((Get-ChildItem -LiteralPath $LinkPath -Force | Select-Object -First 1) -ne $null) {
                if (-not (Move-LegacyHerdrBinDirectory -Path $LinkPath -AllowMigration $AllowLegacyHerdrBinMigration)) {
                    throw "Refusing to replace non-empty directory at $LinkPath with a junction."
                }
            } else {
                Remove-Item -LiteralPath $LinkPath -Recurse -Force
            }
        } else {
            throw "Refusing to replace file at $LinkPath with a junction."
        }
    }

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $LinkPath) | Out-Null
    New-Item -ItemType Junction -Path $LinkPath -Target $TargetPath | Out-Null
}

function Move-LegacyHerdrBinDirectory {
    param(
        [string]$Path,
        [bool]$AllowMigration
    )

    if (-not $AllowMigration) {
        return $false
    }

    $entries = @(Get-ChildItem -LiteralPath $Path -Force)
    if (($entries | Where-Object { $_.PSIsContainer } | Select-Object -First 1) -ne $null) {
        return $false
    }

    if (($entries | Where-Object { $_.Name -ieq "herdr.exe" } | Select-Object -First 1) -eq $null) {
        return $false
    }

    $legacyPath = "$Path.legacy.$([System.Guid]::NewGuid().ToString("N"))"
    Move-Item -LiteralPath $Path -Destination $legacyPath
    Write-Step "Moved legacy Herdr bin directory to $legacyPath."
    return $true
}

function Remove-StaleInstallArtifacts {
    param([string]$ReleasesDir)

    if (-not (Test-Path -LiteralPath $ReleasesDir -PathType Container)) {
        return
    }

    Get-ChildItem -LiteralPath $ReleasesDir -Force -Directory -Filter ".staging.*" -ErrorAction SilentlyContinue |
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
}

function Remove-OldReleases {
    param(
        [string]$ReleasesDir,
        [string]$CurrentReleaseDir,
        [int]$Keep
    )

    if ($Keep -lt 1 -or -not (Test-Path -LiteralPath $ReleasesDir -PathType Container)) {
        return
    }

    $currentFullPath = [System.IO.Path]::GetFullPath($CurrentReleaseDir)
    $releaseDirs = Get-ChildItem -LiteralPath $ReleasesDir -Force -Directory -ErrorAction SilentlyContinue |
        Where-Object { -not $_.Name.StartsWith(".staging.") -and -not $_.Name.StartsWith(".backup.") } |
        Sort-Object LastWriteTimeUtc -Descending
    $kept = 0
    foreach ($dir in $releaseDirs) {
        $dirFullPath = [System.IO.Path]::GetFullPath($dir.FullName)
        if ($dirFullPath.Equals($currentFullPath, [System.StringComparison]::OrdinalIgnoreCase)) {
            $kept += 1
            continue
        }
        if ($kept -lt $Keep) {
            $kept += 1
            continue
        }
        Remove-Item -LiteralPath $dir.FullName -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Resolve-HerdrVersion {
    param(
        [object]$Manifest,
        [string]$SelectedChannel
    )

    if ($SelectedChannel -eq "preview") {
        if ([string]::IsNullOrWhiteSpace([string]$Manifest.base_version) -or [string]::IsNullOrWhiteSpace([string]$Manifest.build_id)) {
            throw "Preview manifest is missing base_version or build_id."
        }
        $canonicalProperty = $Manifest.PSObject.Properties["canonical_build_id"]
        $buildIdentity = if (
            $null -ne $canonicalProperty -and
            -not [string]::IsNullOrWhiteSpace([string]$canonicalProperty.Value)
        ) {
            [string]$canonicalProperty.Value
        } else {
            [string]$Manifest.build_id
        }
        return "$($Manifest.base_version)-preview.$buildIdentity"
    }

    if ([string]::IsNullOrWhiteSpace([string]$Manifest.version)) {
        throw "Stable manifest is missing version."
    }
    return [string]$Manifest.version
}

if ($env:OS -ne "Windows_NT") {
    Write-Error "install.ps1 supports Windows only. Use install.sh on Linux or macOS."
    exit 1
}

if (-not [Environment]::Is64BitOperatingSystem) {
    Write-Error "Herdr requires 64-bit Windows."
    exit 1
}

$architecture = [System.Runtime.InteropServices.RuntimeInformation,mscorlib]::OSArchitecture.ToString()
switch ($architecture) {
    "X64" {
        $target = "windows-x86_64"
        $targetTriple = "x86_64-pc-windows-msvc"
    }
    "Arm64" {
        $target = "windows-x86_64"
        $targetTriple = "x86_64-pc-windows-msvc"
        Write-Step "Windows ARM64 detected; installing the x86_64 build under Windows emulation."
    }
    default {
        Write-Error "Unsupported Windows architecture: $architecture"
        exit 1
    }
}

$herdrHome = if ([string]::IsNullOrWhiteSpace($env:HERDR_HOME)) {
    Join-Path $env:USERPROFILE ".herdr"
} else {
    $env:HERDR_HOME
}
$herdrHome = [System.IO.Path]::GetFullPath($herdrHome)
$standaloneRoot = Join-Path $herdrHome "packages\standalone"
$releasesDir = Join-Path $standaloneRoot "releases"
$currentDir = Join-Path $standaloneRoot "current"
$lockPath = Join-Path $standaloneRoot "install.lock"

$defaultVisibleBinDir = Join-Path $env:LOCALAPPDATA "Programs\Herdr\bin"
$visibleBinDir = if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $defaultVisibleBinDir
} else {
    $InstallDir
}
$allowLegacyVisibleBinMigration = $false
try {
    $allowLegacyVisibleBinMigration = [System.IO.Path]::GetFullPath($visibleBinDir).TrimEnd("\").Equals(
        [System.IO.Path]::GetFullPath($defaultVisibleBinDir).TrimEnd("\"),
        [System.StringComparison]::OrdinalIgnoreCase
    )
} catch {
    $allowLegacyVisibleBinMigration = $false
}

$existingHerdr = Get-HerdrCommandSource
if (-not [string]::IsNullOrWhiteSpace($existingHerdr) -and -not (Test-PathStartsWith -Path $existingHerdr -Prefix $visibleBinDir)) {
    Write-Step "Detected existing Herdr command at $existingHerdr"
    Write-WarningStep "PATH order decides which Herdr runs. This installer will put $visibleBinDir first for future and current PowerShell sessions."
}

if ($useLocalPackage) {
    $versionIdentity = $LocalPackageIdentity
    $asset = [PSCustomObject]@{
        Sha256 = $LocalPackageSha256
        Format = $LocalPackageFormat
    }
} else {
    if (-not $channelWasExplicit) {
        if (-not [string]::IsNullOrWhiteSpace($existingHerdr)) {
            $detectedChannel = [string](& $existingHerdr channel show 2>$null | Select-Object -Last 1)
            $detectedChannel = $detectedChannel.Trim()
            if ($LASTEXITCODE -ne 0 -or $detectedChannel -notin @("stable", "preview")) {
                throw "Could not determine the existing Herdr update channel. Rerun with -Channel stable or -Channel preview."
            }
            $Channel = $detectedChannel
            Write-Step "Preserving existing Herdr $Channel channel"
        } elseif (-not [string]::IsNullOrWhiteSpace($ManifestUrl) -and $ManifestUrl -match "/(?:paired-)?preview\.json$") {
            $Channel = "preview"
        } else {
            $Channel = "stable"
        }
    }

    if ([string]::IsNullOrWhiteSpace($ManifestUrl)) {
        $ManifestUrl = if ($Channel -eq "preview") {
            "https://herdr.dev/preview.json"
        } else {
            "https://herdr.dev/latest.json"
        }
    }

    Write-Step "Fetching Herdr $Channel manifest"
    $manifest = Get-RemoteManifest -Uri $ManifestUrl
    $manifestChannelProperty = $manifest.PSObject.Properties["channel"]
    if (-not $channelWasExplicit -and $null -ne $manifestChannelProperty -and [string]$manifestChannelProperty.Value -eq "preview") {
        $Channel = "preview"
    }
    $isPhaseABridge = Test-PhaseABridgeManifestCandidate -Manifest $manifest
    if (-not $isPhaseABridge) {
        $assetsProperty = $manifest.PSObject.Properties["assets"]
        $assetProperty = if ($null -eq $assetsProperty) {
            $null
        } else {
            $assetsProperty.Value.PSObject.Properties[$target]
        }
        if ($null -eq $assetProperty -and
            -not $channelWasExplicit -and
            $Channel -eq "stable" -and
            $ManifestUrl -match "/latest\.json$") {
            Write-WarningStep "The stable manifest does not include Windows yet; using preview during the stable-channel rollout."
            $Channel = "preview"
            $ManifestUrl = $ManifestUrl.Substring(0, $ManifestUrl.Length - "latest.json".Length) + "preview.json"
            Write-Step "Fetching Herdr preview manifest"
            $manifest = Get-RemoteManifest -Uri $ManifestUrl
            $isPhaseABridge = Test-PhaseABridgeManifestCandidate -Manifest $manifest
        }
    }
    if ($Channel -eq "preview" -or $isPhaseABridge) {
        if ($Channel -ne "preview") {
            throw "Preview Phase A bridge requires the preview channel."
        }
        $previewSelection = Resolve-PreviewManifest -Manifest $manifest -Target $target -ManifestUrl $ManifestUrl
        $asset = $previewSelection.Asset
        $versionIdentity = $previewSelection.VersionIdentity
        $acceptedBuildIds = @($previewSelection.AcceptedBuildIds)
    } else {
        $asset = Get-ManifestAsset -Manifest $manifest -Target $target
        $versionIdentity = Resolve-HerdrVersion -Manifest $manifest -SelectedChannel $Channel
        $acceptedBuildIds = @()
    }
    if (-not [string]::IsNullOrWhiteSpace($ExpectedBuildId) -and $acceptedBuildIds -cnotcontains $ExpectedBuildId) {
        throw "Preview manifest changed while updating. Expected build $ExpectedBuildId but found $($acceptedBuildIds -join ' or '). Run herdr update again."
    }
}
$releaseName = "$(Get-ReleaseStorageKey -VersionIdentity $versionIdentity)-$targetTriple"
$releaseDir = Join-Path $releasesDir $releaseName

Write-Step "Installing Herdr $versionIdentity for $targetTriple"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("herdr-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

try {
    Invoke-WithInstallLock -LockPath $lockPath -Script {
        Remove-StaleInstallArtifacts -ReleasesDir $releasesDir

        if (-not (Test-HerdrReleaseComplete -ReleaseDir $releaseDir -Format $asset.Format -VersionIdentity $versionIdentity -TargetTriple $targetTriple -ExpectedSha256 $asset.Sha256)) {
            $downloadPath = if ($useLocalPackage) {
                $LocalPackagePath
            } else {
                Join-Path $tempDir "herdr-download.$($asset.Format)"
            }
            $stagingDir = Join-Path $releasesDir ".staging.$releaseName.$PID"
            if (-not $useLocalPackage) {
                Write-Step "Downloading Herdr"
                Invoke-CurlDownload -Uri $asset.Url -Destination $downloadPath
            }
            Test-FileDigest -Path $downloadPath -ExpectedDigest $asset.Sha256

            if ($asset.Format -eq "zip") {
                Expand-Archive -LiteralPath $downloadPath -DestinationPath $stagingDir
            } else {
                New-Item -ItemType Directory -Force -Path $stagingDir | Out-Null
                Copy-Item -LiteralPath $downloadPath -Destination (Join-Path $stagingDir "herdr.exe")
            }
            Write-ReleaseIdentityMetadata -ReleaseDir $stagingDir -VersionIdentity $versionIdentity -TargetTriple $targetTriple -Format $asset.Format -Sha256 $asset.Sha256
            if (-not (Test-HerdrReleaseComplete -ReleaseDir $stagingDir -Format $asset.Format -VersionIdentity $versionIdentity -TargetTriple $targetTriple -ExpectedSha256 $asset.Sha256)) {
                throw "Downloaded Herdr package is incomplete or failed ConPTY verification."
            }
            $stagedHerdr = Join-Path $stagingDir "herdr.exe"
            & $stagedHerdr --version *> $null
            if ($LASTEXITCODE -ne 0) {
                throw "Downloaded Herdr command failed verification: $stagedHerdr --version"
            }
            $backupDir = $null
            if (Test-Path -LiteralPath $releaseDir) {
                $backupDir = Join-Path $releasesDir ".backup.$releaseName.$([System.Guid]::NewGuid().ToString('N'))"
                [System.IO.Directory]::Move($releaseDir, $backupDir)
            }
            try {
                Move-DirectoryWithRetry -Source $stagingDir -Destination $releaseDir
            } catch {
                if ($null -ne $backupDir -and -not (Test-Path -LiteralPath $releaseDir)) {
                    [System.IO.Directory]::Move($backupDir, $releaseDir)
                }
                Write-WarningStep "Windows could not activate the downloaded release. Another process may have a package file open, such as antivirus or indexing. No incomplete release was activated. Run herdr update again."
                throw
            }
        }

        $releaseHerdr = Join-Path $releaseDir "herdr.exe"
        & $releaseHerdr --version *> $null
        if ($LASTEXITCODE -ne 0) {
            throw "Installed Herdr command failed verification: $releaseHerdr --version"
        }
        Get-ChildItem -LiteralPath $releasesDir -Force -Directory -Filter ".backup.$releaseName.*" -ErrorAction SilentlyContinue |
            ForEach-Object { Remove-DirectoryWithRetry -Path $_.FullName }

        Set-ManagedJunction -LinkPath $currentDir -TargetPath $releaseDir -ManagedTargetPrefix $releasesDir
        Set-ManagedJunction -LinkPath $visibleBinDir -TargetPath $releaseDir -ManagedTargetPrefix $standaloneRoot -AllowLegacyHerdrBinMigration $allowLegacyVisibleBinMigration

        Remove-OldReleases -ReleasesDir $releasesDir -CurrentReleaseDir $releaseDir -Keep $Retain
    }
} finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}

$userEnvironmentKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Environment")
if ($null -eq $userEnvironmentKey) {
    throw "Unable to open the current user's environment registry key."
}
try {
    $userPathChanged = Update-PathRegistryEntry -EnvironmentKey $userEnvironmentKey -Entry $visibleBinDir
} finally {
    $userEnvironmentKey.Dispose()
}
if ($userPathChanged) {
    Publish-EnvironmentChange
    Write-Step "PATH updated for future PowerShell sessions."
} else {
    Write-Step "$visibleBinDir is already first on PATH."
}

$newProcessPath = Prepend-PathEntry -PathValue $env:Path -Entry $visibleBinDir
if ($newProcessPath -cne $env:Path) {
    $env:Path = $newProcessPath
}

$resolvedHerdr = Get-HerdrCommandSource
if (-not (Test-PathStartsWith -Path $resolvedHerdr -Prefix $visibleBinDir)) {
    Write-WarningStep "PowerShell still resolves herdr to $resolvedHerdr. Open a new PowerShell window or inspect PATH order manually."
}

Write-Step "Current PowerShell session: herdr"
Write-Step "Future PowerShell windows: open a new PowerShell window and run: herdr"
Write-Host "Herdr $versionIdentity installed successfully."
