//! Наскрізний тест пустушки: байти пише exe, ядро бачить лише контракт.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use downloader_core::protocol::{
    Cancel, Progress, ProgressSink, Protocol, RateLimitSupport, Registry, RunContext,
};
use downloader_proto_external::ExternalProtocol;

fn dummy_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dummy-protocol"))
}

fn тимчасова_ціль() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "dl-ext-dummy-{}-{}-{}.bin",
        std::process::id(),
        ns,
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("не прибрав {}: {e}", self.0.display());
        }
    }
}

struct Німий;

impl ProgressSink for Німий {
    fn report(&self, _: Progress) {}
}

#[tokio::test]
async fn dummy_probe_і_run_пишуть_test_на_диск() -> Result<()> {
    let proto = ExternalProtocol::new(dummy_exe());
    let probed = proto.probe("ext:x").await.context("probe dummy")?;

    assert_eq!(probed.files.len(), 1, "пустушка має дати рівно один файл");
    assert_eq!(probed.files[0].suggested_name, "dummy.bin");
    assert_eq!(probed.files[0].size, Some(4));
    assert!(probed.files[0].selected);
    assert_eq!(probed.total_size, Some(4));

    let tmp = Tmp(тимчасова_ціль());
    let resume = proto
        .run(
            RunContext {
                task_id: 1,
                source: "ext:x".to_owned(),
                targets: vec![tmp.0.clone()],
                resume: None,
                cancel: Cancel::new(),
            },
            &Німий,
        )
        .await
        .context("run dummy")?;

    assert!(resume.is_none(), "завершене завдання не лишає стану");
    let bytes = std::fs::read(&tmp.0).with_context(|| format!("читання {}", tmp.0.display()))?;
    assert_eq!(
        bytes, b"TEST",
        "плагін мав сам записати байти в ціль, не через RPC"
    );
    Ok(())
}

#[tokio::test]
async fn реєстр_знаходить_external_і_не_знає_імен_http_hls() -> Result<()> {
    let mut r = Registry::new();
    r.register(Box::new(ExternalProtocol::new(dummy_exe())));

    let found = r
        .find("ext:x")
        .ok_or_else(|| anyhow::anyhow!("реєстр не знайшов ext:x"))?;
    assert_eq!(found.name(), "external");
    assert!(
        r.find("http://example.com/file.bin").is_none(),
        "ядро не має знати HTTP: у реєстрі лише external"
    );
    assert!(
        r.find("https://example.com/a.m3u8").is_none(),
        "ядро не має знати HLS"
    );
    assert!(
        r.by_name("http").is_none(),
        "імені http у реєстрі бути не може"
    );
    assert!(
        r.by_name("hls").is_none(),
        "імені hls у реєстрі бути не може"
    );
    assert_eq!(r.names(), vec!["external"]);

    let tmp = Tmp(тимчасова_ціль());
    let resume = found
        .run(
            RunContext {
                task_id: 2,
                source: "ext:x".to_owned(),
                targets: vec![tmp.0.clone()],
                resume: None,
                cancel: Cancel::new(),
            },
            &Німий,
        )
        .await
        .context("run через Registry")?;
    assert!(resume.is_none());
    assert_eq!(std::fs::read(&tmp.0)?, b"TEST");
    Ok(())
}

#[tokio::test]
async fn падіння_плагіна_несе_код_і_хвіст_stderr() -> Result<()> {
    let proto = ExternalProtocol::new(dummy_exe());
    let tmp = Tmp(тимчасова_ціль());
    let err = match proto
        .run(
            RunContext {
                task_id: 3,
                source: "ext:fail".to_owned(),
                targets: vec![tmp.0.clone()],
                resume: None,
                cancel: Cancel::new(),
            },
            &Німий,
        )
        .await
    {
        Ok(_) => bail!("очікували помилку на ext:fail"),
        Err(e) => e,
    };
    let text = err.to_string();
    assert!(
        text.contains('7'),
        "у помилці має бути код виходу: {text}"
    );
    assert!(
        text.contains("свідомо зламано"),
        "stderr плагіна не можна ковтати: {text}"
    );
    Ok(())
}

#[test]
fn ліміт_швидкості_чесно_непідтримуваний() {
    let p = ExternalProtocol::new(dummy_exe());
    assert_eq!(p.set_rate_limit(5000), RateLimitSupport::Unsupported);
}
