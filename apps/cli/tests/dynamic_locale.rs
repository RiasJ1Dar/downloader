use std::process::Command;

#[test]
fn динамічна_локаль_xx_підхоплюється_через_довкілля_без_правок_коду() {
    let temp_dir = std::env::temp_dir().join(format!("test_dl_l10n_{}", std::process::id()));
    drop(std::fs::create_dir_all(&temp_dir));

    let xx_path = temp_dir.join("xx.ftl");
    let xx_content = "need-url = Рядок XX: вкажіть посилання для завантаження\n";
    std::fs::write(&xx_path, xx_content).expect("запис xx.ftl");

    // Запускаємо dl add без URL з прапорцем --lang xx та каталогом локалізації:
    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .env("DOWNLOADER_L10N_DIR", &temp_dir)
        .args(["--lang", "xx", "add"])
        .output()
        .expect("dl --lang xx add");

    drop(std::fs::remove_file(&xx_path));
    drop(std::fs::remove_dir(&temp_dir));

    // Повинна бути помилка відсутності URL
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Стрічка повинна походити безпосередньо з динамічного xx.ftl
    assert!(
        stderr.contains("Рядок XX: вкажіть посилання для завантаження"),
        "dl зобов'язаний вивести повідомлення з динамічного xx.ftl: {stderr}"
    );
}

#[test]
fn динамічна_локаль_xx_використовує_fallback_на_українську() {
    let temp_dir = std::env::temp_dir().join(format!("test_dl_fallback_{}", std::process::id()));
    drop(std::fs::create_dir_all(&temp_dir));

    let xx_path = temp_dir.join("xx.ftl");
    // Вказуємо лише один специфічний ключ, іншого немає:
    let xx_content = "custom-key = Текст мовою XX\n";
    std::fs::write(&xx_path, xx_content).expect("запис xx.ftl");

    // Запускаємо dl add без URL (де потрібен ключ need-url)
    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .env("DOWNLOADER_L10N_DIR", &temp_dir)
        .args(["--lang", "xx", "add"])
        .output()
        .expect("dl --lang xx add");

    drop(std::fs::remove_file(&xx_path));
    drop(std::fs::remove_dir(&temp_dir));

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Оскільки need-url не визначено в xx.ftl, повинен спрацювати fallback на uk.ftl:
    assert!(
        stderr.contains("вкажіть посилання, --list або --clipboard"),
        "при відсутності ключа в сторонній локалі має спрацювати український fallback: {stderr}"
    );
}

#[test]
fn спроба_вказати_ru_завжди_повертає_українську_в_cli() {
    let temp_dir = std::env::temp_dir().join(format!("test_dl_ru_cli_{}", std::process::id()));
    drop(std::fs::create_dir_all(&temp_dir));

    // Навіть якщо підкласти ru.ftl у каталог:
    let ru_path = temp_dir.join("ru.ftl");
    let ru_content = "need-url = Недозволений рядок\n";
    std::fs::write(&ru_path, ru_content).expect("запис ru.ftl");

    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .env("DOWNLOADER_L10N_DIR", &temp_dir)
        .args(["--lang", "ru", "add"])
        .output()
        .expect("dl --lang ru add");

    drop(std::fs::remove_file(&ru_path));
    drop(std::fs::remove_dir(&temp_dir));

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);

    // ru.ftl повністю ігнорується, виводиться стандартна українська:
    assert!(
        !stderr.contains("Недозволений рядок"),
        "ru.ftl не повинен читатися ні за яких умов!"
    );
    assert!(
        stderr.contains("вкажіть посилання, --list або --clipboard"),
        "замість ru має працювати стандартна українська локаль: {stderr}"
    );
}

#[test]
fn запобіжник_проти_ru_ловить_підкинутий_каталог() {
    let temp_dir = std::env::temp_dir().join(format!("test_dl_ru_guard_{}", std::process::id()));
    drop(std::fs::create_dir_all(&temp_dir));

    let fake_ru = temp_dir.join("ru.ftl");
    std::fs::write(&fake_ru, b"need-url = text\n").expect("запис fake ru");

    let detected = downloader_i18n::шукати_заборонені_ru_файли(&temp_dir);
    drop(std::fs::remove_file(&fake_ru));
    drop(std::fs::remove_dir(&temp_dir));

    assert!(
        !detected.is_empty(),
        "запобіжник репозиторію зобов'язаний виявити ru.ftl при спробі його додавання!"
    );
}
