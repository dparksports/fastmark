// SPDX-License-Identifier: Apache-2.0
using System;
using System.Linq;
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
        // Unpackaged plain-exe launches may not populate args.Arguments;
        // the real command line is always in Environment.GetCommandLineArgs.
        var argv = Environment.GetCommandLineArgs()
            .Skip(1)
            .Where(a => !a.StartsWith('-') && !a.StartsWith('/'))
            .ToArray();
        var file = argv.Length > 0
            ? argv[0]
            : (args.Arguments is { Length: > 0 } via ? via : null);
        new MainWindow(file).Activate();
    }
}
