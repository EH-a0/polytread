use std::fs;
#[cfg(target_os = "windows")]
use std::net::IpAddr;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
#[cfg(target_os = "windows")]
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
#[cfg(target_os = "macos")]
use uuid::Uuid;

#[cfg(any(target_os = "linux", target_os = "macos"))]
const PRIMARY_DNS: &str = "1.1.1.1";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const SECONDARY_DNS: &str = "1.0.0.1";
#[cfg(any(target_os = "windows", target_os = "macos"))]
const CLOUDFLARE_DOH_URL: &str = "https://cloudflare-dns.com/dns-query";
#[cfg(target_os = "linux")]
const CLOUDFLARE_DOT_NAME: &str = "cloudflare-dns.com";

#[derive(Debug, Clone)]
pub struct RemediationOutcome {
    pub user_step: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BackupFile {
    active: bool,
    change: DnsChange,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
enum DnsChange {
    Windows {
        interface_index: u32,
        automatic: bool,
        server_addresses: Vec<String>,
        doh: Vec<WindowsDohBackup>,
    },
    Linux {
        interface_name: String,
    },
    Macos {
        profile_identifier: String,
        profile_path: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowsDohBackup {
    server_address: String,
    #[serde(default = "default_true")]
    existed: bool,
    #[serde(default)]
    doh_template: String,
    auto_upgrade: bool,
    allow_fallback_to_udp: bool,
}

fn default_true() -> bool {
    true
}

#[cfg(target_os = "windows")]
#[derive(Debug, Deserialize)]
struct WindowsCapture {
    interface_index: u32,
    automatic: bool,
    #[serde(default)]
    server_addresses: Vec<String>,
    #[serde(default)]
    doh: Vec<WindowsDohBackup>,
}

pub fn remediation_label() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Windows encrypted DNS on the active network adapter"
    }
    #[cfg(target_os = "linux")]
    {
        "encrypted DNS on the active systemd-resolved network link"
    }
    #[cfg(target_os = "macos")]
    {
        "an Apple-approved encrypted DNS profile"
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        "encrypted DNS"
    }
}

pub async fn apply(backup_path: &Path) -> Result<RemediationOutcome> {
    if let Some(existing) = read_backup(backup_path)?
        && existing.active
    {
        bail!(
            "an earlier PolyTread DNS change still has an active rollback record; run `polytread restore-dns` first"
        );
    }

    let change = capture_change(backup_path).await?;
    write_backup(
        backup_path,
        &BackupFile {
            active: true,
            change,
        },
    )?;
    let backup = read_backup(backup_path)?
        .ok_or_else(|| anyhow!("DNS rollback record disappeared before the change"))?;
    match apply_change(&backup.change).await {
        Ok(outcome) => Ok(outcome),
        Err(apply_error) => {
            #[cfg(target_os = "macos")]
            {
                let mut backup = backup;
                backup.active = false;
                write_backup(backup_path, &backup)?;
                Err(apply_error)
            }
            #[cfg(not(target_os = "macos"))]
            {
                match restore_change(&backup.change).await {
                    Ok(outcome) if outcome.user_step.is_none() => {
                        let mut backup = backup;
                        backup.active = false;
                        write_backup(backup_path, &backup)?;
                        Err(apply_error).context(
                            "the DNS change failed and the original configuration was restored",
                        )
                    }
                    Ok(_) => Err(apply_error).context(
                        "the DNS change failed; run `polytread restore-dns` to finish the rollback",
                    ),
                    Err(rollback_error) => bail!(
                        "the DNS change failed: {apply_error}; automatic rollback also failed: {rollback_error}; run `polytread restore-dns`"
                    ),
                }
            }
        }
    }
}

pub async fn restore(backup_path: &Path) -> Result<RemediationOutcome> {
    let mut backup = read_backup(backup_path)?
        .ok_or_else(|| anyhow!("no PolyTread DNS rollback record exists"))?;
    if !backup.active {
        return Ok(RemediationOutcome { user_step: None });
    }
    let outcome = restore_change(&backup.change).await?;
    if outcome.user_step.is_none() {
        backup.active = false;
        write_backup(backup_path, &backup)?;
    }
    Ok(outcome)
}

pub fn mark_restored_after_user_step(backup_path: &Path) -> Result<()> {
    let mut backup = read_backup(backup_path)?
        .ok_or_else(|| anyhow!("no PolyTread DNS rollback record exists"))?;
    backup.active = false;
    write_backup(backup_path, &backup)
}

async fn capture_change(_backup_path: &Path) -> Result<DnsChange> {
    #[cfg(target_os = "windows")]
    {
        return capture_windows().await;
    }
    #[cfg(target_os = "linux")]
    {
        return capture_linux().await;
    }
    #[cfg(target_os = "macos")]
    {
        return capture_macos(_backup_path);
    }
    #[allow(unreachable_code)]
    {
        let _ = _backup_path;
        bail!("automatic DNS remediation is not supported on this operating system")
    }
}

async fn apply_change(change: &DnsChange) -> Result<RemediationOutcome> {
    match change {
        #[cfg(target_os = "windows")]
        DnsChange::Windows {
            interface_index, ..
        } => apply_windows(*interface_index).await,
        #[cfg(target_os = "linux")]
        DnsChange::Linux { interface_name } => apply_linux(interface_name).await,
        #[cfg(target_os = "macos")]
        DnsChange::Macos { profile_path, .. } => apply_macos(profile_path).await,
        _ => bail!("the DNS rollback record belongs to a different operating system"),
    }
}

async fn restore_change(change: &DnsChange) -> Result<RemediationOutcome> {
    match change {
        #[cfg(target_os = "windows")]
        DnsChange::Windows {
            interface_index,
            automatic,
            server_addresses,
            doh,
        } => restore_windows(*interface_index, *automatic, server_addresses, doh).await,
        #[cfg(target_os = "linux")]
        DnsChange::Linux { interface_name } => restore_linux(interface_name).await,
        #[cfg(target_os = "macos")]
        DnsChange::Macos {
            profile_identifier, ..
        } => restore_macos(profile_identifier).await,
        _ => bail!("the DNS rollback record belongs to a different operating system"),
    }
}

fn read_backup(path: &Path) -> Result<Option<BackupFile>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("the local DNS rollback record is invalid")
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed reading the local DNS rollback record"),
    }
}

