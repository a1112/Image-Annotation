#!/usr/bin/env python3
"""Black-box verification for the server-side image-transform API.

The script uses only the Python standard library.  It uploads a generated PNG
to a data-project object, exercises JPEG/PNG/WebP/TIFF transforms, downloads
the real response bytes through the authenticated content route, and writes a
safe JSON report.  Tokens/passwords are accepted through a file or environment
but are never printed or written to the report.
"""

from __future__ import annotations

import argparse
import binascii
import hashlib
import json
import os
import struct
import sys
import time
import urllib.error
import urllib.request
import uuid
import zlib
from pathlib import Path
from typing import Any


def safe_code(payload: Any) -> str | None:
    return str(payload.get("code")) if isinstance(payload, dict) and payload.get("code") else None


def http_request(
    base_url: str,
    method: str,
    path_or_url: str,
    token: str | None = None,
    body: Any | None = None,
    headers: dict[str, str] | None = None,
) -> tuple[int, Any, dict[str, str]]:
    url = path_or_url if path_or_url.startswith(("http://", "https://")) else f"{base_url.rstrip('/')}{path_or_url}"
    request_headers = {"Accept": "application/json"}
    if token:
        request_headers["Authorization"] = f"Bearer {token}"
    if headers:
        request_headers.update(headers)
    payload: bytes | None
    if body is None:
        payload = None
    elif isinstance(body, (bytes, bytearray)):
        payload = bytes(body)
    else:
        payload = json.dumps(body, separators=(",", ":")).encode()
        request_headers.setdefault("Content-Type", "application/json")
    request = urllib.request.Request(url, data=payload, headers=request_headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            raw = response.read()
            if "application/json" in response.headers.get("Content-Type", ""):
                return response.status, json.loads(raw.decode()), dict(response.headers)
            return response.status, raw, dict(response.headers)
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            parsed: Any = json.loads(raw.decode())
        except (UnicodeDecodeError, json.JSONDecodeError):
            parsed = {"message": raw[:200].decode(errors="replace")}
        return error.code, parsed, dict(error.headers)


def login(base_url: str, credentials_path: str) -> str:
    credentials = json.loads(Path(credentials_path).read_text())
    status, payload, _ = http_request(
        base_url,
        "POST",
        "/api/v1/auth/login",
        body={
            "email": credentials["email"],
            "password": credentials["password"],
            "deviceId": f"image-transform-verifier-{uuid.uuid4()}",
            "platform": "desktop",
            "deviceName": "image-transform-verifier",
        },
    )
    if status != 200 or not isinstance(payload, dict) or not payload.get("accessToken"):
        raise RuntimeError(f"login failed: HTTP {status} code={safe_code(payload)}")
    return str(payload["accessToken"])


def png_fixture(width: int = 4, height: int = 3) -> bytes:
    rows = []
    for y in range(height):
        row = bytearray([0])
        for x in range(width):
            row.extend(((x * 47 + 11) & 255, (y * 71 + 23) & 255, ((x + y) * 31 + 7) & 255))
        rows.append(bytes(row))

    def chunk(kind: bytes, payload: bytes) -> bytes:
        return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", binascii.crc32(kind + payload) & 0xFFFFFFFF)

    raw = zlib.compress(b"".join(rows), level=6)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)) + chunk(b"IDAT", raw) + chunk(b"IEND", b"")


def png_dimensions(data: bytes) -> tuple[int, int] | None:
    if data.startswith(b"\x89PNG\r\n\x1a\n") and data[12:16] == b"IHDR":
        return struct.unpack(">II", data[16:24])
    return None


def jpeg_dimensions(data: bytes) -> tuple[int, int] | None:
    if not data.startswith(b"\xff\xd8"):
        return None
    index = 2
    sof = {0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF}
    while index + 3 < len(data):
        while index < len(data) and data[index] != 0xFF:
            index += 1
        while index < len(data) and data[index] == 0xFF:
            index += 1
        if index >= len(data):
            return None
        marker = data[index]
        index += 1
        if marker in (0xD8, 0xD9):
            continue
        if index + 2 > len(data):
            return None
        length = struct.unpack(">H", data[index : index + 2])[0]
        if length < 2 or index + length > len(data):
            return None
        if marker in sof and length >= 7:
            height, width = struct.unpack(">HH", data[index + 3 : index + 7])
            return width, height
        index += length
    return None


