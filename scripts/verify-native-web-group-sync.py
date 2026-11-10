#!/usr/bin/env python3
"""Verify native -> Web -> native project-group interoperability.

The two native URLs must point at independently rooted acceptance helpers.  A
private credential file is read only to authenticate those helpers (they do
that themselves) and the Web session; credentials, cookies and response
bodies are never written to the report.
"""

from __future__ import annotations

import argparse
import copy
import datetime
import hashlib
import importlib.util
import json
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Any


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load acceptance helper")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ROOT = Path(__file__).resolve().parent
native_module = load_module("native_group_sync_contract", ROOT / "verify-data-project-versioning.py")
web_module = load_module("web_group_sync_contract", ROOT / "verify-web-workspace.py")


class SafeFailure(RuntimeError):
    def __init__(self, message: str, *, status: int | None = None, code: str | None = None):
        super().__init__(message)
        self.status = status
        self.code = code


def value_id(value: Any, *names: str) -> str:
    if isinstance(value, dict):
        for name in names:
            item = value.get(name)
            if isinstance(item, str) and item:
                return item
    raise SafeFailure("response omitted an identifier")


def commit_value(value: Any) -> dict[str, Any]:
    if isinstance(value, dict) and isinstance(value.get("commit"), dict):
        return value["commit"]
    if isinstance(value, dict):
        return value
    raise SafeFailure("commit response was not an object")


def canonical_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def objects_from_state(state: dict[str, Any]) -> list[dict[str, Any]]:
    value = state.get("objects", state.get("annotation", []))
    return value if isinstance(value, list) else []


