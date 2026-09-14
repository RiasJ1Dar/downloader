using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Avalonia.Threading;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using Downloader.Ui.Ipc;

namespace Downloader.Ui.ViewModels;

/// <summary>
/// Головне вікно: список завдань і команди до ядра.
/// </summary>
/// <remarks>
/// Вікно **нічого не рахує саме**. Швидкість, залишок часу й відсоток
/// приходять із ядра готовими — інакше вікно й CLI показували б різні числа
/// для того самого завдання.
///
/// <para>
/// Так само вікно **не опитує** ядро в циклі: воно підписується й чекає
/// знімків. П'ятдесят завдань по чотири рази на секунду — це двісті
/// повідомлень щосекунди, якби кожне завдання слало власну подію.
/// </para>
/// </remarks>
public sealed partial class MainViewModel : ObservableObject
{
    private readonly CancellationTokenSource _cts = new();

    /// <summary>З'єднання для команд. Окреме від того, що слухає події.</summary>
    private CoreClient? _commands;

    /// <summary>
    /// Третє з'єднання — для розкладки сегментів.
    /// </summary>
    /// <remarks>
    /// Окреме навмисно. Протокол — запит-відповідь без ідентифікаторів
    /// звернень, тож двоє, хто пише в одну трубу, рано чи пізно розберуть
    /// чужу відповідь як свою. Дешевше тримати ще одну трубу, ніж
    /// вигадувати теґи звернень.
    /// </remarks>
    private CoreClient? _details;

    [ObservableProperty]
    private string _newUrl = "";

    [ObservableProperty]
    private string _status = "під'єднуюсь до ядра…";

    [ObservableProperty]
    private bool _connected;

    /// <summary>
    /// Виділений рядок — єдиний, для якого питається розкладка сегментів.
    /// </summary>
    [ObservableProperty]
    private TaskRow? _selected;

    public ObservableCollection<TaskRow> Tasks { get; } = new();

    public MainViewModel()
    {
        _ = ConnectLoopAsync();
        _ = DetailsLoopAsync();
    }

    /// <summary>
    /// Триматись за ядро: під'єднатись, слухати, а як обірветься — чекати й
    /// пробувати знову.
    /// </summary>
    /// <remarks>
    /// Ядро може перезапуститись (оновлення, падіння), і вікно має це
    /// пережити мовчки. Вимагати від людини перезапускати вікно — найгірший
    /// із можливих варіантів.
    /// </remarks>
    private async Task ConnectLoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            try
            {
                _commands = await CoreClient.ConnectAsync(_cts.Token);
                _details = await CoreClient.ConnectAsync(_cts.Token);

                await Dispatcher.UIThread.InvokeAsync(() =>
                {
                    Connected = true;
                    Status = "з'єднано з ядром";
                });

                await ListenAsync();
            }
            catch (CoreNotRunningException)
            {
                await ЗакритиТрубиAsync();
                await Dispatcher.UIThread.InvokeAsync(() =>
                {
                    Connected = false;
                    Status = "ядро не запущене — запустіть downloader-core";
                });
            }
            catch (Exception e)
            {
                await ЗакритиТрубиAsync();
                await Dispatcher.UIThread.InvokeAsync(() =>
                {
                    Connected = false;
                    Status = $"зв'язок із ядром обірвався: {e.Message}";
                });
            }

