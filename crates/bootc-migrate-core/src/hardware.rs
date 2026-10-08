//! Hardware and firmware compatibility between this machine and a target image.
//!
//! A re-base replaces the kernel, its modules and `/usr/lib/firmware` with the
//! target image's. Hardware that works today can stop working after the reboot
//! when the target does not ship the driver it uses (an out-of-tree NVIDIA
//! module, a Wi-Fi driver a minimal image leaves out) or the firmware that
//! driver loads. This module compares the two before anything is changed:
//!
//! - **Host**: every device on a user-facing bus (PCI, USB, SDIO, virtio, HID)
//!   that is bound to a driver today, the kernel module behind that driver,
//!   the module's out-of-tree/proprietary taint, and the firmware files the
//!   module can load that this machine actually has.
//! - **Target**: the kernel's `modules.dep`, `modules.builtin`,
//!   `modules.alias` and `modules.builtin.alias`, and the names of the files
//!   under `usr/lib/firmware`, collected during the registry scan's single pass
//!   over the image layers ([`crate::registry::fetch_probe_files_via_registry`]).
//!
//! Only devices whose driver is a **loadable module** on the host are judged.
//! A built-in driver has no module to look for, and the core drivers that
//! distributions build in (PCIe ports, USB hubs, generic HID) do not differ in
//! practice; judging them by driver name would report false gaps.
//!
//! The comparison is pure ([`assess`]); reading `/sys` and calling `modinfo`
//! are injected ([`read_host_hardware_from`]) so tests run on fixtures.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// The target kernel's driver and firmware inventory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetKernel {
    /// Kernel version directory under `usr/lib/modules`.
    pub kver: String,
    /// Loadable module names (normalized, see [`norm_module`]).
    pub modules: BTreeSet<String>,
    /// Built-in module names (normalized).
    pub builtin: BTreeSet<String>,
    /// `(pattern, module)` pairs from `modules.alias` and
    /// `modules.builtin.alias`.
    pub aliases: Vec<(String, String)>,
    /// Firmware file paths relative to `usr/lib/firmware`, compression
    /// suffix removed (see [`norm_firmware`]).
    pub firmware: BTreeSet<String>,
    /// Image layers the registry pass could not read. Non-zero means the
    /// inventory may lack modules or firmware the image does ship, so an
    /// absence is not proof: [`assess`] does not block on it.
    pub layers_skipped: usize,
}

impl TargetKernel {
    /// Whether the target ships the module `module` itself (loadable or
    /// built in).
    pub fn has_module(&self, module: &str) -> bool {
        let m = norm_module(module);
        self.modules.contains(&m) || self.builtin.contains(&m)
    }

    /// The target modules whose aliases claim `modalias`, sorted, deduplicated.
    pub fn alias_drivers(&self, modalias: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .aliases
            .iter()
            .filter(|(pat, _)| glob_match(pat, modalias))
            .map(|(_, m)| m.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Whether the target ships the firmware file `name` (or any file
    /// matching it, when the module declares a pattern).
    pub fn has_firmware(&self, name: &str) -> bool {
        let n = norm_firmware(name);
        if n.contains(['*', '?', '[']) {
            self.firmware.iter().any(|f| glob_match(&n, f))
        } else {
            self.firmware.contains(&n)
        }
    }
}

/// Module names compare with `-` and `_` treated alike, as modprobe does.
pub fn norm_module(name: &str) -> String {
    name.trim().replace('-', "_")
}

/// The module name of a `modules.dep`/`modules.builtin` path:
/// `kernel/drivers/net/e1000e/e1000e.ko.xz` → `e1000e`.
pub fn module_name_from_path(path: &str) -> Option<String> {
    let base = path.trim().rsplit('/').next()?;
    let stem = base
        .strip_suffix(".ko.xz")
        .or_else(|| base.strip_suffix(".ko.zst"))
        .or_else(|| base.strip_suffix(".ko.gz"))
        .or_else(|| base.strip_suffix(".ko"))?;
    (!stem.is_empty()).then(|| norm_module(stem))
}

/// Firmware names compare without their compression suffix: the kernel
/// loads `foo.bin.xz` and `foo.bin.zst` for a request of `foo.bin`, and
/// distributions pick different ones.
pub fn norm_firmware(name: &str) -> String {
    let n = name.trim().trim_start_matches("./").trim_start_matches('/');
    n.strip_suffix(".xz")
        .or_else(|| n.strip_suffix(".zst"))
        .or_else(|| n.strip_suffix(".gz"))
        .unwrap_or(n)
        .to_string()
}

/// Module names from `modules.dep` (`path: deps`) or `modules.builtin`
/// (`path` per line).
pub fn parse_module_list(content: &str) -> BTreeSet<String> {
    content
        .lines()
        .filter_map(|l| module_name_from_path(l.split(':').next().unwrap_or("")))
        .collect()
}

/// `(pattern, module)` pairs from `modules.alias`/`modules.builtin.alias`
/// (`alias <pattern> <module>`).
pub fn parse_module_aliases(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            (f.next()? == "alias").then_some(())?;
            let pat = f.next()?;
            let module = f.next()?;
            Some((pat.to_string(), norm_module(module)))
        })
        .collect()
}

/// Shell-style glob match (`*`, `?`, `[...]`, `[!...]`), the syntax of
/// `modules.alias` patterns and of firmware names some modules declare.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_at(&p, &t)
}