def webp_dimensions(data: bytes) -> tuple[int, int] | None:
    if len(data) < 16 or data[:4] != b"RIFF" or data[8:12] != b"WEBP":
        return None
    offset = 12
    while offset + 8 <= len(data):
        kind = data[offset : offset + 4]
        size = struct.unpack("<I", data[offset + 4 : offset + 8])[0]
        payload = data[offset + 8 : offset + 8 + size]
        if kind == b"VP8X" and len(payload) >= 10:
            width = 1 + int.from_bytes(payload[4:7], "little")
            height = 1 + int.from_bytes(payload[7:10], "little")
            return width, height
        if kind == b"VP8 " and len(payload) >= 12 and payload[3:6] == b"\x9d\x01\x2a":
            width = struct.unpack("<H", payload[6:8])[0] & 0x3FFF
            height = struct.unpack("<H", payload[8:10])[0] & 0x3FFF
            return width, height
        if kind == b"VP8L" and len(payload) >= 5 and payload[0] == 0x2F:
            bits = int.from_bytes(payload[1:5], "little")
            return (bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1
        offset += 8 + size + (size & 1)
    return None


def tiff_dimensions(data: bytes) -> tuple[int, int] | None:
    if len(data) < 8 or data[:2] not in (b"II", b"MM"):
        return None
    little = data[:2] == b"II"
    endian = "<" if little else ">"
    if struct.unpack(endian + "H", data[2:4])[0] != 42:
        return None
    ifd = struct.unpack(endian + "I", data[4:8])[0]
    if ifd + 2 > len(data):
        return None
    count = struct.unpack(endian + "H", data[ifd : ifd + 2])[0]
    width = height = None
    for index in range(count):
        entry = ifd + 2 + index * 12
        if entry + 12 > len(data):
            return None
        tag, value_type, value_count = struct.unpack(endian + "HHI", data[entry : entry + 8])
        value_size = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8}.get(value_type)
        if value_size is None:
            continue
        raw = data[entry + 8 : entry + 12] if value_size * value_count <= 4 else data[struct.unpack(endian + "I", data[entry + 8 : entry + 12])[0] :]
        if not raw:
            continue
        if value_type == 3:
            value = struct.unpack(endian + "H", raw[:2])[0]
        elif value_type == 4:
            value = struct.unpack(endian + "I", raw[:4])[0]
        else:
            continue
        if tag == 256:
            width = value
        elif tag == 257:
            height = value
    return (width, height) if width and height else None


def dimensions(data: bytes, format_name: str) -> tuple[int, int] | None:
    parsers = {
        "jpeg": jpeg_dimensions,
        "png": png_dimensions,
        "webp": webp_dimensions,
        "tiff": tiff_dimensions,
    }
    return parsers[format_name](data)


def expect(checks: list[dict[str, Any]], name: str, passed: bool, detail: str = "") -> None:
    checks.append({"name": name, "passed": passed, **({"detail": detail} if detail else {})})
    if not passed:
        raise RuntimeError(f"check failed: {name}{': ' + detail if detail else ''}")