            // Пауза перед новою спробою: інакше при вимкненому ядрі вікно
            // крутило б цикл на повній швидкості.
            try
            {
                await Task.Delay(TimeSpan.FromSeconds(2), _cts.Token);
            }
            catch (OperationCanceledException)
            {
                return;
            }
        }
    }

    /// <summary>
    /// Закрити з'єднання, які вже не працюють.
    /// </summary>
    /// <remarks>
    /// Без цього кожна невдала спроба лишала по трубі: ядро тримало б сотні
    /// мертвих з'єднань за годину вимкненого вікна.
    /// </remarks>
    private async Task ЗакритиТрубиAsync()
    {
        CoreClient? commands = _commands;
        CoreClient? details = _details;
        _commands = null;
        _details = null;

        foreach (CoreClient? c in new[] { commands, details })
        {
            if (c is null)
            {
                continue;
            }

            try
            {
                await c.DisposeAsync();
            }
            catch (Exception)
            {
                // Труба вже мертва — саме тому ми тут. Закриття закритого
                // нічого не означає.
            }
        }
    }

    /// <summary>
    /// Питати розкладку сегментів для виділеного завдання.
    /// </summary>
    /// <remarks>
    /// Це єдине місце, де вікно **опитує** ядро замість того, щоб чекати
    /// події, — і причина в тому, що розкладка потрібна для одного рядка з
    /// п'ятдесяти. Слати її всім у кожному знімку означало б платити за
    /// сорок дев'ять непотрібних.
    ///
    /// <para>
    /// Питаємо рідше, ніж приходять знімки: смужка сегментів не бігає, а
    /// дихає — двічі на секунду цілком достатньо, щоб побачити роботу поділу.
    /// </para>
    /// </remarks>
    private async Task DetailsLoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            try
            {
                await Task.Delay(TimeSpan.FromMilliseconds(500), _cts.Token);
            }
            catch (OperationCanceledException)
            {
                return;
            }

            TaskRow? row = Selected;
            CoreClient? client = _details;

            if (row is null || client is null)
            {
                continue;
            }

            // Завдання, яке стоїть, розкладки не змінює. Але й прибирати вже
            // намальовану не треба: людина має бачити, де саме вона зупинила
            // качання.
            if (!row.Активне)
            {
                continue;
            }

            try
            {
                Response resp = await client.CallAsync(new DetailsRequest(row.Id), _cts.Token);

                if (!resp.IsError && resp.Parts is not null)
                {
                    List<PartView> parts = resp.Parts;
                    await Dispatcher.UIThread.InvokeAsync(() => row.ЗастосуватиРозкладку(parts));
                }
            }
            catch (OperationCanceledException)
            {
                return;
            }
            catch (Exception)
            {
                // Зв'язок обірвався — про це вже дізнається основний цикл і
                // перепід'єднається. Тут лишається тільки не впасти.
                continue;
            }
        }
    }

    /// <summary>Слухати знімки списку, доки з'єднання живе.</summary>
    private async Task ListenAsync()
    {
        await using var events = await CoreClient.ConnectAsync(_cts.Token);

        await foreach (CoreEvent ev in events.SubscribeAsync(_cts.Token))
        {
            switch (ev.Kind)
            {
                case "snapshot" when ev.Tasks is not null:
                    await Dispatcher.UIThread.InvokeAsync(() => ApplySnapshot(ev.Tasks));
                    break;

                case "finished":
                    await Dispatcher.UIThread.InvokeAsync(() =>
                        Status = $"готово: {ev.Path}");
                    break;

                case "failed":
                    await Dispatcher.UIThread.InvokeAsync(() =>
                        Status = $"завдання {ev.Id} впало: {ev.Message}");
                    break;
            }
        }
    }

    /// <summary>
    /// Накласти знімок на список.
    /// </summary>
    /// <remarks>
    /// ⚠️ Рядки оновлюються **на місці**, а не перестворюються. Інакше
    /// список щочверть секунди скидав би виділення й позицію прокрутки —
    /// користуватися ним було б неможливо.
    /// </remarks>
    private void ApplySnapshot(List<TaskView> snapshot)
    {
        foreach (TaskView view in snapshot)
        {
            TaskRow? row = Tasks.FirstOrDefault(t => t.Id == view.Id);
            if (row is null)
            {
                Tasks.Add(new TaskRow(view));
            }
            else
            {
                row.Update(view);
            }
        }

        // Прибрати те, чого в ядрі вже немає.
        for (int i = Tasks.Count - 1; i >= 0; i--)
        {
            if (snapshot.All(v => v.Id != Tasks[i].Id))
            {
                if (ReferenceEquals(Selected, Tasks[i]))
                {
                    Selected = null;
                }

                Tasks.RemoveAt(i);
            }
        }

        Упорядкувати(snapshot);
    }

    /// <summary>
    /// Привести порядок рядків до порядку зі знімка.
    /// </summary>
    /// <remarks>
    /// ⚠️ Без цього нові завдання опинялися б **унизу**, хоч ядро віддає їх
    /// першими: новий рядок дописується в кінець колекції, бо в знімку його
    /// ще не було. Людина додає посилання й не бачить його там, де щойно
    /// дивилась.
    ///
    /// <para>
    /// Переставляємо через <c>Move</c>, а не перезбиранням списку: перезбирання
    /// скинуло б виділення й позицію прокрутки.
    /// </para>
    /// </remarks>
    private void Упорядкувати(List<TaskView> snapshot)
    {
        for (int i = 0; i < snapshot.Count && i < Tasks.Count; i++)
        {
            if (Tasks[i].Id == snapshot[i].Id)
            {
                continue;
            }

            for (int j = i + 1; j < Tasks.Count; j++)
            {
                if (Tasks[j].Id == snapshot[i].Id)
                {
                    Tasks.Move(j, i);
                    break;
                }
            }
        }
    }

    /// <summary>
    /// Виділення перейшло на інший рядок.
    /// </summary>
    /// <remarks>
    /// Розкладку попереднього треба **забути**: її вже ніхто не оновлює, і
    /// смужка застигла б на випадковому кадрі. Застигла картинка, яка
    /// виглядає живою, гірша за просту смужку прогресу.
    /// </remarks>
    partial void OnSelectedChanged(TaskRow? oldValue, TaskRow? newValue)
    {
        oldValue?.ЗабутиРозкладку();
    }

    [RelayCommand]
    private async Task AddAsync()
    {
        string url = NewUrl.Trim();
        if (url.Length == 0 || _commands is null)
        {
            return;
        }

        Response resp = await _commands.CallAsync(new AddRequest(url), _cts.Token);

        if (resp.IsError)
        {
            Status = resp.Message ?? "ядро відмовило без пояснення";
            return;
        }

        NewUrl = "";
        Status = $"завдання {resp.Id} прийнято";
    }

    [RelayCommand]
    private async Task PauseAsync(TaskRow? row)
    {
        if (row is null || _commands is null)
        {
            return;
        }

        Response resp = await _commands.CallAsync(new PauseRequest(row.Id), _cts.Token);
        if (resp.IsError)
        {
            Status = resp.Message ?? "не вдалося зупинити";
        }
    }

    [RelayCommand]
    private async Task ResumeAsync(TaskRow? row)
    {
        if (row is null || _commands is null)
        {
            return;
        }

        Response resp = await _commands.CallAsync(new ResumeRequest(row.Id), _cts.Token);
        if (resp.IsError)
        {
            Status = resp.Message ?? "не вдалося продовжити";
        }
    }

    [RelayCommand]
    private async Task RemoveAsync(TaskRow? row)
    {
        if (row is null || _commands is null)
        {
            return;
        }

        // Файл не чіпаємо: видалення з диска — окрема, незворотна дія, і
        // робити її «заодно» з прибиранням рядка не можна.
        Response resp = await _commands.CallAsync(
            new RemoveRequest(row.Id, WithFile: false),
            _cts.Token);

        if (resp.IsError)
        {
            Status = resp.Message ?? "не вдалося прибрати";
        }
    }

    public void Shutdown() => _cts.Cancel();
}

