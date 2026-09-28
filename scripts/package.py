#!/usr/bin/env python3
"""
CPR Plugin Packaging Script
Creates a pure GNU tar.gz archive conforming to CPR 3.16+ ValidatedPackage specs.
"""

import argparse
import hashlib
import io
import json
import os
import tarfile

def main():
    parser = argparse.ArgumentParser(description="Package CPR plugin into tar.gz")
    parser.add_argument("--binary", required=True, help="Path to compiled plugin binary")
    parser.add_argument("--manifest", default=None, help="Path to manifest plugin.json")
    parser.add_argument("--os", default="linux", help="Target OS (linux, macos, windows)")
    parser.add_argument("--arch", required=True, help="Target architecture (aarch64, x86_64)")
    parser.add_argument("--out-dir", default="dist", help="Output directory")
    args = parser.parse_args()

    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    manifest_path = args.manifest or os.path.join(repo_root, "plugin.json")
    if not os.path.isfile(manifest_path):
        manifest_path = "plugin.json"

    os.makedirs(args.out_dir, exist_ok=True)

    with open(args.binary, "rb") as f:
        bin_data = f.read()
    bin_sha = hashlib.sha256(bin_data).hexdigest()

    with open(manifest_path, "r", encoding="utf-8") as f:
        manifest = json.load(f)

    plugin_id = f"{manifest['publisher']}.{manifest['name']}"
    version = manifest["version"]
    target_str = f"{args.os}-{args.arch}"

    # Ensure contributes IDs and stages conform to manifest specs
    if "contributes" in manifest:
        if "model_catalog" in manifest["contributes"]:
            mc = manifest["contributes"]["model_catalog"]
            mc.setdefault("id", f"{plugin_id}.model-catalog")
            mc.setdefault("version", 1)
            mc.setdefault("stages", ["registration"])
            mc.setdefault("inputFormats", [])
            mc.setdefault("outputFormats", [])
        if "middleware" in manifest["contributes"]:
            mw = manifest["contributes"]["middleware"]
            mw.setdefault("id", f"{plugin_id}.middleware")
            mw.setdefault("version", 1)
            mw.setdefault("stages", ["request"])
            mw.setdefault("inputFormats", ["openai"])
            mw.setdefault("outputFormats", ["openai"])

    # Set single-target package metadata required by CPR runtime inspection
    manifest["package"] = {
        "protocolVersion": 1,
        "target": {
            "os": args.os,
            "architecture": args.arch
        },
        "files": {
            "bin/plugin": bin_sha
        }
    }

    manifest_bytes = json.dumps(manifest, indent=2, ensure_ascii=False).encode("utf-8")
    archive_name = f"{plugin_id}-{version}-{target_str}.tar.gz"
    archive_path = os.path.join(args.out_dir, archive_name)

    # Use pure GNU tar format with normalized timestamps and permissions
    with tarfile.open(archive_path, "w:gz", format=tarfile.GNU_FORMAT) as tar:
        ti_manifest = tarfile.TarInfo(name="plugin.json")
        ti_manifest.size = len(manifest_bytes)
        ti_manifest.mode = 0o644
        ti_manifest.mtime = 0
        ti_manifest.type = tarfile.REGTYPE
        tar.addfile(ti_manifest, io.BytesIO(manifest_bytes))

        ti_bin = tarfile.TarInfo(name="bin/plugin")
        ti_bin.size = len(bin_data)
        ti_bin.mode = 0o755
        ti_bin.mtime = 0
        ti_bin.type = tarfile.REGTYPE
        tar.addfile(ti_bin, io.BytesIO(bin_data))

    with open(archive_path, "rb") as f:
        archive_sha = hashlib.sha256(f.read()).hexdigest()

    sha_file = f"{archive_path}.sha256"
    with open(sha_file, "w", encoding="utf-8") as f:
        f.write(f"{archive_sha}  {archive_name}\n")

    print(f"Created {archive_path}")
    print(f"  SHA-256: {archive_sha}")
    print(f"  bin/plugin SHA-256: {bin_sha}")

if __name__ == "__main__":
    main()
