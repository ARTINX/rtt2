//! Locate the SEGGER RTT control block from an ELF firmware image.
//!
//! Keil (armlink) emits `.axf`, armgcc/armclang emit `.elf`/`.out`; all are
//! ELF files, so the `_SEGGER_RTT` symbol is read with `object` regardless of
//! the toolchain. Linker `.map` files are not parsed.
use object::{Object, ObjectSymbol};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 8;
/// Firmware ELFs with full debug info are large, but not this large.
const MAX_FIRMWARE_BYTES: u64 = 256 * 1024 * 1024;
const FIRMWARE_EXTENSIONS: [&str; 3] = ["elf", "axf", "out"];
/// SEGGER emits `_SEGGER_RTT`; a few ports drop the leading underscore.
const RTT_SYMBOLS: [&[u8]; 2] = [b"_SEGGER_RTT", b"SEGGER_RTT"];

struct Candidate {
    path: PathBuf,
    address: u64,
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
}

fn is_firmware_file(path: &Path) -> bool {
    extension(path).is_some_and(|ext| FIRMWARE_EXTENSIONS.contains(&ext.as_str()))
}

fn collect_files(root: &Path, depth: usize, found: &mut Vec<PathBuf>) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if depth == 0 => {
            return Err(format!("无法读取 {}: {error}", root.display()));
        }
        // A permission error deep inside a project must not hide a board
        // directory that is readable.
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !matches!(name.as_str(), ".git" | "target" | "node_modules" | ".venv") {
                collect_files(&path, depth + 1, found)?;
            }
        } else if is_firmware_file(&path) {
            found.push(path);
        }
    }
    Ok(())
}

fn parse_elf(path: &Path) -> Option<u64> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() == 0 || metadata.len() > MAX_FIRMWARE_BYTES {
        return None;
    }
    let data = fs::read(path).ok()?;
    let file = object::File::parse(data.as_slice()).ok()?;
    if file.format() != object::BinaryFormat::Elf {
        return None;
    }
    let mut fallback = None;
    for symbol in file.symbols() {
        let Ok(name) = symbol.name_bytes() else {
            continue;
        };
        let preferred = name == RTT_SYMBOLS[0];
        if !preferred && name != RTT_SYMBOLS[1] {
            continue;
        }
        if !symbol.is_definition() {
            continue;
        }
        let address = symbol.address();
        if address == 0 {
            continue;
        }
        if preferred {
            return Some(address);
        }
        fallback = fallback.or(Some(address));
    }
    fallback
}

/// Find the RTT control block address in a project directory or a single
/// firmware ELF.
///
/// Directories are searched recursively for `.elf`/`.axf`/`.out` files.
/// Distinct addresses are rejected, so choosing a repository root cannot
/// silently select another board's firmware.
pub fn find_rtt_address(project: &Path) -> Result<u64, String> {
    let metadata = fs::metadata(project)
        .map_err(|_| format!("工程路径或固件文件不存在: {}", project.display()))?;
    let mut files = Vec::new();
    if metadata.is_file() {
        if !is_firmware_file(project) {
            let map = extension(project).as_deref() == Some("map");
            return Err(format!(
                "不支持的固件文件 {}{}",
                project.display(),
                if map {
                    "；不再解析 .map，请提供 Keil 生成的 .axf 或 armgcc 生成的 .elf"
                } else {
                    "；请指定 .elf/.axf/.out，或包含它们的目录"
                }
            ));
        }
        files.push(project.to_path_buf());
    } else if metadata.is_dir() {
        collect_files(project, 0, &mut files)?;
        if files.is_empty() {
            return Err(format!(
                "{} 下没有找到固件文件（.elf/.axf/.out），请先编译固件",
                project.display()
            ));
        }
    } else {
        return Err(format!("工程路径不是目录或固件文件: {}", project.display()));
    }

    let candidates: Vec<_> = files
        .iter()
        .filter_map(|path| {
            parse_elf(path).map(|address| Candidate {
                path: path.clone(),
                address,
            })
        })
        .collect();
    let Some(first) = candidates.first() else {
        return Err(format!(
            "在 {} 个固件文件中没有找到 _SEGGER_RTT 符号（ELF 可能已被 strip 或过大）；可手动指定 RTT 地址（CLI 的 --address 或界面的“RTT 地址”）",
            files.len()
        ));
    };
    if candidates.iter().any(|c| c.address != first.address) {
        let paths = candidates
            .iter()
            .map(|c| format!("{} = {:#010x}", c.path.display(), c.address))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "找到多个不同 RTT 地址，请选择具体板卡的输出目录或固件文件: {paths}"
        ));
    }
    Ok(first.address)
}