fn write_backup(path: &Path, backup: &BackupFile) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("DNS rollback path has no parent directory"))?;
    fs::create_dir_all(parent).context("failed creating the local configuration directory")?;
    let mut bytes = serde_json::to_vec_pretty(backup).context("failed serializing DNS rollback")?;
    bytes.push(b'\n');
    fs::write(path, bytes).context("failed writing the local DNS rollback record")
}

#[cfg(target_os = "windows")]
async fn capture_windows() -> Result<DnsChange> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$route = Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' |
  Sort-Object RouteMetric, InterfaceMetric |
  Select-Object -First 1
if ($null -eq $route) { throw 'No active IPv4 default route was found.' }
$index = [int]$route.InterfaceIndex
$adapter = Get-NetAdapter -InterfaceIndex $index
$registryPath = "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\{$($adapter.InterfaceGuid)}"
$nameServer = (Get-ItemProperty -LiteralPath $registryPath -Name NameServer -ErrorAction SilentlyContinue).NameServer
$servers = @((Get-DnsClientServerAddress -InterfaceIndex $index -AddressFamily IPv4).ServerAddresses)
$knownDoh = @(Get-DnsClientDohServerAddress -ErrorAction SilentlyContinue)
$doh = @('1.1.1.1','1.0.0.1' | ForEach-Object {
  $server = $_
  $item = $knownDoh | Where-Object ServerAddress -eq $server | Select-Object -First 1
  if ($null -eq $item) {
    [pscustomobject]@{
      server_address = $server
      existed = $false
      doh_template = ''
      auto_upgrade = $false
      allow_fallback_to_udp = $false
    }
  } else {
    [pscustomobject]@{
      server_address = $item.ServerAddress
      existed = $true
      doh_template = [string]$item.DohTemplate
      auto_upgrade = [bool]$item.AutoUpgrade
      allow_fallback_to_udp = [bool]$item.AllowFallbackToUdp
    }
  }
})
[pscustomobject]@{
  interface_index = $index
  automatic = [string]::IsNullOrWhiteSpace([string]$nameServer)
  server_addresses = $servers
  doh = $doh
} | ConvertTo-Json -Compress -Depth 4
"#;
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .output()
        .await
        .context("failed starting Windows DNS inspection")?;
    if !output.status.success() {
        bail!("Windows could not inspect the active DNS configuration");
    }
    let capture: WindowsCapture = serde_json::from_slice(&output.stdout)
        .context("Windows returned an invalid DNS configuration")?;
    for address in &capture.server_addresses {
        address
            .parse::<IpAddr>()
            .with_context(|| format!("Windows returned an invalid DNS server address {address}"))?;
    }
    Ok(DnsChange::Windows {
        interface_index: capture.interface_index,
        automatic: capture.automatic,
        server_addresses: capture.server_addresses,
        doh: capture.doh,
    })
}

