using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Reflection;
using System.Text;

namespace Downloader.Ui.I18n;

/// <summary>
/// Переклади вікна — ті самі файли, що й у ядра.
/// </summary>
/// <remarks>
/// ⚠️ Каталог **один на весь продукт**: `crates/i18n/l10n/*.ftl` вкладаються
/// в цю збірку як ресурс просто з теки ядра. Другої копії рядків немає
/// навмисно — інакше вікно й CLI почали б розходитись у формулюваннях, і
/// ніхто б не помітив, доки хтось не порівняв би їх поруч.
///
/// <para>
/// Читаємо не весь Fluent, а ту його частину, якою користуємось: `ключ =
/// значення`, продовження рядка з відступом, підстановки `{ $ім'я }`.
/// Селекторів і множинних форм тут немає — множину вікно рахує саме
/// (див. <see cref="Множина"/>), бо форми потрібні лише йому.
/// </para>
///
/// <para>
/// Мова фіксується один раз на запуск, як і в ядрі. Перемикання на льоту не
/// передбачене: половина тексту вже намальована, і оновити її без
/// перестворення вікна все одно не вийде.
/// </para>
/// </remarks>
public static class Каталог
{
    private static readonly Dictionary<string, string> Обрана = new(StringComparer.Ordinal);
    private static readonly Dictionary<string, string> Запасна = new(StringComparer.Ordinal);

    /// <summary>Код обраної мови: <c>uk</c> або <c>en</c>.</summary>
    public static string Мова { get; private set; } = "uk";

    static Каталог()
    {
        Мова = ОбратиМову(
            Environment.GetEnvironmentVariable("DOWNLOADER_LANG"),
            CultureInfo.CurrentUICulture.Name);

        // Українська — і типова мова, і резерв: дірку в англійському
        // каталозі краще показати живим текстом, ніж голим ключем.
        Читати($"{Мова}.ftl", Обрана);
        Читати("uk.ftl", Запасна);
    }

    /// <summary>
    /// Обрати мову: змінна середовища, потім ОС.
    /// </summary>
    /// <remarks>
    /// ⚠️ Російська ОС дає **українську** (Р-15). Гілки, яка б увімкнула
    /// російську, немає й не буде — саме тому вибір написаний переліком, а не
    /// через «взяти дволітерний код як є».
    /// </remarks>
    public static string ОбратиМову(string? заявлене, string? os)
    {
        if (Розпізнати(заявлене) is { } явне)
        {
            return явне;
        }

        if (os is not null && os.StartsWith("ru", StringComparison.OrdinalIgnoreCase))
        {
            return "uk";
        }

        return Розпізнати(os) ?? "uk";
    }

    private static string? Розпізнати(string? код) => код?.Trim().ToLowerInvariant() switch
    {
        "en" or "en-us" or "en-gb" => "en",
        "uk" or "uk-ua" => "uk",
        var інше when інше is not null && інше.StartsWith("en", StringComparison.Ordinal) => "en",
        var інше when інше is not null && інше.StartsWith("uk", StringComparison.Ordinal) => "uk",
        _ => null,
    };

    /// <summary>Рядок за ключем.</summary>
    /// <remarks>
    /// Немає ключа — повертаємо сам ключ. Видимий `ui-download` у вікні
    /// знаходять за секунду; мовчазний порожній рядок не знаходять ніколи.
    /// </remarks>
    public static string T(string ключ)
    {
        if (Обрана.TryGetValue(ключ, out string? s) || Запасна.TryGetValue(ключ, out s))
        {
            return s;
        }

        return ключ;
    }

    /// <summary>Рядок із підстановками: <c>T("ui-peak", ("speed", "2 МБ/с"))</c>.</summary>
    public static string T(string ключ, params (string Імʼя, object Значення)[] аргументи)
    {
        string шаблон = T(ключ);

        foreach ((string імʼя, object значення) in аргументи)
        {
            шаблон = Підставити(шаблон, імʼя, значення?.ToString() ?? "");
        }

        return шаблон;
    }