/// <summary>Рядок списку — те саме завдання, але з повідомленнями про зміни.</summary>
public sealed partial class TaskRow : ObservableObject
{
    [ObservableProperty]
    private string _name = "";

    [ObservableProperty]
    private string _status = "";

    [ObservableProperty]
    private double _progress;

    [ObservableProperty]
    private string _sizeText = "";

    [ObservableProperty]
    private string _speedText = "";

    [ObservableProperty]
    private string _etaText = "";

    [ObservableProperty]
    private string? _error;

    /// <summary>Повний розмір — масштаб для смужки сегментів.</summary>
    [ObservableProperty]
    private ulong? _total;

    /// <summary>Частка виконаного, 0…1 — для смужки без розкладки.</summary>
    [ObservableProperty]
    private double _fraction;

    /// <summary>Розкладка частин. Заповнена лише для виділеного рядка.</summary>
    [ObservableProperty]
    private IReadOnlyList<PartView>? _parts;

    /// <summary>Скільки разів змінювалась історія швидкості.</summary>
    /// <remarks>
    /// Графік дивиться саме на це число. Сам список дописується на місці —
    /// див. <see cref="SpeedHistory"/>.
    /// </remarks>
    [ObservableProperty]
    private int _speedRevision;

    /// <summary>Чи качається просто зараз.</summary>
    public bool Активне { get; private set; }

    /// <summary>Найвища швидкість у вікні історії, текстом.</summary>
    [ObservableProperty]
    private string _peakText = "";