def poll_task(base_url: str, token: str, task_id: str, timeout: float = 90.0) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last: dict[str, Any] = {}
    while time.monotonic() < deadline:
        status, payload, _ = http_request(base_url, "GET", f"/api/v1/image-transforms/{task_id}", token)
        if status != 200 or not isinstance(payload, dict):
            raise RuntimeError(f"poll failed: HTTP {status} code={safe_code(payload)}")
        last = payload
        if payload.get("status") in ("succeeded", "failed"):
            return payload
        time.sleep(1)
    raise RuntimeError(f"transform task timed out: {last.get('status')}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default="http://127.0.0.1:16425")
    parser.add_argument("--token", default=os.environ.get("IMAGE_TRANSFORM_ACCESS_TOKEN"))
    parser.add_argument("--credentials", help="JSON file with email/password; never copied to report")
    parser.add_argument("--project-id", required=True)
    parser.add_argument("--viewer-token", default=os.environ.get("IMAGE_TRANSFORM_VIEWER_TOKEN"))
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    token = args.token or (login(args.base_url, args.credentials) if args.credentials else None)
    if not token:
        parser.error("--token, IMAGE_TRANSFORM_ACCESS_TOKEN, or --credentials is required")

    report: dict[str, Any] = {
        "schema": "image-transform-verification/v1",
        "baseUrl": args.base_url,
        "projectId": args.project_id,
        "passed": False,
        "checks": [],
    }
    checks: list[dict[str, Any]] = report["checks"]
    source = png_fixture()
    source_hash = hashlib.sha256(source).hexdigest()
    upload_status, upload_payload, _ = http_request(
        args.base_url,
        "PUT",
        f"/api/v1/data-projects/{args.project_id}/objects/{source_hash}",
        token,
        source,
        {
            "Content-Type": "image/png",
            "X-Object-Size": str(len(source)),
            "X-Object-Kind": "image",
        },
    )
    expect(checks, "upload generated source image", upload_status == 200, f"HTTP {upload_status} code={safe_code(upload_payload)}")
    original_status, original, _ = http_request(
        args.base_url,
        "GET",
        f"/api/v1/data-projects/{args.project_id}/objects/{source_hash}",
        token,
    )
    expect(checks, "read original source image", original_status == 200 and isinstance(original, bytes))
    expect(checks, "source bytes match before transforms", original == source)

    operations: list[tuple[str, dict[str, Any], tuple[int, int]]] = [
        ("jpeg", {}, (4, 3)),
        ("png", {"crop": {"x": 1, "y": 0, "width": 3, "height": 2}, "rotate": 90}, (2, 3)),
        ("webp", {"width": 6, "flip": "horizontal"}, (6, 5)),
        ("tiff", {"rotate": 180}, (4, 3)),
    ]
    for format_name, operations_payload, expected_dimensions in operations:
        body = {
            "operationId": f"image-transform-{format_name}-{uuid.uuid4()}",
            "sourceHash": source_hash,
            "format": format_name,
            **operations_payload,
        }
        create_status, created, _ = http_request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{args.project_id}/image-transforms",
            token,
            body,
        )
        expect(checks, f"enqueue {format_name}", create_status == 202 and isinstance(created, dict))
        task_id = str(created["taskId"])
        result = poll_task(args.base_url, token, task_id)
        expect(checks, f"complete {format_name}", result.get("status") == "succeeded", str(result.get("errorCode")))
        download_url = result.get("downloadUrl")
        expect(checks, f"{format_name} has authenticated download URL", isinstance(download_url, str) and "/download" in download_url)
        downloaded_status, downloaded, _ = http_request(args.base_url, "GET", download_url, token)
        expect(checks, f"download {format_name} bytes", downloaded_status == 200 and isinstance(downloaded, bytes))
        output_hash = hashlib.sha256(downloaded).hexdigest()
        expect(checks, f"{format_name} output hash", output_hash == result.get("outputHash"))
        expect(checks, f"{format_name} output size", len(downloaded) == result.get("outputByteSize"))
        actual_dimensions = dimensions(downloaded, format_name)
        expect(checks, f"{format_name} decoded dimensions", actual_dimensions == expected_dimensions, f"actual={actual_dimensions} expected={expected_dimensions}")
        expect(checks, f"{format_name} metadata dimensions", (result.get("outputWidth"), result.get("outputHeight")) == expected_dimensions)
        retry_status, retry, _ = http_request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{args.project_id}/image-transforms",
            token,
            body,
        )
        expect(checks, f"{format_name} operation idempotency", retry_status == 200 and retry.get("taskId") == task_id and retry.get("created") is False)

    invalid_status, invalid, _ = http_request(
        args.base_url,
        "POST",
        f"/api/v1/data-projects/{args.project_id}/image-transforms",
        token,
        {
            "operationId": f"invalid-dimension-{uuid.uuid4()}",
            "sourceHash": source_hash,
            "format": "png",
            "width": 32_769,
        },
    )
    expect(checks, "reject invalid requested dimensions", invalid_status == 400, f"HTTP {invalid_status} code={safe_code(invalid)}")

    bad_crop_status, bad_crop, _ = http_request(
        args.base_url,
        "POST",
        f"/api/v1/data-projects/{args.project_id}/image-transforms",
        token,
        {
            "operationId": f"invalid-crop-{uuid.uuid4()}",
            "sourceHash": source_hash,
            "format": "png",
            "crop": {"x": 3, "y": 2, "width": 2, "height": 2},
        },
    )
    expect(checks, "enqueue bounded crop for worker validation", bad_crop_status == 202, f"HTTP {bad_crop_status} code={safe_code(bad_crop)}")
    failed = poll_task(args.base_url, token, str(bad_crop["taskId"]))
    expect(checks, "invalid crop fails without output object", failed.get("status") == "failed" and not failed.get("outputHash"))

    final_status, final_source, _ = http_request(
        args.base_url,
        "GET",
        f"/api/v1/data-projects/{args.project_id}/objects/{source_hash}",
        token,
    )
    expect(checks, "source object remains unchanged", final_status == 200 and final_source == source)

    if args.viewer_token:
        viewer_status, viewer_payload, _ = http_request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{args.project_id}/image-transforms",
            args.viewer_token,
            {
                "operationId": f"viewer-write-{uuid.uuid4()}",
                "sourceHash": source_hash,
                "format": "png",
            },
        )
        expect(checks, "viewer cannot enqueue transform", viewer_status == 403, f"HTTP {viewer_status} code={safe_code(viewer_payload)}")

    report["passed"] = True
    report["sourceHash"] = source_hash
    report["sourceByteSize"] = len(source)
    report["checkedAt"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"passed": True, "projectId": args.project_id, "checks": len(checks)}))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # keep terminal output secret-free
        print(f"image-transform verification failed: {error}", file=sys.stderr)
        raise
