#!/usr/bin/env python3
"""
patch_agy.py - Universal Dynamic Patcher for Google Antigravity CLI (agy)
==========================================================================

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

2. Go Linker Optimizations & Identical Code Folding (ICF):
   - On Linux ELF PIE builds, Go's linker aggressively inlines tiny functions and
     merges identical code blocks (ICF).
   - Trap / Pitfall: When a function like `config.SubagentToolsEnabled` is inlined,
     its body disappears, but `runtime.pclntab` may still map the symbol to the
     `RET` instruction of the preceding function (`config.GetAgentScriptReroute`).
     Overwriting 8 bytes at that location clobbers the adjacent function's second
     epilogue branch, skipping stack frame cleanup and causing infinite spin loops
     or segfaults.
   - Solution: Never patch inlined/folded stub symbols. Patch the central gatekeeper
     and verify that every patch target begins with a genuine function prologue.

3. Volatile Closure Numbering (`func1` .. `funcN`):
   - Go names anonymous closures sequentially (`agents.init.func7`, etc.).
   - If upstream source code changes (adding/removing a single closure), the numbers
     shift. Hardcoded symbol names or offsets break on virtually every update.
   - Solution: Dynamically locate closures by scanning for semantic string literals
     (`"teamwork_preview"`, `"enable-teamwork-subagent"`, `"enable-owl-slash-command"`)
     and tracing relative call instructions (`BL` on ARM64, `CALL` on x86_64).

4. Mid-Function / Split-Block Labels in `pclntab`:
   - In ELF PIE binaries, some `pclntab` entries point to internal jump targets
     rather than function entry points.
   - Solution: The patcher scans backward to locate the true Go function prologue:
     * ARM64: Stack-split check `ldr x16, [x28, #16]` (`0xF9400B90`) or post-`RET`.
     * x86_64: Stack-split check `cmp rsp, [r14+0x10]` (`0x49 0x3B 0x66 0x10`) or post-`RET`.

Patching Architecture:
----------------------
Instead of fragile, brute-force patching of dozens of closures, this patcher targets
EXACTLY 4 stable, semantic entry points:
  [1] `slashcommands.hasBuiltinAgents`:
      Discovered by tracing the call inside the `"teamwork_preview"` closure.
      Patching this returns `true` for all restricted slash commands (/boost, /teamwork-preview).
  [2] `builtin/agents` Teamwork Subagent Predicate:
      Discovered by tracing the `"enable-teamwork-subagent"` string reference to its prologue.
  [3] `builtin/agents` Owl Subagent Predicate 1 (DeepCoder):
      Discovered by tracing the first `"enable-owl-slash-command"` reference to its prologue.
  [4] `builtin/agents` Owl Subagent Predicate 2 (DeepInvestigator):
      Discovered by tracing the second `"enable-owl-slash-command"` reference to its prologue.

Usage:
------
  python3 patch_agy.py                  # Patch agy found in PATH in-place
  python3 patch_agy.py <input>          # Patch <input> in-place
  python3 patch_agy.py <input> <output> # Patch <input> to <output>

Dependencies:
-------------
Zero external third-party packages (uses only Python 3 standard library).
"""

import sys
import os
import shutil
import struct
import tempfile
import subprocess
import platform


# ==============================================================================
# Executable Format Parsers (Mach-O, ELF, PE)
# ==============================================================================
# Each binary format parser extracts:
# 1. Architecture (`arm64` or `x86_64`)
# 2. Virtual Address (VA) to File Offset translation (`va_to_offset`)
# 3. Base Virtual Address for the `.text` segment (`get_text_start`)
# ==============================================================================

class BinaryFormat:
    """Factory class to identify and instantiate the appropriate executable parser."""

    @staticmethod
    def parse(data: bytes):
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


class MachO:
    """
    Parser for macOS Mach-O 64-bit binaries.
    
    Parses `LC_SEGMENT_64` (0x19) load commands to map memory segments (such as `__TEXT`)
    and sections (`__text`) from Virtual Address to file offsets.
    """

    def __init__(self, data: bytes):
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

    def va_to_offset(self, va: int):
        """Translates a virtual memory address to a physical file byte offset."""
        for vmaddr, vmsize, fileoff, filesize in self.segments:
            if vmaddr <= va < vmaddr + vmsize:
                return fileoff + (va - vmaddr)
        return None


