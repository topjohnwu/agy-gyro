#!/usr/bin/env python3
"""
patch.py - Universal Dynamic Patcher for Google Antigravity CLI (agy)
==================================================================

Overview & Reverse Engineering Background:
------------------------------------------
Google Antigravity CLI (`agy`) is written in Go and packaged as a standalone
monolithic binary. It contains powerful internal features that Google gates
behind internal account checks and server-side feature flags:

1. `/boost` (Owl Multi-Agent Orchestrator):
   Coordinates specialized subagents (`DeepCoder`, `DeepInvestigator`) for deep
   architectural research and autonomous coding workflows.
2. `/teamwork-preview` (Teamwork Multi-Agent Swarm):
   Coordinates an agent team with dynamic subagent delegation and interactive
   prompt drafting protocols.
3. Built-in Agent Registry (`cortex/customizations/builtin/agents`):
   Registers built-in agent personas into the runtime agent picker.
4. Built-in Slash Commands (`cortex/slashcommands`):
   Registers commands into the interactive and print-mode command registries.

Why Binary Patching Instead of API Proxying / Wrappers?
-------------------------------------------------------
The authorization checks for these features are performed entirely locally
inside the CLI binary prior to issuing network requests:
- Slash commands pass through a central gatekeeper: `slashcommands.hasBuiltinAgents`.
- Subagent definitions pass through experiment check closures in `builtin/agents`.
If these predicate functions return `true` (1), the CLI registers the commands,
expands them during turn execution, and delegates to the corresponding agents.

Challenges Across Architectures, OSes, and Go Releases:
-------------------------------------------------------
1. Cross-Platform Executable Containers:
   - macOS: 64-bit Mach-O (`__TEXT`, `__text` sections).
   - Linux: 64-bit ELF (PIE - Position Independent Executables, `PT_LOAD` segments).
   - Windows: 64-bit PE32+ (`.text` section, `ImageBase` + RVA).
   Every format requires mapping Virtual Addresses (VAs) used by Go's runtime
   tables to physical file offsets.

2. Go Linker Layout & ELF runtime.text Base:
   - On Linux ELF PIE builds, C runtime startup stubs (crt1.o) precede Go code
     in the `.text` section by 0xb0 (176) bytes.
   - Using the ELF section's `sh_addr` directly as `text_start` shifts every
     symbol VA by -0xb0, causing function lookups to point into adjacent
     epilogues or return stubs.
   - Solution: In ELF binaries, scan forward from `.text` for the first Go stack
     prologue to determine the exact `runtime.text` base address.

3. Subagent Tools Gatekeeper (`cortex/config/config.SubagentToolsEnabled`):
   - In addition to the slash command gatekeeper and subagent closures,
     `SubagentToolsEnabled` must return `true`.
   - When false, `cortex/core/components/config.getBaselineConfig` omits
     `invoke_subagent`, `define_subagent`, and `manage_subagents` from the tool
     definitions sent to the LLM, and `builtinSubagents` immediately returns empty.
   - Patching this function injects the subagent tools into the model's schema
     and enables end-to-end multi-agent delegation.

4. Volatile Closure Numbering (`func1` .. `funcN`):
   - Go names anonymous closures sequentially (`agents.init.func7`, etc.).
   - If upstream source code changes (adding/removing a single closure), the numbers
     shift. Hardcoded symbol names or offsets break on virtually every update.
   - Solution: Dynamically locate closures by scanning for semantic string literals
     (`"teamwork_preview"`, `"enable-teamwork-subagent"`, `"enable-owl-slash-command"`)
     and tracing relative call instructions (`BL` on ARM64, `CALL` on x86_64).

5. Go Function Entry Points in `pclntab`:
   - Go's `pclntab` function table (`ftab`) entries map directly to true function
     entry points. ELF PIE binaries are anchored by discovering the `runtime.text`
     entry point in the text section.
   - Pclntab candidates are collected and filtered by `max(nfunc)` to select the main
     binary table over any embedded helpers (e.g., WebM recorder).

6. Builtin Subagent Allowed List Registration (`config.builtinSubagents` / `config.buildDefaultAgent`):
   - When the LLM invokes a subagent (such as `DeepInvestigator` under `/boost`),
     `InvokeSubagentHandler.findAgentByName` validates the requested agent name
     against `CalculateAllowedSubagents`.
   - `CalculateAllowedSubagents` relies on `config.builtinSubagents` (inlined into
     `config.buildDefaultAgent` in `agy` 1.4.2+), which queries `FeatureProvider.IsEnabled` for:
     * `enable-browser-subagent-v2` (browser)
     * `enable-teamwork-subagent` (teamwork_preview)
     * `enable-owl-slash-command` (DeepCoder)
     * `enable-owl-slash-command` (DeepInvestigator)
   - Additionally, `agy` 1.4.2+ introduces `config.deepAgent` (`enable-deepagent` for `/deepagent`)
     and `config.reviewAgent` (`enable-review` for `/review-agent`), which populate
     `DefinedSubagents` and gate the corresponding slash commands.
   - If these feature flags are disabled on the server, `builtinSubagents` returns
     only `["self", "research"]`, causing `findAgentByName` to reject invocations:
     `subagent "DeepInvestigator" not found or not allowed to be invoked`.
   - Solution: By intercepting the indirect call sites to `fp.IsEnabled` inside
     `builtinSubagents` / `buildDefaultAgent`, `deepAgent`, and `reviewAgent` for the
     target feature flags, we unlock `DeepInvestigator`, `DeepCoder`, `teamwork_preview`,
     `deepagent`, and `review-agent` while keeping the unstable browser agent safely disabled.
     Calls are patched using `mov w0, #1` on ARM64 and `movl $1, %eax` + NOPs on x86_64
     to ensure clean 64-bit return registers.

Patching Architecture:
----------------------
Instead of fragile, brute-force patching of dozens of closures, this patcher targets
stable, semantic entry points and registry calls:
  [1] `slashcommands.hasBuiltinAgents` + `deepagent`/`review-agent` slash predicates:
      Central permission checks unlocking restricted slash commands (/boost, /teamwork-preview,
      /deepagent, /review-agent).
  [2] `cortex/config/config.SubagentToolsEnabled`:
      Master tools switch unlocking `invoke_subagent`, `define_subagent`, and `manage_subagents`.
  [3] `builtin/agents` Teamwork Subagent Predicate (`enable-teamwork-subagent`).
  [4] `builtin/agents` Owl Subagent Predicate 1 (DeepCoder).
  [5] `builtin/agents` Owl Subagent Predicate 2 (DeepInvestigator).
  [6] `cortex/core/components/config` (`builtinSubagents` / `buildDefaultAgent`, `deepAgent`, `reviewAgent`):
      Runtime registration call sites patched to permit allowed subagent invocations.

Usage:
------
  python3 scripts/patch.py                  # Patch agy found in PATH in-place
  python3 scripts/patch.py <input>          # Patch <input> in-place
  python3 scripts/patch.py <input> <output> # Patch <input> to <output>

Dependencies:
-------------
Zero external third-party packages (uses only Python 3 standard library).
"""

from __future__ import annotations

from abc import ABC, abstractmethod
import os
import platform
import shutil
import struct
import subprocess
import sys
import tempfile
from typing import Dict, List, Optional, Set, Tuple, Union

BinaryData = Union[bytes, bytearray]


