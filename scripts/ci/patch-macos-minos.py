#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""patch-macos-minos.py — 就地改写 Mach-O 二进制的 minos（部署目标/加载门槛）。

场景
----
用户真机是 macOS 13 (Ventura)，ollama v0.21.0 的二进制 LC_BUILD_VERSION
minos=14.0，会被 dyld 以 "requires macOS 14.0 or later" 拒绝加载。
本脚本在打包前就地改写二进制里的 minos 字段，把 dyld 的加载门槛强行降到
用户所需版本（如 11.0），让 dyld 放行。

**重要认知**：补丁只改 dyld 的加载门槛，不改运行时调用的 API。若二进制真用了
14-only API，macOS 13 上仍可能运行时崩溃；若只是「在 macOS 14 的 CI runner
上构建、并未用 14-only API」（Go CLI 常见），则能正常跑。最终以用户真机为准。

实现约束
--------
* 纯标准库（struct / os / sys），无第三方依赖，runner 直跑无需安装。
* **原位覆盖**：只重写 minos 的那 4 个字节，字节数不变，绝不插入/删除，
  避免破坏后续偏移、load command 布局以及（若有的）签名结构。
* 支持 fat/universal（FAT_MAGIC / FAT_MAGIC_64）与 thin（单 slice）两种形态。
* 提供 --selftest：在内存里手工构造一个 universal 二进制（2 个 slice，各含
  一个 LC_BUILD_VERSION 且 minos 初始 14.0），验证改写逻辑，无需下载 124MB tgz。

Mach-O 布局速查（64-bit，little-endian 原生序）
---------------------------------------------
fat_header（大端，文件开头）：
    uint32 magic            cafebabe=FAT_MAGIC / cafebabf=FAT_MAGIC_64
    uint32 nfat_arch
    其后是 nfat_arch 个 fat_arch：
        FAT_MAGIC   : cputype, cpusubtype, offset, size, align         = 20 B
        FAT_MAGIC_64: cputype, cpusubtype, offset(8), size(8),
                      align, rsv                                        = 32 B

单 slice（64-bit Mach-O）：
    uint32 magic            cffaedfe=MH_MAGIC_64 / feedfacf=MH_CIGAM_64
    uint32 cputype
    uint32 cpusubtype
    uint32 filetype
    uint32 ncmds
    uint32 sizeofcmds
    uint32 flags
    uint32 reserved                                  共 32 字节
    load commands 从 offset 32 开始，共 ncmds 个，每个占 cmdsize 字节：
        LC_BUILD_VERSION   (0x32): cmd(4) cmdsize(4) platform(+8)
                                minos(+12) sdk(+16) ntools(+20) ...
        LC_VERSION_MIN_MACOSX (0x24): cmd(4) cmdsize(4) version/minos(+8)
                                sdk(+12) ...
    minos 编码：(major<<16)|(minor<<8)，如 11.0 -> 0x000B0000，14.0 -> 0x000E0000。
