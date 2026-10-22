// SPDX-License-Identifier: Apache-2.0
// A minimal, dependency-free column splitter: grab it and drag to resize
// the adjacent panel; double-click resets to the default width.

using System;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;

namespace Fastmark;

public sealed class Splitter : Grid
{
    private readonly Border _thumb = new()
    {
        Width = 4,
        HorizontalAlignment = HorizontalAlignment.Center,
        VerticalAlignment = VerticalAlignment.Stretch,
        CornerRadius = new CornerRadius(2),
        Background = null,
    };

    private ColumnDefinition? _column;
    private double _defaultWidth = 280;
    private double _min = 200;
    private double _max = 520;
    private Func<double, double>? _widthFromX;
    private bool _dragging;

    public Splitter()
    {
        Width = 14;
        Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent); // hit-testable
        Children.Add(_thumb);
        ProtectedCursor = InputSystemCursor.Create(InputSystemCursorShape.SizeWestEast);
        PointerEntered += (_, _) =>
        {
            _thumb.Background = (Brush)Application.Current.Resources["ControlStrokeColorDefaultBrush"];
        };
        PointerExited += (_, _) => _thumb.Background = null;
        PointerPressed += OnPressed;
        PointerMoved += OnMoved;
        PointerReleased += OnReleased;
        DoubleTapped += (_, _) => Reset();
    }

    /// <summary>
    /// Bind to the panel column. `widthFromX` maps a window-relative X to the
    /// desired column width (the two splitters mirror each other).
    /// </summary>
    public void Attach(ColumnDefinition column, double defaultWidth, double min, double max,
                       Func<double, double> widthFromX)
    {
        _column = column;
        _defaultWidth = defaultWidth;
        _min = min;
        _max = max;
        _widthFromX = widthFromX;
    }

    public void Reset()
    {
        if (_column is not null)
        {
            _column.Width = new GridLength(_defaultWidth);
        }
    }

    private void OnPressed(object sender, PointerRoutedEventArgs e)
    {
        if (_column is null)
        {
            return;
        }
        _dragging = true;
        CapturePointer(e.Pointer);
        e.Handled = true;
    }

    private void OnMoved(object sender, PointerRoutedEventArgs e)
    {
        if (!_dragging || _column is null)
        {
            return;
        }
        var x = e.GetCurrentPoint(null).Position.X;
        var w = _widthFromX?.Invoke(x) ?? x;
        _column.Width = new GridLength(Math.Clamp(w, _min, _max));
        e.Handled = true;
    }

    private void OnReleased(object sender, PointerRoutedEventArgs e)
    {
        if (_dragging)
        {
            _dragging = false;
            ReleasePointerCapture(e.Pointer);
            e.Handled = true;
            MainWindow.SaveLayout();
        }
    }
}
