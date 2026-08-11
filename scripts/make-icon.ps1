<#
.SYNOPSIS
    Builds assets/mtui.ico from canvas.png.

.DESCRIPTION
    Committed alongside the .ico it produces, because the icon is not a
    straight copy of the drawing and the difference is worth being able to
    reproduce.

    Two things happen here that a plain "save as .ico" would not do:

    The white square goes. canvas.png is a red disc painted on an opaque white
    background, and an icon that keeps it is a white tile on the taskbar rather
    than a disc. The white is removed by flooding inwards from the border, so
    only the paper *around* the disc is taken -- the white ring and the play
    triangle inside it are walled off by solid red and survive. Pixels the
    drawing anti-aliased into the paper are not simply dropped either: a pixel
    that is a mix of red and white becomes pure red at the alpha that mix
    implies, which is what keeps the rim from turning into a pink fringe on a
    dark taskbar.

    And every size is drawn from the 500px original rather than from the next
    size up, so the 16px entry -- the one that ends up in the title bar and the
    taskbar, and the only one most people ever look at -- is a single clean
    downsample instead of a chain of them.

.EXAMPLE
    pwsh -File scripts/make-icon.ps1
#>
[CmdletBinding()]
param(
    [string]$Source,
    [string]$Output
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# Not defaulted in the param block: $PSScriptRoot is not filled in yet while
# those are being bound, and the repo-relative paths would come out rooted at
# whatever directory the script was run from.
$root = Split-Path -Parent $PSScriptRoot
if (-not $Source) { $Source = Join-Path $root 'canvas.png' }
if (-not $Output) { $Output = Join-Path $root 'assets\mtui.ico' }

$source = (Resolve-Path -LiteralPath $Source).Path
$outDir = Split-Path -Parent $Output
if (-not (Test-Path -LiteralPath $outDir)) {
    New-Item -ItemType Directory -Path $outDir | Out-Null
}
$output = Join-Path (Resolve-Path -LiteralPath $outDir).Path (Split-Path -Leaf $Output)

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
    // What Windows asks for, and nothing else. 16 is the title bar and the
    // taskbar, 32 the alt-tab and shortcut overlay, 48 the medium view in
    // Explorer, 256 the extra-large one; 24, 64 and 128 keep the in-between
    // scalings (125%, 150%, 200% displays) from being resampled by the shell.
    static readonly int[] Sizes = { 16, 24, 32, 48, 64, 128, 256 };

    // Entries this size and up are stored as PNG rather than a raw bitmap.
    // A 256px entry uncompressed is 256 KB of the binary; as PNG it is a few.
    // The small ones stay uncompressed, which every icon reader understands.
    const int PngFrom = 128;

    // Anything greener than this is paper or paper blending into the disc.
    // The disc itself is pure red -- green 0 -- so the margin is enormous and
    // the exact threshold does not matter.
    const int PaperGreen = 40;

    public static void Build(string sourcePath, string outputPath)
    {
        using (Bitmap art = Knockout(sourcePath))
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
            Write(outputPath, dims, images);
        }
    }

    /// The drawing with the paper around the disc taken out of it.
    static Bitmap Knockout(string path)
    {
        using (Bitmap src = new Bitmap(path))
        {
            int w = src.Width, h = src.Height;
            int[] px = Read(src);
            bool[] paper = new bool[w * h];
            Flood(px, paper, w, h);

            for (int i = 0; i < px.Length; i++)
            {
                if (!paper[i]) continue;
                // The pixel is red mixed with white. Undo the mix: the green
                // channel says how much white is in it, and what is left is
                // red at that much alpha. Fully white gives alpha 0 -- and it
                // still gives *red*, so that scaling this image later blends
                // red into red at the rim instead of dragging white in.
                int alpha = 255 - ((px[i] >> 8) & 0xFF);
                px[i] = (alpha << 24) | 0x00FF0000;
            }

            return Write(px, w, h);
        }
    }

    /// Marks every pixel reachable from the border without crossing the disc.
    static void Flood(int[] px, bool[] paper, int w, int h)
    {
        Stack<int> stack = new Stack<int>();
        for (int x = 0; x < w; x++)
        {
            Take(px, paper, stack, x);
            Take(px, paper, stack, (h - 1) * w + x);
        }
        for (int y = 0; y < h; y++)
        {
            Take(px, paper, stack, y * w);
            Take(px, paper, stack, y * w + w - 1);
        }
        while (stack.Count > 0)
        {
            int i = stack.Pop();
            int x = i % w, y = i / w;
            if (x > 0) Take(px, paper, stack, i - 1);
            if (x < w - 1) Take(px, paper, stack, i + 1);
            if (y > 0) Take(px, paper, stack, i - w);
            if (y < h - 1) Take(px, paper, stack, i + w);
        }
    }

    static void Take(int[] px, bool[] paper, Stack<int> stack, int i)
    {
        if (paper[i] || ((px[i] >> 8) & 0xFF) <= PaperGreen) return;
        paper[i] = true;
        stack.Push(i);
    }

    static Bitmap Resize(Bitmap src, int size)
    {
        Bitmap dst = new Bitmap(size, size, PixelFormat.Format32bppArgb);
        using (Graphics g = Graphics.FromImage(dst))
        {
            // SourceCopy, not the default SourceOver: the destination starts
            // out transparent, and blending onto it would multiply the alpha
            // of the rim by itself and leave the disc with a chewed edge.
            g.CompositingMode = CompositingMode.SourceCopy;
            g.InterpolationMode = InterpolationMode.HighQualityBicubic;
            g.PixelOffsetMode = PixelOffsetMode.HighQuality;
            using (ImageAttributes attrs = new ImageAttributes())
            {
                // Without this the sampler reads past the edge of the source
                // and mixes in transparent black, thinning the outermost row.
                attrs.SetWrapMode(WrapMode.TileFlipXY);
                g.DrawImage(src, new Rectangle(0, 0, size, size),
                            0, 0, src.Width, src.Height, GraphicsUnit.Pixel, attrs);
            }
        }
        return dst;
    }

    /// An entry in the uncompressed form: a header claiming twice the real
    /// height, the colours bottom-up, then a 1-bit mask underneath them.
    static byte[] Dib(Bitmap b)
    {
        int w = b.Width, h = b.Height;
        int[] px = Read(b);
        int maskStride = ((w + 31) / 32) * 4;

        MemoryStream ms = new MemoryStream();
        BinaryWriter bw = new BinaryWriter(ms);
        bw.Write(40);                               // header size
        bw.Write(w);
        bw.Write(h * 2);                            // colours and mask together
        bw.Write((short)1);                         // planes
        bw.Write((short)32);                        // bits per pixel
        bw.Write(0);                                // uncompressed
        bw.Write(w * h * 4 + maskStride * h);
        bw.Write(0); bw.Write(0); bw.Write(0); bw.Write(0);

        for (int y = h - 1; y >= 0; y--)
            for (int x = 0; x < w; x++)
                bw.Write(px[y * w + x]);            // little-endian: B,G,R,A

        // The mask is what a reader too old to know about the alpha channel
        // uses instead. Nothing modern reads it, and leaving it wrong is how
        // an icon ends up with a black box behind it in some forgotten dialog.
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

    static void Write(string path, List<int> dims, List<byte[]> images)
    {
        using (FileStream fs = new FileStream(path, FileMode.Create, FileAccess.Write))
        {
            BinaryWriter bw = new BinaryWriter(fs);
            bw.Write((short)0);                     // reserved
            bw.Write((short)1);                     // an icon, not a cursor
            bw.Write((short)images.Count);

            int offset = 6 + 16 * images.Count;
            for (int i = 0; i < images.Count; i++)
            {
                // One byte per side, so 256 has to be written as 0.
                byte dim = dims[i] >= 256 ? (byte)0 : (byte)dims[i];
                bw.Write(dim); bw.Write(dim);
                bw.Write((byte)0);                  // not a palette
                bw.Write((byte)0);                  // reserved
                bw.Write((short)1);                 // planes
                bw.Write((short)32);                // bits per pixel
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

    static Bitmap Write(int[] px, int w, int h)
    {
        Bitmap b = new Bitmap(w, h, PixelFormat.Format32bppArgb);
        BitmapData data = b.LockBits(new Rectangle(0, 0, w, h),
                                     ImageLockMode.WriteOnly, PixelFormat.Format32bppArgb);
        for (int y = 0; y < h; y++)
            Marshal.Copy(px, y * w,
                         new IntPtr(data.Scan0.ToInt64() + (long)y * data.Stride), w);
        b.UnlockBits(data);
        return b;
    }
}
'@

[MtuiIcon]::Build($source, $output)
Write-Host ("wrote {0} ({1:N0} bytes)" -f $output, (Get-Item -LiteralPath $output).Length)
