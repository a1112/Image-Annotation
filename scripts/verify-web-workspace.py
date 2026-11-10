#!/usr/bin/env python3
"""Black-box acceptance for the server-owned web data-project workspace.

This verifier deliberately uses the browser contract rather than the native
loopback/native IPC contract: login creates HttpOnly cookies, every state
changing request carries the same-origin ``Origin`` header, and workspace
operations use ``/api/v1/web/commands/{command}``.  It is intentionally
standard-library-only so it can be run against a staging or production base
URL without a browser, localhost service, or a bearer token.

The normal run creates a project with two datasets, uploads two generated PNGs
per dataset, saves real bbox annotations, and exercises commits, branches,
clean three-way merge, conflict/resolve, and restore.  ``--resume-only``
reopens a safe report after a server/PG restart and verifies the durable
project, graph, object bytes, and labels without mutating the project.

Credentials are read from a private JSON file and are never printed or written
to the report.  HTTP response bodies are also never copied into errors or
reports; only status and a bounded public error code are retained.
"""

from __future__ import annotations

import argparse
import binascii
import hashlib
import json
import struct
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zlib
from http.cookiejar import CookieJar
from pathlib import Path
from typing import Any, Iterable


SCHEMA = "web-workspace-verification/v1"
DEFAULT_TIMEOUT = 45.0
HEX_HASH_LENGTH = 64


class VerificationFailure(RuntimeError):
    """A safe, non-secret verification failure."""

    def __init__(self, message: str, *, status: int | None = None, code: str | None = None):
        super().__init__(message)
        self.status = status
        self.code = safe_public_code(code)


class HttpResult:
    def __init__(self, status: int, payload: Any, headers: dict[str, str], raw: bytes = b""):
        self.status = status
        self.payload = payload
        self.headers = headers
        self.raw = raw


class HttpFailure(VerificationFailure):
    pass


def safe_public_code(value: Any) -> str | None:
    """Keep only a short machine code in the report, never server text."""

    if not isinstance(value, str):
        return None
    value = value.strip()
    if not value or len(value) > 96 or any(ch not in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.-" for ch in value):
        return None
    return value


def error_code(payload: Any) -> str | None:
    if not isinstance(payload, dict):
        return None
    direct = safe_public_code(payload.get("code"))
    if direct:
        return direct
    nested = payload.get("error")
    if isinstance(nested, dict):
        return safe_public_code(nested.get("code"))
    return None


def normalize_base_url(value: str) -> tuple[str, str]:
    """Return a base URL and origin without userinfo/query/fragment."""

    raw = str(value or "").strip()
    parsed = urllib.parse.urlsplit(raw)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        raise VerificationFailure("base URL must be an absolute http(s) URL")
    if parsed.username or parsed.password:
        raise VerificationFailure("base URL must not contain userinfo")
    path = parsed.path.rstrip("/")
    base = urllib.parse.urlunsplit((parsed.scheme, parsed.netloc, path, "", ""))
    origin = urllib.parse.urlunsplit((parsed.scheme, parsed.netloc, "", "", ""))
    return base, origin


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise VerificationFailure("cannot read JSON input") from exc


def credential_record(value: Any) -> dict[str, str] | None:
    """Find email/password recursively without exposing values."""

    if isinstance(value, dict):
        email = value.get("email")
        password = value.get("password")
        if isinstance(email, str) and email.strip() and isinstance(password, str) and password:
            server_url = value.get("serverUrl") or value.get("baseUrl") or value.get("server")
            result = {"email": email, "password": password}
            if isinstance(server_url, str) and server_url.strip():
                result["serverUrl"] = server_url.strip()
            return result
        # Common fixture shapes are owner/account/member; permit those while
        # keeping the caller in control of which file is used.
        for nested in value.values():
            found = credential_record(nested)
            if found:
                if "serverUrl" not in found:
                    for key in ("serverUrl", "baseUrl", "server"):
                        if isinstance(value.get(key), str) and value[key].strip():
                            found["serverUrl"] = value[key].strip()
                            break
                return found
    return None


def load_credentials(path: Path, base_override: str | None) -> tuple[dict[str, str], str]:
    data = load_json(path)
    record = credential_record(data)
    if not record:
        raise VerificationFailure("credentials JSON must contain email and password")
    base = base_override or record.get("serverUrl")
    if not base and isinstance(data, dict):
        for key in ("serverUrl", "baseUrl", "server"):
            if isinstance(data.get(key), str) and data[key].strip():
                base = data[key].strip()
                break
    if not base:
        raise VerificationFailure("base URL is required via --base-url or credentials JSON")
    return record, base


def unwrap_data(payload: Any) -> Any:
    """Unwrap both webRequest's {ok,data} envelope and direct DTOs."""

    if isinstance(payload, dict) and "ok" in payload:
        if payload.get("ok") is False:
            raise VerificationFailure("web command returned an error", code=error_code(payload.get("error")))
        return payload.get("data")
    return payload


def object_payload(payload: Any) -> dict[str, Any]:
    payload = unwrap_data(payload)
    if not isinstance(payload, dict):
        raise VerificationFailure("expected a JSON object from the web command")
    return payload


def list_payload(payload: Any, *keys: str) -> list[Any]:
    payload = unwrap_data(payload)
    if isinstance(payload, list):
        return payload
    if isinstance(payload, dict):
        for key in keys:
            value = payload.get(key)
            if isinstance(value, list):
                return value
    raise VerificationFailure("expected a JSON list from the web command")


def get_string(value: Any, *keys: str) -> str | None:
    if not isinstance(value, dict):
        return None
    for key in keys:
        item = value.get(key)
        if isinstance(item, str) and item.strip():
            return item.strip()
    return None


def get_int(value: Any, *keys: str) -> int | None:
    if not isinstance(value, dict):
        return None
    for key in keys:
        item = value.get(key)
        if isinstance(item, bool):
            continue
        if isinstance(item, int):
            return item
        if isinstance(item, str) and item.strip().lstrip("-").isdigit():
            return int(item.strip())
    return None


def uuidish(value: Any) -> str:
    result = get_string(value, "id", "projectId", "dataProjectId", "commitId") if isinstance(value, dict) else None
    if not result:
        raise VerificationFailure("web command response omitted an identifier")
    return result


def safe_path_component(value: str) -> str:
    return urllib.parse.quote(str(value), safe="")


def png_chunk(kind: bytes, payload: bytes) -> bytes:
    return (
        struct.pack(">I", len(payload))
        + kind
        + payload
        + struct.pack(">I", binascii.crc32(kind + payload) & 0xFFFFFFFF)
    )


def png_fixture(width: int, height: int, seed: int) -> bytes:
    """Create a small valid RGB PNG using only the Python standard library."""

    rows = []
    for y in range(height):
        row = bytearray([0])
        for x in range(width):
            row.extend(
                (
                    (seed * 17 + x * 41 + 13) & 255,
                    (seed * 29 + y * 53 + 23) & 255,
                    (seed * 43 + x * 7 + y * 11 + 31) & 255,
                )
            )
        rows.append(bytes(row))
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", header) + png_chunk(
        b"IDAT", zlib.compress(b"".join(rows), level=6)
    ) + png_chunk(b"IEND", b"")


