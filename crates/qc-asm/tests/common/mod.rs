//! Opt-in external decoder: standalone selache `selas` + `seldump` from `$SELACHE`.

use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

/// Decodes our section-2 bytes with selache and returns `(address, hex, text)` lines.
///
/// selas assembles a NOP shell; we replace its bytes with OUR emitted bytes (converted
/// to the big-endian parcels the ELF oracle reads) and mark the section as VISA.
pub fn selache_decode(bytes: &[u8]) -> Vec<(String, String, String)> {
    let tools = PathBuf::from(std::env::var_os("SELACHE").expect("set SELACHE"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("qc-asm-visa-oracle-{}-{nonce}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let source = directory.join("visa.asm");
    let object = directory.join("visa.doj");
    fs::write(
        &source,
        format!(
            ".section/pm qc_asm;\n{}",
            "nop;\n".repeat(bytes.len().div_ceil(6))
        ),
    )
    .unwrap();
    let output = Command::new(tools.join("selas"))
        .args(["-proc", "ADSP-SC589", "-o"])
        .arg(&object)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "selas: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut elf = fs::read(&object).unwrap();
    assert_eq!(&elf[..6], b"\x7fELF\x01\x01");
    let read_u32 = |at: usize| u32::from_le_bytes(elf[at..at + 4].try_into().unwrap()) as usize;
    let read_u16 = |at: usize| u16::from_le_bytes(elf[at..at + 2].try_into().unwrap()) as usize;
    let table = read_u32(32);
    let size = read_u16(46);
    let count = read_u16(48);
    let names_header = table + read_u16(50) * size;
    let names_at = read_u32(names_header + 16);
    let names_size = read_u32(names_header + 20);
    let names = &elf[names_at..names_at + names_size];
    let header = (0..count)
        .map(|n| table + n * size)
        .find(|&header| {
            let at = read_u32(header);
            names[at..].split(|&b| b == 0).next().unwrap() == b"qc_asm"
        })
        .unwrap();
    let code_at = read_u32(header + 16);
    let allocated = read_u32(header + 20);
    assert!(allocated >= bytes.len());
    // ELF oracle consumes big-endian parcels. Convert our ACTUAL section-2
    // bytes, not an independent encoder or the expected numeric vectors.
    for (to, from) in elf[code_at..code_at + bytes.len()]
        .chunks_exact_mut(2)
        .zip(bytes.chunks_exact(2))
    {
        to.copy_from_slice(&[from[1], from[0]]);
    }
    elf[header + 20..header + 24].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    // Seldump's ELF transport identifies VISA sections by sh_entsize=1.
    // This metadata is an oracle convention, not a QC section layout.
    elf[header + 36..header + 40].copy_from_slice(&1_u32.to_le_bytes());
    fs::write(&object, elf).unwrap();
    let output = Command::new(tools.join("seldump"))
        .args(["-ni", "qc_asm"])
        .arg(&object)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "seldump: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(directory).unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let address = fields.first()?;
            (address.len() == 8 && address.chars().all(|c| c.is_ascii_hexdigit())).then(|| {
                (
                    address.to_string(),
                    fields[1].to_string(),
                    fields[2..].join(" "),
                )
            })
        })
        .collect()
}

/// Mnemonic comparison ignoring whitespace and case.
pub fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Asserts that selache decodes `stream` as exactly `expected`, one line per instruction,
/// with parcel addresses, raw parcels and mnemonics all equal.
pub fn assert_selache_decodes(bytes: &[u8], expected: &[(Vec<u16>, &str)]) {
    let lines = selache_decode(bytes);
    assert_eq!(lines.len(), expected.len(), "{lines:#?}");
    let mut address = 0;
    for ((at, hex, text), (parcels, mnemonic)) in lines.iter().zip(expected) {
        assert_eq!(at, &format!("{address:08x}"), "{text}");
        assert_eq!(
            hex,
            &parcels
                .iter()
                .map(|p| format!("{p:04x}"))
                .collect::<String>(),
            "{text}"
        );
        assert_eq!(normalize(text), normalize(mnemonic), "{hex}");
        address += parcels.len();
    }
}