fn glob_at(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    // Last `*` seen: (pattern index after it, text index it matched up to).
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star = Some((pi + 1, ti));
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                '[' => {
                    if let Some((matched, next)) = class_match(&p[pi..], t[ti])
                        && matched
                    {
                        pi += next;
                        ti += 1;
                        continue;
                    }
                }
                c if c == t[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((sp, st)) => {
                pi = sp;
                ti = st + 1;
                star = Some((sp, st + 1));
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Match one character against the bracket expression at the start of `p`.
/// Returns `(matched, length of the expression)`, or `None` when the bracket
/// is not closed (then it is matched literally by the caller's fallback).
fn class_match(p: &[char], c: char) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some('!') | Some('^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == ']' && !first {
            return Some((matched != negate, i + 1));
        }
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            if p[i] <= c && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == c {
                matched = true;
            }
            i += 1;
        }
        first = false;
    }
    None
}

/// Apply one layer's `usr/lib/firmware` listing to the firmware set, oldest
/// layer first: OCI whiteouts (`.wh.<name>`, `.wh..wh..opq`) remove what
/// earlier layers added.
pub fn apply_firmware_listing(set: &mut BTreeSet<String>, names: &[String]) {
    const PREFIX: &str = "usr/lib/firmware/";
    for raw in names {
        let name = raw.trim_start_matches("./");
        let Some(rel) = name.strip_prefix(PREFIX) else {
            continue;
        };
        let (dir, base) = match rel.rsplit_once('/') {
            Some((d, b)) => (Some(d), b),
            None => (None, rel),
        };
        if base == ".wh..wh..opq" {
            let prefix = dir.map(|d| format!("{d}/")).unwrap_or_default();
            set.retain(|f| !f.starts_with(&prefix));
        } else if let Some(gone) = base.strip_prefix(".wh.") {
            let gone = match dir {
                Some(d) => format!("{d}/{gone}"),
                None => gone.to_string(),
            };
            let gone = norm_firmware(&gone);
            set.retain(|f| f != &gone && !f.starts_with(&format!("{gone}/")));
        } else if !base.is_empty() {
            set.insert(norm_firmware(rel));
        }
    }
}

/// A device that is bound to a driver on this machine today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDevice {
    /// `pci`, `usb`, `sdio`, `virtio` or `hid`.
    pub bus: String,
    /// The device's sysfs name (`0000:00:14.3`, `1-2:1.0`).
    pub id: String,
    pub modalias: Option<String>,
    /// The bound driver's name.
    pub driver: String,
    /// The kernel module behind the driver, `None` when it is built in.
    pub module: Option<String>,
    /// PCI class (`0x028000`), for the device kind and its severity.
    pub pci_class: Option<u32>,
}

/// A loadable module this machine uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostModule {
    /// Tainted `O` (out-of-tree) or `P` (proprietary) — NVIDIA, VirtualBox,
    /// ZFS, and DKMS/akmods builds in general.
    pub out_of_tree: bool,
    /// Firmware files the module can load that exist on this machine
    /// (normalized). Modules list every variant they support; the ones this
    /// machine ships are the ones its hardware can be using.
    pub firmware_present: Vec<String>,
}

/// What [`read_host_hardware_from`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostHardware {
    pub devices: Vec<HostDevice>,
    /// Keyed by normalized module name.
    pub modules: BTreeMap<String, HostModule>,
    /// `GenuineIntel`, `AuthenticAMD`, … from `/proc/cpuinfo`.
    pub cpu_vendor: Option<String>,
    /// The host has CPU microcode for its vendor (`intel-ucode/`,
    /// `amd-ucode/`).
    pub has_cpu_microcode: bool,
}

/// Buses judged: the ones carrying the hardware a user notices losing.
const BUSES: &[&str] = &["pci", "usb", "sdio", "virtio", "hid"];