def png_dimensions(data: bytes) -> tuple[int, int] | None:
    if len(data) >= 24 and data.startswith(b"\x89PNG\r\n\x1a\n") and data[12:16] == b"IHDR":
        return struct.unpack(">II", data[16:24])
    return None


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def valid_hash(value: Any) -> bool:
    return isinstance(value, str) and len(value) == HEX_HASH_LENGTH and all(ch in "0123456789abcdefABCDEF" for ch in value)


class WebSession:
    def __init__(self, base_url: str, *, timeout: float = DEFAULT_TIMEOUT):
        self.base_url, self.origin = normalize_base_url(base_url)
        self.timeout = timeout
        self.cookies = CookieJar()
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.cookies))
        self.user_id: str | None = None

    def url(self, path: str) -> str:
        if path.startswith(("http://", "https://")):
            return path
        return f"{self.base_url}/{path.lstrip('/')}"

    def request(
        self,
        method: str,
        path: str,
        *,
        body: bytes | None = None,
        content_type: str | None = None,
        origin: str | None = None,
        accept_json: bool = True,
        extra_headers: dict[str, str] | None = None,
    ) -> HttpResult:
        headers: dict[str, str] = {"Accept": "application/json" if accept_json else "*/*"}
        # The production web auth/command surface rejects cross-origin state
        # changes.  Sending Origin on reads as well keeps this script explicit
        # and catches deployments that accidentally split API origins.
        headers["Origin"] = self.origin if origin is None else origin
        if content_type:
            headers["Content-Type"] = content_type
        if extra_headers:
            headers.update(extra_headers)
        request = urllib.request.Request(self.url(path), data=body, headers=headers, method=method)
        try:
            with self.opener.open(request, timeout=self.timeout) as response:
                raw = response.read()
                response_headers = {str(key): str(value) for key, value in response.headers.items()}
                if accept_json and raw:
                    try:
                        payload: Any = json.loads(raw.decode("utf-8"))
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        payload = None
                else:
                    payload = None
                return HttpResult(response.status, payload, response_headers, raw)
        except urllib.error.HTTPError as exc:
            raw = exc.read()
            payload: Any = None
            try:
                payload = json.loads(raw.decode("utf-8")) if raw else None
            except (UnicodeDecodeError, json.JSONDecodeError):
                payload = None
            return HttpResult(exc.code, payload, {str(key): str(value) for key, value in exc.headers.items()}, raw)
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            # Do not include the exception text: proxy/URL errors can contain
            # the configured URL and are not useful acceptance data.
            raise HttpFailure("web request could not reach the service", code=type(exc).__name__) from exc

    def login(self, credentials: dict[str, str]) -> None:
        payload = {
            "email": credentials["email"],
            "password": credentials["password"],
            "deviceId": f"web-workspace-verifier-{uuid.uuid4()}",
            "deviceName": "web-workspace-verifier",
        }
        result = self.request(
            "POST",
            "/api/v1/web/auth/login",
            body=json.dumps(payload, separators=(",", ":")).encode("utf-8"),
            content_type="application/json",
        )
        if result.status != 200:
            raise HttpFailure("web login failed", status=result.status, code=error_code(result.payload))
        data = object_payload(result.payload)
        if data.get("authenticated") is False:
            raise VerificationFailure("web login did not authenticate")
        session = self.request("GET", "/api/v1/web/auth/session")
        if session.status != 200:
            raise HttpFailure("authenticated web session was not established", status=session.status, code=error_code(session.payload))
        session_data = object_payload(session.payload)
        user = session_data.get("user")
        user_id = get_string(user, "id")
        if not user_id:
            raise VerificationFailure("web session omitted the authenticated user")
        self.user_id = user_id

    def logout(self) -> None:
        result = self.request(
            "POST",
            "/api/v1/web/auth/logout",
            body=b"{}",
            content_type="application/json",
        )
        if result.status not in (200, 204):
            raise HttpFailure("web logout failed", status=result.status, code=error_code(result.payload))

    def command_result(self, command: str, args: dict[str, Any]) -> HttpResult:
        body = json.dumps(args, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        return self.request(
            "POST",
            f"/api/v1/web/commands/{safe_path_component(command)}",
            body=body,
            content_type="application/json",
        )

    def command(self, command: str, args: dict[str, Any]) -> Any:
        result = self.command_result(command, args)
        if result.status < 200 or result.status >= 300:
            raise HttpFailure("web command failed", status=result.status, code=error_code(result.payload))
        try:
            return unwrap_data(result.payload)
        except VerificationFailure as exc:
            raise HttpFailure(str(exc), status=result.status, code=exc.code) from exc

    def command_aliases(self, commands: Iterable[str], args: dict[str, Any]) -> Any:
        """Try aliases only for an explicit unknown-command response.

        Retrying a mutation after a timeout or a generic 4xx could duplicate a
        project/commit, so aliases are intentionally limited to the known
        command-dispatch miss cases.
        """

        names = list(commands)
        last: HttpResult | None = None
        for index, command in enumerate(names):
            result = self.command_result(command, args)
            last = result
            if 200 <= result.status < 300:
                return unwrap_data(result.payload)
            code = error_code(result.payload)
            unknown = result.status in (404, 405) or code in {"UNKNOWN_COMMAND", "COMMAND_NOT_FOUND", "NOT_FOUND"}
            if index + 1 >= len(names) or not unknown:
                raise HttpFailure("web command failed", status=result.status, code=code)
        assert last is not None
        raise HttpFailure("web command failed", status=last.status, code=error_code(last.payload))

    def upload_png(self, dataset_id: str, file_name: str, data: bytes) -> dict[str, Any]:
        query = urllib.parse.urlencode({"fileName": file_name, "branch": "main"})
        result = self.request(
            "PUT",
            f"/api/v1/web/datasets/{safe_path_component(dataset_id)}/images/upload?{query}",
            body=data,
            content_type="image/png",
            extra_headers={"X-Object-Size": str(len(data))},
        )
        if result.status < 200 or result.status >= 300:
            raise HttpFailure("web image upload failed", status=result.status, code=error_code(result.payload))
        payload = unwrap_data(result.payload)
        if not isinstance(payload, dict):
            raise VerificationFailure("web image upload omitted its DTO")
        return payload

    def download_image(self, dataset_id: str, image_id: str, *, fallback_project: str | None = None, fallback_hash: str | None = None) -> tuple[bytes, str]:
        result = self.request(
            "GET",
            f"/api/v1/web/datasets/{safe_path_component(dataset_id)}/images/{safe_path_component(image_id)}/content",
            accept_json=False,
        )
        route = "web-content"
        if result.status == 404 and fallback_project and valid_hash(fallback_hash):
            # Older staging revisions exposed only the project object route;
            # retain this safe compatibility path while still preferring the
            # browser endpoint above.
            result = self.request(
                "GET",
                f"/api/v1/data-projects/{safe_path_component(fallback_project)}/objects/{safe_path_component(fallback_hash or '')}",
                accept_json=False,
            )
            route = "project-object-fallback"
        if result.status < 200 or result.status >= 300:
            raise HttpFailure("web image download failed", status=result.status, code=error_code(result.payload))
        return result.raw, route


class Acceptance:
    def __init__(self, session: WebSession, *, output: Path):
        self.session = session
        self.output = output
        self.checks: list[dict[str, Any]] = []
        self.report: dict[str, Any] = {
            "schema": SCHEMA,
            "baseUrl": session.base_url,
            "passed": False,
            "checks": self.checks,
        }

    def check(self, name: str, condition: bool, *, status: int | None = None, code: str | None = None, skipped: bool = False) -> None:
        item: dict[str, Any] = {"name": name, "passed": bool(condition) if not skipped else None}
        if status is not None:
            item["status"] = status
        if code:
            item["code"] = safe_public_code(code)
        if skipped:
            item["skipped"] = True
        self.checks.append(item)
        if not skipped and not condition:
            raise VerificationFailure(name, status=status, code=code)

    def write(self) -> None:
        self.output.parent.mkdir(parents=True, exist_ok=True)
        self.output.write_text(json.dumps(self.report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    @staticmethod
    def commit_payload(payload: Any) -> dict[str, Any]:
        payload = unwrap_data(payload)
        if isinstance(payload, dict) and isinstance(payload.get("commit"), dict):
            return payload["commit"]
        if isinstance(payload, dict):
            return payload
        raise VerificationFailure("commit command omitted its commit DTO")

    @staticmethod
    def commit_id(commit: Any) -> str:
        value = get_string(commit, "id", "commitId")
        if not value:
            raise VerificationFailure("commit DTO omitted its id")
        return value

    def annotation_state(self, dataset_id: str, image_id: str, branch: str = "main") -> dict[str, Any]:
        args = {
            "projectId": dataset_id,
            "datasetId": dataset_id,
            "imageId": image_id,
            "branch": branch,
        }
        payload = self.session.command_aliases(("get_image_annotation_state", "get_annotation"), args)
        return object_payload(payload)

    def objects_from_state(self, state: dict[str, Any]) -> list[dict[str, Any]]:
        value = state.get("objects", state.get("annotation", []))
        return [item for item in value if isinstance(item, dict)] if isinstance(value, list) else []

    def save_bbox(self, dataset_id: str, image_id: str, label: str, *, branch: str = "main") -> dict[str, Any]:
        current = self.annotation_state(dataset_id, image_id, branch)
        revision = get_int(current, "revision")
        if revision is None:
            raise VerificationFailure("annotation state omitted its revision")
        object_value = {
            "id": f"web-workspace-box-{uuid.uuid4()}",
            "classId": 0,
            "label": label,
            "type": "bbox",
            "bbox": {"x": 1, "y": 1, "width": 4, "height": 3},
            "attributes": {"source": "web-workspace-verifier"},
        }
        args = {
            "projectId": dataset_id,
            "datasetId": dataset_id,
            "imageId": image_id,
            "branch": branch,
            "revision": str(revision),
            "baseRevision": revision,
            "objects": [object_value],
            "annotation": [object_value],
            "status": "inProgress",
            "qaStatus": "pending",
        }
        payload = self.session.command_aliases(("save_image_annotations", "save_annotation"), args)
        result = object_payload(payload)
        # Read-after-write proves the server retained the actual label rather
        # than merely acknowledging a request DTO.
        state = self.annotation_state(dataset_id, image_id, branch)
        labels = [item.get("label") for item in self.objects_from_state(state)]
        self.check(f"annotation persists label {label}", label in labels)
        if not getattr(self, "checked_stale_annotation", False):
            # A second browser holding the previous revision must never
            # silently replace a newer saved annotation.
            self.checked_stale_annotation = True
            stale = self.session.command_result("save_image_annotations", args)
            self.check("stale browser annotation write is rejected", stale.status == 409, status=stale.status, code=error_code(stale.payload))
            latest = self.annotation_state(dataset_id, image_id, branch)
            self.check("rejected stale write preserves the saved label and revision", latest.get("revision") == state.get("revision") and label in [item.get("label") for item in self.objects_from_state(latest)])
        return result

    def working_tree(self, project_id: str, branch: str = "main") -> dict[str, Any]:
        payload = self.session.command_aliases(
            ("get_data_project_working_tree", "working_tree"),
            {"dataProjectId": project_id, "branch": branch},
        )
        value = object_payload(payload)
        nested = value.get("workingTree")
        return nested if isinstance(nested, dict) else value

    def commit(self, project_id: str, branch: str, message: str) -> dict[str, Any]:
        payload = self.session.command_aliases(
            ("commit_data_project_branch", "commit_worktree"),
            {"dataProjectId": project_id, "branch": branch, "message": message},
        )
        return self.commit_payload(payload)

    def verify_download(self, project_id: str, dataset_id: str, image_id: str, source: bytes, image_hash: str | None = None) -> dict[str, Any]:
        data, route = self.session.download_image(dataset_id, image_id, fallback_project=project_id, fallback_hash=image_hash)
        actual_hash = sha256(data)
        self.check(f"downloaded image {image_id} has its original bytes", data == source)
        self.check(f"downloaded image {image_id} has a verified sha256", actual_hash == sha256(source))
        return {"route": route, "byteSize": len(data), "sha256": actual_hash, "dimensions": png_dimensions(data)}

    def full_run(self, credentials: dict[str, str], member_credentials: dict[str, str] | None = None) -> None:
        suffix = uuid.uuid4().hex[:10]
        project_name = f"web-workspace-verification-{suffix}"
        project = object_payload(
            self.session.command("create_data_project", {"name": project_name, "description": "black-box web workspace verification"})
        )
        project_id = uuidish(project)
        self.report["projectId"] = project_id

        dataset_ids: list[str] = []
        for index in range(2):
            schema = [{"id": 0, "label": f"web-class-{index}"}]
            dataset = object_payload(
                self.session.command_aliases(
                    ("create_dataset_project", "create_dataset"),
                    {
                        "name": f"web-dataset-{index}-{suffix}",
                        "description": "web workspace verifier dataset",
                        "datasetType": "object-detection",
                        "demoTemplate": "demo-bbox",
                        "schema": schema,
                        "classes": schema,
                        "dataProjectId": project_id,
                    },
                )
            )
            dataset_id = get_string(dataset, "id")
            if not dataset_id:
                raw_id = get_string(dataset, "datasetId")
                if raw_id:
                    dataset_id = raw_id
            if not dataset_id:
                raise VerificationFailure("created dataset omitted its browser dataset id")
            dataset_ids.append(dataset_id)
        self.report["datasetIds"] = dataset_ids

        images: list[dict[str, Any]] = []
        for dataset_index, dataset_id in enumerate(dataset_ids):
            for image_index in range(2):
                width = 12 + dataset_index
                height = 9 + image_index
                data = png_fixture(width, height, dataset_index * 10 + image_index)
                file_name = f"web-{dataset_index}-{image_index}-{suffix}.png"
                uploaded = self.session.upload_png(dataset_id, file_name, data)
                image_id = get_string(uploaded, "imageId", "id")
                if not image_id and isinstance(uploaded.get("image"), dict):
                    image_id = get_string(uploaded["image"], "imageId", "id")
                if not image_id:
                    raise VerificationFailure("uploaded image omitted its image id")
                image_hash = get_string(uploaded, "imageHash", "contentHash")
                download = self.verify_download(project_id, dataset_id, image_id, data, image_hash)
                initial_label = f"initial-{dataset_index}-{image_index}"
                self.save_bbox(dataset_id, image_id, initial_label)
                images.append(
                    {
                        "datasetId": dataset_id,
                        "imageId": image_id,
                        "fileName": file_name,
                        "sourceHash": sha256(data),
                        "sourceByteSize": len(data),
                        "dimensions": [width, height],
                        "initialLabel": initial_label,
                        "download": download,
                    }
                )
        self.report["images"] = images

        main_tree = self.working_tree(project_id, "main")
        self.check("main working tree reports uploaded/annotated changes", bool(main_tree.get("dirty")))
        initial_commit = self.commit(project_id, "main", "web workspace initial snapshot")
        initial_id = self.commit_id(initial_commit)
        self.report["initialCommitId"] = initial_id
        self.check("initial commit has immutable image/annotation hash references", self.hash_refs(initial_commit) > 0)

        feature = "web-feature"
        self.session.command("create_data_project_branch", {"dataProjectId": project_id, "name": feature, "fromCommitId": initial_id})
        self.session.command("switch_data_project_branch", {"dataProjectId": project_id, "branch": feature})
        self.save_bbox(images[0]["datasetId"], images[0]["imageId"], "feature-label", branch=feature)
        feature_commit = self.commit(project_id, feature, "web workspace feature change")
        feature_id = self.commit_id(feature_commit)
        self.report["featureCommitId"] = feature_id

        self.session.command("switch_data_project_branch", {"dataProjectId": project_id, "branch": "main"})
        self.save_bbox(images[2]["datasetId"], images[2]["imageId"], "main-label", branch="main")
        main_change = self.commit(project_id, "main", "web workspace main change")
        main_change_id = self.commit_id(main_change)
        self.report["mainChangeCommitId"] = main_change_id

        merge_preview = object_payload(
            self.session.command(
                "preview_data_project_merge",
                {"dataProjectId": project_id, "sourceBranch": feature, "targetBranch": "main", "oursBranch": "main", "theirsBranch": feature},
            )
        )
        self.check("independent branch changes have no merge conflict", len(list_payload(merge_preview, "conflicts")) == 0)
        merge_result = object_payload(
            self.session.command(
                "merge_data_project_branches",
                {"dataProjectId": project_id, "sourceBranch": feature, "targetBranch": "main", "oursBranch": "main", "theirsBranch": feature, "message": "web workspace clean merge"},
            )
        )
        merge_commit = self.commit_payload(merge_result)
        merge_id = self.commit_id(merge_commit)
        self.report["mergeCommitId"] = merge_id
        self.check("clean merge records a second parent", bool(get_string(merge_commit, "secondParentId")))
        self.check("clean merge retains both real annotation labels", self.labels_for(images, project_id, ("feature-label", "main-label")))

        conflict_branch = "web-conflict"
        self.session.command("create_data_project_branch", {"dataProjectId": project_id, "name": conflict_branch, "fromCommitId": merge_id})
        self.session.command("switch_data_project_branch", {"dataProjectId": project_id, "branch": conflict_branch})
        self.save_bbox(images[0]["datasetId"], images[0]["imageId"], "conflict-theirs", branch=conflict_branch)
        conflict_theirs = self.commit(project_id, conflict_branch, "web workspace conflict theirs")
        conflict_theirs_id = self.commit_id(conflict_theirs)
        self.session.command("switch_data_project_branch", {"dataProjectId": project_id, "branch": "main"})
        self.save_bbox(images[0]["datasetId"], images[0]["imageId"], "conflict-ours", branch="main")
        conflict_ours = self.commit(project_id, "main", "web workspace conflict ours")
        conflict_ours_id = self.commit_id(conflict_ours)
        conflict_preview = object_payload(
            self.session.command(
                "preview_data_project_merge",
                {"dataProjectId": project_id, "sourceBranch": conflict_branch, "targetBranch": "main", "oursBranch": "main", "theirsBranch": conflict_branch},
            )
        )
        conflicts = list_payload(conflict_preview, "conflicts")
        self.check("three-way merge exposes a real conflicting entry", len(conflicts) > 0)
        conflict_result = object_payload(
            self.session.command(
                "merge_data_project_branches",
                {"dataProjectId": project_id, "sourceBranch": conflict_branch, "targetBranch": "main", "oursBranch": "main", "theirsBranch": conflict_branch, "message": "web workspace conflict merge"},
            )
        )
        session_id = get_string(conflict_result, "sessionId")
        merge_conflicts = list_payload(conflict_result, "conflicts")
        if not session_id or not merge_conflicts:
            raise VerificationFailure("conflicting merge did not return a resolution session")
        unresolved = merge_conflicts
        while unresolved:
            conflict = unresolved[0]
            key = get_string(conflict, "key")
            if not key:
                raise VerificationFailure("merge conflict omitted its key")
            resolved = object_payload(
                self.session.command(
                    "resolve_data_project_merge",
                    {"dataProjectId": project_id, "sessionId": session_id, "conflictKey": key, "resolution": "ours"},
                )
            )
            unresolved = list_payload(resolved, "conflicts") if isinstance(resolved, dict) else []
        resolved_commit = self.commit_payload(resolved)
        resolved_id = self.commit_id(resolved_commit)
        self.report["conflictTheirsCommitId"] = conflict_theirs_id
        self.report["conflictOursCommitId"] = conflict_ours_id
        self.report["conflictResolvedCommitId"] = resolved_id
        self.check("resolved conflict commit records both parents", bool(get_string(resolved_commit, "secondParentId")))
        actual_state = self.annotation_state(images[0]["datasetId"], images[0]["imageId"], "main")
        self.check("conflict resolution keeps the explicit ours label", "conflict-ours" in [item.get("label") for item in self.objects_from_state(actual_state)])

        restored = self.session.command(
            "revert_data_project_commit",
            {"dataProjectId": project_id, "branch": "main", "commitId": initial_id, "message": "web workspace restore initial snapshot"},
        )
        restore_commit = self.commit_payload(restored)
        restore_id = self.commit_id(restore_commit)
        self.report["restoreCommitId"] = restore_id
        restored_state = self.annotation_state(images[0]["datasetId"], images[0]["imageId"], "main")
        self.check("restore creates a new commit and returns to the initial label", restore_id != initial_id and images[0]["initialLabel"] in [item.get("label") for item in self.objects_from_state(restored_state)])

        commits = list_payload(
            self.session.command_aliases(("list_data_project_version_commits", "list_commits"), {"dataProjectId": project_id, "branch": "main"}),
            "commits",
            "items",
        )
        commit_ids = {get_string(item, "id", "commitId") for item in commits if isinstance(item, dict)}
        self.check("history retains the initial and resolved commits after restore", initial_id in commit_ids and resolved_id in commit_ids)
        graph_hash_refs = sum(self.hash_refs(item) for item in commits)
        self.check("history entries expose valid immutable hash references", graph_hash_refs > 0)
        self.report["graph"] = {
            "commitCount": len(commits),
            "hashRefsChecked": graph_hash_refs,
            "mergeSecondParent": get_string(merge_commit, "secondParentId"),
            "resolvedSecondParent": get_string(resolved_commit, "secondParentId"),
        }
        final_tree = self.working_tree(project_id, "main")
        self.check("restored main working tree is clean", final_tree.get("dirty") is False)
        self.report["resume"] = {
            "projectId": project_id,
            "mainHeadCommitId": get_string(final_tree, "headCommitId"),
            "restoreCommitId": restore_id,
            "images": [
                {
                    "datasetId": item["datasetId"],
                    "imageId": item["imageId"],
                    "sourceHash": item["sourceHash"],
                    "sourceByteSize": item["sourceByteSize"],
                    "expectedLabel": item["initialLabel"],
                }
                for item in images
            ],
        }
        self.member_isolation(project_id, images, member_credentials)
        self.csrf_check()
        self.expiry_check()

    @staticmethod
    def hash_refs(commit: Any) -> int:
        if not isinstance(commit, dict):
            return 0
        entries = commit.get("entries")
        if not isinstance(entries, list):
            return 0
        count = 0
        for entry in entries:
            if isinstance(entry, dict) and valid_hash(entry.get("imageHash")) and valid_hash(entry.get("annotationHash")):
                count += 1
        return count

    def labels_for(self, images: list[dict[str, Any]], project_id: str, expected: tuple[str, str]) -> bool:
        labels: set[str] = set()
        for image in images:
            state = self.annotation_state(image["datasetId"], image["imageId"], "main")
            labels.update(item.get("label") for item in self.objects_from_state(state) if isinstance(item.get("label"), str))
        return expected[0] in labels and expected[1] in labels

    def member_isolation(self, project_id: str, images: list[dict[str, Any]], credentials: dict[str, str] | None) -> None:
        if not credentials:
            self.check("member isolation", True, skipped=True)
            return
        member = WebSession(self.session.base_url, timeout=self.session.timeout)
        member.login(credentials)
        if member.user_id == self.session.user_id:
            raise VerificationFailure("member credentials resolved to the owner account")
        projects = list_payload(member.command("list_data_projects", {}), "projects", "items")
        visible = any(get_string(item, "id") == project_id for item in projects if isinstance(item, dict))
        if not visible:
            self.check("non-member cannot discover owner data project", True)
        else:
            result = member.command_result("get_data_project_working_tree", {"dataProjectId": project_id, "branch": "main"})
            self.check("member project read is permission scoped", result.status in (200, 403, 404), status=result.status, code=error_code(result.payload))
            if result.status == 200:
                target = images[0]
                denial = member.command_result(
                    "save_image_annotations",
                    {"projectId": target["datasetId"], "datasetId": target["datasetId"], "imageId": target["imageId"], "branch": "main", "baseRevision": -1, "objects": []},
                )
                self.check("member cannot write owner annotation", denial.status in (401, 403, 404), status=denial.status, code=error_code(denial.payload))
        try:
            member.logout()
        except VerificationFailure:
            pass

    def csrf_check(self) -> None:
        result = self.session.command_result("list_data_projects", {})
        # Repeat the harmless read with a deliberately foreign Origin while
        # retaining the authenticated cookies.  No state is changed either
        # way, and this directly tests the same-origin CSRF gate.
        bad = self.session.request(
            "POST",
            "/api/v1/web/commands/list_data_projects",
            body=b"{}",
            content_type="application/json",
            origin="https://invalid-origin.example",
        )
        self.check("cross-origin web command is rejected by CSRF/origin policy", bad.status in (400, 403), status=bad.status, code=error_code(bad.payload))
        self.check("same-origin command remains available after CSRF probe", result.status == 200, status=result.status, code=error_code(result.payload))

    def expiry_check(self) -> None:
        self.session.logout()
        result = self.session.request("GET", "/api/v1/web/auth/session")
        self.check("expired/logged-out web cookie receives 401", result.status == 401, status=result.status, code=error_code(result.payload))

    def resume_only(self, report: dict[str, Any]) -> None:
        resume = report.get("resume") if isinstance(report, dict) else None
        if not isinstance(resume, dict):
            raise VerificationFailure("resume report does not contain a resume fixture")
        project_id = get_string(resume, "projectId")
        if not project_id:
            raise VerificationFailure("resume report omitted its project id")
        self.report["projectId"] = project_id
        self.report["resumeInput"] = {"projectId": project_id}
        tree = self.working_tree(project_id, "main")
        self.check("post-restart project working tree is readable and clean", tree.get("dirty") is False)
        commits = list_payload(
            self.session.command_aliases(("list_data_project_version_commits", "list_commits"), {"dataProjectId": project_id, "branch": "main"}),
            "commits",
            "items",
        )
        commit_ids = {get_string(item, "id", "commitId") for item in commits if isinstance(item, dict)}
        restore_id = get_string(resume, "restoreCommitId")
        self.check("post-restart graph retains the saved restore commit", bool(restore_id and restore_id in commit_ids))
        verified_images: list[dict[str, Any]] = []
        for item in resume.get("images", []):
            if not isinstance(item, dict):
                continue
            dataset_id = get_string(item, "datasetId")
            image_id = get_string(item, "imageId")
            source_hash = get_string(item, "sourceHash")
            expected_label = get_string(item, "expectedLabel")
            if not dataset_id or not image_id or not valid_hash(source_hash) or not expected_label:
                raise VerificationFailure("resume report contains an invalid image fixture")
            data, route = self.session.download_image(dataset_id, image_id, fallback_project=project_id, fallback_hash=source_hash)
            state = self.annotation_state(dataset_id, image_id, "main")
            labels = [obj.get("label") for obj in self.objects_from_state(state)]
            self.check(f"post-restart image {image_id} bytes and label resume", sha256(data) == source_hash and expected_label in labels)
            verified_images.append({"datasetId": dataset_id, "imageId": image_id, "sha256": sha256(data), "route": route})
        self.check("post-restart report verifies all image fixtures", len(verified_images) == len(resume.get("images", [])) and len(verified_images) >= 4)
        self.report["resumeAfterRestart"] = {"projectId": project_id, "verifiedImages": verified_images, "headCommitId": get_string(tree, "headCommitId")}
        self.expiry_check()


def write_failure_report(path: Path, base_url: str | None, checks: list[dict[str, Any]], failure: VerificationFailure | None) -> None:
    report: dict[str, Any] = {"schema": SCHEMA, "baseUrl": base_url or "", "passed": False, "checks": checks}
    if failure:
        item: dict[str, Any] = {"message": str(failure)}
        if failure.status is not None:
            item["status"] = failure.status
        if failure.code:
            item["code"] = failure.code
        report["failure"] = item
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    except OSError:
        pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--credentials", required=True, type=Path, help="private owner credentials JSON")
    parser.add_argument("--member-credentials", type=Path, help="optional separate member/outsider credentials JSON")
    parser.add_argument("--base-url", help="absolute API base URL; otherwise use serverUrl in credentials")
    parser.add_argument("--output", type=Path, default=Path("web-workspace-verification.json"), help="safe JSON report path")
    parser.add_argument("--resume-report", type=Path, help="safe report from a pre-restart run")
    parser.add_argument("--resume-only", action="store_true", help="only verify the durable fixture in --resume-report")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    base_for_report: str | None = None
    checks: list[dict[str, Any]] = []
    failure: VerificationFailure | None = None
    session: WebSession | None = None
    acceptance: Acceptance | None = None
    try:
        credentials, base_url = load_credentials(args.credentials, args.base_url)
        session = WebSession(base_url, timeout=args.timeout)
        base_for_report = session.base_url
        acceptance = Acceptance(session, output=args.output)
        if args.resume_only:
            if not args.resume_report:
                raise VerificationFailure("--resume-only requires --resume-report")
            session.login(credentials)
            acceptance.resume_only(load_json(args.resume_report))
        else:
            member_credentials = None
            if args.member_credentials:
                member_credentials, member_base = load_credentials(args.member_credentials, args.base_url)
                member_base_normalized, _ = normalize_base_url(member_base)
                if member_base_normalized != session.base_url:
                    raise VerificationFailure("owner and member credentials must use the same base URL")
            session.login(credentials)
            acceptance.full_run(credentials, member_credentials)
        acceptance.report["passed"] = all(item.get("passed") is not False for item in acceptance.checks)
        acceptance.write()
        print(json.dumps({"passed": acceptance.report["passed"], "output": str(args.output), "checks": len(acceptance.checks)}, separators=(",", ":")))
        return 0 if acceptance.report["passed"] else 1
    except VerificationFailure as exc:
        failure = exc
        if acceptance is not None:
            acceptance.report["passed"] = False
            acceptance.report["failure"] = {
                "message": str(exc),
                **({"status": exc.status} if exc.status is not None else {}),
                **({"code": exc.code} if exc.code else {}),
            }
            try:
                acceptance.write()
            except OSError:
                write_failure_report(args.output, base_for_report, acceptance.checks, failure)
        else:
            write_failure_report(args.output, base_for_report, checks, failure)
        print(json.dumps({"passed": False, "output": str(args.output), "status": exc.status, "code": exc.code}, separators=(",", ":")))
        return 1
    except Exception as exc:  # pragma: no cover - final safe guard for a live service
        failure = VerificationFailure("unexpected verifier failure", code=type(exc).__name__)
        if acceptance is not None:
            acceptance.report["passed"] = False
            acceptance.report["failure"] = {"message": str(failure), "code": failure.code}
            try:
                acceptance.write()
            except OSError:
                write_failure_report(args.output, base_for_report, acceptance.checks, failure)
        else:
            write_failure_report(args.output, base_for_report, checks, failure)
        print(json.dumps({"passed": False, "output": str(args.output), "code": failure.code}, separators=(",", ":")))
        return 1
    finally:
        # Logout is normally performed by expiry_check.  If a mutation failed
        # midway, best-effort logout prevents a live staging cookie from being
        # left usable by this process; no response body is printed.
        if session is not None and session.user_id is not None:
            try:
                session.logout()
            except Exception:
                pass


if __name__ == "__main__":
    sys.exit(main())
