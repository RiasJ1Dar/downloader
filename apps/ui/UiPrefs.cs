using System;
using System.IO;
using System.Text.Json;

namespace Downloader.Ui;

/// <summary>
/// Вигляд вікна. Не ядро: ядро не знає про теми, і IPC їх не возить.
/// </summary>
/// <remarks>
/// Файл лежить у LocalAppData поруч із даними людини, не в <c>tasks.db</c>.
/// Пошкоджений або відсутній файл — типова темна тема, вікно не падає.
/// </remarks>
static class UiPrefs
{
    private sealed class FileShape
    {
        public string? Theme { get; set; }
    }

    /// <summary>Типова — темна.</summary>
    public static bool LoadDark()
    {
        string path = FilePath();
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

    public static void SaveDark(bool dark)
    {
        string path = FilePath();
        string dir = Path.GetDirectoryName(path)
            ?? throw new InvalidOperationException("немає теки для ui.json");
        Directory.CreateDirectory(dir);
        File.WriteAllText(
            path,
            JsonSerializer.Serialize(new FileShape { Theme = dark ? "dark" : "light" }));
    }

    private static string FilePath()
    {
        string root = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        return Path.Combine(root, "Downloader", "ui.json");
    }
}