/// Read the host inventory under `sys_root` (normally `/`).
///
/// `firmware_dirs` are the directories searched for firmware (normally
/// `/usr/lib/firmware` and `/lib/firmware`). `modinfo_firmware` returns the
/// `modinfo -F firmware <module>` list for a module name.
pub fn read_host_hardware_from(
    sys_root: &Path,
    firmware_dirs: &[PathBuf],
    modinfo_firmware: &dyn Fn(&str) -> Vec<String>,
) -> HostHardware {
    let mut hw = HostHardware::default();
    let mut seen = BTreeSet::new();
    for bus in BUSES {
        let dir = sys_root.join("sys/bus").join(bus).join("devices");
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
        names.sort();
        for name in names {
            let dev = dir.join(&name);
            let Ok(driver_link) = fs::read_link(dev.join("driver")) else {
                continue;
            };
            let real = fs::canonicalize(&dev).unwrap_or_else(|_| dev.clone());
            if !seen.insert(real) {
                continue;
            }
            let Some(driver) = driver_link.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let module = fs::read_link(dev.join("driver/module"))
                .ok()
                .and_then(|m| m.file_name().and_then(|s| s.to_str()).map(norm_module));
            let modalias = fs::read_to_string(dev.join("modalias"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let pci_class = (*bus == "pci")
                .then(|| fs::read_to_string(dev.join("class")).ok())
                .flatten()
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());
            hw.devices.push(HostDevice {
                bus: (*bus).to_string(),
                id: name.to_string_lossy().into_owned(),
                modalias,
                driver: driver.to_string(),
                module,
                pci_class,
            });
        }
    }

    let used: BTreeSet<String> = hw.devices.iter().filter_map(|d| d.module.clone()).collect();
    for module in used {
        let taint = fs::read_to_string(sys_root.join("sys/module").join(&module).join("taint"))
            .unwrap_or_default();
        let out_of_tree = taint.contains('O') || taint.contains('P');
        let mut firmware_present: Vec<String> = modinfo_firmware(&module)
            .into_iter()
            .map(|f| norm_firmware(&f))
            .filter(|f| firmware_exists(firmware_dirs, f))
            .collect();
        firmware_present.sort();
        firmware_present.dedup();
        hw.modules.insert(
            module,
            HostModule {
                out_of_tree,
                firmware_present,
            },
        );
    }

    hw.cpu_vendor = fs::read_to_string(sys_root.join("proc/cpuinfo"))
        .ok()
        .and_then(|c| {
            c.lines().find_map(|l| {
                l.strip_prefix("vendor_id")
                    .map(|v| v.trim_start_matches([' ', '\t', ':']).trim().to_string())
            })
        });
    hw.has_cpu_microcode = microcode_dir(hw.cpu_vendor.as_deref())
        .is_some_and(|d| firmware_dirs.iter().any(|f| f.join(d).is_dir()));
    hw
}

/// Read this machine's inventory: `/sys`, `/proc/cpuinfo`, the firmware
/// directories, and `modinfo` for the firmware each module declares.
pub fn read_host_hardware() -> HostHardware {
    let firmware_dirs = [
        PathBuf::from("/usr/lib/firmware"),
        PathBuf::from("/lib/firmware"),
    ];
    read_host_hardware_from(Path::new("/"), &firmware_dirs, &|module| {
        std::process::Command::new("modinfo")
            .args(["-F", "firmware", module])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Whether `name` (normalized, possibly a pattern) exists in any firmware
/// directory, compressed or not.
fn firmware_exists(dirs: &[PathBuf], name: &str) -> bool {
    if name.contains(['*', '?', '[']) {
        // A declared pattern: present when its directory has any match.
        let (dir, pat) = name.rsplit_once('/').unwrap_or(("", name));
        return dirs.iter().any(|d| {
            fs::read_dir(d.join(dir)).is_ok_and(|rd| {
                rd.flatten().any(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|f| glob_match(pat, &norm_firmware(f)))
                })
            })
        });
    }
    dirs.iter().any(|d| {
        ["", ".xz", ".zst", ".gz"]
            .iter()
            .any(|ext| d.join(format!("{name}{ext}")).exists())
    })
}

fn microcode_dir(vendor: Option<&str>) -> Option<&'static str> {
    match vendor? {
        "GenuineIntel" => Some("intel-ucode"),
        "AuthenticAMD" => Some("amd-ucode"),
        _ => None,
    }
}

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing; nothing is expected to break.
    Note,
    /// Something may stop working; the migration goes ahead.
    Warning,
    /// Hardware that works today is expected to stop working: storage,
    /// display or network, or an out-of-tree driver. The migration refuses
    /// unless the user accepts it.
    Blocking,
}

/// One compatibility finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    /// What is affected (`PCI 0000:01:00.0 (display, driver nvidia)`).
    pub subject: String,
    /// What is missing and what that means.
    pub detail: String,
}

/// The result of comparing the host with a target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HardwareReport {
    pub target_kernel: Option<String>,
    /// Devices judged (bound to a loadable module on the host).
    pub devices_checked: usize,
    pub findings: Vec<Finding>,
}

impl HardwareReport {
    pub fn blocking(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
    }

    pub fn has_blocking(&self) -> bool {
        self.blocking().next().is_some()
    }
}

/// The kind of a PCI device, from its class, and whether losing it is
/// severe enough to block the migration.
fn pci_kind(class: u32) -> (&'static str, bool) {
    match class >> 16 {
        0x01 => ("storage controller", true),
        0x02 => ("network controller", true),
        0x03 => ("display controller", true),
        0x04 => ("multimedia device", false),
        0x0c if (class >> 8) & 0xff == 0x03 => ("USB controller", true),
        0x0d => ("wireless controller", true),
        0x0c => ("serial bus controller", false),
        0x06 => ("bridge", false),
        0x08 => ("system peripheral", false),
        0x11 => ("signal processing controller", false),
        _ => ("device", false),
    }
}

