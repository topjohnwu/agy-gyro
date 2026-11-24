#!/usr/bin/env python3
"""
download.py - Universal Antigravity CLI (agy) & App Binary Downloader
=====================================================================

Downloads official Google Antigravity CLI (`agy`) binaries or Antigravity
standalone desktop app (`--app`) packages for any supported operating system
and CPU architecture directly from Google's official release endpoints.

Features:
  - Auto-detects local OS & CPU architecture by default.
  - Supports targeting any OS (`darwin`/`mac`, `linux`, `windows`) and
    architecture (`arm64`, `amd64`/`x64`).
  - Supports `--app` to download the standalone Antigravity desktop application
    (`dmg`/`zip` on macOS, `tar.gz`/`appimage`/`deb` on Linux, `exe` on Windows).
  - Supports `--all` to download all 6 cross-platform targets simultaneously.
  - Automatically verifies SHA-512 checksums against official manifests when
    published for the artifact.
  - Automatically unpacks CLI executable binaries from `.tar.gz` archives on
    macOS and Linux.
  - Supports downloading any specific historical release version via `--version`.
  - Supports `--list-versions` to display all published release versions.
  - Pure Python 3 standard library with zero external dependencies.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Dict, List, NamedTuple, Optional, Tuple

# ==============================================================================
# Constants & Platform Matrix
# ==============================================================================

CLI_UPDATER_URL = "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app"
CLI_STORAGE_BASE_URL = "https://storage.googleapis.com/antigravity-public/antigravity-cli"

APP_UPDATER_URL = "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app"
APP_STORAGE_BASE_URL = "https://storage.googleapis.com/antigravity-public/antigravity-hub"

DEFAULT_USER_AGENT = "curl/8.0"


class PlatformInfo(NamedTuple):
    key: str                 # e.g. "darwin_arm64"
    os_name: str             # canonical: "darwin", "linux", "windows"
    arch_name: str           # canonical: "arm64", "amd64"
    os_label: str            # user-facing output slug: "mac", "linux", "windows"
    arch_label: str          # user-facing output slug: "arm64", "x64"
    remote_subdir: str       # e.g. "darwin-arm", "linux-x64", "windows-x64"
    cli_archive: str         # e.g. "cli_mac_arm64.tar.gz", "cli_windows_x64.exe"
    app_manifest: Optional[str]  # GCS YAML manifest name, if published for this target


class AppArtifactSpec(NamedTuple):
    remote_filename: str     # filename on GCS under <remote_subdir>/
    default_host_name: str   # filename when targeting host without --os/--arch
    default_cross_name: str  # filename when targeting --all or explicit --os/--arch
    set_executable: bool     # whether executable bits should be applied on POSIX


class DownloadSpec(NamedTuple):
    product_name: str
    os_name: str
    arch_name: str
    version: str
    url: str
    expected_sha512: Optional[str]
    staging_filename: str
    default_host_name: str
    default_cross_name: str
    raw_filename: Optional[str]
    extract_single_binary: bool
    set_executable: bool


PLATFORM_MATRIX: Dict[str, PlatformInfo] = {
    "darwin_arm64": PlatformInfo(
        key="darwin_arm64",
        os_name="darwin",
        arch_name="arm64",
        os_label="mac",
        arch_label="arm64",
        remote_subdir="darwin-arm",
        cli_archive="cli_mac_arm64.tar.gz",
        app_manifest="latest-arm64-mac.yml",
    ),
    "darwin_amd64": PlatformInfo(
        key="darwin_amd64",
        os_name="darwin",
        arch_name="amd64",
        os_label="mac",
        arch_label="x64",
        remote_subdir="darwin-x64",
        cli_archive="cli_mac_x64.tar.gz",
        app_manifest="latest-x64-mac.yml",
    ),
    "linux_arm64": PlatformInfo(
        key="linux_arm64",
        os_name="linux",
        arch_name="arm64",
        os_label="linux",
        arch_label="arm64",
        remote_subdir="linux-arm",
        cli_archive="cli_linux_arm64.tar.gz",
        app_manifest=None,
    ),
    "linux_amd64": PlatformInfo(
        key="linux_amd64",
        os_name="linux",
        arch_name="amd64",
        os_label="linux",
        arch_label="x64",
        remote_subdir="linux-x64",
        cli_archive="cli_linux_x64.tar.gz",
        app_manifest="latest-x64-linux.yml",
    ),
    "windows_arm64": PlatformInfo(
        key="windows_arm64",
        os_name="windows",
        arch_name="arm64",
        os_label="windows",
        arch_label="arm64",
        remote_subdir="windows-arm",
        cli_archive="cli_windows_arm64.exe",
        app_manifest="latest-arm64-win.yml",
    ),
    "windows_amd64": PlatformInfo(
        key="windows_amd64",
        os_name="windows",
        arch_name="amd64",
        os_label="windows",
        arch_label="x64",
        remote_subdir="windows-x64",
        cli_archive="cli_windows_x64.exe",
        app_manifest="latest-x64-win.yml",
    ),
}

# App package format configuration
APP_DEFAULT_FORMAT: Dict[str, str] = {
    "darwin": "dmg",
    "linux": "tar.gz",
    "windows": "exe",
}

APP_FORMAT_ALIASES: Dict[str, str] = {
    "dmg": "dmg",
    "zip": "zip",
    "tar.gz": "tar.gz",
    "targz": "tar.gz",
    "tgz": "tar.gz",
    "appimage": "appimage",
    "deb": "deb",
    "exe": "exe",
}

APP_FORMAT_SPECS: Dict[Tuple[str, str], Tuple[str, str, bool]] = {
    # (os_name, canonical_format) -> (remote_template, extension, set_executable)
    ("darwin", "dmg"): ("Antigravity.dmg", ".dmg", False),
    ("darwin", "zip"): ("Antigravity.zip", ".zip", False),
    ("linux", "tar.gz"): ("Antigravity.tar.gz", ".tar.gz", False),
    ("linux", "appimage"): ("Antigravity.AppImage", ".AppImage", True),
    ("linux", "deb"): ("Antigravity.deb", ".deb", False),
    ("windows", "exe"): ("Antigravity-{arch_label}.exe", ".exe", True),
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


def normalize_app_format(raw_format: str, os_name: str) -> str:
    cleaned = raw_format.strip().lower()
    canonical = APP_FORMAT_ALIASES.get(cleaned)
    if canonical and (os_name, canonical) in APP_FORMAT_SPECS:
        return canonical
    supported = [fmt for (target_os, fmt) in APP_FORMAT_SPECS if target_os == os_name]
    raise ValueError(
        f"Unsupported app format '{raw_format}' for OS '{os_name}'. "
        f"Supported formats: {', '.join(supported)}"
    )


def detect_host_platform() -> Tuple[str, str]:
    """Detects current host (os, arch) normalized to canonical strings."""
    return normalize_os(platform.system()), normalize_arch(platform.machine())


def get_platform_info(os_name: str, arch_name: str) -> PlatformInfo:
    key = f"{os_name}_{arch_name}"
    if key not in PLATFORM_MATRIX:
        raise ValueError(f"Platform combination '{os_name}_{arch_name}' is not supported.")
    return PLATFORM_MATRIX[key]


def get_app_artifact_spec(info: PlatformInfo, requested_format: Optional[str] = None) -> AppArtifactSpec:
    """Resolves filename and execution metadata for an Antigravity desktop app package."""
    fmt = (
        normalize_app_format(requested_format, info.os_name)
        if requested_format
        else APP_DEFAULT_FORMAT[info.os_name]
    )
    remote_tpl, ext, set_executable = APP_FORMAT_SPECS[(info.os_name, fmt)]
    remote_filename = remote_tpl.format(arch_label=info.arch_label)
    cross_name = f"Antigravity-{info.os_label}-{info.arch_label}{ext}"
    return AppArtifactSpec(
        remote_filename=remote_filename,
        default_host_name=remote_filename,
        default_cross_name=cross_name,
        set_executable=set_executable,
    )


# ==============================================================================
# Network Operations & Manifest Parsing
# ==============================================================================

def http_get_bytes(url: str) -> bytes:
    """Performs HTTP GET and returns raw response bytes."""
    req = urllib.request.Request(url, headers={"User-Agent": DEFAULT_USER_AGENT})
    try:
        with urllib.request.urlopen(req) as resp:
            return resp.read()
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"HTTP {e.code} error fetching {url}: {e.reason}") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"Network error connecting to {url}: {e.reason}") from e


def http_get_json(url: str) -> Any:
    """Performs HTTP GET and returns parsed JSON object."""
    return json.loads(http_get_bytes(url).decode("utf-8"))


def fetch_releases(updater_base_url: str) -> List[Dict[str, str]]:
    """Fetches published releases and build execution IDs."""
    url = f"{updater_base_url}/releases"
    data = http_get_json(url)
    if not isinstance(data, list):
        raise RuntimeError(f"Unexpected response format from releases endpoint: {type(data)}")
    return [
        r for r in data
        if isinstance(r, dict) and r.get("version") and r.get("execution_id")
    ]


def fetch_app_status() -> Optional[Dict[str, Any]]:
    """Best-effort fetch of live rollout status for the Antigravity desktop app."""
    try:
        data = http_get_json(f"{APP_UPDATER_URL}/status")
        return data if isinstance(data, dict) else None
    except Exception:
        return None


def fetch_cli_latest_manifest(platform_key: str) -> Dict[str, Any]:
    """Fetches latest CLI release manifest JSON for a given platform key."""
    url = f"{CLI_UPDATER_URL}/manifests/{platform_key}.json"
    manifest = http_get_json(url)
    if not isinstance(manifest, dict) or "url" not in manifest:
        raise RuntimeError(f"Invalid manifest payload received from {url}")
    return manifest


def normalize_sha512_digest(raw: str) -> Optional[str]:
    """Normalizes a hex or base64 SHA-512 string into lowercase 128-char hex."""
    cleaned = raw.strip().strip("\"'")
    if not cleaned:
        return None
    if len(cleaned) == 128 and all(c in "0123456789abcdefABCDEF" for c in cleaned):
        return cleaned.lower()
    try:
        decoded = base64.b64decode(cleaned, validate=True)
        if len(decoded) == 64:
            return decoded.hex()
    except Exception:
        pass
    return None


def _extract_filename(raw_url_or_path: str) -> str:
    cleaned = raw_url_or_path.strip().strip("\"'")
    parsed_path = urllib.parse.urlsplit(cleaned).path or cleaned
    return urllib.parse.unquote(parsed_path.rsplit("/", 1)[-1])


def parse_electron_builder_manifest(yaml_text: str) -> Dict[str, str]:
    """
    Parses an electron-builder YAML manifest without external dependencies.
    Returns mapping of artifact filename -> lowercase SHA-512 hex digest.
    """
    checksums: Dict[str, str] = {}
    in_files = False
    current_url: Optional[str] = None
    current_sha: Optional[str] = None
    top_path: Optional[str] = None
    top_sha: Optional[str] = None

    def flush_entry() -> None:
        nonlocal current_url, current_sha
        if current_url and current_sha:
            digest = normalize_sha512_digest(current_sha)
            if digest:
                checksums[_extract_filename(current_url)] = digest
        current_url = None
        current_sha = None

    for raw_line in yaml_text.splitlines():
        if not raw_line.strip() or raw_line.lstrip().startswith("#"):
            continue
        indent = len(raw_line) - len(raw_line.lstrip(" "))
        stripped = raw_line.strip()

        if indent == 0 and not stripped.startswith("- "):
            flush_entry()
            if stripped == "files:":
                in_files = True
                continue
            in_files = False
            if ":" in stripped:
                key, val = stripped.split(":", 1)
                key = key.strip()
                val = val.strip().strip("\"'")
                if key == "path":
                    top_path = val
                elif key == "sha512":
                    top_sha = val
            continue

        if not in_files:
            continue

        if stripped.startswith("- "):
            flush_entry()
            stripped = stripped[2:].strip()

        if ":" in stripped:
            key, val = stripped.split(":", 1)
            key = key.strip()
            val = val.strip().strip("\"'")
            if key == "url":
                current_url = val
            elif key == "sha512":
                current_sha = val

    flush_entry()
    if top_path and top_sha:
        top_name = _extract_filename(top_path)
        digest = normalize_sha512_digest(top_sha)
        if digest and top_name not in checksums:
            checksums[top_name] = digest
    return checksums


def fetch_app_manifest_checksum(
    version_dir: str,
    manifest_name: Optional[str],
    remote_filename: str,
) -> Optional[str]:
    """Best-effort lookup of expected SHA-512 hex digest from GCS YAML manifests."""
    if not manifest_name:
        return None
    url = f"{APP_STORAGE_BASE_URL}/{version_dir}/manifest/{manifest_name}"
    try:
        yaml_text = http_get_bytes(url).decode("utf-8")
    except Exception:
        return None
    checksums = parse_electron_builder_manifest(yaml_text)
    return checksums.get(remote_filename)


def format_bytes(num_bytes: int) -> str:
    """Formats bytes to a human-readable string."""
    if num_bytes < 1024:
        return f"{num_bytes} B"
    if num_bytes < 1024 * 1024:
        return f"{num_bytes / 1024:.1f} KB"
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

            if total_size > 0 and downloaded != total_size:
                raise RuntimeError(
                    f"Incomplete download for {url}: received {downloaded} bytes, expected {total_size} bytes"
                )

    except urllib.error.HTTPError as e:
        raise RuntimeError(f"HTTP {e.code} error downloading {url}: {e.reason}") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"Network error downloading {url}: {e.reason}") from e

    return sha512.hexdigest()


# ==============================================================================
# Extraction & Post-Processing
# ==============================================================================

def extract_binary_from_archive(archive_path: str, dest_binary_path: str) -> None:
    """Extracts the 'antigravity' executable from a CLI .tar.gz archive."""
    with tarfile.open(archive_path, "r:gz") as tar:
        member = None
        for m in tar.getmembers():
            if m.name in ("antigravity", "agy", "./antigravity", "./agy") or m.name.endswith("/antigravity"):
                member = m
                break

        if member is None:
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

def _find_release_execution_id(
    updater_base_url: str,
    requested_version: Optional[str],
    releases: Optional[List[Dict[str, str]]] = None,
) -> Tuple[str, str]:
    """Resolves (version, execution_id) for latest or explicitly requested version."""
    if releases is None:
        releases = fetch_releases(updater_base_url)
    if not releases:
        raise RuntimeError(f"No releases found from {updater_base_url}/releases")

    if not requested_version:
        latest = releases[0]
        return latest["version"], latest["execution_id"]

    matching_release = next((r for r in releases if r.get("version") == requested_version), None)
    if not matching_release:
        available_versions = [r.get("version", "") for r in releases[:10]]
        raise ValueError(
            f"Version '{requested_version}' not found. "
            f"Recent available versions: {', '.join(available_versions)}"
        )
    return requested_version, matching_release["execution_id"]


def resolve_download_spec(
    info: PlatformInfo,
    *,
    is_app: bool = False,
    requested_version: Optional[str] = None,
    requested_format: Optional[str] = None,
    raw_archive: bool = False,
    releases: Optional[List[Dict[str, str]]] = None,
) -> DownloadSpec:
    """Resolves complete download metadata for CLI or standalone App target."""
    if is_app:
        artifact = get_app_artifact_spec(info, requested_format)
        version, execution_id = _find_release_execution_id(
            APP_UPDATER_URL, requested_version, releases=releases
        )
        version_dir = f"{version}-{execution_id}"
        url = f"{APP_STORAGE_BASE_URL}/{version_dir}/{info.remote_subdir}/{artifact.remote_filename}"
        expected_sha = fetch_app_manifest_checksum(version_dir, info.app_manifest, artifact.remote_filename)
        return DownloadSpec(
            product_name="Antigravity App",
            os_name=info.os_name,
            arch_name=info.arch_name,
            version=version,
            url=url,
            expected_sha512=expected_sha,
            staging_filename=artifact.remote_filename,
            default_host_name=artifact.default_host_name,
            default_cross_name=artifact.default_cross_name,
            raw_filename=None,
            extract_single_binary=False,
            set_executable=artifact.set_executable,
        )

    is_tar_gz = info.cli_archive.endswith(".tar.gz")
    cross_name = f"agy-{info.os_label}-{info.arch_label}{'.exe' if info.os_name == 'windows' else ''}"
    host_name = "agy.exe" if info.os_name == "windows" else "agy"

    if not requested_version:
        manifest = fetch_cli_latest_manifest(info.key)
        version = str(manifest.get("version", "latest"))
        url = manifest["url"]
        expected_sha = normalize_sha512_digest(str(manifest.get("sha512", "")))
    else:
        version, execution_id = _find_release_execution_id(
            CLI_UPDATER_URL, requested_version, releases=releases
        )
        version_dir = f"{version}-{execution_id}"
        url = f"{CLI_STORAGE_BASE_URL}/{version_dir}/{info.remote_subdir}/{info.cli_archive}"
        expected_sha = None

    return DownloadSpec(
        product_name="Antigravity CLI (agy)",
        os_name=info.os_name,
        arch_name=info.arch_name,
        version=version,
        url=url,
        expected_sha512=expected_sha,
        staging_filename=info.cli_archive,
        default_host_name=host_name,
        default_cross_name=cross_name,
        raw_filename=info.cli_archive,
        extract_single_binary=(is_tar_gz and not raw_archive),
        set_executable=(info.os_name == "windows" or not raw_archive),
    )


def download_spec(spec: DownloadSpec, output_path: str) -> None:
    """Downloads, verifies, and places a single resolved target."""
    print(f"[*] Target:   {spec.os_name} ({spec.arch_name}) [{spec.product_name}]")
    print(f"    Version:  {spec.version}")
    print(f"    Source:   {spec.url}")
    print(f"    Output:   {output_path}")

    output_dir = os.path.dirname(os.path.abspath(output_path))
    os.makedirs(output_dir, exist_ok=True)

    # Stage inside output_dir so atomic os.replace avoids cross-device copies or small /tmp limits.
    with tempfile.TemporaryDirectory(prefix=".agy_dl_", dir=output_dir) as tmpdir:
        download_staging = os.path.join(tmpdir, spec.staging_filename)
        print("    Downloading payload...")
        actual_hash = download_file_with_progress(spec.url, download_staging)

        if spec.expected_sha512:
            if actual_hash.lower() != spec.expected_sha512.lower():
                raise RuntimeError(
                    f"Security halt: SHA-512 checksum mismatch for {spec.staging_filename}!\n"
                    f"  Expected: {spec.expected_sha512}\n"
                    f"  Actual:   {actual_hash}"
                )
            print("    [✓] SHA-512 checksum verified.")
        else:
            print(f"    [i] SHA-512: {actual_hash[:16]}...{actual_hash[-16:]}")

        if spec.extract_single_binary:
            print(f"    Extracting binary to {output_path}...")
            extract_binary_from_archive(download_staging, output_path)
        else:
            print(f"    Writing artifact to {output_path}...")
            os.replace(download_staging, output_path)

        if spec.set_executable:
            make_executable(output_path)
        clear_quarantine_if_macos(output_path)

    print(f"[✓] Successfully downloaded: {output_path}\n")


def resolve_output_path(
    spec: DownloadSpec,
    dest_dir: str,
    *,
    is_batch: bool,
    explicit_output: Optional[str],
    use_host_default_name: bool,
    raw_archive: bool,
) -> str:
    """Determines destination file path for a downloaded artifact."""
    if explicit_output:
        return os.path.abspath(explicit_output)
    if raw_archive and spec.raw_filename:
        filename = spec.raw_filename
    elif is_batch or not use_host_default_name:
        filename = spec.default_cross_name
    else:
        filename = spec.default_host_name
    return os.path.join(dest_dir, filename)


# ==============================================================================
# CLI Commands
# ==============================================================================

def cmd_list_versions(is_app: bool = False) -> None:
    """Prints all available release versions."""
    label = "Antigravity App" if is_app else "Antigravity CLI"
    updater_url = APP_UPDATER_URL if is_app else CLI_UPDATER_URL
    print(f"[*] Querying published {label} releases...")
    releases = fetch_releases(updater_url)
    if not releases:
        print("No releases found.")
        return

    status_info = fetch_app_status() if is_app else None
    print(f"\nFound {len(releases)} published releases:")
    print(f"{'Version':<16} {'Execution ID':<20} {'Status'}")
    print("-" * 58)
    for idx, r in enumerate(releases):
        ver = r.get("version", "")
        eid = r.get("execution_id", "")
        status = ""
        if idx == 0:
            pct = status_info.get("rollout_percentage") if status_info else None
            latest_ver = status_info.get("latest_version") if status_info else None
            if pct is not None and latest_ver == ver and pct < 100:
                status = f" (Latest, {pct}% auto-update rollout)"
            else:
                status = " (Latest)"
        print(f"{ver:<16} {eid:<20}{status}")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Universal Antigravity CLI (agy) & standalone App downloader for any OS and architecture.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Examples:
  # Download latest agy CLI for current host system (saved to ./agy):
  python3 scripts/download.py

  # Download latest standalone Antigravity app for current host system:
  python3 scripts/download.py --app

  # Download all 6 standalone Antigravity app OS/arch targets into ./apps:
  python3 scripts/download.py --app --all --dir ./apps

  # Download latest Antigravity app .deb package for Linux x64:
  python3 scripts/download.py --app --os linux --arch x64 --format deb

  # Download latest agy CLI for Linux x86_64:
  python3 scripts/download.py --os linux --arch x64

  # Download all 6 CLI binaries into a target directory:
  python3 scripts/download.py --all --dir ./binaries

  # List all available versions (CLI or standalone App):
  python3 scripts/download.py --list-versions
  python3 scripts/download.py --app --list-versions
""",
    )

    parser.add_argument(
        "--app",
        action="store_true",
        help="Download the standalone Antigravity desktop app instead of the agy CLI binary.",
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
        help="Download binaries/packages for all 6 supported OS and CPU architecture combinations.",
    )
    parser.add_argument(
        "--format",
        dest="app_format",
        metavar="FORMAT",
        help=(
            "Standalone app package format (--app single target only). "
            "Defaults to each OS's primary package: "
            "macOS: dmg (default), zip; "
            "Linux: tar.gz (default, aliases: tgz/targz), appimage, deb; "
            "Windows: exe (default)."
        ),
    )
    parser.add_argument(
        "-v", "--version",
        dest="version",
        help="Specific release version to download (e.g. 1.5.16 for CLI, 2.35.0 for App). Default: latest.",
    )
    parser.add_argument(
        "--list-versions",
        action="store_true",
        help="List all published release versions from the updater server.",
    )
    parser.add_argument(
        "-o", "--output",
        dest="output",
        help="Custom destination filename/path (only valid when downloading a single target).",
    )
    parser.add_argument(
        "-d", "--dir",
        dest="directory",
        default=".",
        help="Destination directory for downloaded artifacts (default: current directory).",
    )
    parser.add_argument(
        "--raw",
        action="store_true",
        help="Keep the raw CLI archive package (.tar.gz) instead of extracting the binary.",
    )

    return parser


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()

    if args.app_format and not args.app:
        parser.error("--format can only be used together with --app.")

    if args.list_versions:
        try:
            cmd_list_versions(is_app=args.app)
            sys.exit(0)
        except Exception as e:
            sys.stderr.write(f"[-] Error: {e}\n")
            sys.exit(1)

    if args.all:
        if args.output:
            parser.error("--output cannot be specified when downloading --all. Use --dir instead.")
        if args.app_format:
            parser.error("--format cannot be combined with --all because package formats differ by OS.")

        product_label = "Antigravity App" if args.app else "Antigravity CLI"
        print(f"[*] Downloading all {len(PLATFORM_MATRIX)} supported {product_label} targets into '{args.directory}'...")
        success_count = 0
        dest_dir = os.path.abspath(args.directory)
        os.makedirs(dest_dir, exist_ok=True)

        prefetched_releases: Optional[List[Dict[str, str]]] = None
        if args.app or args.version:
            updater_url = APP_UPDATER_URL if args.app else CLI_UPDATER_URL
            try:
                prefetched_releases = fetch_releases(updater_url)
            except Exception as e:
                sys.stderr.write(f"[-] Error: {e}\n")
                sys.exit(1)

        for key in sorted(PLATFORM_MATRIX.keys()):
            info = PLATFORM_MATRIX[key]
            try:
                spec = resolve_download_spec(
                    info,
                    is_app=args.app,
                    requested_version=args.version,
                    raw_archive=args.raw,
                    releases=prefetched_releases,
                )
                out_path = resolve_output_path(
                    spec,
                    dest_dir,
                    is_batch=True,
                    explicit_output=None,
                    use_host_default_name=False,
                    raw_archive=args.raw,
                )
                download_spec(spec, out_path)
                success_count += 1
            except Exception as e:
                sys.stderr.write(f"[-] Error downloading {key}: {e}\n")

        print(f"[✓] Finished batch download: {success_count}/{len(PLATFORM_MATRIX)} succeeded.")
        if success_count < len(PLATFORM_MATRIX):
            sys.exit(1)
        sys.exit(0)

    # Single target selection
    host_os = host_arch = None
    if not args.target_os or not args.target_arch:
        try:
            host_os, host_arch = detect_host_platform()
        except ValueError as e:
            sys.stderr.write(f"[-] Error: {e}\n")
            sys.exit(1)

    try:
        target_os = normalize_os(args.target_os) if args.target_os else host_os
        target_arch = normalize_arch(args.target_arch) if args.target_arch else host_arch
        assert target_os is not None and target_arch is not None
        info = get_platform_info(target_os, target_arch)
        spec = resolve_download_spec(
            info,
            is_app=args.app,
            requested_version=args.version,
            requested_format=args.app_format,
            raw_archive=args.raw,
        )
    except ValueError as e:
        sys.stderr.write(f"[-] Error: {e}\n")
        sys.exit(1)
    except Exception as e:
        sys.stderr.write(f"[-] Error: {e}\n")
        sys.exit(1)

    dest_dir = os.path.abspath(args.directory)
    use_host_default_name = (not args.target_os and not args.target_arch)
    out_path = resolve_output_path(
        spec,
        dest_dir,
        is_batch=False,
        explicit_output=args.output,
        use_host_default_name=use_host_default_name,
        raw_archive=args.raw,
    )

    try:
        download_spec(spec, out_path)
    except Exception as e:
        sys.stderr.write(f"[-] Error: {e}\n")
        sys.exit(1)


if __name__ == "__main__":
    main()