#[cfg(target_os = "windows")]
async fn apply_windows(interface_index: u32) -> Result<RemediationOutcome> {
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
foreach ($server in @('1.1.1.1','1.0.0.1')) {{
  $existing = Get-DnsClientDohServerAddress -ServerAddress $server -ErrorAction SilentlyContinue
  if ($null -eq $existing) {{
    Add-DnsClientDohServerAddress -ServerAddress $server -DohTemplate 'https://cloudflare-dns.com/dns-query' -AllowFallbackToUdp $false -AutoUpgrade $true
  }} else {{
    Set-DnsClientDohServerAddress -ServerAddress $server -DohTemplate 'https://cloudflare-dns.com/dns-query' -AllowFallbackToUdp $false -AutoUpgrade $true
  }}
}}
Set-DnsClientServerAddress -InterfaceIndex {interface_index} -ServerAddresses @('1.1.1.1','1.0.0.1') -Validate
Clear-DnsClientCache
"#
    );
    run_elevated_powershell(&script).await?;
    Ok(RemediationOutcome { user_step: None })
}

#[cfg(target_os = "windows")]
async fn restore_windows(
    interface_index: u32,
    automatic: bool,
    server_addresses: &[String],
    doh: &[WindowsDohBackup],
) -> Result<RemediationOutcome> {
    let dns_restore = if automatic {
        format!(
            "Set-DnsClientServerAddress -InterfaceIndex {interface_index} -ResetServerAddresses"
        )
    } else {
        if server_addresses.is_empty() {
            bail!("the DNS rollback record has no original server addresses");
        }
        let addresses = server_addresses
            .iter()
            .map(|address| format!("'{address}'"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "Set-DnsClientServerAddress -InterfaceIndex {interface_index} -ServerAddresses @({addresses})"
        )
    };
    let mut script = format!("$ErrorActionPreference = 'Stop'\n{dns_restore}\n");
    for item in doh {
        let server = item
            .server_address
            .parse::<IpAddr>()
            .context("the DNS rollback record contains an invalid address")?;
        if !item.existed {
            script.push_str(&format!(
                "Remove-DnsClientDohServerAddress -ServerAddress '{server}' -Confirm:$false -ErrorAction SilentlyContinue\n"
            ));
            continue;
        }
        let template = if item.doh_template.trim().is_empty() {
            CLOUDFLARE_DOH_URL
        } else {
            item.doh_template.as_str()
        };
        let template = powershell_single_quoted(template);
        let auto_upgrade = powershell_bool(item.auto_upgrade);
        let fallback = powershell_bool(item.allow_fallback_to_udp);
        script.push_str(&format!(
            "Set-DnsClientDohServerAddress -ServerAddress '{server}' -DohTemplate '{template}' -AllowFallbackToUdp {fallback} -AutoUpgrade {auto_upgrade}\n"
        ));
    }
    script.push_str("Clear-DnsClientCache\n");
    run_elevated_powershell(&script).await?;
    Ok(RemediationOutcome { user_step: None })
}

#[cfg(target_os = "windows")]
fn powershell_bool(value: bool) -> &'static str {
    if value { "$true" } else { "$false" }
}

#[cfg(target_os = "windows")]
fn powershell_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(target_os = "windows")]
async fn run_elevated_powershell(script: &str) -> Result<()> {
    let bytes = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let launcher = format!(
        "$process = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile','-NonInteractive','-EncodedCommand','{encoded}') -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $process.ExitCode"
    );
    let status = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &launcher])
        .status()
        .await
        .context("failed opening the Windows administrator approval prompt")?;
    if !status.success() {
        bail!("the Windows encrypted DNS change was cancelled or rejected");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn capture_linux() -> Result<DnsChange> {
    require_program("resolvectl").await?;
    let output = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .await
        .context("failed inspecting the Linux default route")?;
    if !output.status.success() {
        bail!("Linux could not inspect the default route");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let tokens = text.split_whitespace().collect::<Vec<_>>();
    let interface_name = tokens
        .windows(2)
        .find_map(|pair| (pair[0] == "dev").then(|| pair[1].to_string()))
        .ok_or_else(|| anyhow!("Linux did not report an active default-route interface"))?;
    Ok(DnsChange::Linux { interface_name })
}

#[cfg(target_os = "linux")]
async fn apply_linux(interface_name: &str) -> Result<RemediationOutcome> {
    let primary = format!("{PRIMARY_DNS}#{CLOUDFLARE_DOT_NAME}");
    let secondary = format!("{SECONDARY_DNS}#{CLOUDFLARE_DOT_NAME}");
    run_privileged("resolvectl", &["dns", interface_name, &primary, &secondary]).await?;
    run_privileged("resolvectl", &["dnsovertls", interface_name, "yes"]).await?;
    let _ = Command::new("resolvectl")
        .arg("flush-caches")
        .status()
        .await;
    Ok(RemediationOutcome { user_step: None })
}

#[cfg(target_os = "linux")]
async fn restore_linux(interface_name: &str) -> Result<RemediationOutcome> {
    run_privileged("resolvectl", &["revert", interface_name]).await?;
    let _ = Command::new("resolvectl")
        .arg("flush-caches")
        .status()
        .await;
    Ok(RemediationOutcome { user_step: None })
}

#[cfg(target_os = "linux")]
async fn require_program(program: &str) -> Result<()> {
    match Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) if status.success() => Ok(()),
        _ => bail!("{program} is required for automatic encrypted DNS on this Linux system"),
    }
}