/// Compare `host` with the target kernel inventory.
pub fn assess(host: &HostHardware, target: &TargetKernel) -> HardwareReport {
    let mut report = HardwareReport {
        target_kernel: Some(target.kver.clone()),
        ..Default::default()
    };

    // Devices sharing a module and an outcome are reported once, naming
    // each. Key: (module, fallback modules; empty = no driver at all).
    let mut lost: BTreeMap<(String, Vec<String>), (Severity, Vec<String>)> = BTreeMap::new();
    for dev in &host.devices {
        let Some(module) = &dev.module else {
            continue;
        };
        report.devices_checked += 1;
        if target.has_module(module) {
            continue;
        }
        let (kind, severe) = dev.pci_class.map_or(("device", false), pci_kind);
        let out_of_tree = host.modules.get(module).is_some_and(|m| m.out_of_tree);
        let fallback = dev
            .modalias
            .as_deref()
            .map(|ma| target.alias_drivers(ma))
            .unwrap_or_default();
        let severity = match (fallback.is_empty(), severe || out_of_tree, out_of_tree) {
            // Nothing in the target can drive it.
            (true, true, _) => Severity::Blocking,
            (true, false, _) => Severity::Warning,
            // Another driver takes over: losing an out-of-tree driver
            // (NVIDIA -> nouveau) loses what the user installed it for.
            (false, _, true) => Severity::Warning,
            (false, _, false) => Severity::Note,
        };
        let entry = lost
            .entry((module.clone(), fallback))
            .or_insert((Severity::Note, Vec::new()));
        entry.0 = entry.0.max(severity);
        entry
            .1
            .push(format!("{} {} ({kind})", dev.bus.to_uppercase(), dev.id));
    }
    for ((module, fallback), (severity, devices)) in lost {
        let out_of_tree = host.modules.get(&module).is_some_and(|m| m.out_of_tree);
        let what = if out_of_tree {
            format!("the out-of-tree driver `{module}`")
        } else {
            format!("the driver `{module}`")
        };
        let detail = match (fallback.is_empty(), out_of_tree) {
            (true, true) => format!(
                "uses {what}, which the target image does not ship, and no driver in the \
                 target kernel ({}) supports this hardware. Out-of-tree drivers (NVIDIA, \
                 VirtualBox, ZFS, DKMS/akmods builds) must be built into the image itself. \
                 After the reboot it is expected to stop working.",
                target.kver
            ),
            (true, false) => format!(
                "uses {what}, and no module in the target kernel ({}) supports it. After \
                 the reboot it is expected to stop working.",
                target.kver
            ),
            (false, true) => format!(
                "uses {what}, which the target image does not ship. After the reboot the \
                 in-kernel `{}` driver takes over: the hardware keeps working, without what \
                 `{module}` provides (for NVIDIA: CUDA, the proprietary OpenGL/Vulkan stack and \
                 its performance). Use an image variant that ships `{module}` to keep it.",
                fallback.join("`/`")
            ),
            (false, false) => format!(
                "uses {what}, which the target kernel does not ship; `{}` supports the same \
                 hardware there and takes over after the reboot.",
                fallback.join("`/`")
            ),
        };
        report.findings.push(Finding {
            severity,
            subject: devices.join(", "),
            detail,
        });
    }

    for (module, info) in &host.modules {
        if info.firmware_present.is_empty() {
            continue;
        }
        // A module the target does not ship is reported above; its firmware
        // is moot.
        if !target.has_module(module) {
            continue;
        }
        let absent: Vec<&String> = info
            .firmware_present
            .iter()
            .filter(|f| !target.has_firmware(f))
            .collect();
        if absent.is_empty() {
            continue;
        }
        let severe_device = host.devices.iter().any(|d| {
            d.module.as_deref() == Some(module.as_str())
                && d.pci_class.is_some_and(|c| pci_kind(c).1)
        });
        let total = info.firmware_present.len();
        let sample: Vec<&str> = absent.iter().take(4).map(|s| s.as_str()).collect();
        let more = absent.len().saturating_sub(sample.len());
        let list = if more > 0 {
            format!("{} and {more} more", sample.join(", "))
        } else {
            sample.join(", ")
        };
        if absent.len() == total {
            report.findings.push(Finding {
                severity: if severe_device {
                    Severity::Blocking
                } else {
                    Severity::Warning
                },
                subject: format!("driver `{module}` firmware"),
                detail: format!(
                    "the target image ships none of the {total} firmware file(s) this machine \
                     has for `{module}` ({list}). Hardware that needs firmware to start is \
                     expected to stop working after the reboot."
                ),
            });
        } else {
            report.findings.push(Finding {
                severity: Severity::Note,
                subject: format!("driver `{module}` firmware"),
                detail: format!(
                    "the target image lacks {} of the {total} firmware file(s) this machine has \
                     for `{module}` ({list}). `{module}` supports several hardware variants and \
                     most of these files are for other models, so this is likely harmless.",
                    absent.len()
                ),
            });
        }
    }

    if let Some(dir) = microcode_dir(host.cpu_vendor.as_deref())
        && host.has_cpu_microcode
        && !target
            .firmware
            .iter()
            .any(|f| f.starts_with(&format!("{dir}/")))
    {
        report.findings.push(Finding {
            severity: Severity::Warning,
            subject: format!(
                "CPU microcode ({})",
                host.cpu_vendor.as_deref().unwrap_or("")
            ),
            detail: format!(
                "this machine has {dir} microcode updates and the target image has none. \
                 The CPU then runs on the microcode its firmware (BIOS/UEFI) loaded, without \
                 the security and stability fixes the update would add."
            ),
        });
    }

    if target.layers_skipped > 0 {
        // An absence read from a partial inventory is not proof of absence.
        for f in &mut report.findings {
            f.severity = f.severity.min(Severity::Warning);
        }
        report.findings.push(Finding {
            severity: Severity::Warning,
            subject: "incomplete check".into(),
            detail: format!(
                "{} layer(s) of the target image could not be read, so the target's driver \
                 and firmware lists may be incomplete. Missing items reported here may in fact \
                 be present; none of them blocks the migration. Re-run the scan to check fully.",
                target.layers_skipped
            ),
        });
    }

    report.findings.sort_by(|a, b| b.severity.cmp(&a.severity));
    report
}

