using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Downloader.Ui.Ipc;

/// <summary>
/// Типи протоколу — дзеркало <c>crates/ipc/src/protocol.rs</c>.
/// </summary>
/// <remarks>
/// ⚠️ Це друге втілення того самого контракту, і розбіжність тут виявиться
/// не при збірці, а посеред роботи — у вигляді поля, якого «раптом немає».
/// Тому: міняєш Rust — міняй і тут, у тому самому коміті.
///
/// <para>
/// Рукостискання звіряє <see cref="ProtocolVersion"/>. Несумісний клієнт
/// дістає чесну відмову з поясненням, а не дивну поведінку через півгодини.
/// </para>
/// </remarks>
public static class Wire
{
    /// <summary>Версія протоколу. Мусить збігатися з ядром.</summary>
    public const uint ProtocolVersion = 1;

    /// <summary>Ім'я каналу, на якому слухає ядро.</summary>
    public static string PipeName =>
        System.OperatingSystem.IsWindows()
            ? "downloader-core"
            : "/tmp/downloader-core.sock";
}

// ── Запити ──────────────────────────────────────────────────────────────

/// <summary>Запит до ядра. Поле <c>kind</c> обирає різновид.</summary>
public abstract record Request
{
    [JsonPropertyName("kind")]
    public abstract string Kind { get; }
}

public sealed record HelloRequest(string Client, uint ProtocolVersion) : Request
{
    public override string Kind => "hello";
}

public sealed record AddRequest(
    string Url,
    string? Dest = null,
    int? Parts = null,
    string? Cookies = null,
    string? Referer = null,
    string? Variant = null) : Request
{
    public override string Kind => "add";
}

public sealed record ListRequest : Request
{
    public override string Kind => "list";
}

public sealed record PauseRequest(long Id) : Request
{
    public override string Kind => "pause";
}

public sealed record ResumeRequest(long Id) : Request
{
    public override string Kind => "resume";
}

public sealed record RemoveRequest(long Id, bool WithFile) : Request
{
    public override string Kind => "remove";
}

/// <summary>
/// Розкладка частин одного завдання.
/// </summary>
/// <remarks>
/// Питається **на вимогу**, для одного розгорнутого рядка. У знімку списку
/// розкладки немає навмисно: помножена на весь список, вона коштувала б
/// дорожче, ніж уся решта знімка разом.
/// </remarks>
public sealed record DetailsRequest(long Id) : Request
{
    public override string Kind => "details";
}

/// <summary>
/// Які варіанти якості має це посилання.
/// </summary>
/// <remarks>
/// Окремий запит, а не частина <see cref="AddRequest"/>: проба коштує
/// мережевого звернення, а для YouTube — ще й запуску yt-dlp. Робити її на
/// кожне додавання, коли вибір нікому не потрібен, означало б платити цю
/// затримку завжди.
/// </remarks>
public sealed record VariantsRequest(string Url) : Request
{
    public override string Kind => "variants";
}

public sealed record SubscribeRequest : Request
{
    public override string Kind => "subscribe";
}

public sealed record PingRequest : Request
{
    public override string Kind => "ping";
}

public sealed record SettingsRequest : Request
{
    public override string Kind => "settings";
}

public sealed record ConfigureRequest(
    uint? MaxConcurrent,
    ulong? RateLimit,
    string? PostAction = null,
    string? ScheduleFrom = null,
    string? ScheduleTo = null,
    string? QuietFrom = null,
    string? QuietTo = null,
    ulong? QuietRate = null) : Request
{
    public override string Kind => "configure";
}

// ── Відповіді ───────────────────────────────────────────────────────────

/// <summary>
/// Відповідь ядра. Розбирається вручну за полем <c>kind</c>: варіанти мають
/// різні набори полів, і безпечніше подивитися на теґ, ніж покладатися на
/// здогадки серіалізатора.
/// </summary>
public sealed class Response
{
    [JsonPropertyName("kind")]
    public string Kind { get; set; } = "";

    [JsonPropertyName("server_version")]
    public string? ServerVersion { get; set; }

    [JsonPropertyName("protocol_version")]
    public uint? ProtocolVersion { get; set; }

    [JsonPropertyName("id")]
    public long? Id { get; set; }

    [JsonPropertyName("tasks")]
    public List<TaskView>? Tasks { get; set; }

    [JsonPropertyName("parts")]
    public List<PartView>? Parts { get; set; }

    [JsonPropertyName("variants")]
    public List<VariantView>? Variants { get; set; }

    [JsonPropertyName("code")]
    public string? Code { get; set; }

