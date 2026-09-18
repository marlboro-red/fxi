#!/usr/bin/env python3
"""Detect tests touching the real default FXI app-data tree.

This intentionally ignores FXI_INDEXES and FXI_APP_DATA. Snapshot metadata only: file type, size
and mtime_ns, including directories and the root itself. Symlinks and Windows
reparse points are recorded without traversing them. This is a cheap test
pollution guard, not a content-integrity check against restored timestamps.
"""
import argparse
import json
import os
from pathlib import Path
import stat
import sys


def unix_home():
    home = os.environ.get("HOME")
    if home:
        return Path(home)
    import pwd
    home = pwd.getpwuid(os.getuid()).pw_dir
    if not home:
        raise OSError("Could not determine home directory")
    return Path(home)


def windows_local_app_data():
    # Match dirs::data_local_dir(): resolve the LocalAppData known folder,
    # rather than assuming the LOCALAPPDATA environment variable is accurate.
    import ctypes
    import uuid

    class Guid(ctypes.Structure):
        _fields_ = [("data1", ctypes.c_uint32), ("data2", ctypes.c_uint16),
                    ("data3", ctypes.c_uint16), ("data4", ctypes.c_ubyte * 8)]

    folder = Guid.from_buffer_copy(
        uuid.UUID("f1b32785-6fba-4fcf-9d55-7b8e7f157091").bytes_le)
    resolve = ctypes.WinDLL("shell32").SHGetKnownFolderPath
    resolve.argtypes = [ctypes.POINTER(Guid), ctypes.c_uint32, ctypes.c_void_p,
                        ctypes.POINTER(ctypes.c_wchar_p)]
    resolve.restype = ctypes.c_long
    release = ctypes.WinDLL("ole32").CoTaskMemFree
    release.argtypes = [ctypes.c_void_p]
    release.restype = None
    result = ctypes.c_wchar_p()
    try:
        code = resolve(ctypes.byref(folder), 0, None, ctypes.byref(result))
        if code != 0 or not result.value:
            raise OSError(f"Cannot resolve LocalAppData known folder: {code}")
        return Path(result.value)
    finally:
        if result:
            release(ctypes.cast(result, ctypes.c_void_p))


def default_app_data():
    """Match src/utils/app_data.rs without creating the directory."""
    if sys.platform == "darwin":
        base = unix_home() / "Library" / "Application Support"
    elif os.name == "nt":
        base = windows_local_app_data()
    else:
        configured = os.environ.get("XDG_DATA_HOME")
        base = (Path(configured) if configured and Path(configured).is_absolute()
                else unix_home() / ".local" / "share")
    return base / "fxi"


def snapshot(root):
    try:
        root_stat = root.lstat()
    except FileNotFoundError:
        return {"exists": False, "entries": {}}
    entries = {}
    pending = [(root, ".", root_stat)]
    while pending:
        path, relative, metadata = pending.pop()
        entries[relative] = {"type": stat.S_IFMT(metadata.st_mode),
                             "size": metadata.st_size,
                             "mtime_ns": metadata.st_mtime_ns}
        reparse = (getattr(metadata, "st_file_attributes", 0)
                   & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0))
        if stat.S_ISDIR(metadata.st_mode) and not reparse:
            with os.scandir(path) as children:
                for child in children:
                    child_path = path / child.name
                    pending.append((child_path, child_path.relative_to(root).as_posix(),
                                    child.stat(follow_symlinks=False)))
    return {"exists": True, "entries": entries}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["snapshot", "verify"])
    parser.add_argument("state", type=Path, help="snapshot JSON outside the watched tree")
    parser.add_argument("--path", type=Path, help="override watched path for guard regression tests")
    args = parser.parse_args()
    try:
        root = Path(os.path.abspath(args.path if args.path is not None else default_app_data()))
        state_path = Path(os.path.abspath(args.state))
        if state_path == root or root in state_path.parents:
            raise ValueError("Snapshot JSON must be outside the watched tree")
        current = snapshot(root)
        if args.operation == "snapshot":
            state_path.parent.mkdir(parents=True, exist_ok=True)
            state_path.write_text(json.dumps({"version": 1, "path": str(root),
                                              "snapshot": current}, sort_keys=True) + "\n",
                                  encoding="utf-8")
            print(f"Recorded app-data state: {root} ({len(current['entries'])} entries)")
            return 0
        saved = json.loads(state_path.read_text(encoding="utf-8"))
        if saved.get("version") != 1 or saved.get("path") != str(root):
            raise ValueError("Snapshot version or watched path does not match")
        previous = saved["snapshot"]
        if previous == current:
            print(f"App-data unchanged: {root}")
            return 0
        before, after = previous["entries"], current["entries"]
        added = sorted(after.keys() - before.keys())
        removed = sorted(before.keys() - after.keys())
        changed = sorted(key for key in before.keys() & after.keys() if before[key] != after[key])
        print(f"Tests changed real app data: {root}", file=sys.stderr)
        print(f"Added {len(added)}, removed {len(removed)}, changed {len(changed)} entries",
              file=sys.stderr)
        for label, names in [("added", added), ("removed", removed), ("changed", changed)]:
            for name in names[:25]:
                print(f"  {label}: {name!r}", file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Cannot check test storage: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