/// The report as text, for the CLI and logs. `target_image` names the image.
pub fn render(report: &HardwareReport, target_image: &str) -> String {
    let mut out = String::new();
    out.push_str("=== Hardware compatibility ===\n");
    out.push_str(&format!(
        "Target image: {target_image} (kernel {})\n",
        report.target_kernel.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "Devices checked: {} (devices bound to a loadable kernel module)\n",
        report.devices_checked
    ));
    if report.findings.is_empty() {
        out.push_str(
            "OK: the target ships a driver for every device checked, and firmware for each \
             driver that loads it.\n",
        );
        return out;
    }
    for f in &report.findings {
        let tag = match f.severity {
            Severity::Blocking => "BLOCKING",
            Severity::Warning => "WARNING",
            Severity::Note => "note",
        };
        out.push_str(&format!("[{tag}] {}: {}\n", f.subject, f.detail));
    }
    out
}

/// Text for a target whose kernel inventory could not be read. A gap in the
/// check is not a pass, so this says so plainly.
pub fn render_unknown(target_image: &str, why: &str) -> String {
    format!(
        "=== Hardware compatibility ===\nTarget image: {target_image}\n\
         [WARNING] Hardware compatibility could NOT be checked ({why}). Before you reboot, \
         check that the target image supports your graphics, network and storage hardware, \
         and any out-of-tree driver you use (for example NVIDIA).\n"
    )
}

/// The decision [`gate`] takes from a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    Proceed,
    /// Blocking findings, accepted with `--accept-hardware-gaps`.
    ProceedAccepted,
    /// Blocking findings in a dry run: report what a real run would refuse.
    WouldRefuse,
    Refuse,
}

/// Pure decision: refuse on a blocking finding unless accepted; a dry run
/// never refuses.
pub fn decide(report: &HardwareReport, accept: bool, dry_run: bool) -> GateDecision {
    match (report.has_blocking(), accept, dry_run) {
        (false, _, _) => GateDecision::Proceed,
        (true, true, _) => GateDecision::ProceedAccepted,
        (true, false, true) => GateDecision::WouldRefuse,
        (true, false, false) => GateDecision::Refuse,
    }
}

/// The flag that accepts blocking findings.
pub const ACCEPT_FLAG: &str = "--accept-hardware-gaps";