#[cfg(target_os = "linux")]
async fn run_privileged(program: &str, args: &[&str]) -> Result<()> {
    if Command::new(program)
        .args(args)
        .status()
        .await
        .is_ok_and(|status| status.success())
    {
        return Ok(());
    }
    for helper in ["pkexec", "sudo"] {
        if Command::new(helper)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success())
        {
            let status = Command::new(helper)
                .arg(program)
                .args(args)
                .status()
                .await
                .with_context(|| format!("failed starting {helper}"))?;
            if status.success() {
                return Ok(());
            }
        }
    }
    bail!("administrator approval for the Linux DNS change was not granted")
}

#[cfg(target_os = "macos")]
fn capture_macos(backup_path: &Path) -> Result<DnsChange> {
    let parent = backup_path
        .parent()
        .ok_or_else(|| anyhow!("DNS rollback path has no parent directory"))?;
    let profile_identifier = format!("xyz.polytread.encrypted-dns.{}", Uuid::new_v4().simple());
    let profile_path = parent.join("polytread-encrypted-dns.mobileconfig");
    write_macos_profile(&profile_path, &profile_identifier)?;
    Ok(DnsChange::Macos {
        profile_identifier,
        profile_path: profile_path.to_string_lossy().into_owned(),
    })
}

#[cfg(target_os = "macos")]
fn write_macos_profile(path: &Path, identifier: &str) -> Result<()> {
    let payload_uuid = Uuid::new_v4().hyphenated().to_string().to_uppercase();
    let settings_uuid = Uuid::new_v4().hyphenated().to_string().to_uppercase();
    let profile = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>PayloadContent</key><array><dict>
<key>DNSSettings</key><dict>
<key>DNSProtocol</key><string>HTTPS</string>
<key>ServerAddresses</key><array><string>{PRIMARY_DNS}</string><string>{SECONDARY_DNS}</string></array>
<key>ServerURL</key><string>{CLOUDFLARE_DOH_URL}</string>
<key>AllowFailover</key><false/>
</dict>
<key>PayloadDisplayName</key><string>PolyTread Encrypted DNS</string>
<key>PayloadIdentifier</key><string>{identifier}.settings</string>
<key>PayloadType</key><string>com.apple.dnsSettings.managed</string>
<key>PayloadUUID</key><string>{settings_uuid}</string>
<key>PayloadVersion</key><integer>1</integer>
</dict></array>
<key>PayloadDisplayName</key><string>PolyTread Encrypted DNS</string>
<key>PayloadIdentifier</key><string>{identifier}</string>
<key>PayloadRemovalDisallowed</key><false/>
<key>PayloadType</key><string>Configuration</string>
<key>PayloadUUID</key><string>{payload_uuid}</string>
<key>PayloadVersion</key><integer>1</integer>
</dict></plist>
"#
    );
    fs::write(path, profile).context("failed writing the macOS encrypted DNS profile")
}

#[cfg(target_os = "macos")]
async fn apply_macos(profile_path: &str) -> Result<RemediationOutcome> {
    let status = Command::new("open")
        .arg(profile_path)
        .status()
        .await
        .context("failed opening the macOS encrypted DNS profile")?;
    if !status.success() {
        bail!("macOS refused to open the encrypted DNS profile");
    }
    Ok(RemediationOutcome {
        user_step: Some(
            "Approve the PolyTread Encrypted DNS profile in System Settings, then return here."
                .to_string(),
        ),
    })
}

#[cfg(target_os = "macos")]
async fn restore_macos(_profile_identifier: &str) -> Result<RemediationOutcome> {
    let status = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.Profiles-Settings.extension")
        .status()
        .await
        .context("failed opening macOS profile settings")?;
    if !status.success() {
        bail!("macOS refused to open profile settings");
    }
    Ok(RemediationOutcome {
        user_step: Some(
            "Remove the PolyTread Encrypted DNS profile in System Settings, then return here."
                .to_string(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_backup_round_trips_without_secrets() {
        let backup = BackupFile {
            active: false,
            change: DnsChange::Linux {
                interface_name: "eth0".to_string(),
            },
        };
        let json = serde_json::to_string(&backup).expect("serialize");
        let decoded: BackupFile = serde_json::from_str(&json).expect("deserialize");
        assert!(!decoded.active);
        assert!(!json.contains("private_key"));
    }
}