class NativeApi:
    def __init__(self, base_url: str):
        self.base_url = base_url.rstrip("/")
        self.client = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def call(self, command: str, args: dict[str, Any] | None = None) -> Any:
        request = urllib.request.Request(
            f"{self.base_url}/api/invoke/{command}",
            data=json.dumps(args or {}, ensure_ascii=False).encode("utf-8"),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with self.client.open(request, timeout=180) as response:
                payload = json.load(response)
        except urllib.error.HTTPError as error:
            try:
                payload = json.loads(error.read())
            except (OSError, ValueError):
                payload = {}
            error_value = payload.get("error") if isinstance(payload, dict) else None
            code = error_value.get("code") if isinstance(error_value, dict) else None
            raise SafeFailure("native command failed", status=error.code, code=code) from error
        if not isinstance(payload, dict) or payload.get("ok") is not True:
            error_value = payload.get("error") if isinstance(payload, dict) else None
            code = error_value.get("code") if isinstance(error_value, dict) else None
            raise SafeFailure("native command failed", code=code)
        return payload.get("data")


def native_dataset_rows(api: NativeApi, project_id: str) -> list[dict[str, Any]]:
    rows = api.call("list_data_project_datasets", {"dataProjectId": project_id})
    if not isinstance(rows, list):
        raise SafeFailure("native dataset listing was not a list")
    return [row for row in rows if isinstance(row, dict)]


def native_group_state(api: NativeApi, project_id: str) -> dict[str, Any]:
    value = api.call("get_data_project_dataset_groups", {"dataProjectId": project_id, "branch": "main"})
    if not isinstance(value, dict):
        raise SafeFailure("native group state was not an object")
    return value


def native_group_rows(api: NativeApi, project_id: str) -> list[dict[str, Any]]:
    return native_group_state(api, project_id).get("projectMetadata", {}).get("datasetGroups", [])


def native_image_snapshot(api: NativeApi, datasets: list[dict[str, Any]]) -> dict[tuple[str, str], dict[str, Any]]:
    result: dict[tuple[str, str], dict[str, Any]] = {}
    for dataset in datasets:
        dataset_id = value_id(dataset, "id", "datasetId")
        dataset_name = str(dataset.get("name") or dataset_id)
        images = api.call(
            "list_project_images",
            {"projectId": dataset_id, "groupId": None, "offset": 0, "limit": 500},
        )
        if not isinstance(images, list):
            raise SafeFailure("native image listing was not a list")
        for image in images:
            image_id = value_id(image, "id", "imageId")
            file_name = str(image.get("fileName") or image_id)
            path_value = api.call(
                "get_file_asset_path", {"projectId": dataset_id, "imageId": image_id}
            )
            path = Path(value_id(path_value, "path", "filePath")) if isinstance(path_value, dict) else Path(str(path_value))
            if not path.is_file():
                raise SafeFailure("native image asset is missing")
            state = api.call(
                "get_image_annotation_state", {"projectId": dataset_id, "imageId": image_id}
            )
            result[(dataset_name, file_name)] = {
                "imageSha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                "objects": objects_from_state(state if isinstance(state, dict) else {}),
            }
    return result


def native_commit_hashes(api: NativeApi, project_id: str) -> tuple[set[str], set[str], bool]:
    rows = api.call(
        "list_data_project_version_commits", {"dataProjectId": project_id, "branch": "main"}
    )
    if not isinstance(rows, list):
        raise SafeFailure("native history was not a list")
    image_hashes: set[str] = set()
    annotation_hashes: set[str] = set()
    has_merge = False
    for commit in rows:
        if not isinstance(commit, dict):
            continue
        has_merge = has_merge or bool(commit.get("secondParentId"))
        for entry in commit.get("entries", []):
            if isinstance(entry, dict):
                for field, target in (("imageHash", image_hashes), ("annotationHash", annotation_hashes)):
                    value = entry.get(field)
                    if isinstance(value, str) and value:
                        target.add(value)
    return image_hashes, annotation_hashes, has_merge


def group_signature(groups: list[dict[str, Any]], dataset_names: dict[str, str]) -> list[tuple[str, tuple[str, ...]]]:
    result = []
    for group in groups:
        if not isinstance(group, dict):
            continue
        names = tuple(sorted(dataset_names.get(str(key), str(key)) for key in group.get("datasetKeys", [])))
        result.append((str(group.get("name") or ""), names))
    return sorted(result)


def web_groups(session: Any, project_id: str) -> dict[str, Any]:
    value = session.command("get_data_project_dataset_groups", {"dataProjectId": project_id, "branch": "main"})
    if not isinstance(value, dict):
        raise SafeFailure("Web group state was not an object")
    return value


def web_dataset_rows(session: Any, project_id: str) -> list[dict[str, Any]]:
    value = session.command("list_data_project_datasets", {"dataProjectId": project_id})
    if not isinstance(value, list):
        raise SafeFailure("Web dataset listing was not a list")
    return [row for row in value if isinstance(row, dict)]


def web_image_snapshot(session: Any, datasets: list[dict[str, Any]], project_id: str) -> dict[tuple[str, str], dict[str, Any]]:
    result: dict[tuple[str, str], dict[str, Any]] = {}
    for dataset in datasets:
        dataset_id = value_id(dataset, "id", "datasetId")
        dataset_name = str(dataset.get("name") or dataset_id)
        images = session.command(
            "list_project_images",
            {"datasetId": dataset_id, "projectId": dataset_id, "groupId": None, "offset": 0, "limit": 500},
        )
        if not isinstance(images, list):
            raise SafeFailure("Web image listing was not a list")
        for image in images:
            image_id = value_id(image, "id", "imageId")
            file_name = str(image.get("fileName") or image_id)
            data, _ = session.download_image(dataset_id, image_id, fallback_project=project_id)
            state = session.command_aliases(
                ("get_image_annotation_state", "get_annotation"),
                {"datasetId": dataset_id, "projectId": dataset_id, "imageId": image_id, "branch": "main"},
            )
            if not isinstance(state, dict):
                raise SafeFailure("Web annotation state was not an object")
            result[(dataset_name, file_name)] = {
                "imageSha256": hashlib.sha256(data).hexdigest(),
                "objects": objects_from_state(state),
            }
    return result


def web_commit_hashes(session: Any, project_id: str) -> tuple[set[str], set[str], bool]:
    rows = session.command_aliases(
        ("list_data_project_version_commits", "list_commits"),
        {"dataProjectId": project_id, "branch": "main"},
    )
    if isinstance(rows, dict):
        rows = rows.get("commits", rows.get("items", []))
    if not isinstance(rows, list):
        raise SafeFailure("Web history was not a list")
    image_hashes: set[str] = set()
    annotation_hashes: set[str] = set()
    has_merge = False
    for commit in rows:
        if not isinstance(commit, dict):
            continue
        has_merge = has_merge or bool(commit.get("secondParentId"))
        for entry in commit.get("entries", []):
            if isinstance(entry, dict):
                for field, target in (("imageHash", image_hashes), ("annotationHash", annotation_hashes)):
                    value = entry.get(field)
                    if isinstance(value, str) and value:
                        target.add(value)
    return image_hashes, annotation_hashes, has_merge


def compare_snapshots(left: dict[tuple[str, str], dict[str, Any]], right: dict[tuple[str, str], dict[str, Any]]) -> bool:
    if set(left) != set(right):
        return False
    return all(
        left[key]["imageSha256"] == right[key]["imageSha256"]
        and canonical_json(left[key]["objects"]) == canonical_json(right[key]["objects"])
        for key in left
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--native-a-url", required=True)
    parser.add_argument("--native-b-url", required=True)
    parser.add_argument("--credentials", required=True, type=Path)
    parser.add_argument("--local-report", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--base-url")
    args = parser.parse_args()

    report: dict[str, Any] = {
        "schema": "native-web-group-sync/v1",
        "passed": False,
        "checkedAt": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "checks": [],
        "native": {"clientAUrl": args.native_a_url, "clientBUrl": args.native_b_url},
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def write() -> None:
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    def check(name: str, condition: bool) -> None:
        report["checks"].append({"name": name, "passed": bool(condition)})
        write()
        if not condition:
            raise SafeFailure(name)
        print(name, flush=True)

    web_session = None
    try:
        fixture = json.loads(args.local_report.read_text(encoding="utf-8"))
        project_a = value_id(fixture, "projectId")
        api_a = NativeApi(args.native_a_url)
        api_b = NativeApi(args.native_b_url)

        datasets_a = native_dataset_rows(api_a, project_a)
        groups_a_before = native_group_rows(api_a, project_a)
        names_a = {value_id(row, "id", "datasetId"): str(row.get("name") or value_id(row, "id", "datasetId")) for row in datasets_a}
        snapshot_a_before = native_image_snapshot(api_a, datasets_a)
        image_hashes_before, annotation_hashes_before, had_merge = native_commit_hashes(api_a, project_a)
        check("native fixture project and groups are available", bool(datasets_a) and bool(groups_a_before))
        check("native fixture retains an immutable merge parent", had_merge)

        remote = api_a.call(
            "create_remote_data_project",
            {"name": "native-web-group-sync-" + uuid.uuid4().hex[:10], "description": "isolated three-client group sync", "organizationId": None},
        )
        remote_id = value_id(remote, "id", "remoteProjectId")
        api_a.call("connect_remote_data_project", {"dataProjectId": project_a, "remoteProjectId": remote_id})
        published = api_a.call("publish_data_project", {"dataProjectId": project_a})
        check("native A publishes project metadata and graph", isinstance(published, dict) and published.get("outcome") == "pushed")

        cloned = api_b.call("clone_remote_data_project", {"remoteProjectId": remote_id})
        project_b = value_id(cloned, "localProjectId", "dataProjectId")
        datasets_b = native_dataset_rows(api_b, project_b)
        names_b = {value_id(row, "id", "datasetId"): str(row.get("name") or value_id(row, "id", "datasetId")) for row in datasets_b}
        check("native B clones every project dataset", len(datasets_b) == len(datasets_a))
        check("native B restores group names and membership", group_signature(native_group_rows(api_a, project_a), names_a) == group_signature(native_group_rows(api_b, project_b), names_b))
        snapshot_b_before = native_image_snapshot(api_b, datasets_b)
        check("native B clone preserves image bytes and labels", compare_snapshots(snapshot_a_before, snapshot_b_before))
        hashes_b, annotations_b, merge_b = native_commit_hashes(api_b, project_b)
        check("native B clone preserves immutable object references", hashes_b == image_hashes_before and annotations_b == annotation_hashes_before)
        check("native B clone preserves the second parent", merge_b)

        credentials, base = web_module.load_credentials(args.credentials, args.base_url)
        web_session = web_module.WebSession(base)
        web_session.login(credentials)
        report["webBaseUrl"] = base
        web_projects = web_session.command("list_data_projects", {})
        web_project = next((row for row in web_projects if isinstance(row, dict) and row.get("id") == remote_id), None)
        check("Web discovers the native-published project", web_project is not None)
        web_datasets = web_dataset_rows(web_session, remote_id)
        web_state_before = web_groups(web_session, remote_id)
        web_names = {value_id(row, "id", "datasetId"): str(row.get("name") or row.get("datasetKey") or value_id(row, "id", "datasetId")) for row in web_datasets}
        web_keys = {str(row.get("datasetKey")): str(row.get("name") or row.get("datasetKey")) for row in web_datasets}
        web_groups_before = web_state_before.get("projectMetadata", {}).get("datasetGroups", [])
        check("Web reads virtual datasets and native group structure", len(web_datasets) == len(datasets_a) and group_signature(web_groups_before, web_keys) == group_signature(groups_a_before, names_a))
        snapshot_web_before = web_image_snapshot(web_session, web_datasets, remote_id)
        check("Web reads original image bytes and labels", compare_snapshots(snapshot_a_before, snapshot_web_before))
        web_hashes, web_annotations, web_merge = web_commit_hashes(web_session, remote_id)
        check("Web sees native immutable object references and merge parent", image_hashes_before.issubset(web_hashes) and annotation_hashes_before.issubset(web_annotations) and web_merge)

        renamed = copy.deepcopy(web_groups_before)
        rename_text = "Web 重命名分组"
        renamed[0]["name"] = rename_text
        web_session.command(
            "set_data_project_dataset_groups",
            {"dataProjectId": remote_id, "branch": "main", "expectedRevision": web_state_before["revision"], "groups": renamed},
        )
        web_rename_commit = commit_value(web_session.command("commit_data_project_branch", {"dataProjectId": remote_id, "branch": "main", "message": "Web rename dataset group"}))
        check("Web commits a group-only rename", value_id(web_rename_commit, "id", "commitId") != "")

        pulled_a = api_a.call("sync_data_project", {"dataProjectId": project_a})
        pulled_b = api_b.call("sync_data_project", {"dataProjectId": project_b})
        check("native A pulls the Web group rename", native_group_rows(api_a, project_a)[0].get("name") == rename_text and pulled_a.get("outcome") in {"pulled", "upToDate"})
        check("native B pulls the Web group rename", native_group_rows(api_b, project_b)[0].get("name") == rename_text and pulled_b.get("outcome") in {"pulled", "upToDate"})

        grouped_a = native_group_state(api_a, project_a)
        grouped_rows_a = copy.deepcopy(grouped_a["projectMetadata"]["datasetGroups"])
        grouped_ids = {key for group in grouped_rows_a for key in group.get("datasetKeys", [])}
        third_a = next((dataset_id for dataset_id in names_a if dataset_id not in grouped_ids), None)
        if third_a is None:
            raise SafeFailure("native fixture has no ungrouped third dataset")
        grouped_rows_a.append({"id": str(uuid.uuid4()), "name": "Native 第三数据集", "datasetKeys": [third_a], "position": len(grouped_rows_a)})
        api_a.call("set_data_project_dataset_groups", {"dataProjectId": project_a, "branch": "main", "expectedRevision": grouped_a["revision"], "groups": grouped_rows_a})
        native_group_commit = commit_value(api_a.call("commit_data_project_branch", {"dataProjectId": project_a, "branch": "main", "message": "Native add third dataset group"}))
        pushed_a = api_a.call("sync_data_project", {"dataProjectId": project_a})
        check("native A pushes a new group after Web changes", pushed_a.get("outcome") == "pushed" and value_id(native_group_commit, "id", "commitId") != "")

        web_state_after = web_groups(web_session, remote_id)
        web_datasets_after = web_dataset_rows(web_session, remote_id)
        web_keys_after = {str(row.get("datasetKey")): str(row.get("name") or row.get("datasetKey")) for row in web_datasets_after}
        check("Web reads native A's third dataset group", any(group.get("name") == "Native 第三数据集" for group in web_state_after.get("projectMetadata", {}).get("datasetGroups", [])) and group_signature(web_state_after["projectMetadata"]["datasetGroups"], web_keys_after) == group_signature(native_group_rows(api_a, project_a), names_a))
        pulled_b_after = api_b.call("sync_data_project", {"dataProjectId": project_b})
        check("native B pulls native A's third dataset group", any(group.get("name") == "Native 第三数据集" for group in native_group_rows(api_b, project_b)) and pulled_b_after.get("outcome") in {"pulled", "upToDate"})

        snapshot_a_after = native_image_snapshot(api_a, native_dataset_rows(api_a, project_a))
        snapshot_b_after = native_image_snapshot(api_b, native_dataset_rows(api_b, project_b))
        snapshot_web_after = web_image_snapshot(web_session, web_dataset_rows(web_session, remote_id), remote_id)
        check("all clients retain original image bytes and labels", compare_snapshots(snapshot_a_before, snapshot_a_after) and compare_snapshots(snapshot_a_before, snapshot_b_after) and compare_snapshots(snapshot_a_before, snapshot_web_after))
        hashes_a_after, annotations_a_after, _ = native_commit_hashes(api_a, project_a)
        hashes_b_after, annotations_b_after, _ = native_commit_hashes(api_b, project_b)
        web_hashes_after, web_annotations_after, _ = web_commit_hashes(web_session, remote_id)
        check("all clients retain immutable image and annotation object identity", image_hashes_before.issubset(hashes_a_after) and image_hashes_before.issubset(hashes_b_after) and image_hashes_before.issubset(web_hashes_after) and annotation_hashes_before.issubset(annotations_a_after) and annotation_hashes_before.issubset(annotations_b_after) and annotation_hashes_before.issubset(web_annotations_after))

        report["fixture"] = {
            "localProjectA": project_a,
            "localProjectB": project_b,
            "remoteProjectId": remote_id,
            "datasetCount": len(datasets_a),
            "webRenameCommitId": value_id(web_rename_commit, "id", "commitId"),
            "nativeThirdGroupCommitId": value_id(native_group_commit, "id", "commitId"),
        }
        report["objectIdentity"] = {
            "imageHashCount": len(image_hashes_before),
            "annotationHashCount": len(annotation_hashes_before),
            "sourceImageCount": len(snapshot_a_before),
        }
        report["passed"] = True
        write()
        print(json.dumps({"passed": True, "checks": len(report["checks"]), "output": str(args.output)}))
        return 0
    except Exception as error:
        report["failure"] = {
            "kind": type(error).__name__,
            "status": getattr(error, "status", None),
            "code": getattr(error, "code", None),
        }
        write()
        print(json.dumps({"passed": False, "kind": type(error).__name__, "status": getattr(error, "status", None), "code": getattr(error, "code", None)}))
        return 1
    finally:
        if web_session is not None:
            try:
                web_session.logout()
            except Exception:
                pass


if __name__ == "__main__":
    raise SystemExit(main())
