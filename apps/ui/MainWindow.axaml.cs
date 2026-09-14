using Avalonia.Controls;
using Downloader.Ui.ViewModels;

namespace Downloader.Ui;

public partial class MainWindow : Window
{
    public MainWindow()
    {
        InitializeComponent();

        // Вікно — клієнт ядра, а не власник рушія. Уся робота лишається в
        // ядрі, навіть коли вікно закрите.
        var vm = new MainViewModel();
        DataContext = vm;

        // ⚠️ Вікно **закривається**, а не ховається (Р-02). Приховане вікно
        // тримало б у пам'яті весь UI-стек; завантаження живуть у ядрі, тож
        // ховати нема чого.
        Closed += (_, _) => vm.Shutdown();
    }
}