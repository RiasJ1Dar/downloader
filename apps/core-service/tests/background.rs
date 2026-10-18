//! Перевірка саме зібраного EXE, а не текстової ознаки у вихідному коді.
#![cfg(windows)]
#![expect(clippy::unwrap_used, reason = "у тесті падіння є діагностикою")]

#[test]
fn ядро_не_створює_консоль_при_автозапуску() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_downloader-core")).unwrap();
    assert_eq!(&exe[..2], b"MZ");
    let pe = u32::from_le_bytes(exe[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&exe[pe..pe + 4], b"PE\0\0");
    let optional = pe + 24;
    let subsystem = u16::from_le_bytes(exe[optional + 68..optional + 70].try_into().unwrap());
    assert_eq!(subsystem, 2, "ядро має бути Windows GUI, а не console EXE");
}
