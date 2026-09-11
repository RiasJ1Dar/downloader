//! Локальний канал: named pipe на Windows, unix-сокет на решті систем.
//!
//! # Чому не TCP на 127.0.0.1
//!
//! Локальний HTTP-порт — найпоширеніше рішення в цьому класі програм, і
//! водночас найгірше:
//!
//! * **pyLoad** отримав pre-auth RCE (CVE-2023-0297) саме на локальному
//!   сервісі;
//! * **gopeed** змушує людину вручну вводити порт і токен у розширення
//!   браузера, і найчастіша скарга на нього — «перехоплення не працює»;
//! * **DLMan** слухає `localhost:7899` без жодного токена — покладається на
//!   те, що порт локальний. Але локальний порт відкритий **будь-якій**
//!   програмі й будь-якому сценарію на сторінці, що вміє `fetch`.
//!
//! Named pipe не має жодної з цих проблем: він не видно ззовні машини, до
//! нього не достукатись із браузерної сторінки, і налаштовувати нічого не
//! треба.
//!
//! # Особливість named pipe, через яку легко втратити з'єднання
//!
//! На відміну від сокета, named pipe **не має окремого об'єкта-слухача**.
//! Сервер створює *екземпляр* каналу, чекає на ньому клієнта — і цей самий
//! екземпляр стає з'єднанням. Щоб прийняти наступного клієнта, треба
//! заздалегідь створити **новий** екземпляр.
//!
//! Якщо створювати його вже після того, як попередній зайняли, лишається
//! вікно, у якому каналу не існує: клієнт, що постукає саме тоді, отримає
//! «файл не знайдено» замість очікування. Тому наступний екземпляр
//! створюється **до** того, як віддати поточний в обробку.

use std::io;

use crate::protocol::PIPE_NAME;

/// Двобічний потік, яким говорять клієнт і сервер.
#[cfg(windows)]
pub type Stream = tokio::net::windows::named_pipe::NamedPipeServer;

/// Двобічний потік з боку клієнта.
#[cfg(windows)]
pub type ClientStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// Двобічний потік, яким говорять клієнт і сервер.
#[cfg(not(windows))]
pub type Stream = tokio::net::UnixStream;

/// Двобічний потік з боку клієнта.
#[cfg(not(windows))]
pub type ClientStream = tokio::net::UnixStream;

/// Слухач вхідних з'єднань.
pub struct Listener {
    #[cfg(windows)]
    next: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    #[cfg(not(windows))]
    inner: tokio::net::UnixListener,
    /// Ім'я каналу — потрібне, щоб створювати наступні екземпляри.
    name: String,
}

impl Listener {
    /// Почати слухати.
    ///
    /// ⚠️ Другий запуск ядра має **впасти тут**, а не мовчки працювати
    /// поруч. Два ядра на одну базу — це два планувальники, які качають ті
    /// самі завдання в той самий файл.
    #[cfg(windows)]
    pub fn bind() -> io::Result<Self> {
        Self::bind_named(PIPE_NAME)
    }

    /// Слухати канал із заданим іменем.
    ///
    /// Ім'я винесене в параметр заради тестів: із фіксованим на весь процес
    /// каналом два тести не могли б працювати водночас, а послідовні тести
    /// чіплялися б за недоприбраний канал попереднього.
    #[cfg(windows)]
    pub fn bind_named(name: &str) -> io::Result<Self> {
        use tokio::net::windows::named_pipe::ServerOptions;

        let server = ServerOptions::new()
            .first_pipe_instance(true)
            // Канал лише для цієї машини: віддалені клієнти відхиляються на
            // рівні системи, а не нашою перевіркою.
            .reject_remote_clients(true)
            .create(name)?;

        Ok(Self {
            next: Some(server),
            name: name.to_owned(),
        })
    }

    /// Почати слухати.
    #[cfg(not(windows))]
    pub fn bind() -> io::Result<Self> {
        Self::bind_named(PIPE_NAME)
    }

    /// Слухати сокет із заданим шляхом.
    #[cfg(not(windows))]
    pub fn bind_named(name: &str) -> io::Result<Self> {
        // Сокет міг лишитись від процесу, який не прибрав за собою.
        let _ = std::fs::remove_file(name);
        let inner = tokio::net::UnixListener::bind(name)?;
        Ok(Self {
            inner,
            name: name.to_owned(),
        })
    }

    /// Дочекатись клієнта.
    #[cfg(windows)]
    pub async fn accept(&mut self) -> io::Result<Stream> {
        use tokio::net::windows::named_pipe::ServerOptions;

        let Some(server) = self.next.take() else {
            return Err(io::Error::other("слухач named pipe втратив екземпляр"));
        };

        server.connect().await?;

        // Новий екземпляр створюємо **одразу**, ще до того, як віддати
        // поточний. Інакше між `connect` і створенням наступного лишається
        // вікно, у якому каналу немає — і клієнт у цю мить дістає відмову.
        self.next = Some(
            ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?,
        );

        Ok(server)
    }

    /// Дочекатись клієнта.
    #[cfg(not(windows))]
    pub async fn accept(&mut self) -> io::Result<Stream> {
        let (stream, _addr) = self.inner.accept().await?;
        Ok(stream)
    }
}

#[cfg(not(windows))]
impl Drop for Listener {
    fn drop(&mut self) {
        // Файл сокета не зникає сам — прибираємо за собою.
        let _ = std::fs::remove_file(&self.name);
    }
}

/// Під'єднатись до ядра.
///
/// [`io::ErrorKind::NotFound`] означає, що ядро не запущене — це звичайна
/// ситуація, а не збій: клієнт має запустити його або сказати про це людині.
#[cfg(windows)]
pub async fn connect() -> io::Result<ClientStream> {
    connect_to(PIPE_NAME).await
}

/// Під'єднатись до каналу із заданим іменем.
#[cfg(windows)]
pub async fn connect_to(name: &str) -> io::Result<ClientStream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    ClientOptions::new().open(name)
}

/// Під'єднатись до ядра.
#[cfg(not(windows))]
pub async fn connect() -> io::Result<ClientStream> {
    connect_to(PIPE_NAME).await
}

/// Під'єднатись до каналу із заданим іменем.
#[cfg(not(windows))]
pub async fn connect_to(name: &str) -> io::Result<ClientStream> {
    tokio::net::UnixStream::connect(name).await
}

/// Чи ядро вже слухає канал.
pub async fn is_core_running() -> bool {
    connect().await.is_ok()
}
