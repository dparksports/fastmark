// SPDX-License-Identifier: Apache-2.0
// P/Invoke bridge to fastmark_ffi.dll (the Rust engine) plus the lazy,
// virtualized block source. The UI never materializes the document - a
// block is fetched from Rust only when a ListView container realizes it.

using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;

namespace Fastmark;

public sealed class DocHandle : IDisposable
{
    public IntPtr Handle { get; private set; }
    public string Path { get; }
    public string Encoding { get; }

    private DocHandle(IntPtr h, string path, string encoding)
    {
        Handle = h;
        Path = path;
        Encoding = encoding;
    }

    public static DocHandle? Open(string path)
    {
        var h = Ffi.fm_open(Ffi.Utf8(path));
        if (h == IntPtr.Zero)
        {
            return null;
        }
        var encoding = Ffi.CallString((buf, cap) => Ffi.fm_encoding(h, buf, cap)) ?? "UTF-8";
        return new DocHandle(h, path, encoding);
    }

    public uint BlockCount => Ffi.fm_block_count(Handle);

    public BlockVm? Block(int index)
    {
        var json = Ffi.CallString((buf, cap) => Ffi.fm_block_json(Handle, (uint)index, buf, cap));
        if (json is null)
        {
            return null;
        }
        return BlockVm.Parse(index, json);
    }

    public List<TocEntry> Toc()
    {
        var json = Ffi.CallString((buf, cap) => Ffi.fm_toc_json(Handle, buf, cap));
        var list = new List<TocEntry>();
        if (json is null)
        {
            return list;
        }
        using var doc = JsonDocument.Parse(json);
        foreach (var e in doc.RootElement.EnumerateArray())
        {
            list.Add(new TocEntry(
                (byte)e.GetProperty("level").GetInt32(),
                (uint)e.GetProperty("block").GetInt32(),
                (uint)e.GetProperty("byte").GetInt32(),
                e.GetProperty("text").GetString() ?? ""));
        }
        return list;
    }

    public List<(uint byteOffset, uint len)> Search(string needle, int limit)
    {
        var hits = new List<(uint, uint)>();
        var json = Ffi.CallString((buf, cap) =>
            Ffi.fm_search_json(Handle, Ffi.Utf8(needle), (uint)limit, buf, cap));
        if (json is null)
        {
            return hits;
        }
        using var doc = JsonDocument.Parse(json);
        foreach (var e in doc.RootElement.EnumerateArray())
        {
            hits.Add(((uint)e.GetProperty("byte").GetInt64(), (uint)e.GetProperty("len").GetInt32()));
        }
        return hits;
    }

    public (string snippet, int line) LineAt(uint byteOffset)
    {
        var json = Ffi.CallString((buf, cap) => Ffi.fm_line_at(Handle, byteOffset, buf, cap));
        if (json is null)
        {
            return ("", 0);
        }
        using var doc = JsonDocument.Parse(json);
        return (doc.RootElement.GetProperty("snippet").GetString() ?? "",
                doc.RootElement.GetProperty("line").GetInt32());
    }

    public uint ByteToBlock(uint byteOffset) => Ffi.fm_byte_to_block(Handle, byteOffset);

    public void Dispose()
    {
        if (Handle != IntPtr.Zero)
        {
            Ffi.fm_release(Handle);
            Handle = IntPtr.Zero;
        }
        GC.SuppressFinalize(this);
    }
}

public sealed record TocEntry(byte Level, uint Block, uint Byte, string Text);

public sealed class BlockVm
{
    public int Index { get; internal set; }
    public string Kind { get; private set; } = "paragraph";
    public int Level { get; private set; }
    public int Depth { get; private set; }
    public string Marker { get; private set; } = "";
    public string? Lang { get; private set; }
    public string? Url { get; private set; }
    public string? Alt { get; private set; }
    public List<LineVm> Lines { get; } = new();
    public List<string[]> Rows { get; } = new();
    public List<int> Aligns { get; } = new();

    public sealed class LineVm
    {
        public string Text = "";
        public readonly List<(int start, int len, int style)> Spans = new();
    }

    // Identity is the block index, so ListView.ScrollIntoView works even
    // when the LRU cache has recycled instances.
    public override bool Equals(object? obj) => obj is BlockVm other && other.Index == Index;
    public override int GetHashCode() => Index;