    [JsonPropertyName("message")]
    public string? Message { get; set; }

    [JsonPropertyName("max_concurrent")]
    public uint? MaxConcurrent { get; set; }

    [JsonPropertyName("rate_limit")]
    public ulong? RateLimit { get; set; }

    [JsonPropertyName("post_action")]
    public string? PostAction { get; set; }

    [JsonPropertyName("schedule_from")]
    public string? ScheduleFrom { get; set; }

    [JsonPropertyName("schedule_to")]
    public string? ScheduleTo { get; set; }

    [JsonPropertyName("quiet_from")]
    public string? QuietFrom { get; set; }

    [JsonPropertyName("quiet_to")]
    public string? QuietTo { get; set; }

    [JsonPropertyName("quiet_rate")]
    public ulong? QuietRate { get; set; }

    /// <summary>Чи це відмова.</summary>
    public bool IsError => Kind == "error";
}

// ── Події ───────────────────────────────────────────────────────────────

/// <summary>Подія від ядра до підписаних клієнтів.</summary>
public sealed class CoreEvent
{
    [JsonPropertyName("kind")]
    public string Kind { get; set; } = "";

    [JsonPropertyName("tasks")]
    public List<TaskView>? Tasks { get; set; }

    [JsonPropertyName("id")]
    public long? Id { get; set; }

    [JsonPropertyName("path")]
    public string? Path { get; set; }

    [JsonPropertyName("bytes")]
    public ulong? Bytes { get; set; }

    [JsonPropertyName("message")]
    public string? Message { get; set; }
}

/// <summary>
/// Завдання очима клієнта.
/// </summary>
/// <remarks>
/// Навмисно пласка структура з готовими полями: вікно нічого не дораховує.
/// Швидкість і залишок часу рахує ядро — інакше два різні клієнти показали б
/// різні числа для того самого завдання.
/// </remarks>
public sealed class TaskView
{
    [JsonPropertyName("id")]
    public long Id { get; set; }

    [JsonPropertyName("url")]
    public string Url { get; set; } = "";

    [JsonPropertyName("name")]
    public string Name { get; set; } = "";

    /// <summary><c>queued</c>, <c>running</c>, <c>paused</c>, <c>done</c>, <c>failed</c>.</summary>
    [JsonPropertyName("status")]
    public string Status { get; set; } = "";

    [JsonPropertyName("done")]
    public ulong Done { get; set; }

    [JsonPropertyName("total")]
    public ulong? Total { get; set; }

    [JsonPropertyName("speed")]
    public ulong Speed { get; set; }

    [JsonPropertyName("eta_secs")]
    public ulong? EtaSecs { get; set; }

    [JsonPropertyName("segments")]
    public int Segments { get; set; }

    [JsonPropertyName("error")]
    public string? Error { get; set; }

    [JsonPropertyName("dest")]
    public string? Dest { get; set; }

    /// <summary>
    /// Частка виконаного від 0 до 1, або <c>null</c>, коли розмір невідомий.
    /// </summary>
    /// <remarks>
    /// Показувати «0 %» на завданні, яке качається на повному ходу, гірше,
    /// ніж не показувати нічого.
    /// </remarks>
    public double? Progress =>
        Total is > 0 ? System.Math.Min(1.0, (double)Done / Total.Value) : null;
}

/// <summary>Одна частина завантаження — звідки, доки й скільки вже є.</summary>
public sealed class PartView
{
    [JsonPropertyName("start")]
    public ulong Start { get; set; }

    /// <summary>Кінець, не включно.</summary>
    [JsonPropertyName("end")]
    public ulong End { get; set; }

    /// <summary>Скільки байтів від <see cref="Start"/> уже на диску.</summary>
    [JsonPropertyName("done")]
    public ulong Done { get; set; }

    public ulong Length => End > Start ? End - Start : 0;
}

/// <summary>Варіант якості: «720p», «лише звук».</summary>
/// <remarks>
/// <c>Id</c> непрозорий — його видав модуль, йому ж він і повернеться.
/// Вікно не намагається його тлумачити, як не намагається й ядро.
/// </remarks>
public sealed class VariantView
{
    [JsonPropertyName("id")]
    public string Id { get; set; } = "";

    [JsonPropertyName("label")]
    public string Label { get; set; } = "";

    [JsonPropertyName("height")]
    public uint? Height { get; set; }

    [JsonPropertyName("size")]
    public ulong? Size { get; set; }

    [JsonPropertyName("note")]
    public string? Note { get; set; }
}