    /// <summary>Скільки частин качається просто зараз.</summary>
    [ObservableProperty]
    private string _segmentsText = "";

    /// <summary>Звідки качається — показуємо в деталях.</summary>
    [ObservableProperty]
    private string _url = "";

    /// <summary>Скільки зразків швидкості тримати.</summary>
    /// <remarks>
    /// Знімки приходять чотири рази на секунду, отже сто двадцять зразків —
    /// це тридцять секунд. Довша історія робить із кривої пляму, коротша не
    /// показує провалів.
    /// </remarks>
    private const int ГлибинаІсторії = 120;

    private readonly List<double> _speeds = new(ГлибинаІсторії);

    /// <summary>
    /// Історія швидкості, найстаріше першим.
    /// </summary>
    /// <remarks>
    /// Той **самий** список від початку до кінця життя рядка: копіювати сто
    /// двадцять чисел щочверть секунди для кожного з п'ятдесяти завдань —
    /// двадцять чотири тисячі копій на секунду ні для чого.
    /// </remarks>
    public IReadOnlyList<double> SpeedHistory => _speeds;

    public long Id { get; }

    public TaskRow(TaskView view)
    {
        Id = view.Id;
        Update(view);
    }

    public void Update(TaskView view)
    {
        Name = view.Name;
        Status = ЛюдськийСтан(view.Status);
        Progress = view.Progress is { } p ? p * 100 : 0;
        Fraction = view.Progress ?? 0;
        Total = view.Total;
        Error = view.Error;
        Активне = view.Status == "running";

        Url = view.Url;

        SegmentsText = view.Segments > 1 ? $"{view.Segments} частин" : "";

        ДописатиШвидкість(view.Speed);

        SizeText = view.Total is { } total
            ? $"{Розмір(view.Done)} / {Розмір(total)}"
            : Розмір(view.Done);

        SpeedText = view.Speed > 0 ? $"{Розмір(view.Speed)}/с" : "";

        EtaText = view.EtaSecs is { } eta && eta > 0 ? Час(eta) : "";
    }

    /// <summary>Додати зразок швидкості й посунути вікно історії.</summary>
    private void ДописатиШвидкість(ulong speed)
    {
        _speeds.Add(speed);

        // Зсув на одну позицію коштує сто двадцять переміщень `double` — це
        // дешевше за ринг-буфер із його розгортанням при кожному малюванні.
        if (_speeds.Count > ГлибинаІсторії)
        {
            _speeds.RemoveAt(0);
        }

        SpeedRevision++;

        double пік = 0;
        foreach (double v in _speeds)
        {
            пік = Math.Max(пік, v);
        }

        PeakText = пік > 0 ? $"пік {Розмір((ulong)пік)}/с" : "";
    }

    /// <summary>Покласти нову розкладку частин.</summary>
    /// <remarks>
    /// Порожню розкладку не затираємо: ядро віддає порожньо й тоді, коли
    /// завдання щойно стало на паузу, а намальовані сегменти — саме те, що
    /// людина в цей момент розглядає.
    /// </remarks>
    public void ЗастосуватиРозкладку(IReadOnlyList<PartView> parts)
    {
        if (parts.Count > 0)
        {
            Parts = parts;
        }
    }

    /// <summary>Забути розкладку — рядок більше не виділений.</summary>
    public void ЗабутиРозкладку() => Parts = null;

    /// <summary>Стан людською мовою, а не кодом протоколу.</summary>
    private static string ЛюдськийСтан(string raw) => raw switch
    {
        "queued" => "у черзі",
        "running" => "качається",
        "paused" => "зупинено",
        "done" => "готово",
        "failed" => "помилка",
        var інше => інше,
    };

    private static string Розмір(ulong bytes)
    {
        string[] одиниці = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
        double value = bytes;
        int unit = 0;

        while (value >= 1024 && unit + 1 < одиниці.Length)
        {
            value /= 1024;
            unit++;
        }

        return unit == 0 ? $"{bytes} {одиниці[unit]}" : $"{value:0.#} {одиниці[unit]}";
    }

    private static string Час(ulong secs) => secs switch
    {
        < 60 => $"{secs} с",
        < 3600 => $"{secs / 60} хв",
        _ => $"{secs / 3600} год {secs % 3600 / 60} хв",
    };
}
