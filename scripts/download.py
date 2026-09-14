#!/usr/bin/env python3
"""
download.py - Universal Antigravity CLI (agy) Binary Downloader
===============================================================

Downloads official Google Antigravity CLI (`agy`) executable binaries for any
operating system and CPU architecture directly from Google's official release
endpoints.

Features:
  - Auto-detects local OS & CPU architecture by default.
  - Supports targeting any OS (`darwin`/`mac`, `linux`, `windows`) and
    architecture (`arm64`, `amd64`/`x64`).
  - Supports `--all` to download all 6 cross-platform targets simultaneously.
  - Automatically verifies SHA-512 checksums against official manifests.
  - Automatically unpacks executable binaries from `.tar.gz` archives on
    macOS and Linux.
  - Supports downloading any specific historical release version via `--version`.
  - Supports `--list-versions` to display all published release versions.
  - Pure Python 3 standard library with zero external dependencies.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from typing import Any, Dict, List, NamedTuple, Optional, Tuple

# ==============================================================================
# Constants & Platform Matrix
# ==============================================================================

BASE_UPDATER_URL = "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app"
STORAGE_BASE_URL = "https://storage.googleapis.com/antigravity-public/antigravity-cli"
DEFAULT_USER_AGENT = "curl/8.0"


class PlatformInfo(NamedTuple):
    key: str                 # e.g. "darwin_arm64"
    os_name: str             # canonical: "darwin", "linux", "windows"
    arch_name: str           # canonical: "arm64", "amd64"
    remote_subdir: str       # e.g. "darwin-arm", "linux-x64", "windows-x64"
    archive_filename: str    # e.g. "cli_mac_arm64.tar.gz", "cli_windows_x64.exe"
    default_output_name: str # e.g. "agy-mac-arm64", "agy-windows-x64.exe"
    is_tar_gz: bool          # True for .tar.gz archives, False for direct executables


PLATFORM_MATRIX: Dict[str, PlatformInfo] = {
    "darwin_arm64": PlatformInfo(
        key="darwin_arm64",
        os_name="darwin",
        arch_name="arm64",
        remote_subdir="darwin-arm",
        archive_filename="cli_mac_arm64.tar.gz",
        default_output_name="agy-mac-arm64",
        is_tar_gz=True,
    ),
    "darwin_amd64": PlatformInfo(
        key="darwin_amd64",
        os_name="darwin",
        arch_name="amd64",
        remote_subdir="darwin-x64",
        archive_filename="cli_mac_x64.tar.gz",
        default_output_name="agy-mac-x64",
        is_tar_gz=True,
    ),
    "linux_arm64": PlatformInfo(
        key="linux_arm64",
        os_name="linux",
        arch_name="arm64",
        remote_subdir="linux-arm",
        archive_filename="cli_linux_arm64.tar.gz",
        default_output_name="agy-linux-arm64",
        is_tar_gz=True,
    ),
    "linux_amd64": PlatformInfo(
        key="linux_amd64",
        os_name="linux",
        arch_name="amd64",
        remote_subdir="linux-x64",
        archive_filename="cli_linux_x64.tar.gz",
        default_output_name="agy-linux-x64",
        is_tar_gz=True,
    ),
    "windows_arm64": PlatformInfo(
        key="windows_arm64",
        os_name="windows",
        arch_name="arm64",
        remote_subdir="windows-arm",
        archive_filename="cli_windows_arm64.exe",
        default_output_name="agy-windows-arm64.exe",
        is_tar_gz=False,
    ),
    "windows_amd64": PlatformInfo(
        key="windows_amd64",
        os_name="windows",
        arch_name="amd64",
        remote_subdir="windows-x64",
        archive_filename="cli_windows_x64.exe",
        default_output_name="agy-windows-x64.exe",
        is_tar_gz=False,
    ),
}

# Normalization aliases
OS_ALIASES: Dict[str, str] = {
    "darwin": "darwin",
    "mac": "darwin",
    "macos": "darwin",
    "osx": "darwin",
    "apple": "darwin",
    "linux": "linux",
    "windows": "windows",
    "win": "windows",
    "win32": "windows",
    "win64": "windows",
}

ARCH_ALIASES: Dict[str, str] = {
    "arm64": "arm64",
    "aarch64": "arm64",
    "arm": "arm64",
    "amd64": "amd64",
    "x86_64": "amd64",
    "x64": "amd64",
    "x86-64": "amd64",
}


# ==============================================================================
# Platform Detection & Normalization
# ==============================================================================

def normalize_os(raw_os: str) -> str:
    cleaned = raw_os.strip().lower()
    if cleaned in OS_ALIASES:
        return OS_ALIASES[cleaned]
    valid = sorted(set(OS_ALIASES.values()))
    raise ValueError(f"Unsupported OS: '{raw_os}'. Supported OS values: {', '.join(valid)}")


def normalize_arch(raw_arch: str) -> str:
    cleaned = raw_arch.strip().lower()
    if cleaned in ARCH_ALIASES:
        return ARCH_ALIASES[cleaned]
    valid = sorted(set(ARCH_ALIASES.values()))
    raise ValueError(f"Unsupported architecture: '{raw_arch}'. Supported arch values: {', '.join(valid)}")


def detect_host_platform() -> Tuple[str, str]:
    """Detects current host (os, arch) normalized to canonical strings."""
    raw_system = platform.system()
    raw_machine = platform.machine()
    return normalize_os(raw_system), normalize_arch(raw_machine)


def get_platform_info(os_name: str, arch_name: str) -> PlatformInfo:
    key = f"{os_name}_{arch_name}"
    if key not in PLATFORM_MATRIX:
        raise ValueError(f"Platform combination '{os_name}_{arch_name}' is not supported.")
    return PLATFORM_MATRIX[key]


# ==============================================================================
# Network Operations & Progress
# ==============================================================================

def http_get_json(url: str) -> Any:
    """Performs HTTP GET and returns parsed JSON object."""
    req = urllib.request.Request(url, headers={"User-Agent": DEFAULT_USER_AGENT})
    try:
        with urllib.request.urlopen(req) as resp:
            data = resp.read().decode("utf-8")
            return json.loads(data)
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"HTTP {e.code} error fetching {url}: {e.reason}") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"Network error connecting to {url}: {e.reason}") from e


def fetch_releases() -> List[Dict[str, str]]:
    """Fetches full list of published releases and build execution IDs."""
    url = f"{BASE_UPDATER_URL}/releases"
    data = http_get_json(url)
    if not isinstance(data, list):
        raise RuntimeError(f"Unexpected response format from releases endpoint: {type(data)}")
    return data


def fetch_latest_manifest(platform_key: str) -> Dict[str, Any]:
    """Fetches latest release manifest for a given platform key."""
    url = f"{BASE_UPDATER_URL}/manifests/{platform_key}.json"
    manifest = http_get_json(url)
    if not isinstance(manifest, dict) or "url" not in manifest:
        raise RuntimeError(f"Invalid manifest payload received from {url}")
    return manifest


def format_bytes(num_bytes: int) -> str:
    """Formats bytes to a human-readable string."""
    if num_bytes < 1024:
        return f"{num_bytes} B"
    elif num_bytes < 1024 * 1024:
        return f"{num_bytes / 1024:.1f} KB"
    else:
        return f"{num_bytes / (1024 * 1024):.1f} MB"


def download_file_with_progress(url: str, dest_path: str) -> str:
    """
    Downloads file from URL to dest_path with progress indication.
    Returns SHA-512 hex digest of the downloaded payload.
    """
    req = urllib.request.Request(url, headers={"User-Agent": DEFAULT_USER_AGENT})
    sha512 = hashlib.sha512()
    chunk_size = 128 * 1024  # 128 KB chunks

    try:
        with urllib.request.urlopen(req) as resp, open(dest_path, "wb") as out_file:
            total_size = int(resp.headers.get("Content-Length", 0))
            downloaded = 0
            is_tty = sys.stdout.isatty()

            while True:
                chunk = resp.read(chunk_size)
                if not chunk:
                    break
                out_file.write(chunk)
                sha512.update(chunk)
                downloaded += len(chunk)

                if is_tty and total_size > 0:
                    percent = (downloaded / total_size) * 100
                    bar_len = 30
                    filled = int(bar_len * downloaded // total_size)
                    bar = "=" * filled + "-" * (bar_len - filled)
                    sys.stdout.write(
                        f"\r    [{bar}] {percent:5.1f}% ({format_bytes(downloaded)} / {format_bytes(total_size)})"
                    )
                    sys.stdout.flush()
                elif is_tty:
                    sys.stdout.write(f"\r    Downloaded {format_bytes(downloaded)}...")
                    sys.stdout.flush()

            if is_tty:
                sys.stdout.write("\n")
                sys.stdout.flush()

    except urllib.error.HTTPError as e:
        raise RuntimeError(f"HTTP {e.code} error downloading {url}: {e.reason}") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"Network error downloading {url}: {e.reason}") from e

    return sha512.hexdigest()


# ==============================================================================
# Extraction & Post-Processing
# ==============================================================================

def extract_binary_from_archive(archive_path: str, dest_binary_path: str) -> None:
    """Extracts the 'antigravity' executable from a .tar.gz archive."""
    with tarfile.open(archive_path, "r:gz") as tar:
        member = None
        for m in tar.getmembers():
            if m.name in ("antigravity", "agy", "./antigravity", "./agy") or m.name.endswith("/antigravity"):
                member = m
                break

        if member is None:
            # Fallback: grab the first file member that has executable flags or is a regular file
            file_members = [m for m in tar.getmembers() if m.isfile()]
            if not file_members:
                raise RuntimeError(f"No executable found in archive: {archive_path}")
            member = file_members[0]

        extracted_file = tar.extractfile(member)
        if extracted_file is None:
            raise RuntimeError(f"Failed to extract member '{member.name}' from {archive_path}")

        temp_dest = dest_binary_path + ".tmp"
        try:
            with open(temp_dest, "wb") as out:
                shutil.copyfileobj(extracted_file, out)
            os.replace(temp_dest, dest_binary_path)
        finally:
            if os.path.exists(temp_dest):
                os.remove(temp_dest)


def make_executable(path: str) -> None:
    """Sets executable bits (0o755) on POSIX platforms."""
    if os.name == "posix":
        try:
            current_mode = os.stat(path).st_mode
            os.chmod(path, current_mode | 0o755)
        except OSError as e:
            sys.stderr.write(f"Warning: Could not set executable bits on {path}: {e}\n")


def clear_quarantine_if_macos(path: str) -> None:
    """Removes com.apple.quarantine attribute if running on macOS."""
    if sys.platform == "darwin":
        import subprocess
        try:
            subprocess.run(
                ["xattr", "-d", "com.apple.quarantine", path],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except Exception:
            pass


# ==============================================================================
# Main Download Orchestration
# ==============================================================================

def resolve_download_metadata(
    info: PlatformInfo,
    requested_version: Optional[str] = None,
) -> Tuple[str, str, Optional[str]]:
    """
    Resolves the version, download URL, and optional expected sha512 checksum.
    Returns: (version, download_url, expected_sha512)
    """
    if not requested_version:
        # Fetch latest manifest for platform
        manifest = fetch_latest_manifest(info.key)
        version = str(manifest.get("version", "latest"))
        url = manifest["url"]
        expected_hash = manifest.get("sha512")
        return version, url, expected_hash

    # Historical or explicit version requested
    releases = fetch_releases()
    matching_release = next((r for r in releases if r.get("version") == requested_version), None)
    if not matching_release:
        available_versions = [r.get("version", "") for r in releases[:10]]
        raise ValueError(
            f"Version '{requested_version}' not found. "
            f"Recent available versions: {', '.join(available_versions)}"
        )

    execution_id = matching_release["execution_id"]
    version_dir = f"{requested_version}-{execution_id}"
    url = f"{STORAGE_BASE_URL}/{version_dir}/{info.remote_subdir}/{info.archive_filename}"
    return requested_version, url, None


def download_target(
    info: PlatformInfo,
    output_path: str,
    requested_version: Optional[str] = None,
    raw_archive: bool = False,
) -> None:
    """Downloads and processes a single platform target."""
    version, url, expected_hash = resolve_download_metadata(info, requested_version)

    print(f"[*] Target: {info.os_name} ({info.arch_name})")
    print(f"    Version:  {version}")
    print(f"    Source:   {url}")
    print(f"    Output:   {output_path}")

    # Ensure output parent directory exists
    output_dir = os.path.dirname(os.path.abspath(output_path))
    os.makedirs(output_dir, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="agy_dl_") as tmpdir:
        download_staging = os.path.join(tmpdir, info.archive_filename)
        print("    Downloading payload...")
        actual_hash = download_file_with_progress(url, download_staging)

        if expected_hash:
            if actual_hash.lower() != expected_hash.lower():
                raise RuntimeError(
                    f"Security halt: SHA-512 checksum mismatch for {info.archive_filename}!\n"
                    f"  Expected: {expected_hash}\n"
                    f"  Actual:   {actual_hash}"
                )
            print("    [✓] SHA-512 checksum verified.")
        else:
            print(f"    [i] SHA-512: {actual_hash[:16]}...{actual_hash[-16:]}")

        if info.is_tar_gz and not raw_archive:
            print(f"    Extracting binary to {output_path}...")
            extract_binary_from_archive(download_staging, output_path)
            make_executable(output_path)
            clear_quarantine_if_macos(output_path)
        else:
            print(f"    Writing binary to {output_path}...")
            shutil.copy2(download_staging, output_path)
            if not raw_archive or output_path.endswith(".exe") or not info.is_tar_gz:
                make_executable(output_path)
                clear_quarantine_if_macos(output_path)

    print(f"[✓] Successfully installed: {output_path}\n")


# ==============================================================================
# CLI Commands
# ==============================================================================

def cmd_list_versions() -> None:
    """Prints all available release versions."""
    print("[*] Querying published Antigravity CLI releases...")
    releases = fetch_releases()
    if not releases:
        print("No releases found.")
        return

    print(f"\nFound {len(releases)} published releases:")
    print(f"{'Version':<16} {'Execution ID':<20} {'Status'}")
    print("-" * 50)
    for idx, r in enumerate(releases):
        ver = r.get("version", "")
        eid = r.get("execution_id", "")
        status = " (Latest)" if idx == 0 else ""
        print(f"{ver:<16} {eid:<20}{status}")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Universal Antigravity CLI (agy) binary downloader for any OS and architecture.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Examples:
  # Download latest agy for current host system (saved to ./agy):
  python3 scripts/download.py

  # Download latest agy for Linux x86_64:
  python3 scripts/download.py --os linux --arch x64

  # Download latest agy for Windows ARM64:
  python3 scripts/download.py --os windows --arch arm64

  # Download all 6 OS/arch binaries into a target directory:
  python3 scripts/download.py --all --dir ./binaries

  # Download a specific historical version:
  python3 scripts/download.py --version 1.2.1

  # List all available versions:
  python3 scripts/download.py --list-versions
""",
    )

    parser.add_argument(
        "--os",
        dest="target_os",
        help="Target operating system (darwin/mac/macos, linux, windows). Default: host OS.",
    )
    parser.add_argument(
        "--arch",
        dest="target_arch",
        help="Target CPU architecture (arm64/aarch64, amd64/x86_64/x64). Default: host arch.",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="Download binaries for all 6 supported OS and CPU architecture combinations.",
    )
    parser.add_argument(
        "-v", "--version",
        dest="version",
        help="Specific release version to download (e.g. 1.2.1). Default: latest stable.",
    )
    parser.add_argument(
        "--list-versions",
        action="store_true",
        help="List all published release versions from the updater server.",
    )
    parser.add_argument(
        "-o", "--output",
        dest="output",
        help="Custom destination filename/path for the binary (only valid when downloading a single target).",
    )
    parser.add_argument(
        "-d", "--dir",
        dest="directory",
        default=".",
        help="Destination directory for downloaded binaries (default: current directory).",
    )
    parser.add_argument(
        "--raw",
        action="store_true",
        help="Keep the raw archive package (.tar.gz) instead of extracting the binary.",
    )

    return parser


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()

    if args.list_versions:
        try:
            cmd_list_versions()
            sys.exit(0)
        except Exception as e:
            sys.stderr.write(f"[-] Error: {e}\n")
            sys.exit(1)

    host_os, host_arch = detect_host_platform()

    if args.all:
        if args.output:
            parser.error("--output cannot be specified when downloading --all. Use --dir instead.")
        print(f"[*] Downloading all 6 supported Antigravity CLI targets into '{args.directory}'...")
        success_count = 0
        dest_dir = os.path.abspath(args.directory)
        os.makedirs(dest_dir, exist_ok=True)

        for key in sorted(PLATFORM_MATRIX.keys()):
            info = PLATFORM_MATRIX[key]
            out_filename = info.archive_filename if args.raw else info.default_output_name
            out_path = os.path.join(dest_dir, out_filename)
            try:
                download_target(info, out_path, requested_version=args.version, raw_archive=args.raw)
                success_count += 1
            except Exception as e:
                sys.stderr.write(f"[-] Error downloading {key}: {e}\n")

        print(f"[✓] Finished batch download: {success_count}/{len(PLATFORM_MATRIX)} succeeded.")
        if success_count < len(PLATFORM_MATRIX):
            sys.exit(1)
        sys.exit(0)

    # Single target selection
    target_os = normalize_os(args.target_os) if args.target_os else host_os
    target_arch = normalize_arch(args.target_arch) if args.target_arch else host_arch

    try:
        info = get_platform_info(target_os, target_arch)
    except ValueError as e:
        sys.stderr.write(f"[-] Error: {e}\n")
        sys.exit(1)

    dest_dir = os.path.abspath(args.directory)
    if args.output:
        out_path = os.path.abspath(args.output)
    else:
        # If user explicitly specifies a cross-platform target, default to e.g. agy-linux-arm64
        # If user defaults to current host platform, default to ./agy (or ./agy.exe on Windows)
        is_host_platform = (target_os == host_os and target_arch == host_arch)
        if args.raw:
            out_filename = info.archive_filename
        elif is_host_platform and not args.target_os and not args.target_arch:
            out_filename = "agy.exe" if target_os == "windows" else "agy"
        else:
            out_filename = info.default_output_name

        out_path = os.path.join(dest_dir, out_filename)

    try:
        download_target(info, out_path, requested_version=args.version, raw_archive=args.raw)
    except Exception as e:
        sys.stderr.write(f"[-] Error: {e}\n")
        sys.exit(1)


if __name__ == "__main__":
    main()
