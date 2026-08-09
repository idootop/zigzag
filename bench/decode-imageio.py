"""用 ImageIO（CGImageSource）把一张图解成 PNG。

质量轴要一个「真值」解码器，默认选 ImageIO——因为应用自己的解码兜底走的就是它
（D-14），拿它当参考才是在量「用户会看到的差别」。方向按 EXIF 烘焙进像素，与
应用一致。

    用法：python3 bench/decode-imageio.py <输入> <输出.png>
    输出：<宽>x<高> orient=<原始方向>
"""

import sys

import Quartz
from CoreFoundation import CFURLCreateFromFileSystemRepresentation

src_path, out_path = sys.argv[1], sys.argv[2]

url = CFURLCreateFromFileSystemRepresentation(
    None, src_path.encode(), len(src_path.encode()), False
)
isrc = Quartz.CGImageSourceCreateWithURL(url, None)
if isrc is None:
    print("DECODE_FAIL")
    sys.exit(1)

# 取主图（index 0）；动图在这里就是首帧
img = Quartz.CGImageSourceCreateImageAtIndex(isrc, 0, None)
props = Quartz.CGImageSourceCopyPropertiesAtIndex(isrc, 0, None) or {}
orient = props.get(Quartz.kCGImagePropertyOrientation, 1)
w, h = Quartz.CGImageGetWidth(img), Quartz.CGImageGetHeight(img)

# 方向 5~8 会交换宽高
swap = orient in (5, 6, 7, 8)
ow, oh = (h, w) if swap else (w, h)

# 必须显式用 sRGB：DeviceRGB 对 CMYK 源是「不做色彩管理」的直转，
# 会把 Generic CMYK 的颜色画歪（基准 22 踩过，见 bench/README.md）。
cs = Quartz.CGColorSpaceCreateWithName(Quartz.kCGColorSpaceSRGB)
ctx = Quartz.CGBitmapContextCreate(
    None, ow, oh, 8, 0, cs,
    Quartz.kCGImageAlphaPremultipliedLast | Quartz.kCGBitmapByteOrder32Big,
)

tx = Quartz.CGAffineTransformIdentity
if orient == 2:
    tx = Quartz.CGAffineTransformMake(-1, 0, 0, 1, ow, 0)
elif orient == 3:
    tx = Quartz.CGAffineTransformMake(-1, 0, 0, -1, ow, oh)
elif orient == 4:
    tx = Quartz.CGAffineTransformMake(1, 0, 0, -1, 0, oh)
elif orient == 5:
    tx = Quartz.CGAffineTransformMake(0, 1, 1, 0, 0, 0)
elif orient == 6:
    tx = Quartz.CGAffineTransformMake(0, -1, 1, 0, 0, oh)
elif orient == 7:
    tx = Quartz.CGAffineTransformMake(0, -1, -1, 0, ow, oh)
elif orient == 8:
    tx = Quartz.CGAffineTransformMake(0, 1, -1, 0, ow, 0)
Quartz.CGContextConcatCTM(ctx, tx)
Quartz.CGContextDrawImage(ctx, Quartz.CGRectMake(0, 0, w, h), img)
out = Quartz.CGBitmapContextCreateImage(ctx)

ourl = CFURLCreateFromFileSystemRepresentation(
    None, out_path.encode(), len(out_path.encode()), False
)
dest = Quartz.CGImageDestinationCreateWithURL(ourl, "public.png", 1, None)
Quartz.CGImageDestinationAddImage(dest, out, None)
Quartz.CGImageDestinationFinalize(dest)
print(f"{ow}x{oh} orient={orient}")
