#!/usr/bin/env python3
"""Synthetic publishable artifacts, byte-identical between runs.

Usage: fixtures.py <kind> <out_dir> [name] [version] [payload_bytes]
Prints `key=value` facts about what it wrote.
"""
import base64
import gzip
import hashlib
import io
import json
import os
import random
import sys
import tarfile
import zipfile

EPOCH = (1980, 1, 1, 0, 0, 0)


def payload(size, seed):
    """Incompressible, so a gzip fixture weighs what it claims, and reproducible."""
    return random.Random(seed).randbytes(size)


def zip_bytes(members):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for path, body in members:
            info = zipfile.ZipInfo(path, EPOCH)
            info.external_attr = 0o644 << 16
            z.writestr(info, body)
    return buf.getvalue()


def tgz_bytes(members):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as t:
        for path, body in members:
            info = tarfile.TarInfo(path)
            info.size = len(body)
            info.mtime = 0
            info.mode = 0o644
            t.addfile(info, io.BytesIO(body))
    return gzip.compress(raw.getvalue(), mtime=0)


def npm(name, version, size):
    body = payload(size, f"{name}{version}")
    tgz = tgz_bytes([("package/package.json", json.dumps({"name": name, "version": version}).encode()),
                     ("package/index.js", body)])
    doc = {
        "name": name,
        "description": "opencargo benchmark fixture",
        "dist-tags": {"latest": version},
        "versions": {version: {"name": name, "version": version, "main": "index.js", "dist": {"shasum": ""}}},
        "_attachments": {f"{name.split('/')[-1]}-{version}.tgz": {
            "content_type": "application/octet-stream",
            "data": base64.b64encode(tgz).decode(),
            "length": len(tgz)}},
    }
    return json.dumps(doc).encode(), "application/json"


def cargo(name, version, size):
    body = payload(size, f"{name}{version}")
    crate = tgz_bytes([(f"{name}-{version}/Cargo.toml",
                        f'[package]\nname = "{name}"\nversion = "{version}"\n'.encode()),
                       (f"{name}-{version}/src/lib.rs", body)])
    meta = json.dumps({"name": name, "vers": version, "deps": [], "features": {},
                       "authors": [], "description": "opencargo benchmark fixture",
                       "license": "MIT"}).encode()
    out = len(meta).to_bytes(4, "little") + meta + len(crate).to_bytes(4, "little") + crate
    return out, "application/octet-stream"


