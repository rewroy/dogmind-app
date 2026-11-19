//! What a report says of the PC beside what Straydog says of its GPU and memory: the CPU, the GPU's PCIe link, Windows' memory compression and
//! the page file. Two RTX 4060 PCs streamed the same number of experts for a prompt and differed fivefold, and their reports had nothing to tell
//! the two apart by. Each fact is read as the system has it now, and one that cannot be read is said to be unknown.

use std::ffi::c_void;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

const GIB: f64 = (1u64 << 30) as f64;

/// A GPU's PCIe link as NVML says it: the generation and the lanes it runs at now, and the most the GPU and the slot allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub name: String,
    pub now: (u32, u32),
    pub most: (u32, u32),
}

/// The page file as the system has it: each file's name, size and use now and at its most (bytes), and how it is set (the registry's
/// PagingFiles: `?:\pagefile.sys` for a size Windows chooses).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageFiles {
    pub files: Vec<(String, u64, u64, u64)>,
    pub setting: Vec<String>,
}

/// The report's lines for these facts, in the report's words.
pub fn lines(cpu: Option<&str>, links: Option<&[Link]>, compression: Option<(bool, u64)>, pages: Option<&PageFiles>) -> String {
    let mut text = format!("CPU: {}\n", cpu.unwrap_or("unknown"));
    match links {
        Some([]) => text.push_str("GPU link: no NVIDIA GPU\n"),
        Some(links) => {
            for link in links {
                text.push_str(&format!("GPU link: PCIe gen {} x{} now, at most gen {} x{} ({}; an idle GPU may run a lower generation)\n",
                                       link.now.0, link.now.1, link.most.0, link.most.1, link.name));
            }
        }
        None => text.push_str("GPU link: unknown (NVML not there)\n"),
    }
    text.push_str(&match compression {
        Some((true, bytes)) => format!("Memory compression: on, {:.2} GiB compressed now\n", bytes as f64 / GIB),
        Some((false, _)) => "Memory compression: off\n".to_string(),
        None => "Memory compression: unknown\n".to_string(),
    });
    match pages {
        Some(pages) => {
            let setting = if pages.setting.is_empty() { "not set".to_string() } else { pages.setting.join(", ") };
            let sized = if pages.setting.iter().any(|line| line.starts_with("?:")) { " (sized by Windows)" } else { "" };
            if pages.files.is_empty() {
                text.push_str(&format!("Page file: none in use; set as {setting}{sized}\n"));
            }
            for (name, size, used, most) in &pages.files {
                text.push_str(&format!("Page file: {name} {:.1} GiB, {:.1} GiB in use, at most {:.1} GiB; set as {setting}{sized}\n",
                                       *size as f64 / GIB, *used as f64 / GIB, *most as f64 / GIB));
            }
        }
        None => text.push_str("Page file: unknown\n"),
    }
    text
}

/// The report's lines, read now.
pub fn read() -> String {
    // SAFETY: each reader asks the system with buffers as long as it says they must be.
    let (cpu, links, compression, pages) = unsafe { (cpu_name(), gpu_links(), memory_compression(), page_files()) };
    lines(cpu.as_deref(), links.as_deref(), compression, pages.as_ref())
}

/// A registry value under HKLM: a string, or a multi-string's strings.
unsafe fn registry_strings(key: &str, value: &str) -> Option<Vec<String>> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ};
    let (key, value) = (wide(key), wide(value));
    let kinds = RRF_RT_REG_SZ | RRF_RT_REG_MULTI_SZ;
    let mut bytes = 0u32;
    if RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), kinds, std::ptr::null_mut(), std::ptr::null_mut(), &mut bytes) != 0 {
        return None;
    }
    let mut buffer = vec![0u16; bytes as usize / 2 + 2];
    if RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), kinds, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut bytes) != 0 {
        return None;
    }
    let units = &buffer[..(bytes as usize / 2).min(buffer.len())];
    Some(units.split(|&unit| unit == 0).map(String::from_utf16_lossy).map(|text| text.trim().to_string()).filter(|text| !text.is_empty()).collect())
}

