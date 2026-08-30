<#
.SYNOPSIS
    Builds MTUI's preview PNG and multi-size Windows icons.

.DESCRIPTION
    Draws four original MTUI marks at 512px, then creates every Windows icon
    size from those masters. Signal combines an open terminal
    prompt with an audio meter; Wave is an oscilloscope trace; Mono is a quiet
    monochrome Signal. Orbit adds a circular music cue without using YouTube's
    red concentric disc or play-button silhouette.

    assets/mtui-icon.svg is the portable vector version of the same artwork.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/make-icon.ps1
#>
[CmdletBinding()]
param(
    [string]$Canvas,
    [string]$Output,
    [string]$WaveOutput,
    [string]$MonoOutput,
    [string]$OrbitOutput
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
if (-not $Canvas) { $Canvas = Join-Path $root 'canvas.png' }
if (-not $Output) { $Output = Join-Path $root 'assets\mtui.ico' }
if (-not $WaveOutput) { $WaveOutput = Join-Path $root 'assets\mtui-wave.ico' }
if (-not $MonoOutput) { $MonoOutput = Join-Path $root 'assets\mtui-mono.ico' }
if (-not $OrbitOutput) { $OrbitOutput = Join-Path $root 'assets\mtui-orbit.ico' }

$canvasDir = Split-Path -Parent $Canvas
$outDir = Split-Path -Parent $Output
$waveOutDir = Split-Path -Parent $WaveOutput
$monoOutDir = Split-Path -Parent $MonoOutput
$orbitOutDir = Split-Path -Parent $OrbitOutput
foreach ($dir in @($canvasDir, $outDir, $waveOutDir, $monoOutDir, $orbitOutDir)) {
    if (-not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Path $dir | Out-Null
    }
}

Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.IO;
using System.Runtime.InteropServices;

public static class MtuiIcon
{
    static readonly int[] Sizes = { 16, 24, 32, 48, 64, 128, 256 };
    const int MasterSize = 512;
    const int PngFrom = 128;

    public static void Build(
        string canvasPath,
        string signalPath,
        string wavePath,
        string monoPath,
        string orbitPath)
    {
        using (Bitmap signal = DrawArt(0))
        {
            signal.Save(canvasPath, ImageFormat.Png);
            BuildIcon(signal, signalPath);
        }
        using (Bitmap wave = DrawArt(1))
        {
            BuildIcon(wave, wavePath);
        }
        using (Bitmap mono = DrawArt(2))
        {
            BuildIcon(mono, monoPath);
        }
        using (Bitmap orbit = DrawArt(3))
        {
            BuildIcon(orbit, orbitPath);
        }
    }

    static void BuildIcon(Bitmap art, string path)
    {
        List<int> dims = new List<int>();
        List<byte[]> images = new List<byte[]>();
        foreach (int size in Sizes)
        {
            using (Bitmap scaled = Resize(art, size))
            {
                dims.Add(size);
                images.Add(size >= PngFrom ? Png(scaled) : Dib(scaled));
            }
        }
        WriteIcon(path, dims, images);
    }

    static Bitmap DrawArt(int style)
    {
        Bitmap art = new Bitmap(MasterSize, MasterSize, PixelFormat.Format32bppArgb);
        using (Graphics g = Graphics.FromImage(art))
        {
            g.Clear(Color.Transparent);
            g.SmoothingMode = SmoothingMode.AntiAlias;
            g.CompositingQuality = CompositingQuality.HighQuality;
            g.PixelOffsetMode = PixelOffsetMode.HighQuality;

            if (style == 3)
            {
                DrawTile(g, Color.FromArgb(255, 7, 24, 33), Color.FromArgb(255, 22, 71, 91));
                DrawOrbit(g);
            }
            else if (style == 1)
            {
                DrawTile(g, Color.FromArgb(255, 17, 13, 31), Color.FromArgb(255, 58, 47, 88));
                DrawWave(g);
            }
            else if (style == 2)
            {
                DrawTile(g, Color.FromArgb(255, 11, 13, 16), Color.FromArgb(255, 48, 54, 61));
                DrawSignal(g, Color.FromArgb(255, 241, 245, 249), Color.FromArgb(255, 148, 163, 184));
            }
            else
            {
                DrawTile(g, Color.FromArgb(255, 11, 16, 32), Color.FromArgb(255, 36, 49, 74));
                DrawSignal(g, Color.FromArgb(255, 73, 215, 242), Color.FromArgb(255, 101, 245, 181));
            }
        }
        return art;
    }

    static void DrawTile(Graphics g, Color background, Color edge)
    {
        using (GraphicsPath tile = RoundedRect(24, 24, 464, 464, 104))
        using (SolidBrush brush = new SolidBrush(background))
            g.FillPath(brush, tile);
        using (GraphicsPath rim = RoundedRect(30, 30, 452, 452, 98))
        using (Pen border = new Pen(edge, 8))
            g.DrawPath(border, rim);
    }

    static void DrawSignal(Graphics g, Color promptColor, Color meterColor)
    {
        Point[] prompt = {
            new Point(122, 186),
            new Point(196, 256),
            new Point(122, 326)
        };
        using (Pen promptPen = new Pen(promptColor, 44))
        {
            promptPen.StartCap = LineCap.Round;
            promptPen.EndCap = LineCap.Round;
            promptPen.LineJoin = LineJoin.Round;
            g.DrawLines(promptPen, prompt);
        }

        int[] xs = { 250, 312, 374 };
        int[] tops = { 216, 158, 202 };
        int[] bottoms = { 296, 354, 310 };
        using (Pen meter = new Pen(meterColor, 38))
        {
            meter.StartCap = LineCap.Round;
            meter.EndCap = LineCap.Round;
            for (int i = 0; i < xs.Length; i++)
                g.DrawLine(meter, xs[i], tops[i], xs[i], bottoms[i]);
        }
    }

    static void DrawWave(Graphics g)
    {
        Point[] points = {
            new Point(98, 272),
            new Point(146, 272),
            new Point(182, 206),
            new Point(224, 326),
            new Point(272, 162),
            new Point(316, 336),
            new Point(354, 230),
            new Point(414, 230)
        };
        using (Pen wave = new Pen(Color.FromArgb(255, 167, 139, 250), 34))
        {
            wave.StartCap = LineCap.Round;
            wave.EndCap = LineCap.Round;
            wave.LineJoin = LineJoin.Round;
            g.DrawLines(wave, points);
        }
        using (SolidBrush cursor = new SolidBrush(Color.FromArgb(255, 251, 191, 36)))
            g.FillRectangle(cursor, 376, 330, 44, 44);
    }

    static void DrawOrbit(Graphics g)
    {
        using (Pen orbit = new Pen(Color.FromArgb(255, 64, 207, 232), 34))
        {
            orbit.StartCap = LineCap.Round;
            orbit.EndCap = LineCap.Round;
            g.DrawArc(orbit, 106, 106, 300, 300, 28, 132);
            g.DrawArc(orbit, 106, 106, 300, 300, 208, 132);
        }

        int[] xs = { 210, 256, 302 };
        int[] tops = { 222, 174, 204 };
        int[] bottoms = { 290, 338, 308 };
        using (Pen meter = new Pen(Color.FromArgb(255, 111, 242, 190), 34))
        {
            meter.StartCap = LineCap.Round;
            meter.EndCap = LineCap.Round;
            for (int i = 0; i < xs.Length; i++)
                g.DrawLine(meter, xs[i], tops[i], xs[i], bottoms[i]);
        }
    }

    static GraphicsPath RoundedRect(int x, int y, int width, int height, int radius)
    {
        GraphicsPath path = new GraphicsPath();
        int d = radius * 2;
        path.AddArc(x, y, d, d, 180, 90);
        path.AddArc(x + width - d, y, d, d, 270, 90);
        path.AddArc(x + width - d, y + height - d, d, d, 0, 90);
        path.AddArc(x, y + height - d, d, d, 90, 90);
        path.CloseFigure();
        return path;
    }

    static Bitmap Resize(Bitmap src, int size)
    {
        Bitmap dst = new Bitmap(size, size, PixelFormat.Format32bppArgb);
        using (Graphics g = Graphics.FromImage(dst))
        {
            g.CompositingMode = CompositingMode.SourceCopy;
            g.InterpolationMode = InterpolationMode.HighQualityBicubic;
            g.PixelOffsetMode = PixelOffsetMode.HighQuality;
            using (ImageAttributes attrs = new ImageAttributes())
            {
                attrs.SetWrapMode(WrapMode.TileFlipXY);
                g.DrawImage(src, new Rectangle(0, 0, size, size),
                            0, 0, src.Width, src.Height, GraphicsUnit.Pixel, attrs);
            }
        }
        return dst;
    }

    static byte[] Dib(Bitmap b)
    {
        int w = b.Width, h = b.Height;
        int[] px = Read(b);
        int maskStride = ((w + 31) / 32) * 4;

        MemoryStream ms = new MemoryStream();
        BinaryWriter bw = new BinaryWriter(ms);
        bw.Write(40);
        bw.Write(w);
        bw.Write(h * 2);
        bw.Write((short)1);
        bw.Write((short)32);
        bw.Write(0);
        bw.Write(w * h * 4 + maskStride * h);
        bw.Write(0); bw.Write(0); bw.Write(0); bw.Write(0);

        for (int y = h - 1; y >= 0; y--)
            for (int x = 0; x < w; x++)
                bw.Write(px[y * w + x]);

        byte[] row = new byte[maskStride];
        for (int y = h - 1; y >= 0; y--)
        {
            Array.Clear(row, 0, row.Length);
            for (int x = 0; x < w; x++)
                if (((px[y * w + x] >> 24) & 0xFF) < 128)
                    row[x / 8] |= (byte)(0x80 >> (x % 8));
            bw.Write(row);
        }

        bw.Flush();
        return ms.ToArray();
    }

    static byte[] Png(Bitmap b)
    {
        MemoryStream ms = new MemoryStream();
        b.Save(ms, ImageFormat.Png);
        return ms.ToArray();
    }

    static void WriteIcon(string path, List<int> dims, List<byte[]> images)
    {
        using (FileStream fs = new FileStream(path, FileMode.Create, FileAccess.Write))
        {
            BinaryWriter bw = new BinaryWriter(fs);
            bw.Write((short)0);
            bw.Write((short)1);
            bw.Write((short)images.Count);

            int offset = 6 + 16 * images.Count;
            for (int i = 0; i < images.Count; i++)
            {
                byte dim = dims[i] >= 256 ? (byte)0 : (byte)dims[i];
                bw.Write(dim); bw.Write(dim);
                bw.Write((byte)0);
                bw.Write((byte)0);
                bw.Write((short)1);
                bw.Write((short)32);
                bw.Write(images[i].Length);
                bw.Write(offset);
                offset += images[i].Length;
            }
            foreach (byte[] image in images) bw.Write(image);
            bw.Flush();
        }
    }

    static int[] Read(Bitmap b)
    {
        Rectangle rect = new Rectangle(0, 0, b.Width, b.Height);
        BitmapData data = b.LockBits(rect, ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
        int[] px = new int[b.Width * b.Height];
        for (int y = 0; y < b.Height; y++)
            Marshal.Copy(new IntPtr(data.Scan0.ToInt64() + (long)y * data.Stride),
                         px, y * b.Width, b.Width);
        b.UnlockBits(data);
        return px;
    }
}
'@

$canvasPath = [System.IO.Path]::GetFullPath($Canvas)
$outputPath = [System.IO.Path]::GetFullPath($Output)
$wavePath = [System.IO.Path]::GetFullPath($WaveOutput)
$monoPath = [System.IO.Path]::GetFullPath($MonoOutput)
$orbitPath = [System.IO.Path]::GetFullPath($OrbitOutput)
[MtuiIcon]::Build($canvasPath, $outputPath, $wavePath, $monoPath, $orbitPath)
Write-Host ("wrote {0}, {1}, {2}, {3}, and {4}" -f $canvasPath, $outputPath, $wavePath, $monoPath, $orbitPath)
