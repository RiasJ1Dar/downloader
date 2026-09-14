using System;
using System.Collections.Generic;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;

namespace Downloader.Ui.Controls;

/// <summary>
/// Графік швидкості за останні секунди.
/// </summary>
/// <remarks>
/// Одне число «2,4 МБ/с» не відрізняє рівне качання від пилки, що падає до
/// нуля щопівсекунди — а це різні хвороби: перше впирається в канал, друге в
/// сервер або в диск. Форма кривої показує це з першого погляду.
///
/// <para>
/// ⚠️ Масштаб по вертикалі — **за максимумом у вікні історії**, а не
/// абсолютний. Інакше на повільному завданні крива лежала б на дні
/// незмінною лінією, і графік не показував би нічого.
/// </para>
///
/// <para>
/// Про <see cref="Revision"/>: зразки передаються тим самим списком, який
/// дописується на місці — копіювати сто двадцять чисел щочверть секунди для
/// кожного рядка немає сенсу. Але тоді Avalonia не бачить зміни: посилання не
/// змінилось. Ревізія — і є той видимий слід, за яким перемальовуємо.
/// </para>
/// </remarks>
public sealed class SpeedChart : Control
{
    /// <summary>Зразки швидкості, байтів на секунду. Найстаріший — перший.</summary>
    public static readonly StyledProperty<IReadOnlyList<double>?> SamplesProperty =
        AvaloniaProperty.Register<SpeedChart, IReadOnlyList<double>?>(nameof(Samples));

    /// <summary>Лічильник змін: зростає — графік перемальовується.</summary>
    public static readonly StyledProperty<int> RevisionProperty =
        AvaloniaProperty.Register<SpeedChart, int>(nameof(Revision));

    /// <summary>Колір кривої.</summary>
    public static readonly StyledProperty<IBrush> LineBrushProperty =
        AvaloniaProperty.Register<SpeedChart, IBrush>(
            nameof(LineBrush),
            new SolidColorBrush(Color.FromRgb(0x4C, 0x9A, 0xFF)));

    /// <summary>Колір заливки під кривою.</summary>
    public static readonly StyledProperty<IBrush> AreaBrushProperty =
        AvaloniaProperty.Register<SpeedChart, IBrush>(
            nameof(AreaBrush),
            new SolidColorBrush(Color.FromArgb(0x38, 0x4C, 0x9A, 0xFF)));

    static SpeedChart()
    {
        AffectsRender<SpeedChart>(
            SamplesProperty,
            RevisionProperty,
            LineBrushProperty,
            AreaBrushProperty);
    }

    public IReadOnlyList<double>? Samples
    {
        get => GetValue(SamplesProperty);
        set => SetValue(SamplesProperty, value);
    }

    public int Revision
    {
        get => GetValue(RevisionProperty);
        set => SetValue(RevisionProperty, value);
    }

    public IBrush LineBrush
    {
        get => GetValue(LineBrushProperty);
        set => SetValue(LineBrushProperty, value);
    }

    public IBrush AreaBrush
    {
        get => GetValue(AreaBrushProperty);
        set => SetValue(AreaBrushProperty, value);
    }

    public override void Render(DrawingContext context)
    {
        Rect поле = new(Bounds.Size);
        IReadOnlyList<double>? зразки = Samples;

        if (поле.Width <= 1 || поле.Height <= 1 || зразки is null || зразки.Count < 2)
        {
            return;
        }

        МалюватиСітку(context, поле);

        double верх = 0;
        foreach (double s in зразки)
        {
            верх = Math.Max(верх, s);
        }

        // Усе нулі — малювати нічого. Плоска лінія по низу створює враження
        // «графік працює, швидкість нуль», хоча завдання може просто стояти.
        if (верх <= 0)
        {
            return;
        }

        // Запас зверху, щоб пік не тикався в межу.
        верх *= 1.15;

        double крок = поле.Width / (зразки.Count - 1);

        var крива = new StreamGeometry();
        var площа = new StreamGeometry();

        using (StreamGeometryContext c = крива.Open())
        using (StreamGeometryContext a = площа.Open())
        {
            Point перша = Точка(0, зразки[0], крок, верх, поле);

            c.BeginFigure(перша, isFilled: false);
            a.BeginFigure(new Point(перша.X, поле.Height), isFilled: true);
            a.LineTo(перша);

            for (int i = 1; i < зразки.Count; i++)
            {
                Point p = Точка(i, зразки[i], крок, верх, поле);
                c.LineTo(p);
                a.LineTo(p);
            }

            a.LineTo(new Point(поле.Width, поле.Height));
            a.EndFigure(isClosed: true);
            c.EndFigure(isClosed: false);
        }

        context.DrawGeometry(AreaBrush, null, площа);
        context.DrawGeometry(null, new Pen(LineBrush, 1.5), крива);
    }

    private static Point Точка(int i, double значення, double крок, double верх, Rect поле)
    {
        double y = поле.Height - поле.Height * Math.Clamp(значення / верх, 0, 1);
        return new Point(i * крок, y);
    }

    /// <summary>
    /// Три горизонтальні риски.
    /// </summary>
    /// <remarks>
    /// Не для точності — числа все одно підписані збоку, — а щоб око мало за
    /// що зачепитись і бачило, наскільки крива просіла.
    /// </remarks>
    private static void МалюватиСітку(DrawingContext context, Rect поле)
    {
        var pen = new Pen(new SolidColorBrush(Color.FromArgb(0x22, 0x88, 0x88, 0x88)), 1);

        for (int i = 1; i <= 3; i++)
        {
            double y = Math.Round(поле.Height * i / 4.0) + 0.5;
            context.DrawLine(pen, new Point(0, y), new Point(поле.Width, y));
        }
    }
}
