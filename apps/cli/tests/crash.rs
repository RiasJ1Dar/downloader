//! Crash-тест: **вбитий процес**, а не перервана задача.
//!
//! Це різні речі. Перервана задача згортається чемно: деструктори
//! відпрацьовують, буфери дописуються. `TerminateProcess` не дає нічого —
//! процес зникає між двома машинними інструкціями, і на диску лишається
//! рівно те, що ми встигли туди покласти й підтвердити `sync`.
//!
//! Саме тут живе найдорожчий клас багів качалки: файл правильної довжини з
//! діркою всередині. Компілятор його не бачить, типи не рятують — ловиться
//! лише так: убити, відновити, звірити SHA-256.

use downloader_testserver::{EvilServer, expected_sha256};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// Тимчасовий файл, що прибирає й себе, і свій файл стану.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("dl-crash-{tag}-{unique}.bin"));
        Self(p)
    }

    fn state(&self) -> PathBuf {
        downloader_core::state::state_path(&self.0)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.state());
    }
}

/// Перший вимір прогресу — з ним порівнюємо останній.
fn viміри_перший(виміри: &[u64]) -> Option<&u64> {
    виміри.first()
}

fn sha256_of(path: &Path) -> anyhow::Result<String> {
    let data = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(hex::encode(h.finalize()))
}

/// Запустити справжній `dl get` окремим процесом.
fn запустити_качання(url: &str, dest: &Path) -> anyhow::Result<Child> {
    let child = Command::new(env!("CARGO_BIN_EXE_dl"))
        .arg("get")
        .arg(url)
        .arg("--out")
        .arg(dest)
        .arg("--parts")
        .arg("4")
        .arg("--min-chunk")
        .arg("4096")
        // Часто, щоб убивство припадало між чекпоінтами, а не до першого.
        .arg("--checkpoint-ms")
        .arg("50")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(child)
}

/// Один цикл: убити посеред роботи, потім дати дотягнути.
///
/// `after` — коли саме вбивати. Різні моменти ловлять різні місця: одразу
/// після старту, посеред сегмента, під час чекпоінта.
fn убити_і_докачати(url: &str, dest: &Path, after: Duration) -> anyhow::Result<()> {
    let mut child = запустити_качання(url, dest)?;
    std::thread::sleep(after);

    // Жорстке вбивство: на Windows це TerminateProcess — жодних деструкторів,
    // жодного дописування буферів.
    let _ = child.kill();
    let _ = child.wait();

    Ok(())
}