# ==============================================================================
# Executable Format Parsers (Mach-O, ELF, PE)
# ==============================================================================
# Each binary format parser extracts:
# 1. Architecture (`arm64` or `x86_64`)
# 2. Virtual Address (VA) to File Offset translation (`va_to_offset`)
# 3. Base Virtual Address for the `.text` segment (`get_text_start`)
# ==============================================================================

class BinaryFormat(ABC):
    """Base class and factory for executable binary formats (Mach-O, ELF, PE)."""
    name: str
    arch: str
    data: BinaryData
    text_start: Optional[int]

    @abstractmethod
    def get_text_start(self) -> int:
        """Returns the virtual address of the .text section."""
        ...

    @abstractmethod
    def va_to_offset(self, va: Optional[int]) -> Optional[int]:
        """Translates a virtual memory address to a physical file byte offset."""
        ...

    @staticmethod
    def parse(data: BinaryData) -> BinaryFormat:
        """Identifies executable format via magic bytes and returns a parsed container."""
        if data[:4] in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"):
            # 0xFEEDFACF (Little-Endian 64-bit Mach-O) or Big-Endian
            return MachO(data)
        elif data[:4] == b"\x7fELF":
            # 0x7F 'E' 'L' 'F' (Standard ELF)
            return ELF(data)
        elif data[:2] == b"MZ":
            # DOS header signature indicating PE/COFF executable
            return PE(data)
        raise ValueError("Unsupported binary format. Expected 64-bit Mach-O, ELF, or PE.")


class MachO(BinaryFormat):
    """
    Parser for macOS Mach-O 64-bit binaries.
    
    Parses `LC_SEGMENT_64` (0x19) load commands to map memory segments (such as `__TEXT`)
    and sections (`__text`) from Virtual Address to file offsets.
    """

    def __init__(self, data: BinaryData):
        self.name = "Mach-O 64-bit"
        self.data = data

        # Mach-O 64-bit header (32 bytes):
        # uint32 magic, int32 cputype, int32 cpusubtype, uint32 filetype,
        # uint32 ncmds, uint32 sizeofcmds, uint32 flags, uint32 reserved
        magic, cputype, cpusubtype, filetype, ncmds, sizeofcmds, flags, reserved = struct.unpack(
            "<IIIIIIII", data[:32]
        )

        # CPU Type: 0x0100000C = CPU_TYPE_ARM64, 0x01000007 = CPU_TYPE_X86_64
        self.arch = "arm64" if cputype == 0x0100000C else "x86_64"
        self.segments = []  # List of tuples: (vmaddr, vmsize, fileoff, filesize)
        self.text_start = None

        off = 32
        for _ in range(ncmds):
            cmd, cmdsize = struct.unpack("<II", data[off : off + 8])
            if cmd == 0x19:  # LC_SEGMENT_64
                segname, vmaddr, vmsize, fileoff, filesize, maxprot, initprot, nsects, sflags = struct.unpack(
                    "<16sQQQQIIII", data[off + 8 : off + 72]
                )
                self.segments.append((vmaddr, vmsize, fileoff, filesize))

                # Track the __TEXT segment and __text section virtual start address
                if segname.rstrip(b"\x00") == b"__TEXT":
                    sec_off = off + 72
                    for _ in range(nsects):
                        sectname, s_seg, s_addr, s_size, s_offset = struct.unpack(
                            "<16s16sQQI", data[sec_off : sec_off + 52]
                        )
                        if sectname.rstrip(b"\x00") == b"__text":
                            self.text_start = s_addr
                        sec_off += 80  # sizeof(struct section_64) = 80 bytes
            off += cmdsize

    def get_text_start(self) -> int:
        """Returns the virtual address of the .text section."""
        return self.text_start or 0

    def va_to_offset(self, va: Optional[int]) -> Optional[int]:
        """Translates a virtual memory address to a physical file byte offset."""
        if va is None:
            return None
        for vmaddr, vmsize, fileoff, filesize in self.segments:
            if filesize > 0 and vmaddr <= va < vmaddr + min(vmsize, filesize):
                return fileoff + (va - vmaddr)
        return None


class ELF(BinaryFormat):
    """
    Parser for Linux ELF 64-bit binaries (including Position Independent Executables).
    
    Parses Program Headers (`PT_LOAD`) for memory mappings and Section Headers (`.text`)
    to determine the base text address needed by Go's `runtime.pclntab`.
    """

    def __init__(self, data: BinaryData):
        self.name = "ELF 64-bit"
        self.data = data

        # e_machine field at byte offset 18 (2 bytes):
        # 183 (0xB7) = AArch64 (ARM64), 62 (0x3E) = AMD x86_64
        e_machine = struct.unpack("<H", data[18:20])[0]
        self.arch = "arm64" if e_machine == 183 else "x86_64"

        e_entry = struct.unpack("<Q", data[24:32])[0]
        e_phoff = struct.unpack("<Q", data[32:40])[0]
        e_phentsize, e_phnum = struct.unpack("<HH", data[54:58])
        self.segments = []  # (p_vaddr, p_memsz, p_offset, p_filesz)
        exec_vaddr = None

        # Parse 56-byte Elf64_Phdr program headers
        for i in range(e_phnum):
            poff = e_phoff + i * e_phentsize
            p_type, p_flags, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_align = struct.unpack(
                "<IIQQQQQQ", data[poff : poff + 56]
            )
            if p_type == 1:  # PT_LOAD
                self.segments.append((p_vaddr, p_memsz, p_offset, p_filesz))
                # PF_X flag = 1 (Executable)
                if (p_flags & 1) != 0 and exec_vaddr is None:
                    exec_vaddr = p_vaddr

        # Locate .text section header (critical for Go 1.20+ pclntab base offset calculation)
        self.text_start = None
        e_shoff = struct.unpack("<Q", data[40:48])[0]
        e_shentsize, e_shnum, e_shstrndx = struct.unpack("<HHH", data[58:64])
        if e_shoff != 0 and e_shnum > 0 and e_shstrndx < e_shnum:
            shstr_hdr = data[e_shoff + e_shstrndx * e_shentsize : e_shoff + (e_shstrndx + 1) * e_shentsize]
            shstr_off = struct.unpack("<Q", shstr_hdr[24:32])[0]
            for i in range(e_shnum):
                sh = data[e_shoff + i * e_shentsize : e_shoff + (i + 1) * e_shentsize]
                sh_name_idx = struct.unpack("<I", sh[0:4])[0]
                sh_flags, sh_addr, sh_offset, sh_size = struct.unpack("<QQQQ", sh[8:40])
                name_end = data.find(b"\x00", shstr_off + sh_name_idx)
                name = data[shstr_off + sh_name_idx : name_end].decode("ascii", errors="ignore")
                if name == ".text":
                    self.text_start = sh_addr
                    # In Go ELF PIE binaries linked with CGO/libc, C runtime startup stubs (crt1.o)
                    # precede Go code in .text. Scan forward for the first Go function prologue
                    # (runtime.text) to ensure pclntab entry offsets resolve to exact symbol VAs.
                    if self.arch == "arm64":
                        for off in range(sh_offset, min(sh_offset + 4096, len(data) - 4), 4):
                            if struct.unpack("<I", data[off : off + 4])[0] == 0xF9400B90:
                                self.text_start = sh_addr + (off - sh_offset)
                                break
                    elif self.arch == "x86_64":
                        for off in range(sh_offset, min(sh_offset + 4096, len(data) - 4)):
                            # cmp rsp, [r14+0x10] (49 3b 66 10) or cmp r12, [r14+0x10] (4d 3b 66 10)
                            if data[off : off + 4] in (b"\x49\x3b\x66\x10", b"\x4d\x3b\x66\x10"):
                                start_off = off
                                # If preceded by lea r12, [rsp-disp] (4c 8d 64 24), adjust start
                                if off >= sh_offset + 5 and data[off - 5 : off - 1] == b"\x4c\x8d\x64\x24":
                                    start_off = off - 5
                                self.text_start = sh_addr + (start_off - sh_offset)
                                break
                    break

        if self.text_start is None:
            self.text_start = e_entry if e_entry != 0 else (exec_vaddr or 0)

    def get_text_start(self) -> int:
        """Returns the virtual address of the .text section."""
        return self.text_start or 0

    def va_to_offset(self, va: Optional[int]) -> Optional[int]:
        """Translates an ELF virtual address to a file offset using PT_LOAD headers."""
        if va is None:
            return None
        for vaddr, memsz, offset, filesz in self.segments:
            if filesz > 0 and vaddr <= va < vaddr + filesz:
                return offset + (va - vaddr)
        return None


