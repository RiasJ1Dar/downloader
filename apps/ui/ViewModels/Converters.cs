using System;
using System.Globalization;
using Avalonia.Data.Converters;
using Avalonia.Media;

namespace Downloader.Ui.ViewModels;

/// <summary>
/// Колір кружечка зв'язку: зелений — ядро відповідає, сірий — ні.
/// </summary>
/// <remarks>
/// Червоний навмисно не беремо: відсутнє ядро — це не аварія, а звичайний
/// стан (ще не запустилось, оновлюється, людина його зупинила). Червоне
/// світло привчає не звертати на себе уваги.
/// </remarks>
public sealed class КолірЗвʼязкуКонвертер : IValueConverter
{
    private static readonly IBrush Живе =
        new SolidColorBrush(Color.FromRgb(0x3F, 0xB9, 0x50));

    private static readonly IBrush Немає =
        new SolidColorBrush(Color.FromArgb(0x88, 0x88, 0x88, 0x88));

    public object Convert(object? value, Type targetType, object? parameter, CultureInfo culture) =>
        value is true ? Живе : Немає;

    public object ConvertBack(
        object? value,
        Type targetType,
        object? parameter,
        CultureInfo culture) =>
        throw new NotSupportedException("колір зв'язку читається лише в один бік");
}