    public static BlockVm Parse(int index, string json)
    {
        var vm = new BlockVm { Index = index };
        using var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;
        vm.Kind = root.GetProperty("kind").GetString() ?? "paragraph";
        vm.Level = root.TryGetProperty("level", out var lvl) ? lvl.GetInt32() : 0;
        vm.Depth = root.TryGetProperty("depth", out var d) ? d.GetInt32() : 0;
        vm.Marker = root.TryGetProperty("marker", out var m) ? m.GetString() ?? "" : "";
        vm.Lang = root.TryGetProperty("lang", out var l) ? l.GetString() : null;
        vm.Url = root.TryGetProperty("url", out var u) ? u.GetString() : null;
        vm.Alt = root.TryGetProperty("alt", out var a) ? a.GetString() : null;
        if (root.TryGetProperty("lines", out var lines))
        {
            foreach (var line in lines.EnumerateArray())
            {
                var lv = new LineVm { Text = line.GetProperty("text").GetString() ?? "" };
                foreach (var span in line.GetProperty("spans").EnumerateArray())
                {
                    lv.Spans.Add((span[0].GetInt32(), span[1].GetInt32(), span[2].GetInt32()));
                }
                vm.Lines.Add(lv);
            }
        }
        if (root.TryGetProperty("rows", out var rows))
        {
            foreach (var row in rows.EnumerateArray())
            {
                vm.Rows.Add(row.EnumerateArray().Select(c => c.GetString() ?? "").ToArray());
            }
        }
        if (root.TryGetProperty("aligns", out var aligns))
        {
            foreach (var al in aligns.EnumerateArray())
            {
                vm.Aligns.Add(al.GetInt32());
            }
        }
        return vm;
    }
}

/// <summary>
/// IReadOnlyList over the whole document with a small LRU: ListView's
/// virtualizing panel realizes only visible items, so this pulls a block
/// from Rust exactly when it becomes visible.
/// </summary>
public sealed class LazyBlockList : IReadOnlyList<BlockVm>
{
    private readonly DocHandle _doc;
    private readonly Dictionary<int, BlockVm> _cache = new();
    private readonly LinkedList<int> _lru = new();
    private const int Cap = 256;

    public LazyBlockList(DocHandle doc) => _doc = doc;

    public int Count => (int)_doc.BlockCount;

    public BlockVm this[int index]
    {
        get
        {
            if (_cache.TryGetValue(index, out var vm))
            {
                Touch(index);
                return vm;
            }
            vm = _doc.Block(index) ?? new BlockVm { Index = index };
            _cache[index] = vm;
            _lru.AddLast(index);
            if (_lru.Count > Cap)
            {
                _cache.Remove(_lru.First!.Value);
                _lru.RemoveFirst();
            }
            return vm;
        }
    }

    private void Touch(int index)
    {
        var node = _lru.Find(index);
        if (node is not null)
        {
            _lru.Remove(node);
            _lru.AddLast(index);
        }
    }

    public IEnumerator<BlockVm> GetEnumerator()
    {
        for (int i = 0; i < Count; i++)
        {
            yield return this[i];
        }
    }

    System.Collections.IEnumerator System.Collections.IEnumerable.GetEnumerator() => GetEnumerator();
}

internal static class Ffi
{
    private const string Dll = "fastmark_ffi";

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern IntPtr fm_open(byte[] pathUtf8);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern void fm_release(IntPtr handle);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern uint fm_block_count(IntPtr handle);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern int fm_block_json(IntPtr handle, uint index, IntPtr buf, int cap);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern int fm_toc_json(IntPtr handle, IntPtr buf, int cap);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern int fm_search_json(IntPtr handle, byte[] needleUtf8, uint limit, IntPtr buf, int cap);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern int fm_line_at(IntPtr handle, uint byteOffset, IntPtr buf, int cap);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern uint fm_byte_to_block(IntPtr handle, uint byteOffset);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern int fm_encoding(IntPtr handle, IntPtr buf, int cap);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern int fm_last_error(IntPtr buf, int cap);

    internal static unsafe byte[] Utf8(string s)
    {
        var bytes = Encoding.UTF8.GetBytes(s);
        var withNul = new byte[bytes.Length + 1];
        bytes.CopyTo(withNul, 0);
        return withNul;
    }

    /// <summary>Buffer-fill convention: size probe, then fill.</summary>
    internal static unsafe string? CallString(Func<IntPtr, int, int> call)
    {
        int needed = call(IntPtr.Zero, 0);
        if (needed <= 1)
        {
            return needed == 1 ? string.Empty : null;
        }
        var buf = new byte[needed];
        fixed (byte* p = buf)
        {
            int written = call((IntPtr)p, needed);
            if (written <= 0)
            {
                return null;
            }
            return Encoding.UTF8.GetString(buf, 0, written - 1);
        }
    }

    internal static string LastError() =>
        CallString(fm_last_error) ?? "unknown error";
}
