//! Secure Boot / UEFI CA preflight for target bootloaders.
//!
//! Firmware launches an EFI loader only when its signature is accepted by the
//! UEFI `db` variable (or by a local hash/MOK policy that this module cannot
//! identify).  A migration must therefore compare the target loader's
//! Authenticode issuer with the Microsoft UEFI CA generations present in `db`
//! before it writes the loader or changes the boot order.
//!
//! The EFI signature-list and X.509 subject parsing is pure.  Reading efivars,
//! extracting Authenticode PKCS#7 blobs, and asking `openssl` to print their
//! certificate issuers are kept at the I/O boundary so the policy remains
//! table-testable without a UEFI host.

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::Command;
use tempfile::NamedTempFile;

/// Linux's EFI variable namespace GUID.
const EFI_GLOBAL_VARIABLE_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const EFI_CERT_X509_GUID: [u8; 16] = [
    0xa1, 0x59, 0xc0, 0xa5, 0xe4, 0x94, 0xa7, 0x4a, 0x87, 0xb5, 0xab, 0x15, 0x5c, 0x2b, 0xf0, 0x72,
];
const COMMON_NAME_OID: [u8; 3] = [0x55, 0x04, 0x03];
const EFIVAR_ATTRIBUTES_LEN: usize = 4;

/// The Microsoft third-party UEFI CA generations understood by the target
/// images and the firmware db preflight.
const MICROSOFT_CA_GENERATIONS: &[(&str, &str)] = &[
    ("Microsoft Corporation UEFI CA 2011", "2011"),
    ("Microsoft UEFI CA 2023", "2023"),
    ("Microsoft Corporation UEFI CA 2023", "2023"),
];

/// Which target loader is about to become firmware-reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetBootloaderKind {
    SystemdBoot,
    Shim,
}

impl TargetBootloaderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SystemdBoot => "systemd-boot",
            Self::Shim => "shim",
        }
    }
}

/// A target EFI loader discovered during the registry scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TargetBootloader {
    pub kind: TargetBootloaderKind,
    /// Archive-relative path in the target image, not the temporary scan path.
    pub path: String,
    /// Whether the PE image carried an Authenticode certificate table.
    pub signature_present: bool,
    /// Recognized Microsoft UEFI CA generations found in its certificate chain.
    pub authorities: Vec<String>,
    /// Why the loader could not be inspected, when the registry scan still
    /// found the file but the local inspection tool was unavailable or failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspection_error: Option<String>,
}

/// Facts read from the host firmware before any migration mutation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SecureBootFacts {
    /// `Some(true)` means Secure Boot is enabled; `None` means the efivar was
    /// unavailable or malformed, so no signing decision is made.
    pub enabled: Option<bool>,
    /// True only when `db` contained at least one structurally valid X.509
    /// certificate. An unreadable/empty db is intentionally distinct: it warns
    /// rather than pretending to prove that no trust path exists.
    pub db_read: bool,
    /// Recognized Microsoft third-party UEFI CA generations in `db`.
    pub trusted_authorities: Vec<String>,
    /// Number of X.509 entries parsed from `db`, including Windows and OEM CAs.
    pub db_x509_certificates: usize,
}

impl SecureBootFacts {
    /// Concise human-readable state for the preflight report.
    pub fn summary(&self) -> String {
        match self.enabled {
            Some(false) => "off".to_string(),
            Some(true) if self.db_read && !self.trusted_authorities.is_empty() => {
                format!(
                    "on (Microsoft UEFI CA {})",
                    human_authorities(&self.trusted_authorities)
                )
            }
            Some(true) if self.db_read => "on (no recognized Microsoft third-party CA)".to_string(),
            Some(true) => "on (UEFI db unreadable)".to_string(),
            None => "unknown".to_string(),
        }
    }
}

/// Result of parsing the signature lists in a UEFI `db` variable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UefiDbSummary {
    pub authorities: Vec<String>,
    pub x509_certificates: usize,
}