def wheel(name, version, size):
    escaped = name.replace("-", "_").replace(".", "_").lower()
    dist_info = f"{escaped}-{version}.dist-info"
    members = [
        (f"{escaped}/__init__.py", f'VERSION = "{version}"\n'.encode()),
        (f"{escaped}/data.bin", payload(size, f"{name}{version}")),
        (f"{dist_info}/METADATA",
         f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nSummary: opencargo benchmark fixture\n\n".encode()),
        (f"{dist_info}/WHEEL",
         b"Wheel-Version: 1.0\nGenerator: opencargo-bench\nRoot-Is-Purelib: true\nTag: py3-none-any\n"),
    ]
    record = "".join(
        "%s,sha256=%s,%d\n" % (p, base64.urlsafe_b64encode(hashlib.sha256(b).digest()).rstrip(b"=").decode(), len(b))
        for p, b in members)
    record += f"{dist_info}/RECORD,,\n"
    return zip_bytes(members + [(f"{dist_info}/RECORD", record.encode())]), f"{escaped}-{version}-py3-none-any.whl"


def nupkg(name, version, size):
    nuspec = f"""<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{name}</id>
    <version>{version}</version>
    <authors>opencargo-bench</authors>
    <description>opencargo benchmark fixture</description>
    <dependencies><group targetFramework="net8.0"></group></dependencies>
  </metadata>
</package>"""
    return zip_bytes([(f"{name}.nuspec", nuspec.encode()),
                      (f"lib/net8.0/{name}.dll", payload(size, f"{name}{version}"))])


def multipart(fields, files, boundary="----opencargo-bench"):
    out = bytearray()
    for key, value in fields:
        out += f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"\r\n\r\n{value}\r\n'.encode()
    for key, filename, body in files:
        out += (f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"; filename="{filename}"\r\n'
                f"Content-Type: application/octet-stream\r\n\r\n").encode()
        out += body + b"\r\n"
    out += f"--{boundary}--\r\n".encode()
    return bytes(out), f"multipart/form-data; boundary={boundary}"


def write(path, data):
    with open(path, "wb") as f:
        f.write(data)
    return len(data)


def main():
    kind, out = sys.argv[1], sys.argv[2]
    name = sys.argv[3] if len(sys.argv) > 3 else "bench-pkg"
    version = sys.argv[4] if len(sys.argv) > 4 else "1.0.0"
    size = int(sys.argv[5]) if len(sys.argv) > 5 else 4096
    os.makedirs(out, exist_ok=True)
    facts = {}

    if kind == "npm":
        body, ctype = npm(name, version, size)
        facts["body"] = os.path.join(out, "npm.json")
        facts["content_type"] = ctype
        facts["bytes"] = write(facts["body"], body)
    elif kind == "cargo":
        body, ctype = cargo(name, version, size)
        facts["body"] = os.path.join(out, "crate.bin")
        facts["content_type"] = ctype
        facts["bytes"] = write(facts["body"], body)
    elif kind == "pypi":
        whl, filename = wheel(name, version, size)
        body, ctype = multipart(
            [(":action", "file_upload"), ("protocol_version", "1"), ("metadata_version", "2.1"),
             ("name", name), ("version", version), ("filetype", "bdist_wheel"),
             ("sha256_digest", hashlib.sha256(whl).hexdigest())],
            [("content", filename, whl)])
        facts["body"] = os.path.join(out, "pypi.multipart")
        facts["content_type"] = ctype
        facts["filename"] = filename
        facts["bytes"] = write(facts["body"], body)
    elif kind == "nuget":
        body, ctype = multipart([], [("package", f"{name}.{version}.nupkg", nupkg(name, version, size))])
        facts["body"] = os.path.join(out, "nuget.multipart")
        facts["content_type"] = ctype
        facts["bytes"] = write(facts["body"], body)
    elif kind == "maven":
        jar = zip_bytes([("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"),
                         ("data.bin", payload(size, f"{name}{version}"))])
        pom = (f"<project><groupId>org.example</groupId><artifactId>{name}</artifactId>"
               f"<version>{version}</version></project>").encode()
        facts["jar"] = os.path.join(out, f"{name}-{version}.jar")
        facts["jar_sha1"] = hashlib.sha1(jar).hexdigest()
        facts["pom"] = os.path.join(out, f"{name}-{version}.pom")
        facts["pom_sha1"] = hashlib.sha1(pom).hexdigest()
        facts["bytes"] = write(facts["jar"], jar) + write(facts["pom"], pom)
    elif kind == "oci":
        layer = gzip.compress(payload(size, f"{name}{version}"), mtime=0)
        config = json.dumps({"architecture": "amd64", "os": "linux",
                             "rootfs": {"type": "layers", "diff_ids": []}}).encode()
        facts["layer"] = os.path.join(out, "layer.tar.gz")
        facts["layer_digest"] = "sha256:" + hashlib.sha256(layer).hexdigest()
        facts["config"] = os.path.join(out, "config.json")
        facts["config_digest"] = "sha256:" + hashlib.sha256(config).hexdigest()
        manifest = json.dumps({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "size": len(config), "digest": facts["config_digest"]},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                        "size": len(layer), "digest": facts["layer_digest"]}],
        }).encode()
        facts["manifest"] = os.path.join(out, "manifest.json")
        facts["bytes"] = (write(facts["layer"], layer) + write(facts["config"], config)
                          + write(facts["manifest"], manifest))
    elif kind == "cargo-bulk":
        count, per_package = int(version), int(sys.argv[6])
        total = 0
        for i in range(count):
            body, _ = cargo(f"{name}-{i // per_package}", "1.0.%d" % (i % per_package), size)
            total += write(os.path.join(out, "%05d.bin" % i), body)
        facts["count"] = count
        facts["bytes"] = total
    elif kind == "npm-bulk":
        count, per_package = int(version), int(sys.argv[6])
        total = 0
        for i in range(count):
            body, _ = npm(f"{name}-{i // per_package}", "1.0.%d" % (i % per_package), size)
            total += write(os.path.join(out, "%05d.json" % i), body)
        facts["count"] = count
        facts["bytes"] = total
    else:
        raise SystemExit(f"fixtures: unknown kind {kind}")

    for key, value in facts.items():
        print(f"{key}={value}")


if __name__ == "__main__":
    main()
