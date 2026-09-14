# Downloader

Менеджер завантажень для Windows. Аналог IDM / Ant Download Manager.

![Архітектура](docs/architecture.png)

Інтерактивна схема — завантаж [`docs/architecture.html`](docs/architecture.html) і відкрий у браузері.
На GitHub клік по HTML показує код, не сторінку (репозиторій приватний). Джерело — `docs/architecture.json`.

## Де що

| | |
|---|---|
| Код | ця тека, `<локальний шлях>` |

`<локальний шлях>`. Тут дозволений лише цей `README.md`.

Мови інтерфейсу: українська (типова) і англійська в першому релізі, далі
інші популярні локалі. Російської в продукті немає.

Розширення: `ext/chrome/` (ID `dcfgihgimkkfkhmbdpogbicigioldhba`).
Ядро при старті кличе `downloader-nmhost --install` (Chrome/Edge/Firefox).
Кнопка «Завантажити» на `<video>` — Shadow DOM. Маніфести `.m3u8`/`.mpd`
зі сторінки підхоплюються. Слухач буфера ввімкнено, вимкнути:
`DOWNLOADER_WATCH_CLIPBOARD=0`. Ядро в треї: тека, буфер, вихід.

Мова CLI: `dl --lang en …` або `DOWNLOADER_LANG=en`. Типова — українська.
Російська мова ОС дає українську, не російську.

## Структура

```
crates/core   ядро: рушій, планувальник, реєстр протоколів. Не знає про UI
apps/cli      командний рядок: перша оболонка й тестовий стенд
```

Далі за планом: `proto-http`, `proto-hls`, `proto-dash`, `proto-external`,
`ipc`, `winutil`, `apps/core-service`, `apps/nmhost`, `ext/`.

Вікно: `apps/ui` (Avalonia). Збірка `dotnet build -c Release apps/ui`.
Поклади `Downloader.Ui.exe` поруч із `downloader-core.exe` — у треї зʼявиться
«Відкрити вікно». Закрите вікно не зупиняє качання.

## Збірка

```
cargo build --workspace
cargo test --workspace
cargo build --release -p downloader-cli -p downloader-core-service
```

Бінарники: `target/release/dl.exe` і `target/release/downloader-core.exe`.

## Мінімальний запуск (Windows)

HTTP без ядра (один процес):

```
dl get https://example.com/file.zip -o file.zip
```

З ядром у треї (завдання переживе закриття консолі). HLS тільки цим шляхом:

```
downloader-core
dl add https://example.com/file.zip
dl add https://cdn.example/master.m3u8 -o video.ts
dl list
dl watch
```

Паузи в ядрі ще немає. Вікна немає — CLI є тестовим стендом.

## Правила, які тримають архітектуру

1. **Ядро не згадує жодного протоколу на ім'я.** HTTP, HLS, торент — усе
   через контракт `Protocol`. Перевіряється тестом із протоколом-пустушкою.
2. **Проковтнута помилка заборонена.** `unwrap`, `expect`, `panic` і
   `let _ =` у ядрі відхиляє збірка (`[lints.clippy]`).
3. **Логіка не заповзає в UI.** Усе, що вміє вікно, спершу вміє CLI.

## Інсталятори

Два пакети, різниця одна — ffmpeg:

| | розмір | коли брати |
|---|---|---|
| `Downloader-X.Y.Z-web.msi` | 43 МБ | звичайний випадок: ffmpeg довантажиться командою `dl ffmpeg-install` |
| `Downloader-X.Y.Z-full.msi` | 95 МБ | машина без мережі або роздача офлайн |

Ставиться **для користувача**, у `%LOCALAPPDATA%\Programs\Downloader`, без
UAC і без служби. Вікно йде з .NET усередині — доставляти рантайм не треба.

Збірка: `pwsh -File packaging\zibraty.ps1 -Version 0.1.0`. Потрібен WiX 6
(`dotnet tool install --global wix`), а для повного пакета — ffmpeg у
`tools\ffmpeg-dist`.

⚠️ Пакети **не підписані**: сертифіката підпису коду немає, тож SmartScreen
попереджатиме при першому запуску.

### Навіщо ffmpeg

Без нього YouTube не завантажується взагалі. Відео й звук роздаються
**окремими доріжками** — навіть 144p не існує одним файлом, — і звести їх
більше нічим.

Береться **LGPL**-збірка, без ключа `enable-gpl`. Це не дрібниця: GPL-варіант
ffmpeg зобов'язав би відкрити вихідний код усієї програми кожному, кому її
дали. LGPL такої вимоги не має, а ми ще й не лінкуємо ffmpeg у свій код —
запускаємо окремим процесом. Текст ліцензії їде поруч, у
`LICENSE-ffmpeg.txt`.