/// Parse the value byte in a Linux `SecureBoot-*` efivar.
///
/// The first four bytes are efivarfs attributes; the variable's value is one
/// byte and must be exactly 0 or 1.
pub fn parse_secure_boot_variable(raw: &[u8]) -> Option<bool> {
    match raw.get(EFIVAR_ATTRIBUTES_LEN) {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

/// Read Secure Boot state and the firmware `db` from an efivarfs directory.
///
/// This function never fails: firmware variables are optional on BIOS hosts,
/// and an inaccessible variable is an unknown fact rather than a reason to
/// abort a migration before the target image has even been inspected.
pub fn read_from_efivars(efivars: &Path) -> SecureBootFacts {
    let secure_boot = efivars.join(format!("SecureBoot-{EFI_GLOBAL_VARIABLE_GUID}"));
    let db = efivars.join(format!("db-{EFI_GLOBAL_VARIABLE_GUID}"));
    let enabled = fs::read(secure_boot)
        .ok()
        .and_then(|raw| parse_secure_boot_variable(&raw));
    let parsed = fs::read(db).ok().and_then(|raw| {
        raw.get(EFIVAR_ATTRIBUTES_LEN..)
            .map(parse_uefi_signature_list_cas)
    });

    let parsed = parsed.unwrap_or_default();
    SecureBootFacts {
        enabled,
        db_read: parsed.x509_certificates > 0,
        trusted_authorities: parsed.authorities,
        db_x509_certificates: parsed.x509_certificates,
    }
}

/// Parse an EFI_SIGNATURE_LIST chain from the value of the firmware `db`
/// variable. Malformed trailing data stops the walk and preserves authorities
/// already understood, matching firmware's bounded-list format without ever
/// looping on a zero-sized claim.
pub fn parse_uefi_signature_list_cas(db: &[u8]) -> UefiDbSummary {
    let mut out = UefiDbSummary::default();
    let mut authorities = BTreeSet::new();
    let mut offset = 0usize;

    while offset + 28 <= db.len() {
        let list_size = le_u32(db, offset + 16) as usize;
        let header_size = le_u32(db, offset + 20) as usize;
        let signature_size = le_u32(db, offset + 24) as usize;
        let Some(list_end) = offset.checked_add(list_size) else {
            break;
        };
        if list_size < 28
            || signature_size <= 16
            || header_size > list_size - 28
            || list_end > db.len()
        {
            break;
        }

        let entry_start = offset + 28 + header_size;
        let signature_type = &db[offset..offset + 16];
        if signature_type == EFI_CERT_X509_GUID {
            let mut entry = entry_start;
            while let Some(entry_end) = entry.checked_add(signature_size) {
                if entry_end > list_end {
                    break;
                }
                let cert = &db[entry + 16..entry_end];
                if is_structural_x509_certificate(cert) {
                    out.x509_certificates += 1;
                    if let Some(subject) = x509_common_name(cert)
                        && let Some(generation) = microsoft_authority_generation(&subject)
                    {
                        authorities.insert(generation.to_string());
                    }
                }
                entry = entry_end;
            }
        }
        offset = list_end;
    }

    out.authorities = authorities.into_iter().collect();
    out
}

/// Parse an EFI PE loader's Authenticode certificate table and return the
/// Microsoft UEFI CA generations named by its PKCS#7 certificate issuers.
///
/// A missing certificate table is a valid, inspectable unsigned image. A
/// malformed PE or unavailable `openssl` is an error that callers record as an
/// inspection warning instead of treating as a trusted loader.
pub fn inspect_efi_file(path: &Path) -> Result<(bool, Vec<String>)> {
    let bytes = fs::read(path).with_context(|| format!("reading EFI loader {}", path.display()))?;
    inspect_efi_bytes(&bytes)
}

/// Inspect a target loader while retaining the registry-relative path even
/// when optional local Authenticode tooling is unavailable.
pub fn inspect_target_bootloader(
    path: &Path,
    logical_path: &str,
    kind: TargetBootloaderKind,
) -> TargetBootloader {
    match inspect_efi_file(path) {
        Ok((signature_present, authorities)) => TargetBootloader {
            kind,
            path: logical_path.to_string(),
            signature_present,
            authorities,
            inspection_error: None,
        },
        Err(error) => TargetBootloader {
            kind,
            path: logical_path.to_string(),
            signature_present: false,
            authorities: Vec::new(),
            inspection_error: Some(format!("{error:#}")),
        },
    }
}

fn inspect_efi_bytes(bytes: &[u8]) -> Result<(bool, Vec<String>)> {
    let blobs = authenticode_pkcs7_blobs(bytes)?;
    if blobs.is_empty() {
        return Ok((false, Vec::new()));
    }

    let mut authorities = BTreeSet::new();
    for blob in blobs {
        let mut cert_file = NamedTempFile::new().context("creating temporary Authenticode file")?;
        cert_file
            .write_all(&blob)
            .context("writing temporary Authenticode file")?;
        let output = Command::new("openssl")
            .args([
                "pkcs7",
                "-inform",
                "DER",
                "-in",
                cert_file
                    .path()
                    .to_str()
                    .ok_or_else(|| anyhow!("temporary Authenticode path is not UTF-8"))?,
                "-print_certs",
                "-noout",
                "-text",
            ])
            .output()
            .context("failed to execute openssl for Authenticode inspection")?;
        if !output.status.success() {
            bail!(
                "openssl could not parse the EFI Authenticode certificate: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        for authority in authorities_from_openssl_text(&String::from_utf8_lossy(&output.stdout)) {
            authorities.insert(authority);
        }
    }
    Ok((true, authorities.into_iter().collect()))
}

/// Extract PKCS#7 payloads from a PE/COFF security directory.
fn authenticode_pkcs7_blobs(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    if data.len() < 0x40 || &data[..2] != b"MZ" {
        bail!("EFI loader is not a PE image (missing MZ header)");
    }
    let pe_offset = le_u32(data, 0x3c) as usize;
    let pe_end = pe_offset
        .checked_add(24)
        .ok_or_else(|| anyhow!("PE header offset overflows"))?;
    if pe_end > data.len() || &data[pe_offset..pe_offset + 4] != b"PE\0\0" {
        bail!("EFI loader is not a PE image (missing PE header)");
    }
    let optional_size = le_u16(data, pe_offset + 20) as usize;
    let optional_start = pe_offset + 24;
    let optional_end = optional_start
        .checked_add(optional_size)
        .ok_or_else(|| anyhow!("PE optional header size overflows"))?;
    if optional_end > data.len() || optional_size < 2 {
        bail!("PE optional header is truncated");
    }
    let magic = le_u16(data, optional_start);
    let directory_start = match magic {
        0x10b => optional_start + 96,
        0x20b => optional_start + 112,
        other => bail!("unsupported PE optional-header magic {other:#x}"),
    };
    let security_entry = directory_start
        .checked_add(4 * 8)
        .ok_or_else(|| anyhow!("PE security-directory offset overflows"))?;
    if security_entry + 8 > optional_end {
        bail!("PE optional header has no security directory");
    }
    let table_offset = le_u32(data, security_entry) as usize;
    let table_size = le_u32(data, security_entry + 4) as usize;
    if table_offset == 0 || table_size == 0 {
        return Ok(Vec::new());
    }
    let table_end = table_offset
        .checked_add(table_size)
        .ok_or_else(|| anyhow!("PE certificate table size overflows"))?;
    if table_end > data.len() {
        bail!("PE certificate table runs past the end of the loader");
    }

    let mut blobs = Vec::new();
    let mut offset = table_offset;
    while offset + 8 <= table_end {
        let length = le_u32(data, offset) as usize;
        if length < 8 {
            break;
        }
        let cert_end = offset
            .checked_add(length)
            .ok_or_else(|| anyhow!("EFI certificate length overflows"))?;
        if cert_end > table_end {
            break;
        }
        // WIN_CERT_TYPE_PKCS_SIGNED_DATA = 0x0002. Other certificate types
        // are not Authenticode PKCS#7 and cannot identify a CA signer here.
        if le_u16(data, offset + 6) == 2 {
            blobs.push(data[offset + 8..cert_end].to_vec());
        }
        let aligned = length
            .checked_add(7)
            .map(|n| n & !7)
            .ok_or_else(|| anyhow!("EFI certificate alignment overflows"))?;
        offset = offset
            .checked_add(aligned)
            .ok_or_else(|| anyhow!("EFI certificate offset overflows"))?;
    }
    Ok(blobs)
}

/// Extract recognized issuer names from `openssl pkcs7 -print_certs -text`.
/// Kept pure so the authority mapping has no process dependency in tests.
pub fn authorities_from_openssl_text(text: &str) -> Vec<String> {
    let mut authorities = BTreeSet::new();
    for line in text.lines() {
        let Some((_, name)) = line.split_once("Issuer:") else {
            continue;
        };
        if let Some(generation) = microsoft_authority_generation(name) {
            authorities.insert(generation.to_string());
        }
    }
    authorities.into_iter().collect()
}

/// Decide whether the firmware can launch every target loader of `kind`.
///
/// A refusal is emitted only when Secure Boot is known to be on, the firmware
/// `db` was read, the target loader's Microsoft signer was identified, and the
/// two authority sets do not intersect. Unknown db contents or an uninspectable
/// target loader warn instead; local MOK/hash trust is outside this parser.
pub fn enforce_target_bootloaders(
    facts: &SecureBootFacts,
    loaders: &[TargetBootloader],
    kind: TargetBootloaderKind,
    force: bool,
) -> Result<()> {
    let verdict = check_target_bootloaders(facts, loaders, kind);
    if verdict.blocked {
        if force {
            eprintln!(
                "[secure-boot] WARNING: {} --force was supplied; continuing with the target loader.",
                verdict.message
            );
        } else {
            bail!("{}", verdict.message);
        }
    } else if !verdict.message.is_empty() {
        eprintln!("[secure-boot] WARNING: {}", verdict.message);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SecureBootVerdict {
    blocked: bool,
    message: String,
}

fn check_target_bootloaders(
    facts: &SecureBootFacts,
    loaders: &[TargetBootloader],
    kind: TargetBootloaderKind,
) -> SecureBootVerdict {
    if facts.enabled != Some(true) {
        return SecureBootVerdict {
            blocked: false,
            message: String::new(),
        };
    }

    let candidates: Vec<&TargetBootloader> = loaders.iter().filter(|l| l.kind == kind).collect();
    if candidates.is_empty() {
        return SecureBootVerdict {
            blocked: false,
            message: format!(
                "Secure Boot is enabled, but the target image's {} loader was not found during the registry scan; its firmware signature cannot be checked before installation.",
                kind.as_str()
            ),
        };
    }

    if candidates
        .iter()
        .any(|loader| loader.authorities.is_empty())
    {
        let paths = candidates
            .iter()
            .filter(|loader| loader.authorities.is_empty())
            .map(|loader| loader.path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return SecureBootVerdict {
            blocked: false,
            message: format!(
                "Secure Boot is enabled, but the target {} loader(s) ({paths}) could not be matched to a recognized Microsoft UEFI CA. The firmware may reject the loader; verify its signing or use a trusted local MOK/hash policy before rebooting.",
                kind.as_str()
            ),
        };
    }

    if !facts.db_read {
        return SecureBootVerdict {
            blocked: false,
            message: "Secure Boot is enabled, but the firmware UEFI db could not be read. The target loader's signing authority could not be checked before installation.".to_string(),
        };
    }

    if facts.trusted_authorities.is_empty() {
        return SecureBootVerdict {
            blocked: true,
            message: format!(
                "Secure Boot is enabled, but the firmware UEFI db contains no recognized Microsoft third-party CA while the target {} loader is signed by Microsoft UEFI CA {}. Nothing has been changed. Enable the firmware option often called \"Allow Microsoft 3rd Party UEFI CA\", turn Secure Boot off, or choose another target image.",
                kind.as_str(),
                human_authorities(&candidates[0].authorities)
            ),
        };
    }

    for loader in candidates {
        if !intersects(&facts.trusted_authorities, &loader.authorities) {
            return SecureBootVerdict {
                blocked: true,
                message: format!(
                    "Secure Boot trusts Microsoft UEFI CA {}, but the target {} loader `{}` is signed by Microsoft UEFI CA {}. Nothing has been changed. Choose a target signed by a CA in the firmware db, turn Secure Boot off, or use --force only after verifying another trust path.",
                    human_authorities(&facts.trusted_authorities),
                    kind.as_str(),
                    loader.path,
                    human_authorities(&loader.authorities)
                ),
            };
        }
    }

    SecureBootVerdict {
        blocked: false,
        message: String::new(),
    }
}

fn intersects(left: &[String], right: &[String]) -> bool {
    left.iter()
        .any(|value| right.iter().any(|other| value == other))
}

fn human_authorities(authorities: &[String]) -> String {
    match authorities {
        [] => "unknown".to_string(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        many => format!(
            "{} and {}",
            many[..many.len() - 1].join(", "),
            many[many.len() - 1]
        ),
    }
}

fn microsoft_authority_generation(name: &str) -> Option<&'static str> {
    MICROSOFT_CA_GENERATIONS
        .iter()
        .find_map(|(pattern, generation)| name.contains(pattern).then_some(*generation))
}

fn le_u16(data: &[u8], offset: usize) -> u16 {
    data.get(offset..offset + 2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .unwrap_or_default()
}

fn le_u32(data: &[u8], offset: usize) -> u32 {
    data.get(offset..offset + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .unwrap_or_default()
}

/// `(tag, content_start, content_end, next_offset)` for one DER TLV.
fn der_tlv(data: &[u8], offset: usize) -> Option<(u8, usize, usize, usize)> {
    let tag = *data.get(offset)?;
    let first_length = *data.get(offset + 1)?;
    let (length, header_len) = if first_length & 0x80 == 0 {
        (first_length as usize, 2usize)
    } else {
        let count = (first_length & 0x7f) as usize;
        if count == 0 || count > 4 {
            return None;
        }
        let length_start = offset.checked_add(2)?;
        let length_end = length_start.checked_add(count)?;
        let length_bytes = data.get(length_start..length_end)?;
        let mut length = 0usize;
        for byte in length_bytes {
            length = length.checked_mul(256)?.checked_add(*byte as usize)?;
        }
        (length, 2 + count)
    };
    let content_start = offset.checked_add(header_len)?;
    let content_end = content_start.checked_add(length)?;
    if content_end > data.len() {
        return None;
    }
    Some((tag, content_start, content_end, content_end))
}

fn is_structural_x509_certificate(cert: &[u8]) -> bool {
    let Some((outer_tag, outer_start, _, _)) = der_tlv(cert, 0) else {
        return false;
    };
    if outer_tag != 0x30 {
        return false;
    }
    let Some((tbs_tag, tbs_start, tbs_end, _)) = der_tlv(cert, outer_start) else {
        return false;
    };
    if tbs_tag != 0x30 {
        return false;
    }

    let mut cursor = tbs_start;
    if let Some((tag, _, _, next)) = der_tlv(cert, cursor)
        && tag == 0xa0
    {
        cursor = next;
    }
    for field_index in 0..5 {
        let Some(field) = der_tlv(cert, cursor) else {
            return false;
        };
        if field_index == 4 {
            return field.0 == 0x30 && field.2 <= tbs_end;
        }
        cursor = field.3;
    }
    false
}

fn x509_common_name(cert: &[u8]) -> Option<String> {
    let (outer_tag, outer_start, outer_end, _) = der_tlv(cert, 0)?;
    if outer_tag != 0x30 || outer_end > cert.len() {
        return None;
    }
    let (tbs_tag, tbs_start, tbs_end, _) = der_tlv(cert, outer_start)?;
    if tbs_tag != 0x30 {
        return None;
    }

    let mut cursor = tbs_start;
    if let Some((tag, _, _, next)) = der_tlv(cert, cursor)
        && tag == 0xa0
    {
        cursor = next;
    }
    let serial = der_tlv(cert, cursor)?;
    cursor = serial.3;
    let signature = der_tlv(cert, cursor)?;
    cursor = signature.3;
    let issuer = der_tlv(cert, cursor)?;
    cursor = issuer.3;
    let validity = der_tlv(cert, cursor)?;
    cursor = validity.3;
    let subject = der_tlv(cert, cursor)?;

    if subject.0 != 0x30 || subject.2 > tbs_end {
        return None;
    }
    find_common_name(cert, subject.1, subject.2)
}

fn find_common_name(data: &[u8], start: usize, end: usize) -> Option<String> {
    let mut cursor = start;
    while cursor < end {
        let (tag, content_start, content_end, next) = der_tlv(data, cursor)?;
        if next > end {
            return None;
        }
        if tag == 0x06 && data.get(content_start..content_end) == Some(COMMON_NAME_OID.as_slice()) {
            let (value_tag, value_start, value_end, _) = der_tlv(data, next)?;
            return decode_directory_string(value_tag, &data[value_start..value_end]);
        }
        if tag & 0x20 != 0
            && let Some(value) = find_common_name(data, content_start, content_end)
        {
            return Some(value);
        }
        cursor = next;
    }
    None
}

fn decode_directory_string(tag: u8, bytes: &[u8]) -> Option<String> {
    match tag {
        0x0c | 0x13 | 0x14 | 0x16 => Some(String::from_utf8_lossy(bytes).into_owned()),
        0x1e if bytes.len().is_multiple_of(2) => {
            let units = (0..bytes.len())
                .step_by(2)
                .map(|offset| u16::from_be_bytes([bytes[offset], bytes[offset + 1]]));
            Some(String::from_utf16_lossy(&units.collect::<Vec<_>>()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        assert!(content.len() < 128);
        let mut out = vec![tag, content.len() as u8];
        out.extend_from_slice(content);
        out
    }

    fn fake_certificate(common_name: &str) -> Vec<u8> {
        let oid = der(0x06, &COMMON_NAME_OID);
        let value = der(0x0c, common_name.as_bytes());
        let attribute = der(0x30, &[oid, value].concat());
        let rdn = der(0x31, &attribute);
        let name = der(0x30, &rdn);
        let integer = der(0x02, &[1]);
        let signature = der(0x30, &[]);
        let validity = der(0x30, &[]);
        let tbs = der(
            0x30,
            &[
                integer.clone(),
                signature.clone(),
                name.clone(),
                validity,
                name,
            ]
            .concat(),
        );
        der(0x30, &[tbs, signature].concat())
    }

    fn signature_list(signature_type: [u8; 16], certs: &[Vec<u8>]) -> Vec<u8> {
        let signature_size = certs.iter().map(|cert| 16 + cert.len()).max().unwrap_or(0);
        let list_size = 28 + certs.len() * signature_size;
        let mut out = Vec::new();
        out.extend_from_slice(&signature_type);
        out.extend_from_slice(&(list_size as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(signature_size as u32).to_le_bytes());
        for cert in certs {
            out.extend_from_slice(&[0u8; 16]);
            out.extend_from_slice(cert);
            out.resize(out.len() + signature_size - 16 - cert.len(), 0);
        }
        out
    }

    fn loader(kind: TargetBootloaderKind, authorities: &[&str]) -> TargetBootloader {
        TargetBootloader {
            kind,
            path: "usr/lib/systemd/boot/efi/systemd-bootx64.efi".into(),
            signature_present: true,
            authorities: authorities.iter().map(|s| (*s).to_string()).collect(),
            inspection_error: None,
        }
    }

    #[test]
    fn parses_secure_boot_variable_value() {
        assert_eq!(parse_secure_boot_variable(&[7, 0, 0, 0, 1]), Some(true));
        assert_eq!(parse_secure_boot_variable(&[7, 0, 0, 0, 0]), Some(false));
        assert_eq!(parse_secure_boot_variable(&[7, 0, 0, 0, 2]), None);
        assert_eq!(parse_secure_boot_variable(&[7, 0, 0, 0]), None);
    }

    #[test]
    fn reads_secure_boot_facts_from_efivarfs_shape() {
        let dir = tempdir().unwrap();
        let secure_boot_name = format!("SecureBoot-{EFI_GLOBAL_VARIABLE_GUID}");
        let db_name = format!("db-{EFI_GLOBAL_VARIABLE_GUID}");
        fs::write(dir.path().join(secure_boot_name), [7, 0, 0, 0, 1]).unwrap();
        let mut db = vec![7, 0, 0, 0];
        db.extend(signature_list(
            EFI_CERT_X509_GUID,
            &[fake_certificate("Microsoft UEFI CA 2023")],
        ));
        fs::write(dir.path().join(db_name), db).unwrap();

        let facts = read_from_efivars(dir.path());
        assert_eq!(facts.enabled, Some(true));
        assert!(facts.db_read);
        assert_eq!(facts.trusted_authorities, vec!["2023"]);
        assert_eq!(facts.db_x509_certificates, 1);
    }

    #[test]
    fn parses_x509_signature_lists_and_skips_non_x509_lists() {
        let mut db = signature_list([0x26; 16], &[vec![0u8; 32]]);
        db.extend(signature_list(
            EFI_CERT_X509_GUID,
            &[
                fake_certificate("Microsoft Corporation UEFI CA 2011"),
                fake_certificate("Microsoft UEFI CA 2023"),
                fake_certificate("Microsoft Option ROM UEFI CA 2023"),
            ],
        ));
        let parsed = parse_uefi_signature_list_cas(&db);
        assert_eq!(parsed.authorities, vec!["2011", "2023"]);
        assert_eq!(parsed.x509_certificates, 3);
    }

    #[test]
    fn malformed_signature_lists_terminate_without_inventing_trust() {
        let mut zero_size = vec![0u8; 28];
        zero_size[..16].copy_from_slice(&EFI_CERT_X509_GUID);
        assert_eq!(
            parse_uefi_signature_list_cas(&zero_size),
            UefiDbSummary::default()
        );

        let cert = signature_list(
            EFI_CERT_X509_GUID,
            &[fake_certificate("Microsoft UEFI CA 2023")],
        );
        for cut in [0, 1, 27, 28, cert.len() / 2, cert.len() - 1] {
            let parsed = parse_uefi_signature_list_cas(&cert[..cut]);
            assert!(parsed.authorities.len() <= 1);
        }
    }

    #[test]
    fn maps_openssl_issuer_lines_only() {
        let text = "Issuer: C = US, O = Microsoft Corporation, CN = Microsoft Corporation UEFI CA 2011\nIssuer: CN = Microsoft UEFI CA 2023\nIssuer: CN = Microsoft Option ROM UEFI CA 2023\n";
        assert_eq!(authorities_from_openssl_text(text), vec!["2011", "2023"]);
    }

    #[test]
    fn secure_boot_gate_blocks_only_a_known_mismatch() {
        let cases = [
            (
                "known mismatch",
                true,
                true,
                vec!["2023"],
                vec!["2011"],
                true,
                false,
            ),
            (
                "overlap",
                true,
                true,
                vec!["2023"],
                vec!["2023"],
                false,
                false,
            ),
            (
                "db unreadable",
                true,
                false,
                vec![],
                vec!["2011"],
                false,
                true,
            ),
            (
                "db read with no third-party CA",
                true,
                true,
                vec![],
                vec!["2011"],
                true,
                false,
            ),
            (
                "secure boot off",
                false,
                true,
                vec![],
                vec!["2011"],
                false,
                false,
            ),
            (
                "unknown loader",
                true,
                true,
                vec!["2023"],
                vec![],
                false,
                true,
            ),
        ];
        for (name, enabled, db_read, trusted, staged, blocked, warning) in cases {
            let facts = SecureBootFacts {
                enabled: Some(enabled),
                db_read,
                trusted_authorities: trusted.into_iter().map(String::from).collect(),
                db_x509_certificates: usize::from(db_read),
            };
            let check = check_target_bootloaders(
                &facts,
                &[loader(TargetBootloaderKind::SystemdBoot, &staged)],
                TargetBootloaderKind::SystemdBoot,
            );
            assert_eq!(check.blocked, blocked, "{name}");
            assert_eq!(!check.message.is_empty(), blocked || warning, "{name}");
        }
    }

    #[test]
    fn refusal_explains_the_recovery_without_internals() {
        let facts = SecureBootFacts {
            enabled: Some(true),
            db_read: true,
            trusted_authorities: vec!["2023".into()],
            db_x509_certificates: 1,
        };
        let check = check_target_bootloaders(
            &facts,
            &[loader(TargetBootloaderKind::SystemdBoot, &["2011"])],
            TargetBootloaderKind::SystemdBoot,
        );
        assert!(check.blocked);
        for expected in [
            "Nothing has been changed",
            "2023",
            "2011",
            "Secure Boot off",
        ] {
            assert!(
                check.message.contains(expected),
                "missing {expected}: {}",
                check.message
            );
        }
        for forbidden in ["EFI_SIGNATURE_LIST", "Authenticode", "PKCS#7"] {
            assert!(!check.message.contains(forbidden), "leaked {forbidden}");
        }
    }
}
