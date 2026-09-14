using System;
using Avalonia.Markup.Xaml;

namespace Downloader.Ui.I18n;

/// <summary>
/// Переклад просто в розмітці: <c>Content="{i18n:T ui-download}"</c>.
/// </summary>
/// <remarks>
/// Без цього кожен підпис довелося б прив'язувати до властивості
/// у ViewModel — три десятки властивостей, які нічого не роблять, крім
/// повернення сталого рядка.
///
/// <para>
/// Значення обчислюється один раз, під час розбору розмітки: мова
/// зафіксована на весь запуск, тож перераховувати нічого.
/// </para>
/// </remarks>
public sealed class TExtension : MarkupExtension
{
    public TExtension()
    {
    }

    public TExtension(string ключ) => Ключ = ключ;

    /// <summary>Ключ у каталозі, наприклад <c>ui-download</c>.</summary>
    public string Ключ { get; set; } = "";

    public override object ProvideValue(IServiceProvider serviceProvider) => Каталог.T(Ключ);
}