/// Check this machine's hardware against `target_image`, print the report,
/// and refuse (an error) when hardware that works today is expected to stop
/// working, unless `accept`. A target that cannot be inspected is reported
/// loudly and does not block: the check is a safety net, not a precondition
/// for migrating at all.
pub fn check_and_gate(target_image: &str, accept: bool, dry_run: bool) -> anyhow::Result<()> {
    let Some(caps) = crate::cross_base::scan_target_capabilities_with_retries(
        target_image,
        "hardware compatibility",
    ) else {
        eprint!(
            "{}",
            render_unknown(target_image, "the target image could not be scanned")
        );
        return Ok(());
    };
    let Some(kernel) = caps.kernel.as_ref() else {
        eprint!(
            "{}",
            render_unknown(target_image, "the target image ships no kernel modules")
        );
        return Ok(());
    };
    let report = assess(&read_host_hardware(), kernel);
    print!("{}", render(&report, target_image));
    match decide(&report, accept, dry_run) {
        GateDecision::Proceed => Ok(()),
        GateDecision::ProceedAccepted => {
            eprintln!(
                "Proceeding: {ACCEPT_FLAG} accepts the BLOCKING finding(s) above. That hardware \
                 is expected to stop working after the reboot."
            );
            Ok(())
        }
        GateDecision::WouldRefuse => {
            eprintln!(
                "[DRY RUN] A real run would refuse here because of the BLOCKING finding(s) \
                 above, unless {ACCEPT_FLAG} is given."
            );
            Ok(())
        }
        GateDecision::Refuse => anyhow::bail!(
            "refusing to migrate: hardware that works on this machine today is expected to \
             stop working on {target_image} (BLOCKING findings above). Choose an image variant \
             that supports it (for NVIDIA, an -nvidia image), or re-run with {ACCEPT_FLAG} to \
             accept the loss."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn module_names_from_paths() {
        for (path, want) in [
            ("kernel/drivers/net/e1000e/e1000e.ko.xz", Some("e1000e")),
            ("kernel/drivers/gpu/drm/i915/i915.ko.zst", Some("i915")),
            ("extra/nvidia-drm.ko", Some("nvidia_drm")),
            ("kernel/sound/snd-hda-intel.ko.gz", Some("snd_hda_intel")),
            ("modules.order", None),
            ("", None),
        ] {
            assert_eq!(module_name_from_path(path).as_deref(), want, "{path}");
        }
    }

    #[test]
    fn module_list_parses_dep_and_builtin() {
        let dep = "kernel/drivers/net/e1000e/e1000e.ko.xz: kernel/net/ptp.ko.xz\n\
                   extra/nvidia.ko:\n";
        assert_eq!(
            parse_module_list(dep),
            ["e1000e", "nvidia"].iter().map(|s| s.to_string()).collect()
        );
        let builtin = "kernel/drivers/hid/hid-generic.ko\nkernel/drivers/usb/core/usbcore.ko\n";
        assert!(parse_module_list(builtin).contains("hid_generic"));
    }

    #[test]
    fn aliases_parse_and_skip_other_lines() {
        let a = "# Aliases extracted from modules themselves.\n\
                 alias pci:v00008086d000015B8sv*sd*bc*sc*i* e1000e\n\
                 alias usb:v0BDAp8179d*dc*dsc*dp*ic*isc*ip*in* r8188eu\n\
                 softdep foo pre: bar\n";
        let got = parse_module_aliases(a);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].1, "r8188eu");
    }

    #[test]
    fn glob_matches_modalias_patterns() {
        let cases = [
            (
                "pci:v00008086d000015B8sv*sd*bc*sc*i*",
                "pci:v00008086d000015B8sv00001028sd000007A1bc02sc00i00",
                true,
            ),
            (
                "pci:v00008086d000015B8sv*sd*bc*sc*i*",
                "pci:v00008086d000015B9sv00001028sd000007A1bc02sc00i00",
                false,
            ),
            (
                "pci:v*d*sv*sd*bc03sc*i*",
                "pci:v000010DEd00002484sv00001458sd00004037bc03sc00i00",
                true,
            ),
            ("usb:v0BDAp81[0-9A]9d*", "usb:v0BDAp8179d0000dc00", true),
            ("usb:v0BDAp81[!7]9d*", "usb:v0BDAp8179d0000dc00", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("*", "", true),
            ("iwlwifi-*.ucode", "iwlwifi-ty-a0-gf-a0-89.ucode", true),
            ("abc", "abcd", false),
        ];
        for (p, t, want) in cases {
            assert_eq!(glob_match(p, t), want, "{p} vs {t}");
        }
    }

    #[test]
    fn firmware_names_ignore_compression() {
        assert_eq!(
            norm_firmware("i915/tgl_dmc_ver2_12.bin.xz"),
            "i915/tgl_dmc_ver2_12.bin"
        );
        assert_eq!(
            norm_firmware("./amdgpu/navi10_me.bin.zst"),
            "amdgpu/navi10_me.bin"
        );
        assert_eq!(norm_firmware("regulatory.db"), "regulatory.db");
    }

    #[test]
    fn firmware_listing_applies_layers_and_whiteouts() {
        let mut set = BTreeSet::new();
        let l1: Vec<String> = [
            "./usr/lib/firmware/iwlwifi-cc-a0-77.ucode.xz",
            "usr/lib/firmware/nvidia/tu102/gsp.bin.zst",
            "usr/lib/firmware/nvidia/ga102/gsp.bin.zst",
            "usr/lib/firmware/",
            "usr/share/doc/README",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        apply_firmware_listing(&mut set, &l1);
        assert!(set.contains("iwlwifi-cc-a0-77.ucode"));
        assert!(set.contains("nvidia/tu102/gsp.bin"));
        assert_eq!(set.len(), 3, "{set:?}");

        let l2: Vec<String> = [
            "usr/lib/firmware/.wh.iwlwifi-cc-a0-77.ucode.xz",
            "usr/lib/firmware/nvidia/.wh..wh..opq",
            "usr/lib/firmware/nvidia/ad102/gsp.bin.zst",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        apply_firmware_listing(&mut set, &l2);
        assert_eq!(
            set,
            ["nvidia/ad102/gsp.bin"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
    }

    fn target() -> TargetKernel {
        TargetKernel {
            kver: "6.17.1-300.fc43.x86_64".into(),
            modules: ["e1000e", "iwlwifi", "snd_hda_intel", "i915"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            builtin: ["hid_generic"].iter().map(|s| s.to_string()).collect(),
            aliases: vec![("pci:v000010DEd*sv*sd*bc03sc*i*".into(), "nouveau".into())],
            firmware: [
                "iwlwifi-cc-a0-77.ucode",
                "i915/tgl_dmc.bin",
                "intel-ucode/06-8c-01",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            layers_skipped: 0,
        }
    }

    fn pci(id: &str, module: &str, class: u32, modalias: &str) -> HostDevice {
        HostDevice {
            bus: "pci".into(),
            id: id.into(),
            modalias: Some(modalias.into()),
            driver: module.into(),
            module: Some(module.into()),
            pci_class: Some(class),
        }
    }

    #[test]
    fn assess_table() {
        struct Case {
            name: &'static str,
            devices: Vec<HostDevice>,
            modules: Vec<(&'static str, HostModule)>,
            vendor: Option<&'static str>,
            microcode: bool,
            want: Vec<(Severity, &'static str)>,
        }
        let none = || HostModule::default();
        let cases = vec![
            Case {
                name: "everything present",
                devices: vec![
                    pci("0000:00:1f.6", "e1000e", 0x020000, "pci:v00008086d000015B8"),
                    pci(
                        "0000:00:14.3",
                        "iwlwifi",
                        0x028000,
                        "pci:v00008086d0000A0F0",
                    ),
                ],
                modules: vec![
                    ("e1000e", none()),
                    (
                        "iwlwifi",
                        HostModule {
                            out_of_tree: false,
                            firmware_present: vec!["iwlwifi-cc-a0-77.ucode".into()],
                        },
                    ),
                ],
                vendor: Some("GenuineIntel"),
                microcode: true,
                want: vec![],
            },
            Case {
                name: "nvidia missing on the target: nouveau takes over, warn",
                devices: vec![pci(
                    "0000:01:00.0",
                    "nvidia",
                    0x030000,
                    "pci:v000010DEd00002484sv00001458sd00004037bc03sc00i00",
                )],
                modules: vec![(
                    "nvidia",
                    HostModule {
                        out_of_tree: true,
                        firmware_present: vec![],
                    },
                )],
                vendor: None,
                microcode: false,
                want: vec![(Severity::Warning, "`nouveau` driver takes over")],
            },
            Case {
                name: "in-tree driver replaced by another in-tree driver is a note",
                devices: vec![pci(
                    "0000:01:00.0",
                    "nvidiafb",
                    0x030000,
                    "pci:v000010DEd00002484sv00001458sd00004037bc03sc00i00",
                )],
                modules: vec![("nvidiafb", none())],
                vendor: None,
                microcode: false,
                want: vec![(Severity::Note, "`nouveau` supports the same")],
            },
            Case {
                name: "out-of-tree driver with no in-kernel fallback blocks",
                devices: vec![pci(
                    "0000:02:00.0",
                    "vboxdrv_pci",
                    0x088000,
                    "pci:v000080EEd0000CAFE",
                )],
                modules: vec![(
                    "vboxdrv_pci",
                    HostModule {
                        out_of_tree: true,
                        firmware_present: vec![],
                    },
                )],
                vendor: None,
                microcode: false,
                want: vec![(Severity::Blocking, "the out-of-tree driver `vboxdrv_pci`")],
            },
            Case {
                name: "missing Wi-Fi driver blocks; missing audio driver only warns",
                devices: vec![
                    pci(
                        "0000:03:00.0",
                        "rtw89_8852be",
                        0x028000,
                        "pci:v000010ECd0000B852",
                    ),
                    pci(
                        "0000:00:1f.3",
                        "snd_sof_pci",
                        0x040300,
                        "pci:v00008086d0000A0C8",
                    ),
                ],
                modules: vec![("rtw89_8852be", none()), ("snd_sof_pci", none())],
                vendor: None,
                microcode: false,
                want: vec![
                    (Severity::Blocking, "driver `rtw89_8852be`"),
                    (Severity::Warning, "driver `snd_sof_pci`"),
                ],
            },
            Case {
                name: "firmware: none shipped for a display driver blocks",
                devices: vec![pci(
                    "0000:00:02.0",
                    "i915",
                    0x030000,
                    "pci:v00008086d00009A49",
                )],
                modules: vec![(
                    "i915",
                    HostModule {
                        out_of_tree: false,
                        firmware_present: vec!["i915/adlp_dmc.bin".into()],
                    },
                )],
                vendor: None,
                microcode: false,
                want: vec![(Severity::Blocking, "ships none of the 1 firmware")],
            },
            Case {
                name: "firmware: some variants missing is a note",
                devices: vec![pci(
                    "0000:00:14.3",
                    "iwlwifi",
                    0x028000,
                    "pci:v00008086d0000A0F0",
                )],
                modules: vec![(
                    "iwlwifi",
                    HostModule {
                        out_of_tree: false,
                        firmware_present: vec![
                            "iwlwifi-cc-a0-77.ucode".into(),
                            "iwlwifi-so-a0-gf-a0-86.ucode".into(),
                        ],
                    },
                )],
                vendor: None,
                microcode: false,
                want: vec![(Severity::Note, "lacks 1 of the 2")],
            },
            Case {
                name: "microcode missing on the target warns",
                devices: vec![],
                modules: vec![],
                vendor: Some("AuthenticAMD"),
                microcode: true,
                want: vec![(Severity::Warning, "amd-ucode")],
            },
            Case {
                name: "built-in host drivers are not judged",
                devices: vec![HostDevice {
                    bus: "pci".into(),
                    id: "0000:00:1c.0".into(),
                    modalias: None,
                    driver: "pcieport".into(),
                    module: None,
                    pci_class: Some(0x060400),
                }],
                modules: vec![],
                vendor: None,
                microcode: false,
                want: vec![],
            },
        ];
        for c in cases {
            let host = HostHardware {
                devices: c.devices,
                modules: c
                    .modules
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                cpu_vendor: c.vendor.map(str::to_string),
                has_cpu_microcode: c.microcode,
            };
            let r = assess(&host, &target());
            assert_eq!(
                r.findings.len(),
                c.want.len(),
                "{}: {:#?}",
                c.name,
                r.findings
            );
            for (f, (sev, needle)) in r.findings.iter().zip(&c.want) {
                assert_eq!(f.severity, *sev, "{}: {f:?}", c.name);
                assert!(
                    f.detail.contains(needle) || f.subject.contains(needle),
                    "{}: {needle:?} not in {f:?}",
                    c.name
                );
            }
            assert_eq!(
                r.has_blocking(),
                c.want.iter().any(|(s, _)| *s == Severity::Blocking),
                "{}",
                c.name
            );
        }
    }

    #[test]
    fn partial_inventory_never_blocks() {
        let host = HostHardware {
            devices: vec![pci(
                "0000:03:00.0",
                "rtw89_8852be",
                0x028000,
                "pci:v000010ECd0000B852",
            )],
            modules: [("rtw89_8852be".to_string(), HostModule::default())].into(),
            ..Default::default()
        };
        let complete = assess(&host, &target());
        assert!(complete.has_blocking());
        let partial = assess(
            &host,
            &TargetKernel {
                layers_skipped: 1,
                ..target()
            },
        );
        assert!(!partial.has_blocking(), "{:#?}", partial.findings);
        assert!(
            partial
                .findings
                .iter()
                .any(|f| f.subject == "incomplete check")
        );
    }

    #[test]
    fn devices_sharing_a_missing_module_are_reported_once() {
        let host = HostHardware {
            devices: vec![
                pci(
                    "0000:05:00.0",
                    "mt7921e",
                    0x028000,
                    "pci:v000014C3d00000608",
                ),
                pci(
                    "0000:06:00.0",
                    "mt7921e",
                    0x028000,
                    "pci:v000014C3d00000616",
                ),
            ],
            modules: [("mt7921e".to_string(), HostModule::default())].into(),
            ..Default::default()
        };
        let r = assess(&host, &target());
        assert_eq!(r.findings.len(), 1);
        assert!(
            r.findings[0].subject.contains("0000:05:00.0")
                && r.findings[0].subject.contains("0000:06:00.0")
        );
        assert_eq!(r.devices_checked, 2);
    }

    /// A fixture `/sys` with a PCI device on a module, a duplicate symlink
    /// path to the same device, and a built-in-driver device.
    #[test]
    fn host_inventory_from_fixture_sysfs() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let mk = |p: &str| fs::create_dir_all(r.join(p)).unwrap();
        mk("sys/devices/pci0000:00/0000:00:14.3");
        mk("sys/devices/pci0000:00/0000:00:1c.0");
        mk("sys/bus/pci/drivers/iwlwifi");
        mk("sys/bus/pci/drivers/pcieport");
        mk("sys/module/iwlwifi");
        mk("sys/bus/pci/devices");
        mk("usr/lib/firmware/intel-ucode");
        mk("proc");
        let wifi = r.join("sys/devices/pci0000:00/0000:00:14.3");
        fs::write(wifi.join("modalias"), "pci:v00008086d0000A0F0\n").unwrap();
        fs::write(wifi.join("class"), "0x028000\n").unwrap();
        symlink(r.join("sys/bus/pci/drivers/iwlwifi"), wifi.join("driver")).unwrap();
        symlink(
            r.join("sys/module/iwlwifi"),
            r.join("sys/bus/pci/drivers/iwlwifi/module"),
        )
        .unwrap();
        fs::write(r.join("sys/module/iwlwifi/taint"), "\n").unwrap();
        let port = r.join("sys/devices/pci0000:00/0000:00:1c.0");
        fs::write(port.join("class"), "0x060400\n").unwrap();
        symlink(r.join("sys/bus/pci/drivers/pcieport"), port.join("driver")).unwrap();
        symlink(&wifi, r.join("sys/bus/pci/devices/0000:00:14.3")).unwrap();
        symlink(&port, r.join("sys/bus/pci/devices/0000:00:1c.0")).unwrap();
        // The same device reached a second time through another bus listing.
        mk("sys/bus/virtio/devices");
        symlink(&wifi, r.join("sys/bus/virtio/devices/dup")).unwrap();
        fs::write(r.join("usr/lib/firmware/iwlwifi-cc-a0-77.ucode.xz"), "").unwrap();
        fs::write(
            r.join("proc/cpuinfo"),
            "processor\t: 0\nvendor_id\t: GenuineIntel\n",
        )
        .unwrap();

        let fw = [r.join("usr/lib/firmware")];
        let hw = read_host_hardware_from(r, &fw, &|m| {
            assert_eq!(m, "iwlwifi");
            vec![
                "iwlwifi-cc-a0-77.ucode".into(),
                "iwlwifi-so-a0-gf-a0-86.ucode".into(),
            ]
        });
        assert_eq!(hw.devices.len(), 2, "{:#?}", hw.devices);
        let w = hw.devices.iter().find(|d| d.id == "0000:00:14.3").unwrap();
        assert_eq!(w.module.as_deref(), Some("iwlwifi"));
        assert_eq!(w.pci_class, Some(0x028000));
        assert_eq!(w.modalias.as_deref(), Some("pci:v00008086d0000A0F0"));
        let p = hw.devices.iter().find(|d| d.id == "0000:00:1c.0").unwrap();
        assert_eq!(p.module, None);
        assert_eq!(p.driver, "pcieport");
        assert_eq!(
            hw.modules["iwlwifi"].firmware_present,
            vec!["iwlwifi-cc-a0-77.ucode".to_string()]
        );
        assert!(!hw.modules["iwlwifi"].out_of_tree);
        assert_eq!(hw.cpu_vendor.as_deref(), Some("GenuineIntel"));
        assert!(hw.has_cpu_microcode);
    }

    #[test]
    fn gate_decision_table() {
        let blocking = HardwareReport {
            findings: vec![Finding {
                severity: Severity::Blocking,
                subject: "x".into(),
                detail: "y".into(),
            }],
            ..Default::default()
        };
        let warning = HardwareReport {
            findings: vec![Finding {
                severity: Severity::Warning,
                subject: "x".into(),
                detail: "y".into(),
            }],
            ..Default::default()
        };
        for (report, accept, dry, want) in [
            (&warning, false, false, GateDecision::Proceed),
            (&blocking, false, false, GateDecision::Refuse),
            (&blocking, true, false, GateDecision::ProceedAccepted),
            (&blocking, false, true, GateDecision::WouldRefuse),
            (&blocking, true, true, GateDecision::ProceedAccepted),
        ] {
            assert_eq!(
                decide(report, accept, dry),
                want,
                "accept={accept} dry={dry}"
            );
        }
    }

    #[test]
    fn render_says_ok_or_lists_findings() {
        let ok = render(
            &HardwareReport {
                target_kernel: Some("6.1".into()),
                devices_checked: 3,
                findings: vec![],
            },
            "img",
        );
        assert!(ok.contains("OK:") && ok.contains("Devices checked: 3"));
        let bad = render(
            &HardwareReport {
                target_kernel: Some("6.1".into()),
                devices_checked: 1,
                findings: vec![Finding {
                    severity: Severity::Blocking,
                    subject: "PCI x".into(),
                    detail: "gone".into(),
                }],
            },
            "img",
        );
        assert!(bad.contains("[BLOCKING] PCI x: gone"));
        assert!(render_unknown("img", "no kernel modules found").contains("could NOT be checked"));
    }
}