/// Parse a user supplied control block address.
///
/// `0x`-prefixed input is hexadecimal; plain digits are decimal; anything else
/// (`nm`/`readelf` style output such as `20009fa4`) is read as hexadecimal.
pub fn parse_address(input: &str) -> Result<u64, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("RTT 地址不能为空".into());
    }
    let hex = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"));
    let parsed = match hex {
        Some(digits) => u64::from_str_radix(&digits.replace('_', ""), 16),
        None if text.bytes().all(|byte| byte.is_ascii_digit()) => text.parse::<u64>(),
        None => u64::from_str_radix(&text.replace('_', ""), 16),
    };
    let address =
        parsed.map_err(|_| format!("无法解析 RTT 地址 {input}；十六进制请写作 0x20009FA4"))?;
    if address == 0 {
        return Err("RTT 地址不能为 0".into());
    }
    if address % 4 != 0 {
        return Err(format!("RTT 地址 0x{address:X} 不是 4 字节对齐"));
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rtt2-address-{}-{name}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Writes a relocatable ELF with one data symbol, as armgcc/Keil would.
    fn write_elf(dir: &Path, name: &str, symbol: &str, value: u64) {
        use object::write::{Object, Symbol, SymbolSection};
        use object::{
            Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind, SymbolScope,
        };
        let mut object = Object::new(BinaryFormat::Elf, Architecture::Arm, Endianness::Little);
        let section = object.add_section(Vec::new(), b".data".to_vec(), object::SectionKind::Data);
        object.set_section_data(section, vec![0u8; 0x1000], 4);
        object.add_symbol(Symbol {
            name: symbol.as_bytes().to_vec(),
            value,
            size: 168,
            kind: SymbolKind::Data,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        });
        fs::write(dir.join(name), object.write().unwrap()).unwrap();
    }

    #[test]
    fn parses_elf_symbols_and_accepts_a_single_file() {
        let root = temp_dir("elf");
        write_elf(&root, "firmware.elf", "_SEGGER_RTT", 0x9f4);
        assert_eq!(find_rtt_address(&root).unwrap(), 0x9f4);
        assert_eq!(find_rtt_address(&root.join("firmware.elf")).unwrap(), 0x9f4);

        let nested = root.join("MDK-ARM");
        fs::create_dir_all(&nested).unwrap();
        write_elf(&nested, "Kit-C.axf", "SEGGER_RTT", 0x1200);
        // Keil `.axf` and armgcc `.elf` are both ELF, but distinct addresses
        // stay ambiguous.
        let error = find_rtt_address(&root).unwrap_err();
        assert!(error.contains("多个不同"), "{error}");
        fs::remove_file(nested.join("Kit-C.axf")).unwrap();
        write_elf(&nested, "app.out", "SEGGER_RTT", 0x9f4);
        assert_eq!(find_rtt_address(&root).unwrap(), 0x9f4);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn map_files_are_not_an_address_source() {
        let root = temp_dir("map");
        fs::write(
            root.join("Kit-C.map"),
            "Image Symbol Table\n    _SEGGER_RTT  0x20009fa4   Data  168 segger_rtt.o(.bss)\n",
        )
        .unwrap();
        let error = find_rtt_address(&root).unwrap_err();
        assert!(error.contains("没有找到固件文件"), "{error}");
        let error = find_rtt_address(&root.join("Kit-C.map")).unwrap_err();
        assert!(error.contains("不再解析 .map"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_symbol_reports_the_symbol_name() {
        let root = temp_dir("no-symbol");
        write_elf(&root, "firmware.elf", "application_entry", 0x9f4);
        let error = find_rtt_address(&root).unwrap_err();
        assert!(error.contains("_SEGGER_RTT"), "{error}");
        assert!(error.contains("--address"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_manual_addresses() {
        for input in ["0x20009fa4", "0X20009FA4", "20009fa4", "0x2000_9fa4"] {
            assert_eq!(parse_address(input).unwrap(), 0x20009fa4, "{input}");
        }
        assert_eq!(parse_address("536870912").unwrap(), 536_870_912);
        for invalid in ["", "0", "0x0", "0x20009fa5", "garbage", "0xZZ"] {
            assert!(parse_address(invalid).is_err(), "{invalid}");
        }
    }
}
