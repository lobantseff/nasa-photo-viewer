#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///

"""
Build and package NASA Photo Viewer for distribution.

One archive per platform, named after the version `git describe` reports, so
an artifact always names the commit it was built from:

    macOS    NASA Photo Viewer.app inside nasa-photo-viewer-<v>-macos-aarch64.tar.gz
    Linux    binary, icon, desktop entry, install.sh and a .deb, inside
             nasa-photo-viewer-<v>-linux-x86_64.tar.gz
    Windows  nasa-photo-viewer.exe inside nasa-photo-viewer-<v>-windows-x86_64.zip

Usage:
    ./package_release.py                 # write into dist/
    ./package_release.py /path/to/dist   # write elsewhere
    ./package_release.py --install       # also install locally
    ./package_release.py --skip-build    # package an existing release build
"""

from __future__ import annotations

import argparse
import os
import platform
import plistlib
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

APP_NAME = "nasa-photo-viewer"
DISPLAY_NAME = "NASA Photo Viewer"
ROOT = Path(__file__).resolve().parent


# --------------------------------------------------------------------------
# Shell helpers
# --------------------------------------------------------------------------


def run(cmd: list[str], cwd: Path | None = None, capture: bool = False) -> str:
    """Run a command, failing loudly with its own diagnostics."""
    printable = " ".join(cmd)
    print(f"  $ {printable}")
    result = subprocess.run(
        cmd,
        cwd=cwd or ROOT,
        text=True,
        capture_output=capture,
    )
    if result.returncode != 0:
        if capture and result.stderr:
            print(result.stderr, file=sys.stderr)
        die(f"command failed ({result.returncode}): {printable}")
    return (result.stdout or "").strip() if capture else ""


def git(args: list[str]) -> str | None:
    """Run a git command, returning None rather than failing."""
    try:
        result = subprocess.run(
            ["git", *args], cwd=ROOT, text=True, capture_output=True
        )
    except FileNotFoundError:
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip()


def die(message: str) -> None:
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


def require(tool: str, hint: str) -> None:
    if shutil.which(tool) is None:
        die(f"{tool} is not installed. {hint}")


# --------------------------------------------------------------------------
# Version
# --------------------------------------------------------------------------


def version() -> str:
    """The version to stamp on the artifacts.

    Read from git rather than Cargo.toml, matching build.rs, so the archive
    name, the Info.plist and the version the application reports cannot
    disagree.
    """
    described = git(
        ["describe", "--tags", "--long", "--always", "--abbrev=8", "--dirty=.dirty"]
    )
    if not described:
        die("cannot determine the version: no git history here")

    body, dirty = (
        (described[: -len(".dirty")], ".dirty")
        if described.endswith(".dirty")
        else (described, "")
    )

    tag, _, hash_part = body.rpartition("-g")
    if not tag:
        # No tag yet: `--always` fell back to a bare hash.
        return f"0.0.0.dev0+{body}{dirty}"

    tag, _, count = tag.rpartition("-")
    commits = int(count) if count.isdigit() else 0
    release = tag[1:] if tag.startswith("v") else tag

    if commits == 0 and not dirty:
        return release
    return f"{release}.dev{commits}+{hash_part}{dirty}"


# --------------------------------------------------------------------------
# Archives
# --------------------------------------------------------------------------


def make_targz(source: Path, archive: Path) -> None:
    """Archive `source` with itself as the single top-level entry."""
    archive.parent.mkdir(parents=True, exist_ok=True)
    if archive.exists():
        archive.unlink()
    with tarfile.open(archive, "w:gz") as tar:
        tar.add(source, arcname=source.name)
    report(archive)


def make_zip(source: Path, archive: Path) -> None:
    archive.parent.mkdir(parents=True, exist_ok=True)
    if archive.exists():
        archive.unlink()
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as zf:
        if source.is_dir():
            for path in sorted(source.rglob("*")):
                zf.write(path, path.relative_to(source.parent))
        else:
            zf.write(source, source.name)
    report(archive)


def report(path: Path) -> None:
    size = path.stat().st_size / (1024 * 1024)
    print(f"  {path.name}  ({size:.1f} MiB)")


# --------------------------------------------------------------------------
# Build
# --------------------------------------------------------------------------


def cargo_build() -> None:
    print("Building release binary")
    run(["cargo", "build", "--release", "--locked"])


def release_binary() -> Path:
    name = f"{APP_NAME}.exe" if platform.system() == "Windows" else APP_NAME
    path = ROOT / "target" / "release" / name
    if not path.exists():
        die(f"{path} is missing; build without --skip-build")
    return path


# --------------------------------------------------------------------------
# macOS
# --------------------------------------------------------------------------


def package_macos(ver: str, dist: Path, skip_build: bool) -> Path:
    require("cargo-bundle", "Install it with: cargo install cargo-bundle")

    bundle_root = ROOT / "target" / "release" / "bundle" / "osx"
    if bundle_root.exists():
        shutil.rmtree(bundle_root)

    print("Bundling the .app")
    # cargo-bundle rebuilds unless the binary is already current, so --skip-build
    # only saves the work when a release build is genuinely present.
    run(["cargo", "bundle", "--release"])

    app = bundle_root / f"{DISPLAY_NAME}.app"
    if not app.exists():
        found = list(bundle_root.glob("*.app"))
        if not found:
            die(f"cargo-bundle produced no .app in {bundle_root}")
        app = found[0]

    stamp_plist(app, ver)

    archive = dist / f"{APP_NAME}-{ver}-macos-aarch64.tar.gz"
    make_targz(app, archive)
    return app