class PE(BinaryFormat):
    """
    Parser for Windows PE32+ (64-bit Portable Executable) binaries.
    
    Parses COFF Header, Optional Header (ImageBase), and Section Table
    to translate RVAs (Relative Virtual Addresses) to raw file offsets.
    """

    def __init__(self, data: BinaryData):
        self.name = "PE32+ 64-bit"
        self.data = data

        # DOS Header e_lfanew at offset 60 points to the PE Signature
        pe_off = struct.unpack("<I", data[60:64])[0]
        # Machine type: 0xAA64 = ARM64, 0x8664 = AMD64 (x86_64)
        machine = struct.unpack("<H", data[pe_off + 4 : pe_off + 6])[0]
        self.arch = "arm64" if machine == 0xAA64 else "x86_64"

        num_sections = struct.unpack("<H", data[pe_off + 6 : pe_off + 8])[0]
        opt_hdr_size = struct.unpack("<H", data[pe_off + 20 : pe_off + 22])[0]
        opt_hdr = pe_off + 24
        # ImageBase in PE32+ (64-bit) is a 64-bit integer at offset 24 of Optional Header
        self.image_base = struct.unpack("<Q", data[opt_hdr + 24 : opt_hdr + 32])[0]

        sec_off = opt_hdr + opt_hdr_size
        self.sections = []  # (vaddr, vsize, raw_ptr, raw_size)
        self.text_start = None

        for i in range(num_sections):
            soff = sec_off + i * 40
            name, vsize, vaddr, raw_size, raw_ptr = struct.unpack("<8sIIII", data[soff : soff + 24])
            self.sections.append((vaddr, vsize, raw_ptr, raw_size))
            if name.rstrip(b"\x00") == b".text":
                self.text_start = self.image_base + vaddr

    def get_text_start(self) -> int:
        """Returns the virtual address of the .text section."""
        return self.text_start or self.image_base

    def va_to_offset(self, va: Optional[int]) -> Optional[int]:
        """Translates a PE virtual address (ImageBase + RVA) to physical file offset."""
        if va is None:
            return None
        rva = va - self.image_base
        for vaddr, vsize, raw_ptr, raw_size in self.sections:
            if raw_size > 0 and vaddr <= rva < vaddr + min(vsize, raw_size):
                return raw_ptr + (rva - vaddr)
        return None

    def strip_authenticode(self, data: bytearray) -> bytearray:
        """
        Removes invalid Authenticode digital signature and clears PE CheckSum.
        
        When a signed Windows PE binary is modified on disk, the signature's
        authenticode hash becomes invalid (HashMismatch). The Windows image loader
        rejects execution of tampered signed binaries ('The request is not supported' /
        ERROR_NOT_SUPPORTED). Stripping the Certificate Table directory and clearing
        the checksum allows the binary to execute as a standard unsigned binary.
        """
        if len(data) < 64:
            return data
        pe_off = struct.unpack("<I", data[60:64])[0]
        if pe_off + 28 > len(data) or data[pe_off : pe_off + 4] != b"PE\x00\x00":
            return data

        opt_hdr = pe_off + 24
        magic = struct.unpack("<H", data[opt_hdr : opt_hdr + 2])[0]
        if magic == 0x20B:  # PE32+ (64-bit)
            dir_offset = opt_hdr + 112
            num_rva_offset = opt_hdr + 108
        elif magic == 0x10B:  # PE32 (32-bit)
            dir_offset = opt_hdr + 96
            num_rva_offset = opt_hdr + 92
        else:
            return data

        num_rva = struct.unpack("<I", data[num_rva_offset : num_rva_offset + 4])[0]
        # Certificate Table is entry 4 in DataDirectory (IMAGE_DIRECTORY_ENTRY_SECURITY)
        if num_rva > 4 and dir_offset + 40 <= len(data):
            sec_dir = dir_offset + 32  # 4 * 8 bytes
            cert_off, cert_sz = struct.unpack("<II", data[sec_dir : sec_dir + 8])
            if cert_sz > 0:
                print(f"[*] Stripping invalid Authenticode signature ({cert_sz:,} bytes)...")
                # Clear security directory entry
                data[sec_dir : sec_dir + 8] = b"\x00" * 8
                # Clear PE checksum (offset 64 of Optional Header)
                if opt_hdr + 68 <= len(data):
                    data[opt_hdr + 64 : opt_hdr + 68] = b"\x00" * 4
                # Truncate certificate table if appended to end of file
                if cert_off <= len(data) and cert_off + cert_sz >= len(data) - 16:
                    data = data[:cert_off]
        return data


# ==============================================================================
# Go pclntab & Semantic Resolver
# ==============================================================================
# The `runtime.pclntab` (Program Counter Line Table) is embedded into every Go
# binary. It contains a complete table of all function names, entry points,
# argument layouts, and stack-split metadata used by the Go runtime for stack
# traces, profiling, and garbage collection.
#
# By parsing pclntab, we dynamically obtain function virtual addresses without
# relying on compiler symbol tables or external debug info.
# ==============================================================================

