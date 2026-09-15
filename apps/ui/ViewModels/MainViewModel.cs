using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Diagnostics;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Input.Platform;
using Avalonia.Styling;
using Avalonia.Threading;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using Downloader.Ui.I18n;
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
    private string _status = Каталог.T("ui-connecting");

    [ObservableProperty]
    private bool _connected;

    /// <summary>Варіанти якості для посилання, яке зараз у полі.</summary>
    public ObservableCollection<VariantRow> Variants { get; } = new();

    /// <summary>Обраний варіант. <c>null</c> — вибір ще не зроблено.</summary>
    [ObservableProperty]
    private VariantRow? _selectedVariant;

    /// <summary>Чи показувати панель вибору якості.</summary>
    [ObservableProperty]
    private bool _qualityOpen;

    /// <summary>Чи триває проба посилання.</summary>
    [ObservableProperty]
    private bool _probing;

    /// <summary>
    /// Виділений рядок — єдиний, для якого питається розкладка сегментів.
    /// </summary>
    [ObservableProperty]
    private TaskRow? _selected;

    [ObservableProperty]
    private string _maxConcurrentText = "3";

    [ObservableProperty]
    private string _rateLimitText = "0";

    /// <summary>0 — нічого, 1 — сон, 2 — вимкнути ПК.</summary>
    [ObservableProperty]
    private int _postActionIndex;

    [ObservableProperty]
    private string _scheduleFromText = "";

    [ObservableProperty]
    private string _scheduleToText = "";

    [ObservableProperty]
    private string _quietFromText = "";

    [ObservableProperty]
    private string _quietToText = "";

    [ObservableProperty]
    private string _quietRateText = "0";

    /// <summary>
    /// Тема вікна. Не йде в ядро: IPC і <c>tasks.db</c> про вигляд не знають.
    /// </summary>
    [ObservableProperty]
    private bool _darkTheme = true;

    public ObservableCollection<TaskRow> Tasks { get; } = new();

    public MainViewModel()
    {
        DarkTheme = UiPrefs.LoadDark();
        ApplyTheme(DarkTheme);
        _ = ConnectLoopAsync();
        _ = DetailsLoopAsync();
    }

    partial void OnDarkThemeChanged(bool value)
    {
        ApplyTheme(value);
        UiPrefs.SaveDark(value);
    }

    private static void ApplyTheme(bool dark)
    {
        if (Application.Current is { } app)
        {
            app.RequestedThemeVariant = dark ? ThemeVariant.Dark : ThemeVariant.Light;
        }
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
                    Status = Каталог.T("ui-connected");
                });

                await LoadSettingsAsync();
                await ListenAsync();
            }
            catch (CoreNotRunningException)
            {
                await ЗакритиТрубиAsync();
                await Dispatcher.UIThread.InvokeAsync(() =>
                {
                    Connected = false;
                    Status = Каталог.T("ui-core-missing");
                });
            }
            catch (Exception e)
            {
                await ЗакритиТрубиAsync();
                await Dispatcher.UIThread.InvokeAsync(() =>
                {
                    Connected = false;
                    Status = Каталог.T("ui-link-lost", ("message", e.Message));
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
                        Status = Каталог.T("ui-finished", ("path", ev.Path ?? "")));
                    break;

                case "failed":
                    await Dispatcher.UIThread.InvokeAsync(() =>
                        Status = Каталог.T("task-failed", ("id", ev.Id ?? 0), ("message", ev.Message ?? "")));
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
        var existingById = new Dictionary<long, TaskRow>(Tasks.Count);
        foreach (TaskRow task in Tasks)
        {
            existingById[task.Id] = task;
        }

        var snapshotIds = new HashSet<long>(snapshot.Count);
        foreach (TaskView view in snapshot)
        {
            snapshotIds.Add(view.Id);
            if (existingById.TryGetValue(view.Id, out TaskRow? row))
            {
                row.Update(view);
            }
            else
            {
                Tasks.Add(new TaskRow(view));
            }
        }

        // Прибрати те, чого в ядрі вже немає (O(1) перевірка через HashSet замість O(N*M) All).
        for (int i = Tasks.Count - 1; i >= 0; i--)
        {
            if (!snapshotIds.Contains(Tasks[i].Id))
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

    private async Task LoadSettingsAsync()
    {
        if (_commands is null)
        {
            return;
        }

        try
        {
            Response resp = await _commands.CallAsync(new SettingsRequest(), _cts.Token);
            if (resp.Kind == "settings")
            {
                await Dispatcher.UIThread.InvokeAsync(() =>
                {
                    if (resp.MaxConcurrent is uint n)
                    {
                        MaxConcurrentText = n.ToString();
                    }

                    if (resp.RateLimit is ulong r)
                    {
                        RateLimitText = (r / 1024).ToString();
                    }

                    PostActionIndex = resp.PostAction switch
                    {
                        "sleep" => 1,
                        "shutdown" => 2,
                        _ => 0,
                    };
                    ScheduleFromText = resp.ScheduleFrom ?? "";
                    ScheduleToText = resp.ScheduleTo ?? "";
                    QuietFromText = resp.QuietFrom ?? "";
                    QuietToText = resp.QuietTo ?? "";
                    QuietRateText = ((resp.QuietRate ?? 0) / 1024).ToString();
                });
            }
        }
        catch (Exception)
        {
            // Старе ядро без Settings — поля лишаються типовими.
        }
    }

    [RelayCommand]
    private async Task ApplySettingsAsync()
    {
        if (_commands is null)
        {
            return;
        }

        uint? max = uint.TryParse(MaxConcurrentText, out uint n) && n >= 1 ? n : null;
        ulong? rate = ulong.TryParse(RateLimitText, out ulong kb) ? kb * 1024 : null;
        string after = PostActionIndex switch
        {
            1 => "sleep",
            2 => "shutdown",
            _ => "none",
        };
        ulong? quiet = ulong.TryParse(QuietRateText, out ulong qkb) ? qkb * 1024 : 0;

        Response resp = await _commands.CallAsync(
            new ConfigureRequest(
                max,
                rate,
                after,
                ScheduleFromText,
                ScheduleToText,
                QuietFromText,
                QuietToText,
                quiet),
            _cts.Token);
        Status = resp.IsError
            ? resp.Message ?? Каталог.T("ui-apply-failed")
            : Каталог.T("set-applied");
    }

    [RelayCommand]
    private void OpenFolder(TaskRow? row)
    {
        string? dest = row?.Dest;
        if (string.IsNullOrEmpty(dest))
        {
            Status = Каталог.T("ui-path-unknown");
            return;
        }

        try
        {
            Process.Start(new ProcessStartInfo
            {
                FileName = "explorer.exe",
                Arguments = $"/select,\"{dest}\"",
                UseShellExecute = true,
            });
        }
        catch (Exception e)
        {
            Status = Каталог.T("ui-open-failed", ("message", e.Message));
        }
    }

    [RelayCommand]
    private async Task AddAsync()
    {
        string url = NewUrl.Trim();
        if (url.Length == 0 || _commands is null)
        {
            return;
        }

        Response resp = await _commands.CallAsync(
            new AddRequest(url, Variant: SelectedVariant?.Id),
            _cts.Token);

        if (resp.IsError)
        {
            Status = resp.Message ?? Каталог.T("ui-core-refused");
            return;
        }

        NewUrl = "";
        ЗакритиВибір();
        Status = Каталог.T("ui-task-accepted", ("id", resp.Id ?? 0));
    }

    /// <summary>
    /// Показати, які якості пропонує посилання.
    /// </summary>
    /// <remarks>
    /// Окрема дія, а не частина «Завантажити»: проба коштує мережевого
    /// звернення, а для YouTube — ще й запуску yt-dlp на кілька секунд.
    /// Платити цю затримку кожному, хто просто качає файл, не варто.
    /// </remarks>
    [RelayCommand]
    private async Task ShowQualityAsync()
    {
        string url = NewUrl.Trim();
        if (url.Length == 0 || _commands is null || Probing)
        {
            return;
        }

        Probing = true;
        Status = Каталог.T("ui-probing");

        try
        {
            Response resp = await _commands.CallAsync(new VariantsRequest(url), _cts.Token);

            if (resp.IsError)
            {
                Status = resp.Message ?? Каталог.T("ui-core-refused");
                return;
            }

            Variants.Clear();
            foreach (VariantView v in resp.Variants ?? new List<VariantView>())
            {
                Variants.Add(new VariantRow(v));
            }

            if (Variants.Count == 0)
            {
                // Не помилка: звичайний файл має один вигляд.
                Status = Каталог.T("no-variants");
                QualityOpen = false;
                return;
            }

            SelectedVariant = Variants[0];
            QualityOpen = true;
            Status = "";
        }
        catch (Exception e)
        {
            Status = Каталог.T("ui-link-lost", ("message", e.Message));
        }
        finally
        {
            Probing = false;
        }
    }

    /// <summary>Закрити вибір, не обираючи нічого.</summary>
    [RelayCommand]
    private void CancelQuality() => ЗакритиВибір();

    private void ЗакритиВибір()
    {
        QualityOpen = false;
        SelectedVariant = null;
        Variants.Clear();
    }

    /// <summary>Вставити http(s) з буфера й одразу додати, якщо є посилання.</summary>
    [RelayCommand]
    private async Task PasteAddAsync()
    {
        IClipboard? clip = ClipboardOfWindow();
        if (clip is null)
        {
            Status = Каталог.T("ui-clipboard-unavailable");
            return;
        }

        string? text;
        try
        {
            text = await clip.TryGetTextAsync();
        }
        catch (Exception e)
        {
            Status = Каталог.T("ui-clipboard-error", ("message", e.Message));
            return;
        }

        if (string.IsNullOrWhiteSpace(text))
        {
            Status = Каталог.T("ui-clipboard-empty");
            return;
        }

        string? url = FirstHttp(text);
        if (url is null)
        {
            NewUrl = text.Trim();
            Status = Каталог.T("ui-clipboard-nolink");
            return;
        }

        NewUrl = url;
        await AddAsync();
    }

    private static IClipboard? ClipboardOfWindow()
    {
        if (Application.Current?.ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            return desktop.MainWindow?.Clipboard;
        }

        return null;
    }

    private static string? FirstHttp(string text)
    {
        foreach (string raw in text.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
        {
            string line = raw.Trim();
            if (line.StartsWith("http://", StringComparison.OrdinalIgnoreCase)
                || line.StartsWith("https://", StringComparison.OrdinalIgnoreCase))
            {
                if (line.Contains("w3.org/", StringComparison.OrdinalIgnoreCase)
                    || line.Contains("schema.org", StringComparison.OrdinalIgnoreCase))
                {
                    continue;
                }

                return line;
            }
        }

        return null;
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
            Status = resp.Message ?? Каталог.T("ui-pause-failed");
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
            Status = resp.Message ?? Каталог.T("ui-resume-failed");
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
            Status = resp.Message ?? Каталог.T("ui-remove-failed");
        }
    }

    public void Shutdown() => _cts.Cancel();
}

/// <summary>Один варіант якості у переліку.</summary>
public sealed class VariantRow
{
    public VariantRow(VariantView v)
    {
        Id = v.Id;
        Label = v.Label;

        string розмір = v.Size is { } b ? TaskRow.ЛюдськийРозмір(b) : "";
        string нота = v.Note ?? "";

        // «720p · 12,4 МБ · avc1» — рівно те, чого вистачає для вибору.
        Details = string.Join(
            " · ",
            new[] { розмір, нота }.Where(x => x.Length > 0));
    }

    public string Id { get; }

    public string Label { get; }

    /// <summary>Розмір і кодек одним рядком.</summary>
    public string Details { get; }
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

    [ObservableProperty]
    private string? _dest;

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
        Dest = view.Dest;

        SegmentsText = view.Segments > 1
            ? $"{view.Segments} {Каталог.Множина(view.Segments, "ui-seg-one", "ui-seg-few", "ui-seg-many")}"
            : "";

        ДописатиШвидкість(view.Speed);

        SizeText = view.Total is { } total
            ? $"{ЛюдськийРозмір(view.Done)} / {ЛюдськийРозмір(total)}"
            : ЛюдськийРозмір(view.Done);

        SpeedText = view.Speed > 0
            ? ЛюдськийРозмір(view.Speed) + Каталог.T("ui-per-sec")
            : "";

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

        PeakText = пік > 0
            ? Каталог.T("ui-peak", ("speed", ЛюдськийРозмір((ulong)пік) + Каталог.T("ui-per-sec")))
            : "";
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
        "queued" => Каталог.T("ui-st-queued"),
        "running" => Каталог.T("ui-st-running"),
        "paused" => Каталог.T("ui-st-paused"),
        "done" => Каталог.T("ui-st-done"),
        "failed" => Каталог.T("ui-st-failed"),
        var інше => інше,
    };

    internal static string ЛюдськийРозмір(ulong bytes)
    {
        string[] одиниці =
        [
            Каталог.T("ui-b"), Каталог.T("ui-kb"), Каталог.T("ui-mb"),
            Каталог.T("ui-gb"), Каталог.T("ui-tb"),
        ];
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
        < 60 => $"{secs} {Каталог.T("ui-sec")}",
        < 3600 => $"{secs / 60} {Каталог.T("ui-min")}",
        _ => $"{secs / 3600} {Каталог.T("ui-hour")} {secs % 3600 / 60} {Каталог.T("ui-min")}",
    };
}