    /// <summary>
    /// Замінити <c>{ $імʼя }</c> на значення.
    /// </summary>
    /// <remarks>
    /// Fluent дозволяє пробіли всередині дужок як завгодно, тож шукаємо не
    /// точний рядок, а дужку з тим самим іменем.
    /// </remarks>
    private static string Підставити(string шаблон, string імʼя, string значення)
    {
        var результат = new StringBuilder(шаблон.Length + значення.Length);
        int i = 0;

        while (i < шаблон.Length)
        {
            int відкрита = шаблон.IndexOf('{', i);
            if (відкрита < 0)
            {
                результат.Append(шаблон, i, шаблон.Length - i);
                break;
            }

            int закрита = шаблон.IndexOf('}', відкрита);
            if (закрита < 0)
            {
                результат.Append(шаблон, i, шаблон.Length - i);
                break;
            }

            string усередині = шаблон[(відкрита + 1)..закрита].Trim();
            результат.Append(шаблон, i, відкрита - i);

            if (усередині.Length > 1 && усередині[0] == '$' && усередині[1..] == імʼя)
            {
                результат.Append(значення);
            }
            else
            {
                результат.Append(шаблон, відкрита, закрита - відкрита + 1);
            }

            i = закрита + 1;
        }

        return результат.ToString();
    }

    /// <summary>
    /// Українська множина: 1 сегмент, 2 сегменти, 5 сегментів.
    /// </summary>
    /// <remarks>
    /// Форм три, і правило не зводиться до «один чи багато», як в англійській.
    /// Тримати його в каталозі не вийшло б: Fluent-селектори читає ядро, а
    /// рахує тут вікно. Англійський каталог просто дає однакове слово двом
    /// останнім формам.
    /// </remarks>
    public static string Множина(long n, string один, string кілька, string багато)
    {
        long остання = Math.Abs(n) % 10;
        long дві = Math.Abs(n) % 100;

        if (остання == 1 && дві != 11)
        {
            return T(один);
        }

        if (остання is >= 2 and <= 4 && ПозаВинятком(дві))
        {
            return T(кілька);
        }

        return T(багато);

        static bool ПозаВинятком(long дві) => дві is < 12 or > 14;
    }

    /// <summary>Прочитати каталог із вкладеного ресурсу.</summary>
    private static void Читати(string файл, Dictionary<string, string> куди)
    {
        Assembly збірка = typeof(Каталог).Assembly;
        string? імʼя = Array.Find(
            збірка.GetManifestResourceNames(),
            n => n.EndsWith(файл, StringComparison.OrdinalIgnoreCase));

        if (імʼя is null)
        {
            // Каталогу немає — вікно працюватиме на ключах. Це помітно з
            // першого погляду, а падати через переклад не варто.
            return;
        }

        using Stream? потік = збірка.GetManifestResourceStream(імʼя);
        if (потік is null)
        {
            return;
        }

        using var читач = new StreamReader(потік, Encoding.UTF8);
        Розібрати(читач, куди);
    }

    /// <summary>Розбір `ключ = значення` з продовженнями.</summary>
    private static void Розібрати(TextReader читач, Dictionary<string, string> куди)
    {
        string? ключ = null;
        var значення = new StringBuilder();

        while (читач.ReadLine() is { } рядок)
        {
            if (рядок.Length == 0 || рядок.TrimStart().StartsWith('#'))
            {
                Закрити(ключ, значення, куди);
                ключ = null;
                continue;
            }

            // Рядок із відступом — продовження попереднього значення.
            if (char.IsWhiteSpace(рядок[0]) && ключ is not null)
            {
                значення.Append(' ').Append(рядок.Trim());
                continue;
            }

            int рівність = рядок.IndexOf('=');
            if (рівність <= 0)
            {
                continue;
            }

            Закрити(ключ, значення, куди);
            ключ = рядок[..рівність].Trim();
            значення.Clear().Append(рядок[(рівність + 1)..].Trim());
        }

        Закрити(ключ, значення, куди);
    }

    private static void Закрити(
        string? ключ,
        StringBuilder значення,
        Dictionary<string, string> куди)
    {
        if (ключ is not null && ключ.Length > 0)
        {
            куди[ключ] = значення.ToString();
        }

        значення.Clear();
    }
}