class GoPclnTab:
    """Parses Go's runtime.pclntab and performs dynamic disassembly and semantic analysis."""

    def __init__(self, data: BinaryData, container: BinaryFormat):
        self.data = data
        self.container = container
        self.funcs: Dict[str, int] = {}  # Symbol Name -> Virtual Address (int)
        self.func_sizes: Dict[int, int] = {}  # Virtual Address (int) -> Function Byte Size (int)
        self.text_start = 0
        self._parse()

    def _parse(self):
        """Locates the pclntab magic header and parses the function symbol table."""
        # pclntab Magic Headers:
        # Go 1.20+: 0xFFFFFFF1 (\xf1\xff\xff\xff)
        # Go 1.18-1.19: 0xFFFFFFF0 (\xf0\xff\xff\xff)
        magics = [b"\xf1\xff\xff\xff", b"\xf0\xff\xff\xff"]
        candidates = []
        for m in magics:
            idx = self.data.find(m)
            while idx != -1:
                if idx + 72 <= len(self.data):
                    magic, pad, minLC, ptrSize, nfunc = struct.unpack("<IHBBQ", self.data[idx : idx + 16])
                    # Validate standard header constraints (64-bit pointers, reasonable function count)
                    if minLC in (1, 2, 4) and ptrSize == 8 and 1000 < nfunc < 500000:
                        candidates.append((nfunc, idx))
                idx = self.data.find(m, idx + 4)

        if not candidates:
            raise ValueError("Could not locate Go runtime.pclntab in binary.")

        # Pick the candidate with the maximum function count to ensure the main Go binary
        # pclntab is selected over any embedded helper binaries (e.g. WebM recorder).
        _, pcln_off = max(candidates, key=lambda c: c[0])

        # Go 1.20+ Header layout (72 bytes):
        # uint32 magic, uint16 pad, uint8 minLC, uint8 ptrSize, uint64 nfunc,
        # uint64 nfiles, uint64 textStart, uint64 funcnametab, uint64 cutab,
        # uint64 filetab, uint64 pctab, uint64 pclntab
        (
            magic,
            pad,
            minLC,
            ptrSize,
            nfunc,
            nfiles,
            textStart,
            funcnametab,
            cutab,
            filetab,
            pctab,
            pclntab,
        ) = struct.unpack("<IHBBQQQQQQQQ", self.data[pcln_off : pcln_off + 72])

        # textStart in Go 1.20+ is the base address for all relative function offsets.
        # In PIE binaries where textStart in header is 0, fall back to container .text start.
        self.text_start = textStart if textStart != 0 else self.container.get_text_start()
        table = self.data[pcln_off + pclntab : pcln_off + pclntab + 8 * (nfunc + 1)]
        funcnames = self.data[pcln_off + funcnametab :]

        for i in range(nfunc):
            entryOff, funcOff = struct.unpack("<II", table[i * 8 : (i + 1) * 8])
            f_data = self.data[pcln_off + pclntab + funcOff : pcln_off + pclntab + funcOff + 8]
            f_entryOff, nameOff = struct.unpack("<II", f_data)
            end = funcnames.find(b"\x00", nameOff)
            name = funcnames[nameOff:end].decode("ascii", errors="ignore")
            va = self.text_start + entryOff
            self.funcs[name] = va
            if (i + 2) * 8 <= len(table):
                next_entryOff = struct.unpack("<I", table[(i + 1) * 8 : (i + 1) * 8 + 4])[0]
                if next_entryOff > entryOff:
                    self.func_sizes[va] = next_entryOff - entryOff

    def decode_arm64_string_refs(self, fn_va: int, length: int = 512):
        """
        Disassembles ARM64 instructions looking for ADRP + ADD string/global reference pairs.
        
        ARM64 loads 64-bit addresses using a 2-instruction sequence:
          1. ADRP Xd, #page_imm (computes 4KB-aligned page address relative to PC)
          2. ADD Xd, Xn, #imm12 (adds lower 12-bit offset within that 4KB page)
        Tracks ADRP base pages per register across a short instruction window so
        pairs remain matched even if the compiler schedules instructions between them.
        """
        off = self.container.va_to_offset(fn_va)
        if off is None or off + length > len(self.data):
            return []
        fn_bytes = self.data[off : off + length]
        refs = []
        adrp_pages: Dict[int, Tuple[int, int]] = {}  # reg -> (byte_offset, page_va)
        for i in range(0, len(fn_bytes) - 3, 4):
            inst = struct.unpack("<I", fn_bytes[i : i + 4])[0]

            # Match ADRP: 1_00_10000_... (bits 31, 28:24 = 1 10000)
            if (inst & 0x9F000000) == 0x90000000:
                rd = inst & 0x1F
                immlo = (inst >> 29) & 0x3
                immhi = (inst >> 5) & 0x7FFFF
                imm = (immhi << 2) | immlo
                # Sign-extend 21-bit signed immediate
                if imm & (1 << 20):
                    imm -= 1 << 21
                pc = fn_va + i
                page = (pc & ~0xFFF) + (imm << 12)
                adrp_pages[rd] = (i, page)

            # Match ADD (immediate): 1001000100_... (bits 31:22 = 0x244)
            elif (inst & 0xFFC00000) == 0x91000000:
                rd = inst & 0x1F
                rn = (inst >> 5) & 0x1F
                imm12 = (inst >> 10) & 0xFFF
                if rn in adrp_pages:
                    adrp_i, page = adrp_pages[rn]
                    if i - adrp_i <= 16:
                        refs.append(page + imm12)
                    if rd == rn:
                        del adrp_pages[rn]
        return refs

    def decode_x86_string_refs(self, fn_va: int, length: int = 512, include_mov: bool = False):
        """
        Disassembles x86_64 instructions looking for RIP-relative LEA (and optionally MOV) references.
        
        Sequence:
          LEA/MOV r64, [rip + disp32] (Opcode: 0x48/0x4C 0x8D/0x8B ModR/M=0x05)
        """
        off = self.container.va_to_offset(fn_va)
        if off is None or off + length > len(self.data):
            return []
        fn_bytes = self.data[off : off + length]
        refs = []
        opcodes = (0x8D, 0x8B) if include_mov else (0x8D,)
        for i in range(0, len(fn_bytes) - 7):
            # REX.W prefix (0x48 or 0x4C) + 0x8D (LEA) / 0x8B (MOV) + ModR/M with RIP-relative addressing (0x05)
            if fn_bytes[i] in (0x48, 0x4C) and fn_bytes[i + 1] in opcodes and (fn_bytes[i + 2] & 0xC7) == 0x05:
                disp = struct.unpack("<i", fn_bytes[i + 3 : i + 7])[0]
                rip = fn_va + i + 7
                refs.append(rip + disp)
        return refs

    def decode_inlined_ascii_immediates(self, fn_va: int, length: int = 512) -> List[bytes]:
        """
        Reconstructs inlined 8-byte ASCII string constants from register-immediate
        instructions in a completely register-agnostic manner:
          - ARM64: Decodes MOVZ (0x52800000) + MOVK (0x72800000) sequences on any register X0..X30.
          - x86_64: Decodes MOVABS $imm64, r64 (REX.W + 0xB8..0xBF) on any register RAX..R15.
        Go compilers frequently inline short string equality comparisons (8-16 bytes)
        as 64-bit integer immediate comparisons rather than rodata pointers.
        """
        off = self.container.va_to_offset(fn_va)
        if off is None or off + length > len(self.data):
            return []
        fn_bytes = self.data[off : off + length]
        results: List[bytes] = []

        if self.container.arch == "arm64":
            regs: Dict[int, int] = {}
            for i in range(0, len(fn_bytes) - 3, 4):
                inst = struct.unpack("<I", fn_bytes[i : i + 4])[0]
                if (inst & 0x7F800000) == 0x52800000:  # MOVZ
                    rd = inst & 0x1F
                    hw = (inst >> 21) & 0x3
                    imm16 = (inst >> 5) & 0xFFFF
                    regs[rd] = imm16 << (hw * 16)
                elif (inst & 0x7F800000) == 0x72800000:  # MOVK
                    rd = inst & 0x1F
                    hw = (inst >> 21) & 0x3
                    imm16 = (inst >> 5) & 0xFFFF
                    regs[rd] = (regs.get(rd, 0) & ~(0xFFFF << (hw * 16))) | (imm16 << (hw * 16))
                    if hw == 3:
                        b = regs[rd].to_bytes(8, "little")
                        if all(32 <= x < 127 for x in b):
                            results.append(b)
        else:
            for i in range(0, len(fn_bytes) - 9):
                if fn_bytes[i] in (0x48, 0x49) and 0xB8 <= fn_bytes[i + 1] <= 0xBF:
                    b = bytes(fn_bytes[i + 2 : i + 10])
                    if all(32 <= x < 127 for x in b):
                        results.append(b)

        return results

    def get_string_at_va(self, va: int, max_len: int = 40) -> bytes:
        """Reads a printable ASCII string slice at the given virtual address."""
        off = self.container.va_to_offset(va)
        if off is None or off >= len(self.data):
            return b""
        s = b""
        for b in self.data[off : off + max_len]:
            if 32 <= b < 127:
                s += bytes([b])
            else:
                break
        return s

    def find_closure_by_string(self, package_prefix: str, target_str: str):
        """
        Finds anonymous closures in a package whose code references a target string literal.
        
        Scans only candidate functions containing '.func' to avoid scanning the entire binary.
        """
        candidates = [
            (name, va)
            for name, va in self.funcs.items()
            if name.startswith(package_prefix) and ".func" in name
        ]
        target_bytes = target_str.encode("ascii")
        matching = []

        is_arm64 = self.container.arch == "arm64"
        decoder = self.decode_arm64_string_refs if is_arm64 else self.decode_x86_string_refs

        for name, va in candidates:
            size = self.func_sizes.get(va, 256)
            refs = decoder(va, length=size)
            for r in refs:
                s = self.get_string_at_va(r, len(target_bytes) + 8)
                if target_bytes in s:
                    matching.append((name, va))
                    break
        return matching

    def find_slash_gatekeeper(self):
        """
        Dynamically locates the central permission gatekeeper (`hasBuiltinAgents`).
        
        Reverse Engineering Discovery:
        ------------------------------
        In `slashcommands.go`, restricted slash commands (/boost, /teamwork-preview,
        /browser, /browser-vision) do not implement individual permission checks.
        Instead, their registration closures load the command name and immediately
        call a single shared function: `slashcommands.hasBuiltinAgents(ctx, name)`.
        
        Strategy:
        1. Prioritize direct symbol lookup in pclntab for `slashcommands.hasBuiltinAgents`.
           Go pclntab symbol entries point directly to the true function entry.
        2. Fallback to tracing relative call instructions from slash command registration closures.
        
        Result: Unlocks ALL restricted slash commands simultaneously with ZERO collateral damage.
        """
        # 1. Prioritize direct symbol lookup in pclntab (exact entry point)
        for name, va in self.funcs.items():
            if name.endswith("slashcommands.hasBuiltinAgents"):
                return va

        # 2. Fallback to tracing relative call instructions from registration closures
        slash_pkg = "google3/third_party/jetski/cortex/slashcommands"
        is_arm64 = self.container.arch == "arm64"
        decoder = self.decode_arm64_string_refs if is_arm64 else self.decode_x86_string_refs

        for target_str in ("teamwork_preview", "DeepCoder", "DeepInvestigator"):
            target_bytes = target_str.encode("ascii")
            for name, va in self.funcs.items():
                if not name.startswith(slash_pkg):
                    continue
                size = self.func_sizes.get(va, 256)
                refs = decoder(va, length=size)
                for r in refs:
                    s = self.get_string_at_va(r, len(target_bytes) + 4)
                    if target_bytes in s:
                        # Find the first call instruction in this closure
                        off = self.container.va_to_offset(va)
                        if off is None:
                            continue
                        chunk = self.data[off : off + size]
                        if is_arm64:
                            for j in range(0, len(chunk) - 4, 4):
                                inst = struct.unpack("<I", chunk[j : j + 4])[0]
                                # Match ARM64 BL (Branch with Link): Opcode 0x94000000 (bits 31:26 = 100101)
                                if (inst & 0xFC000000) == 0x94000000:
                                    imm26 = inst & 0x3FFFFFF
                                    if imm26 & (1 << 25):
                                        imm26 -= 1 << 26
                                    return va + j + imm26 * 4
                        else:
                            for j in range(0, len(chunk) - 5):
                                # Match x86_64 CALL rel32: Opcode 0xE8
                                if chunk[j] == 0xE8:
                                    disp32 = struct.unpack("<i", chunk[j + 1 : j + 5])[0]
                                    rip = va + j + 5
                                    return rip + disp32
        return None

    def find_slash_predicates(self) -> List[Tuple[str, int]]:
        """
        Locates additional slash command availability predicates in `cortex/slashcommands`
        (such as `/deepagent` and `/review-agent` introduced in agy 1.4.2+).

        Uses a two-tier portable discovery strategy:
        1. Primary (Data-table inspection): Locate `slashcommands.GetSystemSlashCommandsDefinitions`,
           resolve its reference to the global `systemSlashCommands` slice header, and read each
           command entry's `{def_ptr, closure_ptr}` struct to map command names directly to their
           availability predicate function VAs.
        2. Fallback (Register-agnostic instruction & immediate decoding): Scan `slashcommands.init.func*`
           closures for either rodata string references or inlined 64-bit ASCII immediate constants
           (`MOVZ`/`MOVK` on ARM64, `MOVABS` on x86_64) independent of compiler register allocation.
        """
        target_cmds = (b"deepagent", b"review-agent")
        results: List[Tuple[str, int]] = []
        found_tags = set()
        is_arm64 = self.container.arch == "arm64"

        # 1. Primary: Parse global systemSlashCommands slice via GetSystemSlashCommandsDefinitions
        for name, va in self.funcs.items():
            if not name.endswith("slashcommands.GetSystemSlashCommandsDefinitions"):
                continue
            refs = (
                self.decode_arm64_string_refs(va, length=128)
                if is_arm64
                else self.decode_x86_string_refs(va, length=128, include_mov=True)
            )
            for r in refs:
                off = self.container.va_to_offset(r)
                if off is None or off + 24 > len(self.data):
                    continue
                ptr, length, cap = struct.unpack("<QQQ", self.data[off : off + 24])
                if not (1 <= length <= 64 and length <= cap <= 128):
                    continue
                arr_off = self.container.va_to_offset(ptr)
                if arr_off is None or arr_off + length * 8 > len(self.data):
                    continue
                for i in range(length):
                    elem_va = struct.unpack("<Q", self.data[arr_off + i * 8 : arr_off + (i + 1) * 8])[0]
                    elem_off = self.container.va_to_offset(elem_va)
                    if elem_off is None or elem_off + 16 > len(self.data):
                        continue
                    def_va, clos_va = struct.unpack("<QQ", self.data[elem_off : elem_off + 16])
                    def_off = self.container.va_to_offset(def_va)
                    clos_off = self.container.va_to_offset(clos_va)
                    if def_off is None or clos_off is None or def_off + 32 > len(self.data) or clos_off + 8 > len(self.data):
                        continue
                    s_ptr, s_len = struct.unpack("<QQ", self.data[def_off + 16 : def_off + 32])
                    if not (1 <= s_len <= 32):
                        continue
                    cmd = self.get_string_at_va(s_ptr, s_len)
                    fn_va = struct.unpack("<Q", self.data[clos_off : clos_off + 8])[0]
                    if cmd in target_cmds and fn_va in self.func_sizes:
                        tag = cmd.decode("ascii")
                        if tag not in found_tags:
                            found_tags.add(tag)
                            results.append((f"Slash command predicate (/{tag})", fn_va))

        if len(found_tags) == len(target_cmds):
            return results

        # 2. Fallback: Scan slashcommands.init.func* closures via rodata refs + register-agnostic immediates
        decoder = self.decode_arm64_string_refs if is_arm64 else self.decode_x86_string_refs
        for name, va in self.funcs.items():
            if "slashcommands.init.func" not in name:
                continue
            size = self.func_sizes.get(va, 256)
            matched_tag = None

            for r in decoder(va, length=size):
                s = self.get_string_at_va(r, 24)
                for cmd in target_cmds:
                    if s.startswith(cmd):
                        matched_tag = cmd.decode("ascii")
                        break
                if matched_tag:
                    break

            if matched_tag is None:
                for imm_bytes in self.decode_inlined_ascii_immediates(va, length=size):
                    for cmd in target_cmds:
                        if imm_bytes.startswith(cmd[:8]):
                            matched_tag = cmd.decode("ascii")
                            break
                    if matched_tag:
                        break

            if matched_tag is not None and matched_tag not in found_tags:
                found_tags.add(matched_tag)
                results.append((f"Slash command predicate (/{matched_tag})", va))

        return results

    def resolve_function_entry(self, va: int) -> int:
        """
        Bypassed: Go pclntab ftab entries already point directly to function entry points.
        Backward scanning is unnecessary and misidentified large-frame functions on x86_64
        (e.g. builtinSubagents shifted by -46 bytes into morestack slow-path).
        """
        return va

    def find_subagent_tools_gatekeeper(self):
        """
        Locates the `cortex/config/config.SubagentToolsEnabled` gatekeeper function.
        
        Reverse Engineering Discovery:
        ------------------------------
        `SubagentToolsEnabled` is the master feature switch in `cortex/config`:
        1. In `cortex/core/components/config.getBaselineConfig` (or `buildDefaultAgent`), it gates
           injecting `invoke_subagent`, `manage_subagents`, and `define_subagent` tools into
           the LLM tool registry (`baselineConfig.Tools`).
        2. In `cortex/core/components/config.builtinSubagents` (or `buildDefaultAgent`), it gates
           initialization of the built-in subagent list.
        Without patching this function to return true, the model never receives
        subagent tools in its schema and subagent delegation fails completely.
        """
        # Primary: direct symbol lookup in pclntab
        for name, va in self.funcs.items():
            if name.endswith("config.SubagentToolsEnabled"):
                return va

        # Fallback: scan for reference to string "invoke-subagent-config"
        is_arm64 = self.container.arch == "arm64"
        decoder = self.decode_arm64_string_refs if is_arm64 else self.decode_x86_string_refs
        cfg_pkg = "google3/third_party/jetski/cortex/config"
        target_bytes = b"invoke-subagent-config"
        for name, va in self.funcs.items():
            if not name.startswith(cfg_pkg):
                continue
            size = self.func_sizes.get(va, 256)
            refs = decoder(va, length=size)
            for r in refs:
                s = self.get_string_at_va(r, len(target_bytes) + 4)
                if target_bytes in s:
                    return va
        return None

    def find_subagent_predicates(self):
        """
        Dynamically finds the 3 subagent predicate functions in `builtin/agents`.
        
        Reverse Engineering Discovery:
        ------------------------------
        During `agents.init`, Antigravity conditionally registers:
          - `teamwork-subagent`: Guarded by experiment flag "enable-teamwork-subagent".
          - `DeepCoder` (Owl): Guarded by experiment flag "enable-owl-slash-command".
          - `DeepInvestigator` (Owl): Guarded by experiment flag "enable-owl-slash-command".
        
        Strategy:
        1. Primary: Iterate over `pclntab` function entries in `builtin/agents` (`agents.init.func*`)
           using their exact `pclntab` sizes and decode string references. This does not depend
           on compiler stack-check prologue opcodes.
        2. Fallback: Scan the raw `builtin/agents` code range and walk backward to the function
           stack-check prologue if `pclntab` closure entries are unavailable.
        """
        agents_pkg = "google3/third_party/jetski/cortex/customizations/builtin/agents"
        entries = []
        seen = set()
        is_arm64 = self.container.arch == "arm64"
        decoder = self.decode_arm64_string_refs if is_arm64 else self.decode_x86_string_refs

        # 1. Primary: Pclntab function-boundary scan
        for name, va in self.funcs.items():
            if not name.startswith(agents_pkg) or name.endswith(".init") or name.endswith(".init.0"):
                continue
            size = self.func_sizes.get(va, 512)
            for r in decoder(va, length=size):
                s = self.get_string_at_va(r, 40)
                if s.startswith(b"enable-teamwork-subagent") or s.startswith(b"enable-owl-slash-command"):
                    if va not in seen:
                        seen.add(va)
                        tag = "teamwork-subagent" if s.startswith(b"enable-teamwork-subagent") else "owl-subagent"
                        entries.append((f"Subagent predicate ({tag})", va))
                    break

        if entries:
            return entries

        # 2. Fallback: Raw range scan + backward prologue walk
        pkg_vas = [va for name, va in self.funcs.items() if name.startswith(agents_pkg)]
        if not pkg_vas:
            return []
        min_va, max_va = min(pkg_vas), max(pkg_vas) + 0x2000

        if is_arm64:
            for va in range(min_va, max_va, 4):
                off = self.container.va_to_offset(va)
                if off is None or off + 8 > len(self.data):
                    continue
                inst1 = struct.unpack("<I", self.data[off : off + 4])[0]
                inst2 = struct.unpack("<I", self.data[off + 4 : off + 8])[0]
                # Match ADRP + ADD
                if (inst1 & 0x9F000000) == 0x90000000 and (inst2 & 0xFFC00000) == 0x91000000:
                    immlo = (inst1 >> 29) & 0x3
                    immhi = (inst1 >> 5) & 0x7FFFF
                    imm = (immhi << 2) | immlo
                    if imm & (1 << 20):
                        imm -= 1 << 21
                    page = (va & ~0xFFF) + (imm << 12)
                    rn2 = (inst2 >> 5) & 0x1F
                    rd1 = inst1 & 0x1F
                    imm12 = (inst2 >> 10) & 0xFFF
                    if rn2 == rd1:
                        target = page + imm12
                        s = self.get_string_at_va(target, 40)
                        if b"enable-teamwork-subagent" in s or b"enable-owl-slash-command" in s:
                            # Walk backward from this instruction to find the function entry prologue
                            for step in range(4, 512, 4):
                                p_off = off - step
                                if p_off < 0:
                                    break
                                if struct.unpack("<I", self.data[p_off : p_off + 4])[0] == 0xF9400B90:
                                    entry = va - step
                                    if entry not in seen:
                                        seen.add(entry)
                                        tag = "teamwork-subagent" if s.startswith(b"enable-teamwork-subagent") else "owl-subagent"
                                        entries.append((f"Subagent predicate ({tag})", entry))
                                    break
        else:
            # x86_64
            for va in range(min_va, max_va):
                off = self.container.va_to_offset(va)
                if off is None or off + 7 > len(self.data):
                    continue
                # Match LEA r64, [rip + disp32]
                if self.data[off] in (0x48, 0x4C) and self.data[off + 1] == 0x8D and (self.data[off + 2] & 0xC7) == 0x05:
                    disp = struct.unpack("<i", self.data[off + 3 : off + 7])[0]
                    target = va + 7 + disp
                    s = self.get_string_at_va(target, 40)
                    if b"enable-teamwork-subagent" in s or b"enable-owl-slash-command" in s:
                        for step in range(1, 512):
                            p_off = off - step
                            if p_off < 0:
                                break
                            if self.data[p_off : p_off + 4] == b"\x49\x3b\x66\x10":
                                entry = va - step
                                if entry not in seen:
                                    seen.add(entry)
                                    tag = "teamwork-subagent" if s.startswith(b"enable-teamwork-subagent") else "owl-subagent"
                                    entries.append((f"Subagent predicate ({tag})", entry))
                                break
        return entries

    def find_builtin_subagents(self) -> int | None:
        """
        Locates the primary subagent configuration function in `cortex/core/components/config`
        (`config.builtinSubagents`, `config.buildDefaultAgent`, or `config.getBaselineConfig`).
        """
        for suffix in (
            "config.builtinSubagents",
            "config.buildDefaultAgent",
            "config.getBaselineConfig",
        ):
            for name, va in self.funcs.items():
                if name.endswith(suffix):
                    return va
        for name, va in self.funcs.items():
            if "cortex/core/components/config/" in name:
                return va
        return None

    def find_subagent_feature_calls(self, func_va: int) -> list[int]:
        """
        Finds the indirect call sites to FeatureProvider.IsEnabled across
        `cortex/core/components/config` (`builtinSubagents`, `buildDefaultAgent`,
        `deepAgent`, `reviewAgent`, or any future refactored config helpers) for:
          - 'enable-teamwork-subagent'
          - 'enable-owl-slash-command'
          - 'enable-deepagent'
          - 'enable-review'
        while skipping 'enable-browser-subagent-v2' and unrelated feature flags to avoid
        unintended side effects or the unhandled Playwright driver 404 download bug.

        Uses register-agnostic indirect call matching:
          - ARM64: Matches any `BLR Xn` (`(inst & 0xFFFFFC1F) == 0xD63F0000`).
          - x86_64: Matches any `CALLQ *%rN` (`FF D0..D7` or `41 FF D0..D7`).

        Returns file offsets of the call instructions to patch.
        """
        target_vas = [func_va]
        for name, va in self.funcs.items():
            if "cortex/core/components/config/" in name and va not in target_vas:
                target_vas.append(va)

        allowed_flags = (
            b"enable-teamwork-subagent",
            b"enable-owl-slash-command",
            b"enable-deepagent",
            b"enable-review",
        )
        is_arm64 = self.container.arch == "arm64"
        decoder = self.decode_arm64_string_refs if is_arm64 else self.decode_x86_string_refs

        matched_offsets: list[int] = []
        for va in target_vas:
            off = self.container.va_to_offset(va)
            if off is None:
                continue
            size = self.func_sizes.get(va, 0x400)
            chunk = self.data[off : off + size]

            if is_arm64:
                for pos in range(0, len(chunk) - 3, 4):
                    inst = struct.unpack("<I", chunk[pos : pos + 4])[0]
                    # Match any ARM64 BLR Xn: 1101011_0001_11111_000000_rn_00000
                    if (inst & 0xFFFFFC1F) != 0xD63F0000:
                        continue
                    win_start = max(0, pos - 32)
                    refs = decoder(va + win_start, length=pos - win_start)
                    matched = any(
                        self.get_string_at_va(r, 32).startswith(flag)
                        for r in refs
                        for flag in allowed_flags
                    )
                    if not matched and va == func_va and size <= 0x400:
                        pre = chunk[max(0, pos - 16) : pos]
                        matched = bytes.fromhex("e2077db2") in pre or bytes.fromhex("020380d2") in pre
                    if matched:
                        matched_offsets.append(off + pos)
            else:
                pos = 0
                while pos < len(chunk) - 1:
                    # Match x86_64 CALLQ *%rN (FF D0..D7)
                    if chunk[pos] == 0xFF and 0xD0 <= chunk[pos + 1] <= 0xD7:
                        win_start = max(0, pos - 32)
                        refs = decoder(va + win_start, length=pos - win_start)
                        matched = any(
                            self.get_string_at_va(r, 32).startswith(flag)
                            for r in refs
                            for flag in allowed_flags
                        )
                        if not matched and va == func_va and size <= 0x400:
                            pre = chunk[max(0, pos - 16) : pos]
                            matched = b"\xb9\x18\x00\x00\x00" in pre
                        if matched:
                            matched_offsets.append(off + pos)
                            pos += 2
                            continue
                    pos += 1

        return matched_offsets


