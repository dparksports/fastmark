// SPDX-License-Identifier: Apache-2.0
using Microsoft.UI.Xaml;

namespace Fastmark;

public partial class App : Application
{
    public App()
    {
        InitializeComponent();
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        var file = args.Arguments is { Length: > 0 } a ? a : null;
        new MainWindow(file).Activate();
    }
}
