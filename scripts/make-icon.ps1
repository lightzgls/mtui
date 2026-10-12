<#
.SYNOPSIS
    Builds MTUI's preview PNG and multi-size Windows icons.

.DESCRIPTION
    Draws playing and quiet versions of MTUI's mint strip icon at 512px, then
    creates Windows icons at 16 through 256px. The four rounded strips settle
    into a flat strip when playback pauses or stops. The quiet icon uses a
    -quiet filename suffix.
    assets/mtui-icon.svg and assets/mtui-icon-quiet.svg are portable vectors.
    assets/previews/tray-icons.png compares both states at real tray sizes.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/make-icon.ps1
#>
[CmdletBinding()]
param(
    [string]$Canvas,
    [string]$Output,
    [string]$Preview
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
if (-not $Canvas) { $Canvas = Join-Path $root 'canvas.png' }
if (-not $Output) { $Output = Join-Path $root 'assets\mtui.ico' }
if (-not $Preview) { $Preview = Join-Path $root 'assets\previews\tray-icons.png' }

$canvasDir = Split-Path -Parent $Canvas
$outDir = Split-Path -Parent $Output
$previewDir = Split-Path -Parent $Preview
foreach ($dir in @($canvasDir, $outDir, $previewDir)) {
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
    static readonly int[] Sizes = { 16, 20, 24, 32, 40, 48, 64, 128, 256 };
    const int MasterSize = 512;
    const int PngFrom = 128;

    public static void Build(string canvasPath, string outputPath, string previewPath)
    {
        using (Bitmap playing = DrawArt(true))
        using (Bitmap quiet = DrawArt(false))
        {
            playing.Save(canvasPath, ImageFormat.Png);
            BuildIcon(playing, outputPath);
            string quietPath = Path.Combine(Path.GetDirectoryName(outputPath),
                Path.GetFileNameWithoutExtension(outputPath) + "-quiet.ico");
            BuildIcon(quiet, quietPath);
        }
        DrawPreview(previewPath);
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

    static Bitmap DrawArt(bool playing)
    {
        Bitmap art = new Bitmap(MasterSize, MasterSize, PixelFormat.Format32bppArgb);
        using (Graphics g = Graphics.FromImage(art))
        {
            g.Clear(Color.Transparent);
            g.SmoothingMode = SmoothingMode.AntiAlias;
            g.CompositingQuality = CompositingQuality.HighQuality;
            g.PixelOffsetMode = PixelOffsetMode.HighQuality;

            DrawFlatTile(g);
            DrawSignal(g, Color.FromArgb(255, 141, 239, 193), playing);
        }
        return art;
    }

    static void DrawFlatTile(Graphics g)
    {
        using (GraphicsPath tile = RoundedRect(16, 16, 480, 480, 128))
        using (SolidBrush brush = new SolidBrush(Color.FromArgb(255, 21, 27, 35)))
            g.FillPath(brush, tile);
    }

    static void DrawSignal(Graphics g, Color ink, bool playing)
    {
        int[] xs = { 128, 212, 296, 380 };
        int[] tops = { 212, 172, 144, 196 };
        int[] bottoms = { 300, 340, 368, 316 };
        using (Pen strip = new Pen(playing ? ink : Color.FromArgb(255, 170, 181, 189), 40))
        {
            strip.StartCap = LineCap.Round;
            strip.EndCap = LineCap.Round;
            for (int i = 0; i < xs.Length; i++)
            {
                if (playing) g.DrawLine(strip, xs[i], tops[i], xs[i], bottoms[i]);
                else g.DrawLine(strip, xs[i] - 10, 256, xs[i] + 10, 256);
            }
        }
    }

    static void DrawPreview(string path)
    {
        using (Bitmap preview = new Bitmap(1000, 660, PixelFormat.Format32bppArgb))
        using (Graphics g = Graphics.FromImage(preview))
        using (Bitmap playing = DrawArt(true))
        using (Bitmap quiet = DrawArt(false))
        using (Font heading = new Font("Segoe UI", 26, FontStyle.Bold))
        using (Font label = new Font("Segoe UI", 16, FontStyle.Regular))
        using (Font small = new Font("Segoe UI", 11, FontStyle.Regular))
        using (SolidBrush ink = new SolidBrush(Color.FromArgb(232, 236, 239)))
        using (SolidBrush secondary = new SolidBrush(Color.FromArgb(158, 171, 180)))
        using (SolidBrush light = new SolidBrush(Color.FromArgb(238, 241, 244)))
        using (SolidBrush darkInk = new SolidBrush(Color.FromArgb(63, 74, 84)))
        {
            g.Clear(Color.FromArgb(13, 18, 24));
            g.TextRenderingHint = System.Drawing.Text.TextRenderingHint.AntiAliasGridFit;
            g.DrawString("MTUI / Strip icon", heading, ink, 48, 30);
            g.DrawString("Four rounded strips. Raised while playing, flat at rest.", label, secondary, 48, 88);
            using (Bitmap activeLarge = Resize(playing, 192))
            using (Bitmap quietLarge = Resize(quiet, 192))
            {
                g.DrawImageUnscaled(activeLarge, 154, 150);
                g.DrawImageUnscaled(quietLarge, 654, 150);
            }
            g.DrawString("Playing", label, ink, 213, 366);
            g.DrawString("Paused / stopped", label, ink, 665, 366);
            g.DrawString("ACTUAL TRAY SIZES", small, secondary, 48, 426);
            for (int row = 0; row < 2; row++)
            {
                int y = 466 + row * 88;
                if (row == 1) g.FillRectangle(light, 32, y - 8, 936, 80);
                g.DrawString(row == 0 ? "Dark taskbar" : "Light taskbar", small,
                    row == 0 ? secondary : darkInk, 48, y + 19);
                int[] sizes = { 16, 20, 24, 32 };
                for (int i = 0; i < sizes.Length; i++)
                {
                    int size = sizes[i];
                    for (int state = 0; state < 2; state++)
                    {
                        int x = 244 + i * 80 + state * 380;
                        using (Bitmap icon = Resize(state == 0 ? playing : quiet, size))
                            g.DrawImageUnscaled(icon, x + (32 - size) / 2, y);
                        g.DrawString(size.ToString() + "px", small,
                            row == 0 ? secondary : darkInk, x, y + 40);
                    }
                }
            }
            preview.Save(path, ImageFormat.Png);
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
$previewPath = [System.IO.Path]::GetFullPath($Preview)
[MtuiIcon]::Build($canvasPath, $outputPath, $previewPath)
Write-Host ("wrote playing and quiet icons, {0}, and {1}" -f $canvasPath, $previewPath)