# ==============================================================================
# Patcher Core
# ==============================================================================

def patch_binary(input_path: str, output_path: str):
    """
    Loads, dynamically resolves, patches, and writes the modified agy binary.
    
    Safety Guarantees:
    - Writes to a temporary file first, then atomically replaces destination.
    - Preserves file permissions and execution bit.
    - Re-signs Mach-O binaries with ad-hoc signature on macOS (required by AMFI).
    - Strips invalid Authenticode signatures and clears checksum on Windows PE binaries.
    """
    print(f"[*] Reading source binary: {input_path}")
    with open(input_path, "rb") as f:
        data = bytearray(f.read())

    container = BinaryFormat.parse(data)
    print(f"[*] Detected format: {container.name} ({container.arch})")

    pclntab = GoPclnTab(data, container)
    print(f"[*] Parsed {len(pclntab.funcs):,} Go symbols from runtime.pclntab")

    # Determine architecture-specific "return true" instruction bytes:
    # ARM64:
    #   mov x0, #1  -> 0x20 0x00 0x80 0xD2
    #   ret         -> 0xC0 0x03 0x5F 0xD6
    # x86_64:
    #   mov eax, 1  -> 0xB8 0x01 0x00 0x00 0x00
    #   ret         -> 0xC3
    if container.arch == "arm64":
        patch_return_true = bytes.fromhex("200080d2c0035fd6")
    elif container.arch == "x86_64":
        patch_return_true = bytes.fromhex("b801000000c3")
    else:
        raise ValueError(f"Unsupported architecture: {container.arch}")

    targets_to_patch = []
    seen_vas = set()

    # 1. Central Slash Commands Gatekeeper (unlocks /boost, /teamwork-preview, /browser, /browser-vision)
    gatekeeper_va = pclntab.find_slash_gatekeeper()
    assert gatekeeper_va is not None, "Could not locate slash commands gatekeeper (hasBuiltinAgents)."
    targets_to_patch.append(("Slash commands gatekeeper (hasBuiltinAgents)", gatekeeper_va))
    seen_vas.add(gatekeeper_va)

    # 1b. Additional Slash Command Predicates (unlocks /deepagent, /review-agent in agy 1.4.2+)
    for label, entry in pclntab.find_slash_predicates():
        if entry not in seen_vas:
            targets_to_patch.append((label, entry))
            seen_vas.add(entry)

    # 2. Subagent Tools Gatekeeper (SubagentToolsEnabled - unlocks invoke_subagent, define_subagent, manage_subagents)
    subagent_tools_va = pclntab.find_subagent_tools_gatekeeper()
    assert subagent_tools_va is not None, "Could not locate subagent tools gatekeeper (SubagentToolsEnabled)."
    if subagent_tools_va not in seen_vas:
        targets_to_patch.append(("Subagent tools gatekeeper (SubagentToolsEnabled)", subagent_tools_va))
        seen_vas.add(subagent_tools_va)

    # 3. Builtin Subagent Predicates (teamwork-subagent, DeepCoder, DeepInvestigator)
    subagent_targets = pclntab.find_subagent_predicates()
    for label, entry in subagent_targets:
        if entry not in seen_vas:
            targets_to_patch.append((label, entry))
            seen_vas.add(entry)

    if not targets_to_patch:
        raise RuntimeError("No target predicates found to patch.")

    print(f"[*] Identified {len(targets_to_patch)} predicates to patch:")
    for label, va in targets_to_patch:
        off = container.va_to_offset(va)
        print(f"    - {label}: VA={hex(va)} -> FileOffset={hex(off) if off else 'N/A'}")

    # Overwrite the entry instructions of each target with `return true`
    for label, va in targets_to_patch:
        off = container.va_to_offset(va)
        if off is None or off + len(patch_return_true) > len(data):
            raise RuntimeError(f"Cannot map VA {hex(va)} to file offset for {label}")
        data[off : off + len(patch_return_true)] = patch_return_true

    # 4. Builtin Subagents Allowed List Registration (builtinSubagents / buildDefaultAgent)
    # Patches the runtime registry so invoke_subagent permits DeepInvestigator,
    # DeepCoder, teamwork_preview, deepagent, and review-agent, while keeping browser disabled.
    builtin_subagents_va = pclntab.find_builtin_subagents()
    assert builtin_subagents_va is not None, "Could not locate config.builtinSubagents or config.buildDefaultAgent in binary."
    call_offsets = pclntab.find_subagent_feature_calls(builtin_subagents_va)
    assert len(call_offsets) >= 3, (
        f"Expected at least 3 builtinSubagents feature check calls to patch in {hex(builtin_subagents_va)}, "
        f"found {len(call_offsets)}"
    )
    print(f"[*] Identified {len(call_offsets)} builtinSubagents feature check calls to patch:")
    if container.arch == "arm64":
        call_patch = bytes.fromhex("20008052")  # mov w0, #1
        for call_off in call_offsets:
            print(f"    - Subagent feature call: FileOffset={hex(call_off)}")
            data[call_off : call_off + len(call_patch)] = call_patch
    else:
        # x86_64: In Go's register ABI (ABIInternal), string length for `fp.IsEnabled(flag)` is passed
        # in ECX via `movl $<len>, %ecx` (0xB9 <len> 0x00 0x00 0x00) immediately preceding `callq *%rN`.
        # Replace `movl $<len>, %ecx` + `callq *%rN` with `movl $1, %eax` + NOPs to set RAX = 1 cleanly.
        for call_off in call_offsets:
            win_start = max(0, call_off - 16)
            pre = data[win_start:call_off]
            mov_idx = -1
            for i in range(len(pre) - 5, -1, -1):
                if pre[i] == 0xB9 and 1 <= pre[i + 1] <= 64 and pre[i + 2 : i + 5] == b"\x00\x00\x00":
                    mov_idx = i
                    break
            if mov_idx != -1:
                mov_off = win_start + mov_idx
                span = (call_off + 2) - mov_off
                patch = b"\xb8\x01\x00\x00\x00" + b"\x90" * (span - 5)
                print(f"    - Subagent feature call: FileOffset={hex(mov_off)} (span {span} bytes: mov eax, 1 + nops)")
                data[mov_off : mov_off + span] = patch
            else:
                # In-place 2-byte fallback: replace `callq *%rN` (FF D0..D7) with `movb $1, %al` (B0 01).
                call_start = call_off - 1 if data[call_off - 1] == 0x41 else call_off
                call_len = (call_off + 2) - call_start
                patch = b"\xb0\x01" + b"\x90" * (call_len - 2)
                data[call_start : call_start + call_len] = patch
                print(f"    - Subagent feature call: FileOffset={hex(call_start)} (in-place mov al, 1)")

    # For Windows PE binaries, strip the invalid Authenticode signature
    if isinstance(container, PE):
        data = container.strip_authenticode(data)

    # Safe atomic write via temporary file in the destination directory
    out_dir = os.path.dirname(os.path.abspath(output_path))
    os.makedirs(out_dir, exist_ok=True)
    temp_file = tempfile.NamedTemporaryFile(dir=out_dir, delete=False, prefix=".agy_patch_")
    temp_path = temp_file.name
    try:
        temp_file.write(data)
        temp_file.flush()
        temp_file.close()

        # Copy executable permissions from source binary
        st = os.stat(input_path)
        os.chmod(temp_path, st.st_mode | 0o111)

        # macOS: Code-sign with ad-hoc signature (-s -) to satisfy Apple Mobile File Integrity (AMFI)
        if platform.system() == "Darwin" and container.name.startswith("Mach-O"):
            print("[*] Re-signing binary with macOS ad-hoc signature...")
            subprocess.run(["codesign", "-f", "-s", "-", temp_path], check=True, capture_output=True)

        # Atomic replacement: replaces the destination binary without leaving it partially written
        os.replace(temp_path, output_path)
        print(f"[+] Successfully wrote patched binary to: {output_path}")

    except Exception as e:
        if os.path.exists(temp_path):
            os.remove(temp_path)
        raise e