def stamp_plist(app: Path, ver: str) -> None:
    """Write the real version into the bundle's Info.plist.

    cargo-bundle copies the version from Cargo.toml, which this project does
    not maintain; without this, Finder's Get Info would contradict the version
    the application itself reports.
    """
    plist_path = app / "Contents" / "Info.plist"
    if not plist_path.exists():
        die(f"{plist_path} is missing from the bundle")

    with plist_path.open("rb") as fh:
        plist = plistlib.load(fh)

    # CFBundleVersion must be digits and dots; the development suffix is kept
    # for the human-readable string only.
    numeric = ver.split(".dev")[0].split("+")[0]
    plist["CFBundleShortVersionString"] = ver
    plist["CFBundleVersion"] = numeric

    with plist_path.open("wb") as fh:
        plistlib.dump(plist, fh)
    print(f"  Info.plist version -> {ver}")


def install_macos(app: Path) -> None:
    target = Path("/Applications") / app.name
    if target.exists():
        shutil.rmtree(target)
    shutil.copytree(app, target)
    print(f"Installed {target}")


# --------------------------------------------------------------------------
# Linux
# --------------------------------------------------------------------------


def package_linux(ver: str, dist: Path, skip_build: bool) -> Path:
    require("cargo-deb", "Install it with: cargo install cargo-deb")

    binary = release_binary()

    print("Building the .deb")
    # cargo-deb would otherwise take the version from Cargo.toml, which this
    # project does not maintain. Its version field rejects the `+` in a
    # development version, so that becomes a `~`.
    deb_version = ver.replace("+", "~")
    deb_out = run(
        ["cargo", "deb", "--no-build", "--deb-version", deb_version],
        capture=True,
    )
    deb = Path(deb_out.splitlines()[-1].strip()) if deb_out else None
    if deb is None or not deb.exists():
        candidates = sorted((ROOT / "target" / "debian").glob("*.deb"))
        if not candidates:
            die("cargo-deb produced no .deb")
        deb = candidates[-1]
    print(f"  {deb.name}")

    # One archive holding everything: the .deb for Debian and Ubuntu, and the
    # binary plus install.sh for everyone else.
    staging = ROOT / "target" / "release" / "bundle" / f"{APP_NAME}-{ver}"
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(parents=True)

    shutil.copy2(binary, staging / APP_NAME)
    (staging / APP_NAME).chmod(0o755)
    shutil.copy2(deb, staging / deb.name)
    shutil.copy2(ROOT / "assets" / "AppIcon.png", staging / "AppIcon.png")
    shutil.copy2(
        ROOT / "assets" / f"{APP_NAME}.desktop", staging / f"{APP_NAME}.desktop"
    )
    install_sh = staging / "install.sh"
    shutil.copy2(ROOT / "assets" / "install.sh", install_sh)
    install_sh.chmod(0o755)
    if (ROOT / "README.md").exists():
        shutil.copy2(ROOT / "README.md", staging / "README.md")

    archive = dist / f"{APP_NAME}-{ver}-linux-x86_64.tar.gz"
    make_targz(staging, archive)
    return staging


def install_linux(staging: Path) -> None:
    run(["./install.sh"], cwd=staging)


# --------------------------------------------------------------------------
# Windows
# --------------------------------------------------------------------------


def package_windows(ver: str, dist: Path, skip_build: bool) -> Path:
    binary = release_binary()

    staging = ROOT / "target" / "release" / "bundle" / f"{APP_NAME}-{ver}"
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(parents=True)

    shutil.copy2(binary, staging / binary.name)
    if (ROOT / "README.md").exists():
        shutil.copy2(ROOT / "README.md", staging / "README.md")

    archive = dist / f"{APP_NAME}-{ver}-windows-x86_64.zip"
    make_zip(staging, archive)
    return staging


def install_windows(staging: Path) -> None:
    target = Path(os.environ.get("LOCALAPPDATA", Path.home())) / "Programs" / APP_NAME
    target.mkdir(parents=True, exist_ok=True)
    for item in staging.iterdir():
        shutil.copy2(item, target / item.name)
    print(f"Installed into {target}")
    print("Add it to your PATH, or create a shortcut to the .exe.")


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=f"Build and package {DISPLAY_NAME} for the current platform."
    )
    parser.add_argument(
        "dist",
        nargs="?",
        default=str(ROOT / "dist"),
        help="directory to write the archives into (default: dist/)",
    )
    parser.add_argument(
        "--install",
        action="store_true",
        help="also install the result on this machine",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="package the existing release build instead of rebuilding",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    dist = Path(args.dist).expanduser().resolve()
    dist.mkdir(parents=True, exist_ok=True)

    ver = version()
    system = platform.system()
    print(f"{DISPLAY_NAME} {ver} — packaging for {system}")

    if ".dirty" in ver:
        print("  note: the tree has uncommitted changes, so this is not a release")

    if not args.skip_build:
        cargo_build()

    if system == "Darwin":
        artefact = package_macos(ver, dist, args.skip_build)
        installer = install_macos
    elif system == "Linux":
        artefact = package_linux(ver, dist, args.skip_build)
        installer = install_linux
    elif system == "Windows":
        artefact = package_windows(ver, dist, args.skip_build)
        installer = install_windows
    else:
        die(f"unsupported platform: {system}")

    if args.install:
        print("Installing")
        installer(artefact)

    print(f"\nArtifacts in {dist}")


if __name__ == "__main__":
    main()