class ELF:
    """
    Parser for Linux ELF 64-bit binaries (including Position Independent Executables).
    
    Parses Program Headers (`PT_LOAD`) for memory mappings and Section Headers (`.text`)
    to determine the base text address needed by Go's `runtime.pclntab`.
    """

    def __init__(self, data: bytes):
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
                    break

        if self.text_start is None:
            self.text_start = e_entry if e_entry != 0 else (exec_vaddr or 0)

    def get_text_start(self) -> int:
        """Returns the virtual address of the .text section."""
        return self.text_start or 0

    def va_to_offset(self, va: int):
        """Translates an ELF virtual address to a file offset using PT_LOAD headers."""
        for vaddr, memsz, offset, filesz in self.segments:
            if vaddr <= va < vaddr + memsz:
                return offset + (va - vaddr)
        return None


class PE:
    """
    Parser for Windows PE32+ (64-bit Portable Executable) binaries.
    
    Parses COFF Header, Optional Header (ImageBase), and Section Table
    to translate RVAs (Relative Virtual Addresses) to raw file offsets.
    """

    def __init__(self, data: bytes):
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

    def va_to_offset(self, va: int):
        """Translates a PE virtual address (ImageBase + RVA) to physical file offset."""
        rva = va - self.image_base
        for vaddr, vsize, raw_ptr, raw_size in self.sections:
            if vaddr <= rva < vaddr + vsize:
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

    def __init__(self, data: bytes, container: BinaryFormat):
        self.data = data
        self.container = container
        self.funcs = {}  # Symbol Name -> Virtual Address (int)
        self.text_start = 0
        self._parse()

    def _parse(self):
        """Locates the pclntab magic header and parses the function symbol table."""
        # pclntab Magic Headers:
        # Go 1.20+: 0xFFFFFFF1 (\xf1\xff\xff\xff)
        # Go 1.18-1.19: 0xFFFFFFF0 (\xf0\xff\xff\xff)
        magics = [b"\xf1\xff\xff\xff", b"\xf0\xff\xff\xff"]
        pcln_off = None
        for m in magics:
            idx = self.data.find(m)
            while idx != -1:
                if idx + 72 <= len(self.data):
                    magic, pad, minLC, ptrSize, nfunc = struct.unpack("<IHBBQ", self.data[idx : idx + 16])
                    # Validate standard header constraints (64-bit pointers, reasonable function count)
                    if minLC in (1, 2, 4) and ptrSize == 8 and 1000 < nfunc < 500000:
                        pcln_off = idx
                        break
                idx = self.data.find(m, idx + 4)
            if pcln_off is not None:
                break

        if pcln_off is None:
            raise ValueError("Could not locate Go runtime.pclntab in binary.")

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
        table = self.data[pcln_off + pclntab : pcln_off + pclntab + 8 * nfunc]
        funcnames = self.data[pcln_off + funcnametab :]

        for i in range(nfunc):
            entryOff, funcOff = struct.unpack("<II", table[i * 8 : (i + 1) * 8])
            f_data = self.data[pcln_off + pclntab + funcOff : pcln_off + pclntab + funcOff + 8]
            f_entryOff, nameOff = struct.unpack("<II", f_data)
            end = funcnames.find(b"\x00", nameOff)
            name = funcnames[nameOff:end].decode("ascii", errors="ignore")
            self.funcs[name] = self.text_start + entryOff

    def decode_arm64_string_refs(self, fn_va: int, length: int = 512):
        """
        Disassembles ARM64 instructions looking for ADRP + ADD string reference pairs.
        
        ARM64 loads 64-bit string addresses using a 2-instruction sequence:
          1. ADRP Xd, #page_imm (computes 4KB-aligned page address relative to PC)
          2. ADD Xd, Xn, #imm12 (adds lower 12-bit offset within that 4KB page)
        """
        off = self.container.va_to_offset(fn_va)
        if off is None or off + length > len(self.data):
            return []
        fn_bytes = self.data[off : off + length]
        refs = []
        for i in range(0, len(fn_bytes) - 8, 4):
            inst1 = struct.unpack("<I", fn_bytes[i : i + 4])[0]
            inst2 = struct.unpack("<I", fn_bytes[i + 4 : i + 8])[0]

            # Match ADRP: 1_00_10000_... (bits 31, 28:24 = 1 10000)
            if (inst1 & 0x9F000000) == 0x90000000:
                rd1 = inst1 & 0x1F
                immlo = (inst1 >> 29) & 0x3
                immhi = (inst1 >> 5) & 0x7FFFF
                imm = (immhi << 2) | immlo
                # Sign-extend 21-bit signed immediate
                if imm & (1 << 20):
                    imm -= 1 << 21
                pc = fn_va + i
                page = (pc & ~0xFFF) + (imm << 12)

                # Match ADD (immediate): 1001000100_... (bits 31:22 = 0x244)
                if (inst2 & 0xFFC00000) == 0x91000000:
                    rd2 = inst2 & 0x1F
                    rn2 = (inst2 >> 5) & 0x1F
                    imm12 = (inst2 >> 10) & 0xFFF
                    # Verify ADD uses the exact same register populated by ADRP
                    if rn2 == rd1:
                        refs.append(page + imm12)
        return refs

    def decode_x86_string_refs(self, fn_va: int, length: int = 512):
        """
        Disassembles x86_64 instructions looking for RIP-relative LEA references.
        
        Sequence:
          LEA r64, [rip + disp32] (Opcode: 0x48/0x4C 0x8D ModR/M=0x05)
        """
        off = self.container.va_to_offset(fn_va)
        if off is None or off + length > len(self.data):
            return []
        fn_bytes = self.data[off : off + length]
        refs = []
        for i in range(0, len(fn_bytes) - 7):
            # REX.W prefix (0x48 or 0x4C) + 0x8D (LEA) + ModR/M with RIP-relative addressing (0x05)
            if fn_bytes[i] in (0x48, 0x4C) and fn_bytes[i + 1] == 0x8D and (fn_bytes[i + 2] & 0xC7) == 0x05:
                disp = struct.unpack("<i", fn_bytes[i + 3 : i + 7])[0]
                rip = fn_va + i + 7
                refs.append(rip + disp)
        return refs

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
            # Bound search to 256 bytes from function entry to avoid bleeding into adjacent functions
            refs = decoder(va, length=256)
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
        
        Rather than patching 4 different closures (which may be inlined or point to
        internal jump targets in ELF PIE builds), this method:
        1. Finds the closure referencing "teamwork_preview", "DeepCoder", or "DeepInvestigator".
        2. Disassembles forward to find the relative call instruction (`BL` on ARM64, `CALL` on x86_64).
        3. Follows the relative branch to resolve the exact gatekeeper address.
        
        Result: Unlocks ALL restricted slash commands simultaneously with ZERO collateral damage.
        """
        slash_pkg = "google3/third_party/jetski/cortex/slashcommands"
        is_arm64 = self.container.arch == "arm64"
        decoder = self.decode_arm64_string_refs if is_arm64 else self.decode_x86_string_refs

        for target_str in ("teamwork_preview", "DeepCoder", "DeepInvestigator"):
            target_bytes = target_str.encode("ascii")
            for name, va in self.funcs.items():
                if not name.startswith(slash_pkg):
                    continue
                refs = decoder(va, length=256)
                for r in refs:
                    s = self.get_string_at_va(r, len(target_bytes) + 4)
                    if target_bytes in s:
                        # Find the first call instruction in this closure
                        off = self.container.va_to_offset(va)
                        if off is None:
                            continue
                        chunk = self.data[off : off + 256]
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
        # Fallback to direct symbol lookup in pclntab if call tracing fails
        for name, va in self.funcs.items():
            if name.endswith("slashcommands.hasBuiltinAgents"):
                return va
        return None

    def resolve_function_entry(self, va: int):
        """
        Ensures a candidate address points to a genuine Go function prologue.
        
        Reverse Engineering Discovery:
        ------------------------------
        In optimized ELF PIE binaries, some pclntab symbols point to internal basic
        blocks or loop headers instead of function entries. Writing `mov x0, 1; ret`
        at an internal label skips caller stack deallocation (`ldr x30, [sp], #N`)
        and triggers `SIGSEGV` or memory corruption.
        
        This method checks for the standard Go compiler stack-split prologue:
        - ARM64: `ldr x16, [x28, #16]` (`0xF9400B90`) where X28 holds the `g` pointer.
        - x86_64: `cmp rsp, [r14+0x10]` (`0x49 0x3B 0x66 0x10`) where R14 holds `g`.
        
        If the current instruction is not a prologue, it scans backward up to 300 bytes
        to locate the real entry point, or the byte immediately following the preceding `RET`.
        """
        off = self.container.va_to_offset(va)
        if off is None:
            return va

        if self.container.arch == "arm64":
            inst = struct.unpack("<I", self.data[off : off + 4])[0]
            if inst == 0xF9400B90:  # ldr x16, [x28, #16]
                return va
            for step in range(4, 300, 4):
                prev_off = off - step
                if prev_off < 0:
                    break
                p_inst = struct.unpack("<I", self.data[prev_off : prev_off + 4])[0]
                if p_inst == 0xF9400B90:
                    return va - step
                if p_inst == 0xD65F03C0:  # ret
                    cur = prev_off + 4
                    # Skip alignment NOPs (0x1F2003D5) or UDF traps (0x00000000)
                    while cur < off and self.data[cur : cur + 4] in (b"\x1f\x20\x03\xd5", b"\x00\x00\x00\x00"):
                        cur += 4
                    return va - (off - cur)
        elif self.container.arch == "x86_64":
            if self.data[off : off + 4] == b"\x49\x3b\x66\x10":  # cmp rsp, [r14+0x10]
                return va
            for step in range(1, 300):
                prev_off = off - step
                if prev_off < 0:
                    break
                if self.data[prev_off : prev_off + 4] == b"\x49\x3b\x66\x10":
                    return va - step
                if self.data[prev_off] == 0xC3:  # ret
                    cur = prev_off + 1
                    # Skip NOPs (0x90) or INT3 traps (0xCC)
                    while cur < off and self.data[cur] in (0x90, 0xCC):
                        cur += 1
                    return va - (off - cur)
        return va

    def find_subagent_predicates(self):
        """
        Dynamically finds the 3 subagent predicate functions in `builtin/agents`.
        
        Reverse Engineering Discovery:
        ------------------------------
        During `agents.init`, Antigravity conditionally registers:
          - `teamwork-subagent`: Guarded by experiment flag "enable-teamwork-subagent".
          - `DeepCoder` (Owl): Guarded by experiment flag "enable-owl-slash-command".
          - `DeepInvestigator` (Owl): Guarded by experiment flag "enable-owl-slash-command".
        
        This method scans the code range of `builtin/agents` for references to these
        strings, and for each reference site, walks backward to the function prologue.
        This guarantees we patch the actual predicate function bodies rather than
        inlined stubs or registration tables.
        """
        agents_pkg = "google3/third_party/jetski/cortex/customizations/builtin/agents"
        pkg_vas = [va for name, va in self.funcs.items() if name.startswith(agents_pkg)]
        if not pkg_vas:
            return []
        min_va, max_va = min(pkg_vas), max(pkg_vas) + 0x2000

        entries = []
        seen = set()
        is_arm64 = self.container.arch == "arm64"

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
    if gatekeeper_va:
        resolved_va = pclntab.resolve_function_entry(gatekeeper_va)
        targets_to_patch.append(("Slash commands gatekeeper (hasBuiltinAgents)", resolved_va))
        seen_vas.add(resolved_va)

    # 2. Builtin Subagent Predicates (teamwork-subagent, DeepCoder, DeepInvestigator)
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
        print("  patch_agy.py                  # Patch agy in PATH in-place")
        print("  patch_agy.py <input>          # Patch <input> in-place")
        print("  patch_agy.py <input> <output> # Patch <input> to <output>")
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
        print("  patch_agy.py                  # Patch agy in PATH in-place", file=sys.stderr)
        print("  patch_agy.py <input>          # Patch <input> in-place", file=sys.stderr)
        print("  patch_agy.py <input> <output> # Patch <input> to <output>", file=sys.stderr)
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