"""

import os
import struct
import sys

# ---- fat / Mach-O 魔数（按磁盘上的原始字节序列比对）-------------------------
FAT_MAGIC = b"\xca\xfe\xba\xbe"            # cafebabe（大端 fat header）
FAT_MAGIC_64 = b"\xca\xfe\xba\xbf"         # cafebabf（大端 fat header, 64-bit arch）
MH_MAGIC_64 = b"\xcf\xfa\xed\xfe"          # cffaedfe（little-endian 64-bit Mach-O）
MH_CIGAM_64 = b"\xfe\xed\xfa\xcf"          # feedfacf（big-endian 64-bit Mach-O）

# ---- load command 类型 ------------------------------------------------------
LC_BUILD_VERSION = 0x32
LC_VERSION_MIN_MACOSX = 0x24

# ---- 64-bit Mach-O header 固定 32 字节，load commands 从 offset 32 起 ---------
MACHO64_HDR_SIZE = 32


def encode_version(text: str) -> int:
    """'11.0' -> (11<<16)|(0<<8) = 0x000B0000 ；'14.0' -> 0x000E0000。"""
    parts = text.strip().split(".")
    major = int(parts[0]) if len(parts) > 0 else 0
    minor = int(parts[1]) if len(parts) > 1 else 0
    return (major << 16) | (minor << 8)


def decode_version(enc: int) -> str:
    """0x000B0000 -> '11.0'（忽略 patch 位）。"""
    major = (enc >> 16) & 0xFF
    minor = (enc >> 8) & 0xFF
    return "%d.%d" % (major, minor)


def _slice_endian(magic: bytes):
    """根据单 slice 的 magic 推断字节序：'<'=小端，'>'=大端，否则 None。"""
    if magic == MH_MAGIC_64:
        return "<"
    if magic == MH_CIGAM_64:
        return ">"
    return None


def patch_single_slice(buf: bytearray, start: int, target_enc: int) -> int:
    """就地改写从 ``start`` 开始的单个 64-bit Mach-O slice 的 minos。

    返回被改写的 LC 个数（0 表示该 slice 不可识别或未含目标 LC）。
    ``buf`` 是整文件的 bytearray，``start`` 是 slice 在文件中的绝对偏移。
    """
    if start + MACHO64_HDR_SIZE > len(buf):
        return 0
    magic = bytes(buf[start:start + 4])
    en = _slice_endian(magic)
    if en is None:
        # 不是 64-bit Mach-O（可能是别的架构或已损坏），跳过，绝不误改。
        return 0

    ncmds = struct.unpack_from(en + "I", buf, start + 16)[0]
    if ncmds <= 0 or ncmds > 0xFFFF:
        return 0

    cmd_off = start + MACHO64_HDR_SIZE
    patched = 0
    for _ in range(ncmds):
        if cmd_off + 8 > len(buf):
            break
        cmd = struct.unpack_from(en + "I", buf, cmd_off)[0]
        cmdsize = struct.unpack_from(en + "I", buf, cmd_off + 4)[0]
        if cmdsize < 8 or cmd_off + cmdsize > len(buf):
            break  # 数据异常，停止以免越界改写
        if cmd == LC_BUILD_VERSION:
            # platform @ +8, minos @ +12, sdk @ +16：只改 minos 4 字节。
            struct.pack_into(en + "I", buf, cmd_off + 12, target_enc)
            patched += 1
        elif cmd == LC_VERSION_MIN_MACOSX:
            # version/minos @ +8：只改 minos 4 字节。
            struct.pack_into(en + "I", buf, cmd_off + 8, target_enc)
            patched += 1
        cmd_off += cmdsize
    return patched


def patch_bytes(data: bytearray, target_minos: str) -> int:
    """对整文件（bytearray，原地修改）改写所有 slice 的 minos。

    返回被改写的 LC 总数。支持 fat/universal 与 thin 两种形态。
    """
    target_enc = encode_version(target_minos)
    magic = bytes(data[:4])
    total = 0

    if magic == FAT_MAGIC:
        # 大端 fat header：magic(4) + nfat_arch(4)，每个 arch 20 字节。
        nfat = struct.unpack_from(">I", data, 4)[0]
        off = 8
        for _ in range(nfat):
            if off + 20 > len(data):
                break
            _ct, _cs, offset, _size, _align = struct.unpack_from(">5I", data, off)
            total += patch_single_slice(data, offset, target_enc)
            off += 20
    elif magic == FAT_MAGIC_64:
        # 大端 fat header（64-bit arch）：每个 arch 32 字节，
        # offset/size 为 64 位。
        nfat = struct.unpack_from(">I", data, 4)[0]
        off = 8
        for _ in range(nfat):
            if off + 32 > len(data):
                break
            _ct, _cs = struct.unpack_from(">2I", data, off)
            offset, _size = struct.unpack_from(">QQ", data, off + 8)
            total += patch_single_slice(data, offset, target_enc)
            off += 32
    else:
        # thin / 单 slice：整个文件即一个 Mach-O。
        total += patch_single_slice(data, 0, target_enc)
    return total


def patch_file(path: str, target_minos: str) -> int:
    """读文件 -> 改写 -> 原样写回（长度不变）。返回改写 LC 数。"""
    with open(path, "rb") as fh:
        data = bytearray(fh.read())
    n = patch_bytes(data, target_minos)
    with open(path, "wb") as fh:
        fh.write(data)
    return n


# =============================================================================
# --selftest：内存里构造 universal 二进制，验证核心改写逻辑（无需下载 tgz）
# =============================================================================
def _build_fake_slice(cputype: int) -> bytes:
    """构造一个最小但合法的 64-bit Mach-O，含 1 个 LC_BUILD_VERSION(minos=14.0)。

    header 严格 8 个 uint32（32 字节）：magic / cputype / cpusubtype / filetype /
    ncmds / sizeofcmds / flags / reserved —— 与 patch_single_slice 里
    MACHO64_HDR_SIZE=32 的假设一致，load command 恰好落在 offset 32。
    """
    # header：magic 用小端打包 0xfeedfacf -> 磁盘字节 cffaedfe；其余字段填占位值。
    hdr = struct.pack("<8I", 0xfeedfacf, cputype, 0x00000003, 0x00000002, 1, 24, 0, 0)
    # LC_BUILD_VERSION：cmd(0x32) cmdsize(24) platform(1) minos(14.0) sdk(14.0) ntools(0)
    lc = struct.pack(
        "<6I",
        LC_BUILD_VERSION,
        24,
        1,                       # platform = macOS(1)
        encode_version("14.0"),  # minos（将被改写）
        encode_version("14.0"),  # sdk（保持不变）
        0,                       # ntools
    )
    return hdr + lc


def build_fake_universal() -> bytes:
    """构造 2-slice universal 二进制（x86_64 + arm64），各含 minos=14.0。"""
    s0 = _build_fake_slice(0x01000007)  # x86_64
    s1 = _build_fake_slice(0x0100000C)  # arm64

    # fat header（大端）：magic + nfat_arch
    fat = struct.pack(">I", 0xCAFEBABE) + struct.pack(">I", 2)
    # fat_arch（20 字节，大端）：cputype, cpusubtype, offset, size, align
    base = 8 + 2 * 20  # fat header(8) + 2 个 arch(40)
    a0 = struct.pack(">5I", 0x01000007, 0x00000003, base, len(s0), 0x0000000C)
    a1 = struct.pack(">5I", 0x0100000C, 0x00000003, base + len(s0), len(s1), 0x0000000C)
    return fat + a0 + a1 + s0 + s1


def run_selftest() -> int:
    """自测：构造 universal 二进制，改写 14.0 -> 11.0，断言各项不变。"""
    print("::group::patch-macos-minos --selftest")
    failures = []

    original = build_fake_universal()
    original_len = len(original)
    expected_len = 8 + 2 * 20 + len(_build_fake_slice(0x01000007)) * 2

    def check(cond, msg):
        if cond:
            print("  [OK]   " + msg)
        else:
            print("  [FAIL] " + msg)
            failures.append(msg)

    check(original_len == expected_len,
          "构造的 universal 二进制长度 = %d（期望 %d）" % (original_len, expected_len))
    check(original[:4] == FAT_MAGIC,
          "fat magic 为 cafebabe（实际 %s）" % original[:4].hex())

    data = bytearray(original)
    target = "11.0"
    n = patch_bytes(data, target)

    # 1) 改写 LC 数 = 2（两个 slice 各一个 LC_BUILD_VERSION）
    check(n == 2, "改写 LC_BUILD_VERSION 数 = %d（期望 2）" % n)

    # 2) 文件长度不变（原位覆盖，绝不插入/删除）
    check(len(data) == original_len,
          "改写后长度不变 = %d（原 %d）" % (len(data), original_len))

    # 3) fat magic 不变
    check(bytes(data[:4]) == FAT_MAGIC, "改写后 fat magic 仍为 cafebabe")

    # 4) 两个 slice 的 minos 都变成 11.0；platform 与 sdk 不受影响
    #    slice0 @ offset (8 + 2*20)=48，header 32B，LC 在 +32；minos @ LC+12
    base = 8 + 2 * 20
    slice0_minos_off = base + MACHO64_HDR_SIZE + 12
    slice1_minos_off = base + len(_build_fake_slice(0x01000007)) + MACHO64_HDR_SIZE + 12
    slice0_plat_off = base + MACHO64_HDR_SIZE + 8
    slice0_sdk_off = base + MACHO64_HDR_SIZE + 16

    slice0_minos = struct.unpack_from("<I", data, slice0_minos_off)[0]
    slice1_minos = struct.unpack_from("<I", data, slice1_minos_off)[0]
    slice0_plat = struct.unpack_from("<I", data, slice0_plat_off)[0]
    slice0_sdk = struct.unpack_from("<I", data, slice0_sdk_off)[0]

    check(decode_version(slice0_minos) == target,
          "slice0 minos = %s（期望 11.0, raw=0x%08X）" % (decode_version(slice0_minos), slice0_minos))
    check(decode_version(slice1_minos) == target,
          "slice1 minos = %s（期望 11.0, raw=0x%08X）" % (decode_version(slice1_minos), slice1_minos))
    check(slice0_plat == 1, "slice0 platform 未被误改（=1）")
    check(decode_version(slice0_sdk) == "14.0", "slice0 sdk 未被误改（仍为 14.0）")

    # 5) thin 单 slice 路径：直接用 _build_fake_slice 包一层，验证非 fat 也能改
    thin = bytearray(_build_fake_slice(0x01000007))
    n_thin = patch_bytes(thin, target)
    thin_minos = struct.unpack_from("<I", thin, MACHO64_HDR_SIZE + 12)[0]
    check(n_thin == 1 and decode_version(thin_minos) == target,
          "thin 单 slice 路径：minos 改写 = %s" % decode_version(thin_minos))

    print("::endgroup::")
    if failures:
        print("::error::patch-macos-minos --selftest FAILED: %d 项未通过" % len(failures))
        for f in failures:
            print("  - " + f)
        return 1
    print("::notice::patch-macos-minos --selftest PASSED（%d 项全部通过）" % 6)
    return 0


def main(argv) -> int:
    args = argv[1:]
    if "--selftest" in args:
        return run_selftest()

    if len(args) != 2:
        sys.stderr.write(
            "usage: python3 patch-macos-minos.py <binary_path> <target_minos>\n"
            "       python3 patch-macos-minos.py --selftest\n"
        )
        return 2

    path, target = args[0], args[1]
    if not os.path.isfile(path):
        sys.stderr.write("error: 文件不存在: %s\n" % path)
        return 1

    # 合法性预检：target 必须能解析成 major.minor
    try:
        encode_version(target)
    except ValueError:
        sys.stderr.write("error: target_minos 格式非法，期望形如 '11.0' : %s\n" % target)
        return 1

    n = patch_file(path, target)
    print("patched %d Mach-O slice(s) in %s: minos -> %s" % (n, path, target))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