unsafe fn cpu_name() -> Option<String> {
    registry_strings(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString")?.into_iter().next()
}

/// Each NVIDIA GPU's link, from NVML in the system folder (the driver's): none where there is no NVML, an empty list where it has no GPU.
unsafe fn gpu_links() -> Option<Vec<Link>> {
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    type Init = unsafe extern "C" fn() -> i32;
    type Count = unsafe extern "C" fn(*mut u32) -> i32;
    type Handle = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
    type Name = unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> i32;
    type Value = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;
    let library = LoadLibraryExW(wide("nvml.dll").as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
    if library.is_null() {
        return None;
    }
    let function = |name: &std::ffi::CStr| GetProcAddress(library, name.as_ptr().cast());
    let found = (|| {
        let init: Init = std::mem::transmute(function(c"nvmlInit_v2")?);
        let shutdown: Init = std::mem::transmute(function(c"nvmlShutdown")?);
        let count: Count = std::mem::transmute(function(c"nvmlDeviceGetCount_v2")?);
        let handle: Handle = std::mem::transmute(function(c"nvmlDeviceGetHandleByIndex_v2")?);
        let name: Name = std::mem::transmute(function(c"nvmlDeviceGetName")?);
        let values: [Value; 4] = [
            std::mem::transmute(function(c"nvmlDeviceGetCurrPcieLinkGeneration")?),
            std::mem::transmute(function(c"nvmlDeviceGetCurrPcieLinkWidth")?),
            std::mem::transmute(function(c"nvmlDeviceGetMaxPcieLinkGeneration")?),
            std::mem::transmute(function(c"nvmlDeviceGetMaxPcieLinkWidth")?),
        ];
        if init() != 0 {
            return None;
        }
        let mut links = Vec::new();
        let mut devices = 0u32;
        if count(&mut devices) == 0 {
            for index in 0..devices {
                let mut device = std::ptr::null_mut();
                if handle(index, &mut device) != 0 {
                    continue;
                }
                let mut text = [0u8; 96];
                let named = if name(device, text.as_mut_ptr(), text.len() as u32) == 0 {
                    String::from_utf8_lossy(&text[..text.iter().position(|&byte| byte == 0).unwrap_or(text.len())]).into_owned()
                } else {
                    format!("GPU {index}")
                };
                let mut read = [0u32; 4];
                for (value, slot) in values.iter().zip(read.iter_mut()) {
                    if value(device, slot) != 0 {
                        *slot = 0;
                    }
                }
                links.push(Link { name: named, now: (read[0], read[1]), most: (read[2], read[3]) });
            }
        }
        shutdown();
        Some(links)
    })();
    FreeLibrary(library);
    found
}

/// `NtQuerySystemInformation` of `class` into a buffer that grows until it holds what the system has: the buffer as the system wrote it (the
/// addresses in it point into it, so it is not copied), in u64s so that its structures are aligned.
unsafe fn system_information(class: u32) -> Option<Vec<u64>> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    type Query = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;
    let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
    if ntdll.is_null() {
        return None;
    }
    let query: Query = std::mem::transmute(GetProcAddress(ntdll, c"NtQuerySystemInformation".as_ptr().cast())?);
    let mut size = 1u32 << 16;
    for _ in 0..8 {
        let mut buffer = vec![0u64; size as usize / 8 + 1];
        let mut needed = 0u32;
        let status = query(class, buffer.as_mut_ptr().cast(), size, &mut needed);
        if status == 0 {
            return Some(buffer);
        }
        // (STATUS_INFO_LENGTH_MISMATCH: a larger buffer)
        if status as u32 != 0xC000_0004 {
            return None;
        }
        size = needed.max(size * 2).saturating_add(1 << 14);
    }
    None
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..at + 4).map(|slice| u32::from_le_bytes(slice.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    bytes.get(at..at + 8).map(|slice| u64::from_le_bytes(slice.try_into().unwrap()))
}

/// A UNICODE_STRING at `at` (its length in bytes, then its buffer's address 8 bytes on), read where the address lies inside `bytes`, whose own
/// address is `base`.
fn unicode_at(bytes: &[u8], base: usize, at: usize) -> Option<String> {
    let length = bytes.get(at..at + 2).map(|slice| u16::from_le_bytes(slice.try_into().unwrap()))? as usize;
    let address = u64_at(bytes, at + 8)? as usize;
    if length == 0 || address < base {
        return Some(String::new());
    }
    let offset = address - base;
    let units: Vec<u16> = bytes.get(offset..offset + length)?.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    Some(String::from_utf16_lossy(&units))
}

/// Whether Windows compresses memory, and how much its store holds now: the "Memory Compression" process and its working set
/// (SystemProcessInformation, x64: the name at 0x38, the working set at 0x90).
unsafe fn memory_compression() -> Option<(bool, u64)> {
    let words = system_information(5)?;
    let bytes = std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 8);
    let base = words.as_ptr() as usize;
    let mut at = 0usize;
    loop {
        let next = u32_at(bytes, at)? as usize;
        if unicode_at(bytes, base, at + 0x38)? == "Memory Compression" {
            return Some((true, u64_at(bytes, at + 0x90)?));
        }
        if next == 0 {
            return Some((false, 0));
        }
        at += next;
    }
}

/// The page files in use (SystemPageFileInformation: sizes in pages of 4 KiB, the name at 16) and how they are set.
unsafe fn page_files() -> Option<PageFiles> {
    const PAGE: u64 = 4096;
    let setting = registry_strings(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management", "PagingFiles").unwrap_or_default();
    let words = system_information(18)?;
    let bytes = std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 8);
    let base = words.as_ptr() as usize;
    let mut files = Vec::new();
    let mut at = 0usize;
    loop {
        let next = u32_at(bytes, at)? as usize;
        let (size, used, most) = (u32_at(bytes, at + 4)? as u64, u32_at(bytes, at + 8)? as u64, u32_at(bytes, at + 12)? as u64);
        if size > 0 {
            files.push((unicode_at(bytes, base, at + 16)?.trim_start_matches(r"\??\").to_string(), size * PAGE, used * PAGE, most * PAGE));
        }
        if next == 0 {
            break;
        }
        at += next;
    }
    Some(PageFiles { files, setting })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report's lines say each fact, an unknown one as unknown, and what a link and a page file setting mean.
    #[test]
    fn the_lines_say_each_fact_and_what_is_not_known() {
        let link = Link { name: "NVIDIA GeForce RTX 4060".into(), now: (4, 8), most: (4, 8) };
        let pages = PageFiles { files: vec![(r"C:\pagefile.sys".into(), 10 << 30, 3 << 29, 2 << 30)], setting: vec![r"?:\pagefile.sys".into()] };
        let text = lines(Some("AMD Ryzen 7 7700"), Some(&[link]), Some((true, 3 << 29)), Some(&pages));
        assert_eq!(text, "CPU: AMD Ryzen 7 7700\n\
                          GPU link: PCIe gen 4 x8 now, at most gen 4 x8 (NVIDIA GeForce RTX 4060; an idle GPU may run a lower generation)\n\
                          Memory compression: on, 1.50 GiB compressed now\n\
                          Page file: C:\\pagefile.sys 10.0 GiB, 1.5 GiB in use, at most 2.0 GiB; set as ?:\\pagefile.sys (sized by Windows)\n");
        let none = lines(None, None, None, None);
        assert_eq!(none, "CPU: unknown\nGPU link: unknown (NVML not there)\nMemory compression: unknown\nPage file: unknown\n");
        let off = lines(Some("x"), Some(&[]), Some((false, 0)), Some(&PageFiles { files: Vec::new(), setting: vec![r"C:\pagefile.sys 4096 8192".into()] }));
        assert!(off.contains("GPU link: no NVIDIA GPU\n") && off.contains("Memory compression: off\n") && off.contains("Page file: none in use; set as C:\\pagefile.sys 4096 8192\n"), "{off}");
    }

    /// On this PC each fact is read: a CPU's name, the page file's setting, and no fact unknown that Windows has.
    #[test]
    fn this_pc_s_facts_are_read() {
        let text = read();
        assert!(!text.contains("CPU: unknown") && !text.contains("Memory compression: unknown") && !text.contains("Page file: unknown"), "{text}");
    }
}