# ==============================================================================
# CLI Entry Point
# ==============================================================================

def main():
    """
    Parses CLI arguments and executes the patching routine.
    
    Argument Handling:
      0 args: Finds `agy` in PATH, patches to a temp file, and atomically replaces it.
      1 arg:  Patches the specified file in-place via a temp file and atomic replace.
      2 args: Patches the file at arg 1 and writes the output to arg 2.
    """
    args = sys.argv[1:]
    if any(arg in ("-h", "--help") for arg in args):
        print("Usage:")
        print("  patch.py                  # Patch agy in PATH in-place")
        print("  patch.py <input>          # Patch <input> in-place")
        print("  patch.py <input> <output> # Patch <input> to <output>")
        sys.exit(0)

    if len(args) == 0:
        agy_path = shutil.which("agy")
        if not agy_path:
            print("Error: Could not find 'agy' executable in PATH.", file=sys.stderr)
            sys.exit(1)
        input_path = os.path.realpath(agy_path)
        output_path = input_path
        banner = f"[*] No arguments provided. Defaulting to PATH executable:\n    {input_path}"
    elif len(args) == 1:
        input_path = os.path.realpath(args[0])
        output_path = input_path
        banner = f"[*] 1 argument provided. Patching in-place:\n    {input_path}"
    elif len(args) == 2:
        input_path = os.path.realpath(args[0])
        output_path = os.path.realpath(args[1])
        banner = f"[*] 2 arguments provided.\n    Input:  {input_path}\n    Output: {output_path}"
    else:
        print("Usage:", file=sys.stderr)
        print("  patch.py                  # Patch agy in PATH in-place", file=sys.stderr)
        print("  patch.py <input>          # Patch <input> in-place", file=sys.stderr)
        print("  patch.py <input> <output> # Patch <input> to <output>", file=sys.stderr)
        sys.exit(1)

    if not os.path.isfile(input_path):
        print(f"Error: Input file does not exist: {input_path}", file=sys.stderr)
        sys.exit(1)

    print(banner)

    try:
        patch_binary(input_path, output_path)
    except Exception as err:
        print(f"[-] Patch failed: {err}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
