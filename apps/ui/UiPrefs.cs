using System;
using System.IO;
using System.Text.Json;

namespace Downloader.Ui;

/// <summary>
/// Вигляд вікна. Не ядро: ядро не знає про теми, і IPC їх не возить.
/// </summary>
/// <remarks>
/// Файл лежить у LocalAppData або поруч із застосунком у портативному режимі
/// (при наявності marker-файлу <c>portable.txt</c>).
/// Пошкоджений або відсутній файл — типова темна тема, вікно не падає.
/// </remarks>
static class UiPrefs
{
    private sealed class FileShape
    {
        public string? Theme { get; set; }
    }

    /// <summary>
    /// Перевірка портативного режиму: наявність <c>portable.txt</c> поруч із бінарником.
    /// </summary>
    public static bool IsPortable(string? baseDir = null)
    {
        string dir = baseDir ?? AppContext.BaseDirectory;
        return File.Exists(Path.Combine(dir, "portable.txt"));
    }

    /// <summary>
    /// Тека даних UI: тека застосунку в портативному режимі,
    /// або %LOCALAPPDATA%\Downloader у звичайному.
    /// </summary>
    public static string DataDir(string? baseDir = null)
    {
        string dir = baseDir ?? AppContext.BaseDirectory;
        if (IsPortable(dir))
        {
            return dir;
        }

        string root = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        if (string.IsNullOrWhiteSpace(root))
        {
            return dir;
        }

        return Path.Combine(root, "Downloader");
    }

    public static string FilePath(string? baseDir = null)
    {
        return Path.Combine(DataDir(baseDir), "ui.json");
    }

    /// <summary>Типова — темна.</summary>
    public static bool LoadDark(string? baseDir = null)
    {
        string path = FilePath(baseDir);
        if (!File.Exists(path))
        {
            return true;
        }

        try
        {
            FileShape? s = JsonSerializer.Deserialize<FileShape>(File.ReadAllText(path));
            return !string.Equals(s?.Theme, "light", StringComparison.OrdinalIgnoreCase);
        }
        catch (JsonException)
        {
            return true;
        }
        catch (IOException)
        {
            return true;
        }
    }

    public static void SaveDark(bool dark, string? baseDir = null)
    {
        string path = FilePath(baseDir);
        string dir = Path.GetDirectoryName(path)
            ?? throw new InvalidOperationException("немає теки для ui.json");
        Directory.CreateDirectory(dir);
        File.WriteAllText(
            path,
            JsonSerializer.Serialize(new FileShape { Theme = dark ? "dark" : "light" }));
    }
}

