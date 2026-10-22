// SPDX-License-Identifier: Apache-2.0
// The native shell: all UI is WinUI 3; documents come from the Rust engine
// through Native.cs, one JSON block per realized container.

using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Shapes;
using Path = System.IO.Path;
using Iw = WinRT.Interop.InitializeWithWindow;
using Windows.Storage.Pickers;

namespace Fastmark;

public sealed partial class MainWindow : Window
{
    private static MainWindow? _main;

    private DocHandle? _doc;
    private LazyBlockList? _blocks;
    private string _root = Directory.GetCurrentDirectory();
    private CancellationTokenSource? _searchCts;

    public MainWindow(string? startFile)
    {
        InitializeComponent();
        SystemBackdrop = new MicaBackdrop();
        Title = "fastmark";

        // Resolve early: arguments (and shell paths) may be relative to a
        // working directory that differs from ours.
        string? abs = null;
        if (startFile is not null)
        {
            try
            {
                abs = Path.GetFullPath(startFile);
            }
            catch
            {
                abs = null;
            }
        }
        if (abs is not null && File.Exists(abs))
        {
            Log($"start file resolved: {abs}");
            OpenDocument(abs);
            _root = Path.GetDirectoryName(abs) ?? _root;
        }
        else
        {
            if (startFile is not null)
            {
                Log($"start file unusable: '{startFile}' -> '{abs}'");
            }
            ShowEmpty();
        }
        _main = this;
        AttachSplitters();
        RestoreLayout();
        _ = BuildTreeAsync(_root);
    }

    // ------------------------------------------------------------------
    // Panel layout: resizable + collapsible side panes, persisted
    // ------------------------------------------------------------------

    private void AttachSplitters()
    {
        LeftSplitter.Attach(LeftColumn, 280, 200, 480, x => x);
        RightSplitter.Attach(RightColumn, 280, 200, 520,
            x => Root.ActualWidth > 0 ? Root.ActualWidth - x : x);
    }

