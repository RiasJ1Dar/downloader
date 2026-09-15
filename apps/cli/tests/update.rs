use std::process::Command;

#[test]
fn update_check_з_новішою_версією_показує_пакунки_та_хеші() {
    let temp_dir = std::env::temp_dir();
    let manifest_path = temp_dir.join(format!("test_manifest_newer_{}.json", std::process::id()));

    let manifest_content = r#"{
        "version": "0.2.0",
        "min_supported_version": "0.1.0",
        "release_date": "2026-10-01",
        "changelog_url": "https://github.com/RiasJ1Dar/downloader/blob/master/CHANGELOG.txt",
        "packages": [
            {
                "name": "Downloader-0.2.0-web.msi",
                "type": "msi-web",
                "url": "https://example.com/Downloader-0.2.0-web.msi",
                "size": 45728392,
                "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            }
        ]
    }"#;
    std::fs::write(&manifest_path, manifest_content).expect("запис маніфесту");

    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args([
            "update",
            "--check",
            "--manifest",
            manifest_path.to_str().expect("UTF-8 path"),
        ])
        .output()
        .expect("dl update --check");

    let _ = std::fs::remove_file(&manifest_path);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");

    assert!(stdout.contains("v0.2.0"), "має згадувати версію 0.2.0");
    assert!(stdout.contains("Знайдено нову версію"), "має виявляти нову версію");
    assert!(stdout.contains("Downloader-0.2.0-web.msi"), "має виводити назву пакунка");
    assert!(stdout.contains("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), "має показувати SHA-256");
    assert!(stdout.contains("тихе автоматичне оновлення вимкнено"), "має нагадувати про безпеку");
}

#[test]
fn update_check_з_актуальною_версією_повідомляє_що_оновлення_не_потрібне() {
    let temp_dir = std::env::temp_dir();
    let manifest_path = temp_dir.join(format!("test_manifest_current_{}.json", std::process::id()));

    let manifest_content = r#"{
        "version": "0.1.0",
        "packages": []
    }"#;
    std::fs::write(&manifest_path, manifest_content).expect("запис маніфесту");

    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args([
            "update",
            "--check",
            "--manifest",
            manifest_path.to_str().expect("UTF-8 path"),
        ])
        .output()
        .expect("dl update --check");

    let _ = std::fs::remove_file(&manifest_path);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");

    assert!(stdout.contains("Встановлено актуальну версію"), "має констатувати актуальність");
}

#[test]
fn update_verify_file_підтверджує_правильний_хеш() {
    let temp_dir = std::env::temp_dir();
    let manifest_path = temp_dir.join(format!("test_manifest_verify_{}.json", std::process::id()));
    let test_file = temp_dir.join(format!("test_file_{}.bin", std::process::id()));

    // Вміст "abc" дає SHA-256: ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    std::fs::write(&test_file, b"abc").expect("запис тестового файлу");

    let manifest_content = r#"{
        "version": "0.2.0",
        "packages": [
            {
                "name": "package.bin",
                "type": "bin",
                "url": "https://example.com/package.bin",
                "size": 3,
                "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            }
        ]
    }"#;
    std::fs::write(&manifest_path, manifest_content).expect("запис маніфесту");

    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args([
            "update",
            "--check",
            "--manifest",
            manifest_path.to_str().expect("UTF-8"),
            "--verify-file",
            test_file.to_str().expect("UTF-8"),
        ])
        .output()
        .expect("dl update verify");

    let _ = std::fs::remove_file(&manifest_path);
    let _ = std::fs::remove_file(&test_file);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("ЗБІГ: Хеш точно відповідає пакунку 'package.bin'"));
}

#[test]
fn update_verify_file_відхиляє_підроблений_файл() {
    let temp_dir = std::env::temp_dir();
    let manifest_path = temp_dir.join(format!("test_manifest_fake_{}.json", std::process::id()));
    let test_file = temp_dir.join(format!("test_fake_file_{}.bin", std::process::id()));

    // Вміст "xyz" дає геть інший хеш
    std::fs::write(&test_file, b"xyz").expect("запис підробленого файлу");

    let manifest_content = r#"{
        "version": "0.2.0",
        "packages": [
            {
                "name": "package.bin",
                "type": "bin",
                "url": "https://example.com/package.bin",
                "size": 3,
                "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            }
        ]
    }"#;
    std::fs::write(&manifest_path, manifest_content).expect("запис маніфесту");

    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args([
            "update",
            "--check",
            "--manifest",
            manifest_path.to_str().expect("UTF-8"),
            "--verify-file",
            test_file.to_str().expect("UTF-8"),
        ])
        .output()
        .expect("dl update verify fail");

    let _ = std::fs::remove_file(&manifest_path);
    let _ = std::fs::remove_file(&test_file);

    assert!(!output.status.success(), "команда має завершитись помилкою при невідповідності хешу");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Хеш файлу НЕ збігається") || stderr.contains("перевірка цілісності провалена"));
}

#[test]
fn тихе_оновлення_заборонено_і_попереджає_користувача() {
    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["update", "--check=false"])
        .output()
        .expect("dl update --check=false");

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Тихе автооновлення заборонено проєктом"));
}