#[test]
fn вбитий_процес_докачується_і_дає_побайтово_той_самий_файл() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let server = rt.block_on(EvilServer::start())?;

    // 8 МБ по 1 МБ/с на з'єднання: при чотирьох потоках це близько двох
    // секунд — свідомо більше за суму всіх наших пауз (1.4 с). Інакше
    // качання встигало б завершитись посеред циклу, вимірів прогресу було б
    // замало, і перевірка монотонності нічого б не показала.
    let scenario = "/slow-range/8m/1m";
    let url = server.url(scenario);
    let tmp = Temp::new("kill");

    // Убиваємо в різні моменти. Кожен раз на диску лишається те, що встиг
    // зафіксувати чекпоінт, — і наступний запуск мусить **продовжити**.
    //
    // ⚠️ Найважливіша перевірка тесту — саме монотонність прогресу. Без неї
    // тест був би зелений і тоді, коли кожен запуск качає файл із нуля:
    // підсумковий SHA-256 в обох випадках однаковий, і доказу докачування
    // не було б жодного.
    let mut виміри: Vec<u64> = Vec::new();
    let mut попередній = 0u64;
    let mut завершилось = false;

    for (крок, затримка) in [150u64, 400, 250, 600].iter().enumerate() {
        убити_і_докачати(&url, &tmp.0, Duration::from_millis(*затримка))?;

        assert!(
            tmp.0.exists(),
            "крок {крок}: цільовий файл мав лишитись на диску після вбивства"
        );

        match downloader_core::state::load(&tmp.0) {
            Ok(saved) => {
                let тепер = saved.downloaded();
                assert!(
                    тепер >= попередній,
                    "крок {крок}: прогрес відкотився з {попередній} до {тепер} —                      значить качання почалось з нуля замість докачування"
                );
                попередній = тепер;
                виміри.push(тепер);
            }
            Err(downloader_core::state::LoadError::Absent) => {
                // Стан зник — качання встигло завершитись повністю.
                завершилось = true;
                break;
            }
            Err(e) => anyhow::bail!("крок {крок}: файл стану непридатний: {e}"),
        }
    }

    assert!(
        виміри.len() >= 2,
        "вимірів прогресу лише {} — замало, щоб довести докачування: {виміри:?}",
        виміри.len()
    );
    assert!(
        виміри.last() > viміри_перший(&виміри),
        "прогрес не зріс між убивствами: {виміри:?} — кожен запуск качав із нуля"
    );
    let _ = завершилось;

    // Останній запуск — без убивства, доводимо до кінця.
    let mut child = запустити_качання(&url, &tmp.0)?;
    let status = child.wait()?;
    assert!(
        status.success(),
        "останній запуск мав дотягнути файл, а вийшов зі статусом {status:?}"
    );

    assert_eq!(
        sha256_of(&tmp.0)?,
        expected_sha256(scenario)?,
        "файл, зібраний після чотирьох убивств, мусить збігатися побайтово з цілим"
    );

    assert!(
        !tmp.state().exists(),
        "після успіху файл стану має зникнути"
    );

    rt.block_on(server.shutdown());
    Ok(())
}

#[test]
fn вбивство_до_першого_чекпоінта_не_псує_наступну_спробу() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let server = rt.block_on(EvilServer::start())?;

    let scenario = "/slow-range/1m/1m";
    let url = server.url(scenario);
    let tmp = Temp::new("early");

    // Убиваємо майже одразу: стан ще не встиг записатись, на диску або нічого,
    // або порожній файл потрібного розміру. Наступна спроба мусить із цим
    // упоратись, а не спіткнутись об «файл уже є».
    убити_і_докачати(&url, &tmp.0, Duration::from_millis(20))?;

    let mut child = запустити_качання(&url, &tmp.0)?;
    assert!(child.wait()?.success(), "друга спроба мала завершитись успіхом");

    assert_eq!(
        sha256_of(&tmp.0)?,
        expected_sha256(scenario)?,
        "після раннього вбивства файл усе одно мусить зібратись правильно"
    );

    rt.block_on(server.shutdown());
    Ok(())
}

#[test]
fn завантажений_файл_позначено_як_отриманий_з_мережі() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let server = rt.block_on(EvilServer::start())?;

    let scenario = "/plain/64k";
    let tmp = Temp::new("motw");

    // Беремо `output()`, а не `wait()`: якщо качання впаде, тест мусить
    // показати **чому**, а не лише «мало завершитись успіхом». Мовчазний
    // провал у тесті коштує стільки ж, скільки проковтнута помилка в коді.
    let out = Command::new(env!("CARGO_BIN_EXE_dl"))
        .arg("get")
        .arg(server.url(scenario))
        .arg("--out")
        .arg(&tmp.0)
        .args(["--parts", "4", "--min-chunk", "4096", "--checkpoint-ms", "50"])
        .output()?;

    assert!(
        out.status.success(),
        "качання впало ({:?})
stdout: {}
stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Без цієї мітки SmartScreen не попередить людину про завантажений
    // виконуваний файл — а нас почнуть вважати засобом обходу захисту.
    assert!(
        downloader_winutil::is_marked_internet(&tmp.0),
        "завантажений файл лишився без Mark-of-the-Web"
    );

    rt.block_on(server.shutdown());
    Ok(())
}