    private static string LayoutFile() => System.IO.Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
        "fastmark", "layout.txt");

    internal static void SaveLayout()
    {
        if (_main is null)
        {
            return;
        }
        try
        {
            var lines = new[]
            {
                $"left={_main.LeftColumn.Width.Value:0}",
                $"right={_main.RightColumn.Width.Value:0}",
            };
            System.IO.File.WriteAllLines(LayoutFile(), lines);
        }
        catch
        {
            /* layout persistence is best-effort */
        }
    }

    private void RestoreLayout()
    {
        try
        {
            if (!System.IO.File.Exists(LayoutFile()))
            {
                return;
            }
            foreach (var line in System.IO.File.ReadAllLines(LayoutFile()))
            {
                var kv = line.Split('=', 2);
                if (kv.Length != 2)
                {
                    continue;
                }
                if (double.TryParse(kv[1], out var w))
                {
                    if (kv[0] == "left")
                    {
                        LeftColumn.Width = new GridLength(Math.Clamp(w, 200, 480));
                    }
                    else if (kv[0] == "right")
                    {
                        RightColumn.Width = new GridLength(Math.Clamp(w, 200, 520));
                    }
                }
            }
        }
        catch
        {
            /* fresh layout on any parse trouble */
        }
    }

    private void SetLeftPanel(bool visible)
    {
        LeftPanel.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
        LeftSplitter.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
        LeftColumn.Width = visible ? new GridLength(LeftColumn.Width.Value > 0
            ? LeftColumn.Width.Value : 280) : new GridLength(0);
    }

    private void SetRightPanel(bool visible)
    {
        RightPanel.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
        RightSplitter.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
        RightColumn.Width = visible ? new GridLength(RightColumn.Width.Value > 0
            ? RightColumn.Width.Value : 280) : new GridLength(0);
    }

    private bool LeftVisible => LeftPanel.Visibility == Visibility.Visible;
    private bool RightVisible => RightPanel.Visibility == Visibility.Visible;

    private void OnCollapseLeft(object sender, RoutedEventArgs e)
    {
        SetLeftPanel(false);
        FilesToggle.IsChecked = false;
    }

    private void OnCollapseRight(object sender, RoutedEventArgs e)
    {
        SetRightPanel(false);
        OutlineToggle.IsChecked = false;
    }

    private void OnToggleLeft(object sender, RoutedEventArgs e)
    {
        SetLeftPanel(!LeftVisible);
        FilesToggle.IsChecked = LeftVisible;
        SaveLayout();
    }

    private void OnToggleRight(object sender, RoutedEventArgs e)
    {
        SetRightPanel(!RightVisible);
        OutlineToggle.IsChecked = RightVisible;
        SaveLayout();
    }



    // ------------------------------------------------------------------
    // Document
    // ------------------------------------------------------------------

    private void ShowEmpty()
    {
        EmptyState.Visibility = Visibility.Visible;
        Reader.Visibility = Visibility.Collapsed;
        Results.Visibility = Visibility.Collapsed;
    }

    private void OpenDocument(string path)
    {
        var doc = DocHandle.Open(path);
        if (doc is null)
        {
            Log($"open FAILED: {path}: {Ffi.LastError()}");
            _ = ShowErrorAsync($"Cannot open {Path.GetFileName(path)}: {Ffi.LastError()}");
            return;
        }
        Log($"open ok: {path} blocks={doc.BlockCount} enc={doc.Encoding}");
        _doc?.Dispose();
        _doc = doc;
        _blocks = new LazyBlockList(doc);
        Title = $"{Path.GetFileName(path)} — fastmark";
        Reader.ItemsSource = _blocks;
        Reader.Visibility = Visibility.Visible;
        Results.Visibility = Visibility.Collapsed;
        EmptyState.Visibility = Visibility.Collapsed;
        LoadOutline(doc);
        ScrollToBlock(0);
    }

    private void LoadOutline(DocHandle doc)
    {
        var items = new List<TocItem>();
        foreach (var e in doc.Toc())
        {
            items.Add(new TocItem(e.Text, new Thickness(4 + (e.Level - 1) * 12, 2, 4, 2),
                                  e.Level <= 2 ? 13.0 : 12.0, e.Block));
        }
        OutlineList.ItemsSource = items;
    }

    private void ScrollToBlock(int index)
    {
        if (_blocks is null || index < 0 || index >= _blocks.Count)
        {
            return;
        }
        Reader.ScrollIntoView(_blocks[index]);
        Reader.UpdateLayout();
    }

    private void OnOutlineClick(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is TocItem item)
        {
            ScrollToBlock((int)item.Block);
        }
    }

    // ------------------------------------------------------------------
    // Reader rendering: one container per block, visuals built in code
    // ------------------------------------------------------------------

    private int _realized;

    private void OnBlockRealized(ListViewBase sender, ContainerContentChangingEventArgs args)
    {
        if (args.InRecycleQueue)
        {
            if (args.ItemContainer.ContentTemplateRoot is ContentPresenter cp)
            {
                cp.Content = null;
            }
            return;
        }
        if (args.Item is not BlockVm vm)
        {
            return;
        }
        try
        {
            if (args.ItemContainer.ContentTemplateRoot is ContentPresenter cp)
            {
                cp.Content = BuildBlockVisual(vm);
                _realized++;
                if (_realized <= 3)
                {
                    Log($"realized block {vm.Index} kind={vm.Kind} lines={vm.Lines.Count}");
                }
            }
            else
            {
                Log($"block {vm.Index}: no ContentPresenter root");
            }
        }
        catch (Exception ex)
        {
            Log($"block {vm.Index} render failed: {ex.Message}");
        }
        args.Handled = true;
    }

    internal static void Log(string message)
    {
        try
        {
            System.IO.File.AppendAllText(
                System.IO.Path.Combine(System.IO.Path.GetTempPath(), "fastmark-native.log"),
                $"{DateTime.Now:HH:mm:ss.fff} {message}\n");
        }
        catch
        {
            /* logging must never crash the app */
        }
    }

    private UIElement BuildBlockVisual(BlockVm vm)
    {
        switch (vm.Kind)
        {
            case "rule":
                return new Rectangle
                {
                    Height = 1,
                    Margin = new Thickness(0, 14, 0, 14),
                    Fill = Dim(),
                    Opacity = 0.35,
                };
            case "image":
            {
                var sp = new StackPanel { Margin = new Thickness(0, 8, 0, 8) };
                if (vm.Url is { } url && !url.StartsWith("http") && _doc is not null)
                {
                    try
                    {
                        var abs = Path.GetFullPath(Path.Combine(
                            Path.GetDirectoryName(_doc.Path) ?? ".", url));
                        if (File.Exists(abs))
                        {
                            sp.Children.Add(new Image
                            {
                                Source = new Microsoft.UI.Xaml.Media.Imaging.BitmapImage(new Uri(abs)),
                                MaxHeight = 560,
                                Stretch = Stretch.Uniform,
                                HorizontalAlignment = HorizontalAlignment.Left,
                            });
                        }
                    }
                    catch
                    {
                        /* relative image outside the tree */
                    }
                }
                sp.Children.Add(new TextBlock
                {
                    Text = $"↗ {vm.Alt}",
                    Opacity = 0.55,
                    FontSize = 12,
                    Margin = new Thickness(0, 4, 0, 0),
                });
                return sp;
            }
            case "table":
            {
                var sp = new StackPanel();
                var widths = ColumnWidths(vm);
                for (int r = 0; r < vm.Rows.Count; r++)
                {
                    var cells = new List<string>();
                    for (int c = 0; c < vm.Rows[r].Length; c++)
                    {
                        var text = vm.Rows[r][c];
                        var w = c < widths.Count ? widths[c] : 3;
                        cells.Add(c == 0 ? text.PadRight(w) : text.PadLeft(w));
                    }
                    var header = r == 0;
                    sp.Children.Add(new TextBlock
                    {
                        Text = string.Join("  ", cells),
                        FontFamily = Mono,
                        FontSize = 12.5,
                        FontWeight = header ? Microsoft.UI.Text.FontWeights.SemiBold
                                            : Microsoft.UI.Text.FontWeights.Normal,
                        Opacity = header ? 1.0 : 0.85,
                    });
                }
                return Card(sp);
            }
            case "code" or "html":
            {
                var sp = new StackPanel();
                foreach (var line in vm.Lines)
                {
                    sp.Children.Add(new TextBlock
                    {
                        Text = line.Text,
                        FontFamily = Mono,
                        FontSize = 12.5,
                        IsTextSelectionEnabled = true,
                    });
                }
                return Card(sp);
            }
            default:
                return BuildRich(vm);
        }
    }

    private static List<int> ColumnWidths(BlockVm vm)
    {
        var n = vm.Rows.Count == 0 ? 0 : vm.Rows.Max(r => r.Length);
        var widths = Enumerable.Repeat(3, n).ToList();
        foreach (var row in vm.Rows)
        {
            for (int c = 0; c < row.Length; c++)
            {
                widths[c] = Math.Max(widths[c], row[c].Length);
            }
        }
        return widths;
    }

    private UIElement BuildRich(BlockVm vm)
    {
        double size = vm.Kind switch
        {
            "heading" => vm.Level switch { 1 => 26, 2 => 20, 3 => 17, 4 => 15, _ => 14 },
            _ => 14,
        };
        var weight = vm.Kind == "heading" && vm.Level <= 2
            ? Microsoft.UI.Text.FontWeights.SemiBold
            : Microsoft.UI.Text.FontWeights.Normal;
        var indent = vm.Depth > 0
            ? new Thickness(16 + (vm.Depth - 1) * 18, 2, 0, 2)
            : new Thickness(0, 2, 0, 2);

        var rtb = new RichTextBlock
        {
            TextWrapping = TextWrapping.Wrap,
            IsTextSelectionEnabled = true,
        };
        for (int li = 0; li < vm.Lines.Count; li++)
        {
            var line = vm.Lines[li];
            var para = new Paragraph();
            if (vm.Kind == "item" && li == 0 && vm.Marker.Length > 0)
            {
                para.Inlines.Add(new Run
                {
                    Text = vm.Marker,
                    FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
                    FontSize = size,
                });
            }
            foreach (var (start, len, style) in line.Spans)
            {
                var len2 = Math.Min(len, Math.Max(0, line.Text.Length - start));
                if (len2 == 0)
                {
                    continue;
                }
                var run = new Run
                {
                    Text = line.Text.Substring(start, len2),
                    FontSize = size,
                    FontWeight = weight,
                };
                if ((style & 2) != 0)
                {
                    run.FontStyle = Windows.UI.Text.FontStyle.Italic;
                }
                if ((style & 1) != 0)
                {
                    run.FontWeight = Microsoft.UI.Text.FontWeights.Bold;
                }
                if ((style & 4) != 0)
                {
                    run.FontFamily = Mono;
                    run.FontSize = size - 1.5;
                }
                if ((style & 8) != 0)
                {
                    run.Foreground = Accent();
                }
                else if ((style & 16) != 0 || (style & 32) != 0)
                {
                    run.Foreground = Dim();
                }
                para.Inlines.Add(run);
            }
            rtb.Blocks.Add(para);
        }

        if (vm.Kind == "quote")
        {
            var grid = new Grid { Margin = indent };
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(3) });
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(12) });
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            var bar = new Rectangle { Fill = Dim(), Opacity = 0.5, RadiusX = 1.5, RadiusY = 1.5 };
            Grid.SetColumn(bar, 0);
            grid.Children.Add(bar);
            Grid.SetColumn(rtb, 2);
            grid.Children.Add(rtb);
            return grid;
        }
        rtb.Margin = indent;
        return rtb;
    }

    private Border Card(UIElement child) => new()
    {
        Child = child,
        Padding = new Thickness(12, 10, 12, 10),
        Margin = new Thickness(0, 6, 0, 6),
        CornerRadius = new CornerRadius(8),
        Background = (Brush)Application.Current.Resources["CardBackgroundFillColorDefaultBrush"],
        BorderBrush = (Brush)Application.Current.Resources["CardStrokeColorDefaultBrush"],
        BorderThickness = new Thickness(1),
    };

    private static FontFamily Mono => new("Cascadia Mono, Consolas");

    private Brush Accent() =>
        (Brush)Application.Current.Resources["AccentTextFillColorPrimaryBrush"];

    private Brush Dim() =>
        (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"];

    // ------------------------------------------------------------------
    // File tree (lazy; skip rules mirror the Rust walker)
    // ------------------------------------------------------------------

    private static readonly string[] SkipDirs =
    {
        ".git", ".hg", ".svn", ".venv", "node_modules", "target", "out", "build", "dist",
        "__pycache__", "AppData", "Application Data", "Local Settings", "$RECYCLE.BIN",
        "ProgramData", "bin", "obj",
    };

    private sealed class FsNode
    {
        public FsNode(string name, string path, bool isDir)
        {
            Name = name;
            Path = path;
            IsDir = isDir;
        }

        public string Name { get; }
        public string Path { get; }
        public bool IsDir { get; }
        public override string ToString() => Name;
    }

    private async Task BuildTreeAsync(string root)
    {
        FileTree.RootNodes.Clear();
        var node = await MakeDirNodeAsync(root, topLevel: true);
        if (node is not null)
        {
            FileTree.RootNodes.Add(node);
            node.IsExpanded = true;
            Log($"tree root added: {root} children={node.Children.Count}");
        }
    }

    private static bool IsHidden(FileSystemInfo info)
    {
        try
        {
            return (info.Attributes & FileAttributes.Hidden) != 0;
        }
        catch
        {
            return true;
        }
    }

    private async Task<TreeViewNode?> MakeDirNodeAsync(string dir, bool topLevel)
    {
        var node = new TreeViewNode
        {
            Content = new FsNode(Path.GetFileName(dir.TrimEnd('\\', '/')), dir, true),
        };
        await FillChildrenAsync(node);
        if (!topLevel && node.Children.Count == 0)
        {
            return null; // no Markdown anywhere beneath - prune
        }
        return node;
    }

    private async Task FillChildrenAsync(TreeViewNode node)
    {
        if (node.Content is not FsNode { IsDir: true } self)
        {
            return;
        }
        var dir = self.Path;
        node.Children.Clear();
        try
        {
            var dirs = Directory.EnumerateDirectories(dir)
                .Where(d => !IsHidden(new DirectoryInfo(d)))
                .Where(d => !SkipDirs.Contains(
                    new DirectoryInfo(d).Name, StringComparer.OrdinalIgnoreCase))
                .OrderBy(d => d, StringComparer.OrdinalIgnoreCase);
            foreach (var sub in dirs)
            {
                var child = await MakeDirNodeAsync(sub, topLevel: false);
                if (child is not null)
                {
                    node.Children.Add(child);
                }
            }
            var files = Directory.EnumerateFiles(dir)
                .Where(f => !IsHidden(new FileInfo(f)))
                .Where(f => Path.GetExtension(f).ToLowerInvariant()
                    is ".md" or ".markdown" or ".mdx")
                .OrderBy(f => f, StringComparer.OrdinalIgnoreCase);
            foreach (var file in files)
            {
                node.Children.Add(new TreeViewNode
                {
                    Content = new FsNode(Path.GetFileName(file), file, false),
                });
            }
        }
        catch (UnauthorizedAccessException)
        {
            /* protected dir: leave whatever loaded */
        }
        node.HasUnrealizedChildren = false;
    }

    private async void OnTreeInvoke(TreeView sender, TreeViewItemInvokedEventArgs args)
    {
        if (args.InvokedItem is not TreeViewNode node)
        {
            return;
        }
        if (node.Content is FsNode fs)
        {
            if (!fs.IsDir)
            {
                OpenDocument(fs.Path);
            }
            else if (node.HasUnrealizedChildren)
            {
                await FillChildrenAsync(node);
            }
        }
    }

    // ------------------------------------------------------------------
    // Pickers + theme
    // ------------------------------------------------------------------

    private async void OnPickFolder(object sender, RoutedEventArgs e)
    {
        var picker = new FolderPicker { SuggestedStartLocation = PickerLocationId.DocumentsLibrary };
        picker.FileTypeFilter.Add("*");
        Iw.Initialize(
            picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        var folder = await picker.PickSingleFolderAsync();
        if (folder is not null)
        {
            _root = folder.Path;
            await BuildTreeAsync(_root);
        }
    }

    private async void OnOpenFile(object sender, RoutedEventArgs e)
    {
        var picker = new FileOpenPicker { SuggestedStartLocation = PickerLocationId.DocumentsLibrary };
        picker.FileTypeFilter.Add(".md");
        picker.FileTypeFilter.Add(".markdown");
        picker.FileTypeFilter.Add(".mdx");
        Iw.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        var file = await picker.PickSingleFileAsync();
        if (file is not null)
        {
            OpenDocument(file.Path);
        }
    }

    private void OnToggleTheme(object sender, RoutedEventArgs e)
    {
        Root.RequestedTheme = Root.RequestedTheme == ElementTheme.Dark
            ? ElementTheme.Light
            : ElementTheme.Dark;
    }

    // ------------------------------------------------------------------
    // Folder-wide search (Rust engine per file, streamed into the list)
    // ------------------------------------------------------------------

    private async void OnSearchSubmit(AutoSuggestBox sender, AutoSuggestBoxQuerySubmittedEventArgs args)
    {
        var query = args.QueryText.Trim();
        if (query.Length == 0 || _searchCts is not null)
        {
            return;
        }
        _searchCts = new CancellationTokenSource();
        var ct = _searchCts.Token;
        var hits = new ObservableCollection<HitVm>();
        Results.ItemsSource = hits;
        Results.Visibility = Visibility.Visible;
        Reader.Visibility = Visibility.Collapsed;
        EmptyState.Visibility = Visibility.Collapsed;
        Title = $"search “{query}” — fastmark";

        try
        {
            foreach (var file in EnumerateMarkdown(_root))
            {
                if (ct.IsCancellationRequested || hits.Count > 400)
                {
                    break;
                }
                using var doc = DocHandle.Open(file);
                if (doc is null)
                {
                    continue;
                }
                foreach (var (byteOffset, _) in doc.Search(query, 20))
                {
                    var (snippet, line) = doc.LineAt(byteOffset);
                    hits.Add(new HitVm(Path.GetFileName(file), file, line, $"{line}", snippet, byteOffset));
                }
            }
        }
        catch
        {
            /* walker races with the filesystem */
        }
        finally
        {
            _searchCts = null;
        }
    }

    private static IEnumerable<string> EnumerateMarkdown(string root)
    {
        var stack = new Stack<string>();
        stack.Push(root);
        while (stack.Count > 0)
        {
            var dir = stack.Pop();
            foreach (var e in SafeEntries(dir))
            {
                bool keep = false, descend = false;
                try
                {
                    var attr = File.GetAttributes(e);
                    if ((attr & FileAttributes.Directory) != 0)
                    {
                        var name = Path.GetFileName(e);
                        descend = !(name.StartsWith('.') || name.StartsWith('$') ||
                            SkipDirs.Contains(name, StringComparer.OrdinalIgnoreCase) ||
                            (attr & FileAttributes.Hidden) != 0 ||
                            (attr & FileAttributes.ReparsePoint) != 0);
                    }
                    else
                    {
                        keep = Path.GetExtension(e).ToLowerInvariant()
                            is ".md" or ".markdown" or ".mdx";
                    }
                }
                catch
                {
                    /* skip unreadable entries */
                }
                if (descend)
                {
                    stack.Push(e);
                }
                else if (keep)
                {
                    yield return e;
                }
            }
        }
    }

    private static List<string> SafeEntries(string dir)
    {
        try
        {
            return Directory.EnumerateFileSystemEntries(dir).ToList();
        }
        catch
        {
            return new List<string>();
        }
    }

    private void OnResultClick(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is not HitVm hit)
        {
            return;
        }
        if (_doc is null ||
            !string.Equals(_doc.Path, hit.Path, StringComparison.OrdinalIgnoreCase))
        {
            OpenDocument(hit.Path);
        }
        if (_doc is not null)
        {
            var block = (int)_doc.ByteToBlock(hit.Byte);
            if (block >= 0 && block < _doc.BlockCount)
            {
                ScrollToBlock(block);
            }
        }
        Reader.Visibility = Visibility.Visible;
        Results.Visibility = Visibility.Collapsed;
    }

    private async Task ShowErrorAsync(string message) =>
        await new ContentDialog
        {
            Title = "fastmark",
            Content = message,
            CloseButtonText = "OK",
            XamlRoot = Content.XamlRoot,
        }.ShowAsync();
}

public sealed record TocItem(string Text, Thickness Indent, double Size, uint Block);

public sealed record HitVm(
    string File, string Path, int Line, string LineLabel, string Snippet, uint Byte);
