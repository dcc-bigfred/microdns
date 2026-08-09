//! Build and release metadata for microdns.
//!
//! - **Build-time:** `build_commit` / `build_time` from `build.rs` env.
//! - **Post-build:** optional ELF section `.microdns.version` JSON
//!   `{"version":"v1.2.3","commit":"abc1234"}` injected by release retag.
//!
//! Simplified vs microinit: only reads the section from `current_exe`
//! (no Android `dladdr` library path).

use std::fs;
use std::path::Path;
use std::sync::OnceLock;

/// ELF section name (must match org `.github` `inject-elf-version.sh` section arg).
pub const SECTION_NAME: &str = ".microdns.version";

/// Public version payload returned by `microdns info` / `--version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// Release tag from ELF section, or `"dev"` when absent.
    pub version: String,
    /// Short/full commit of the release tag (ELF); empty when absent.
    pub tag_commit: String,
    /// CI / build-time git SHA.
    pub build_commit: String,
    /// UTC build timestamp (ISO-8601) when available.
    pub build_time: String,
}

#[derive(Debug, serde::Deserialize)]
struct SectionPayload {
    #[serde(default)]
    version: String,
    #[serde(default)]
    commit: String,
}

static INFO: OnceLock<Info> = OnceLock::new();

/// Returns process version info (cached).
#[must_use]
pub fn info() -> Info {
    INFO.get_or_init(load).clone()
}

fn load() -> Info {
    let mut out = Info {
        version: "dev".into(),
        tag_commit: String::new(),
        build_commit: option_env!("MICRODNS_GIT_COMMIT")
            .unwrap_or("unknown")
            .into(),
        build_time: option_env!("MICRODNS_BUILD_TIME").unwrap_or("").into(),
    };
    if let Ok(path) = std::env::current_exe() {
        if let Some((v, c)) = read_section_from(&path) {
            if !v.is_empty() {
                out.version = v;
            }
            if !c.is_empty() {
                out.tag_commit = c;
            }
        }
    }
    out
}

/// Read `.microdns.version` from an ELF path. Public for tests.
#[must_use]
pub fn read_section_from(path: &Path) -> Option<(String, String)> {
    let data = fs::read(path).ok()?;
    let raw = elf_section_data(&data, SECTION_NAME)?;
    if raw.is_empty() {
        return None;
    }
    if let Ok(payload) = serde_json::from_slice::<SectionPayload>(raw) {
        let v = payload.version.trim().to_string();
        let c = payload.commit.trim().to_string();
        if v.is_empty() && c.is_empty() {
            return None;
        }
        return Some((v, c));
    }
    let v = String::from_utf8_lossy(raw).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some((v, String::new()))
    }
}

fn elf_section_data<'a>(data: &'a [u8], name: &str) -> Option<&'a [u8]> {
    if data.len() < 16 || &data[0..4] != b"\x7fELF" {
        return None;
    }
    let class = data[4]; // 1=32, 2=64
    let endian = data[5]; // 1=LE, 2=BE
    let le = endian == 1;
    if !le && endian != 2 {
        return None;
    }

    match class {
        1 => elf32_section(data, name, le),
        2 => elf64_section(data, name, le),
        _ => None,
    }
}

fn u16_at(data: &[u8], off: usize, le: bool) -> Option<u16> {
    let b = data.get(off..off + 2)?;
    Some(if le {
        u16::from_le_bytes([b[0], b[1]])
    } else {
        u16::from_be_bytes([b[0], b[1]])
    })
}

fn u32_at(data: &[u8], off: usize, le: bool) -> Option<u32> {
    let b = data.get(off..off + 4)?;
    Some(if le {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    } else {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    })
}

fn u64_at(data: &[u8], off: usize, le: bool) -> Option<u64> {
    let b = data.get(off..off + 8)?;
    Some(if le {
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    } else {
        u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    })
}

fn elf64_section<'a>(data: &'a [u8], name: &str, le: bool) -> Option<&'a [u8]> {
    let shoff = u64_at(data, 40, le)? as usize;
    let shentsize = u16_at(data, 58, le)? as usize;
    let shnum = u16_at(data, 60, le)? as usize;
    let shstrndx = u16_at(data, 62, le)? as usize;
    if shentsize < 64 || shnum == 0 || shstrndx >= shnum {
        return None;
    }
    let str_hdr = shoff.checked_add(shstrndx.checked_mul(shentsize)?)?;
    let str_off = u64_at(data, str_hdr + 24, le)? as usize;
    let str_size = u64_at(data, str_hdr + 32, le)? as usize;
    let strtab = data.get(str_off..str_off.checked_add(str_size)?)?;

    for i in 0..shnum {
        let hdr = shoff.checked_add(i.checked_mul(shentsize)?)?;
        let name_off = u32_at(data, hdr, le)? as usize;
        let sec_name = cstr_at(strtab, name_off)?;
        if sec_name != name {
            continue;
        }
        let offset = u64_at(data, hdr + 24, le)? as usize;
        let size = u64_at(data, hdr + 32, le)? as usize;
        return data.get(offset..offset.checked_add(size)?);
    }
    None
}

fn elf32_section<'a>(data: &'a [u8], name: &str, le: bool) -> Option<&'a [u8]> {
    let shoff = u32_at(data, 32, le)? as usize;
    let shentsize = u16_at(data, 46, le)? as usize;
    let shnum = u16_at(data, 48, le)? as usize;
    let shstrndx = u16_at(data, 50, le)? as usize;
    if shentsize < 40 || shnum == 0 || shstrndx >= shnum {
        return None;
    }
    let str_hdr = shoff.checked_add(shstrndx.checked_mul(shentsize)?)?;
    let str_off = u32_at(data, str_hdr + 16, le)? as usize;
    let str_size = u32_at(data, str_hdr + 20, le)? as usize;
    let strtab = data.get(str_off..str_off.checked_add(str_size)?)?;

    for i in 0..shnum {
        let hdr = shoff.checked_add(i.checked_mul(shentsize)?)?;
        let name_off = u32_at(data, hdr, le)? as usize;
        let sec_name = cstr_at(strtab, name_off)?;
        if sec_name != name {
            continue;
        }
        let offset = u32_at(data, hdr + 16, le)? as usize;
        let size = u32_at(data, hdr + 20, le)? as usize;
        return data.get(offset..offset.checked_add(size)?);
    }
    None
}

fn cstr_at(data: &[u8], off: usize) -> Option<&str> {
    let slice = data.get(off..)?;
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    std::str::from_utf8(&slice[..end]).ok()
}

/// Hostname for daemon info / DNS-SD instance names.
#[must_use]
pub fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| fs::read_to_string("/etc/hostname"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Format [`Info`] for CLI output.
pub fn format_info(info: &Info) -> String {
    let mut lines = vec![
        format!("version: {}", info.version),
        format!("build_commit: {}", info.build_commit),
    ];
    if !info.tag_commit.is_empty() {
        lines.push(format!("tag_commit: {}", info.tag_commit));
    }
    if !info.build_time.is_empty() {
        lines.push(format!("build_time: {}", info.build_time));
    }
    lines.push(format!("hostname: {}", hostname()));
    lines.join("\n")
}
