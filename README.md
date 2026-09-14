# Downloader

Менеджер завантажень для Windows. Аналог IDM / Ant Download Manager.

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
