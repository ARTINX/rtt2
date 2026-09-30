use std::fs;
use std::path::{Path, PathBuf};

fn map_files(root: &Path, depth: usize, found: &mut Vec<PathBuf>) -> Result<(), String> {
    if depth > 8 {
        return Ok(());
    }
    let entries = fs::read_dir(root).map_err(|e| format!("无法读取 {}: {e}", root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !matches!(name.as_str(), ".git" | "target" | "node_modules" | ".venv") {
                map_files(&path, depth + 1, found)?;
            }
        } else if path
            .extension()
            .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("map"))
        {
            found.push(path);
        }
    }
    Ok(())
}

fn parse_map(path: &Path) -> Option<u64> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut in_symbols = false;
    let mut section_address = None;
    for line in text.lines() {
        if line.contains("Image Symbol Table") {
            in_symbols = true;
            continue;
        }
        if !in_symbols {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 2 {
            continue;
        }
        let address = fields[1]
            .strip_prefix("0x")
            .and_then(|hex| u64::from_str_radix(hex, 16).ok());
        if fields[0] == "_SEGGER_RTT" {
            return address;
        }
        if line.contains("segger_rtt.o(.bss)") && fields[0] == ".bss" {
            section_address = address;
        }
    }
    section_address
}

/// Find the RTT control block by searching Keil MAP files below a project path.
/// Distinct addresses are rejected, so choosing the repository root cannot
/// silently select a different board's MAP file.
pub fn find_rtt_address(project: &Path) -> Result<u64, String> {
    if !project.is_dir() {
        return Err(format!("工程路径不是目录: {}", project.display()));
    }
    let mut files = Vec::new();
    map_files(project, 0, &mut files)?;
    if files.is_empty() {
        return Err(format!(
            "{} 下没有找到 .map 文件，请先编译固件",
            project.display()
        ));
    }
    let mut candidates = Vec::new();
    for file in files {
        if let Some(address) = parse_map(&file) {
            candidates.push((file, address));
        }
    }
    if candidates.is_empty() {
        return Err("MAP 文件中没有 _SEGGER_RTT 符号".to_string());
    }
    let first = candidates[0].1;
    if candidates.iter().any(|(_, address)| *address != first) {
        let paths = candidates
            .iter()
            .map(|(path, address)| format!("{} = {address:#010x}", path.display()))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "找到多个不同 RTT 地址，请选择具体板卡的输出目录: {paths}"
        ));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keil_symbol_and_rejects_ambiguity() {
        let root = std::env::temp_dir().join(format!("rtt2-address-{}", std::process::id()));
        let one = root.join("one");
        let two = root.join("two");
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&two).unwrap();
        let text =
            "Image Symbol Table\n    _SEGGER_RTT  0x20009fa4   Data  168 segger_rtt.o(.bss)\n";
        fs::write(one.join("Kit-C.map"), text).unwrap();
        assert_eq!(find_rtt_address(&one).unwrap(), 0x20009fa4);
        fs::write(
            two.join("Kit-miao.map"),
            text.replace("20009fa4", "20002000"),
        )
        .unwrap();
        assert!(find_rtt_address(&root).unwrap_err().contains("多个不同"));
        fs::remove_dir_all(root).unwrap();
    }
}
