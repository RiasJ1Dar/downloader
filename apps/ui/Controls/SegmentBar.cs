using System;
using System.Collections.Generic;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;
using Avalonia.Styling;
using Downloader.Ui.Ipc;

namespace Downloader.Ui.Controls;

/// <summary>
/// Смужка сегментів: де файл уже є, а де ще діра.
/// </summary>
/// <remarks>
/// Це не декорація. Файл качається з кількох місць одночасно, і звичайна
/// смужка прогресу («73 %») цього не показує — вона однаково виглядає і коли
/// працюють вісім потоків, і коли лишився один, що тягне хвіст. Саме тому це
/// вікно в IDM відкривають: щоб побачити, чи поділ живий.
///
/// <para>
/// ⚠️ Частини приходять із ядра й **не обов'язково впорядковані**: динамічний
/// поділ вставляє нову частину в середину, коли вільний робітник краде
/// половину залишку найповільнішого. Тому малюємо кожну на її власному місці
/// за <c>start</c>, а не одну за одною.
/// </para>
///
/// <para>
/// Порожня розкладка — не помилка, а звичайний стан: завдання стоїть, уже
/// завершене або качається одним потоком. Тоді показуємо просте тло з
/// часткою виконаного, щоб смужка не блимала при паузі.
/// </para>
/// </remarks>
public sealed class SegmentBar : Control
{
    /// <summary>Частини завантаження. <c>null</c> або порожньо — простий режим.</summary>
    public static readonly StyledProperty<IReadOnlyList<PartView>?> PartsProperty =
        AvaloniaProperty.Register<SegmentBar, IReadOnlyList<PartView>?>(nameof(Parts));

    /// <summary>Повний розмір файлу. Без нього немає масштабу.</summary>
    public static readonly StyledProperty<ulong?> TotalProperty =
        AvaloniaProperty.Register<SegmentBar, ulong?>(nameof(Total));

    /// <summary>
    /// Частка виконаного (0…1) для простого режиму — коли розкладки немає.
    /// </summary>
    public static readonly StyledProperty<double> FractionProperty =
        AvaloniaProperty.Register<SegmentBar, double>(nameof(Fraction));

    /// <summary>Колір готових байтів.</summary>
    public static readonly StyledProperty<IBrush> DoneBrushProperty =
        AvaloniaProperty.Register<SegmentBar, IBrush>(
            nameof(DoneBrush),
            new SolidColorBrush(Color.FromRgb(0x4C, 0x9A, 0xFF)));

    /// <summary>Колір ще не завантаженого.</summary>
    public static readonly StyledProperty<IBrush> HoleBrushProperty =
        AvaloniaProperty.Register<SegmentBar, IBrush>(
            nameof(HoleBrush),
            new SolidColorBrush(Color.FromArgb(0x33, 0x80, 0x80, 0x80)));

    static SegmentBar()
    {
        // Без цього смужка перемалювалась би лише при зміні розміру вікна:
        // числа оновлюються, картинка стоїть.
        AffectsRender<SegmentBar>(
            PartsProperty,
            TotalProperty,
            FractionProperty,
            DoneBrushProperty,
            HoleBrushProperty);
    }

    public IReadOnlyList<PartView>? Parts
    {
        get => GetValue(PartsProperty);
        set => SetValue(PartsProperty, value);
    }

    public ulong? Total
    {
        get => GetValue(TotalProperty);
        set => SetValue(TotalProperty, value);
    }

    public double Fraction
    {
        get => GetValue(FractionProperty);
        set => SetValue(FractionProperty, value);
    }

    public IBrush DoneBrush
    {
        get => GetValue(DoneBrushProperty);
        set => SetValue(DoneBrushProperty, value);
    }

    public IBrush HoleBrush
    {
        get => GetValue(HoleBrushProperty);
        set => SetValue(HoleBrushProperty, value);
    }

    public override void Render(DrawingContext context)
    {
        Rect поле = new(Bounds.Size);
        if (поле.Width <= 0 || поле.Height <= 0)
        {
            return;
        }

        context.FillRectangle(HoleBrush, поле, 2);

        IReadOnlyList<PartView>? parts = Parts;
        ulong total = Total ?? 0;

        if (parts is null || parts.Count == 0 || total == 0)
        {
            МалюватиПросто(context, поле);
            return;
        }

        foreach (PartView part in parts)
        {
            if (part.Done == 0)
            {
                continue;
            }

            double x = поле.Width * part.Start / total;

            // Частину, меншу за піксель, все одно видно як волосину: інакше
            // щойно вкраденого сегмента не було б помітно доти, доки він не
            // накачає відчутний шмат.
            double w = Math.Max(1.0, поле.Width * part.Done / total);

            // Обрізаємо по правому краю: `done` може на тік випереджати
            // `total`, поки розмір ще уточнюється.
            if (x >= поле.Width)
            {
                continue;
            }

            w = Math.Min(w, поле.Width - x);

            context.FillRectangle(DoneBrush, new Rect(x, 0, w, поле.Height), 2);
        }

        МалюватиМежі(context, поле, parts, total);
    }

    /// <summary>Один суцільний шмат — коли розкладки немає.</summary>
    private void МалюватиПросто(DrawingContext context, Rect поле)
    {
        double частка = Math.Clamp(Fraction, 0, 1);
        if (частка <= 0)
        {
            return;
        }

        context.FillRectangle(
            DoneBrush,
            new Rect(0, 0, поле.Width * частка, поле.Height),
            2);
    }

    /// <summary>
    /// Тонкі риски на початках частин.
    /// </summary>
    /// <remarks>
    /// Без них дві сусідні частини, що зійшлися впритул, читаються як одна
    /// велика — і людина бачить «усе добре» там, де насправді сім потоків із
    /// восьми вже закінчили роботу.
    /// </remarks>
    private static void МалюватиМежі(
        DrawingContext context,
        Rect поле,
        IReadOnlyList<PartView> parts,
        ulong total)
    {
        bool dark = Application.Current?.ActualThemeVariant == ThemeVariant.Dark;
        var pen = new Pen(
            new SolidColorBrush(
                dark
                    ? Color.FromArgb(0x66, 0xFF, 0xFF, 0xFF)
                    : Color.FromArgb(0x55, 0, 0, 0)),
            1);

        foreach (PartView part in parts)
        {
            if (part.Start == 0)
            {
                continue;
            }

            double x = Math.Round(поле.Width * part.Start / total) + 0.5;
            if (x <= 0 || x >= поле.Width)
            {
                continue;
            }

            context.DrawLine(pen, new Point(x, 0), new Point(x, поле.Height));
        }
    }
}
