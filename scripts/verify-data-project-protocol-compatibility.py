#!/usr/bin/env python3
"""Verify legacy data-project protocol compatibility against staging.

The verifier creates only named, isolated projects.  It checks that an old
REST graph push cannot drop enabled ``projectMetadata.datasetGroups`` and that
an old commit without that field remains readable as an empty group set when
the project has no groups enabled.  Credentials are loaded from the path
provided on the command line and are never written to the report or stdout.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
import uuid
from pathlib import Path
from typing import Any


SCRIPT_DIR = Path(__file__).resolve().parent
OUTPUT_SCHEMA = "data-project-protocol-compatibility/v1"


def load_module(name: str, path: Path) -> Any:
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load verifier helper")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


web = load_module("protocol_compat_web", SCRIPT_DIR / "verify-web-workspace.py")
cloud = load_module("protocol_compat_cloud", SCRIPT_DIR / "verify-data-project-cloud.py")


def public_code(value: Any) -> str | None:
    return cloud.safe_code(value)


def object_value(value: Any) -> dict[str, Any]:
    value = web.unwrap_data(value)
    if not isinstance(value, dict):
        raise RuntimeError("expected an object response")
    return value


def identifier(value: Any) -> str:
    if isinstance(value, dict):
        for key in ("id", "dataProjectId", "projectId", "commitId"):
            item = value.get(key)
            if isinstance(item, str) and item:
                return item
    raise RuntimeError("response omitted an identifier")


def commit_identifier(value: Any) -> str | None:
    if not isinstance(value, dict):
        return None
    for key in ("id", "commitId"):
        item = value.get(key)
        if isinstance(item, str) and item:
            return item
    return None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--credentials", required=True, type=Path)
    parser.add_argument("--base-url")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    credentials, base_url = web.load_credentials(args.credentials, args.base_url)
    checks: list[dict[str, Any]] = []
    report: dict[str, Any] = {
        "schema": OUTPUT_SCHEMA,
        "baseUrl": base_url,
        "passed": False,
        "checks": checks,
        "coverageReferences": {
            "artifact": "artifacts/three-part-workspace-20261027/staging-groups.json",
            "checks": [
                "foreign dataset keys are rejected",
                "branch checkout restores its own group names",
            ],
        },
    }
    admin_token: str | None = None
    web_session: Any | None = None
    grouped_project: str | None = None
    legacy_project: str | None = None

    def check(name: str, passed: bool, *, status: int | None = None, code: str | None = None) -> None:
        item: dict[str, Any] = {"name": name, "passed": bool(passed)}
        if status is not None:
            item["httpStatus"] = status
        if code:
            item["errorCode"] = code
        checks.append(item)
        if not passed:
            raise RuntimeError(name)

    def rest(path: str, method: str = "GET", body: Any | None = None) -> tuple[int, Any, dict[str, str]]:
        return cloud.request(base_url, method, path, admin_token, body)

    try:
        login = cloud.login(
            base_url,
            credentials["email"],
            credentials["password"],
            f"protocol-compat-{uuid.uuid4()}",
        )
        admin_token = login["accessToken"]
        web_session = web.WebSession(base_url)
        web_session.login(credentials)

        # Web creation initializes the branch/worktree.  The old REST client
        # is then used only for the compatibility push under test.
        grouped_project = identifier(
            object_value(
                web_session.command(
                    "create_data_project",
                    {
                        "name": f"protocol-compat-groups-{uuid.uuid4().hex[:10]}",
                        "description": "legacy protocol compatibility fixture",
                    },
                )
            )
        )
        dataset = object_value(
            web_session.command_aliases(
                ("create_dataset_project", "create_dataset"),
                {
                    "name": f"protocol-compat-dataset-{uuid.uuid4().hex[:8]}",
                    "description": "isolated compatibility dataset",
                    "datasetType": "object-detection",
                    "demoTemplate": "empty",
                    "schema": [{"id": 0, "label": "compat-class"}],
                    "classes": [{"id": 0, "label": "compat-class"}],
                    "dataProjectId": grouped_project,
                },
            )
        )
        dataset_id = identifier(dataset)
        dataset_rows = web.list_payload(
            web_session.command(
                "list_data_project_datasets", {"dataProjectId": grouped_project}
            ),
            "datasets",
            "items",
        )
        dataset_row = next(
            (
                row
                for row in dataset_rows
                if isinstance(row, dict)
                and (row.get("id") == dataset_id or row.get("datasetId") == dataset_id)
            ),
            {},
        )
        dataset_key = dataset_row.get("datasetKey")
        if not isinstance(dataset_key, str) or not dataset_key:
            dataset_key = dataset_id

        groups_before = object_value(
            web_session.command(
                "get_data_project_dataset_groups",
                {"dataProjectId": grouped_project, "branch": "main"},
            )
        )
        group = {
            "id": str(uuid.uuid4()),
            "name": "Legacy compatibility group",
            "datasetKeys": [dataset_key],
            "position": 0,
        }
        groups_after = object_value(
            web_session.command(
                "set_data_project_dataset_groups",
                {
                    "dataProjectId": grouped_project,
                    "branch": "main",
                    "expectedRevision": groups_before.get("revision"),
                    "groups": [group],
                },
            )
        )
        check(
            "grouped fixture enables non-empty projectMetadata",
            groups_after.get("projectMetadata", {}).get("datasetGroups") == [group],
        )
        web_session.command(
            "commit_data_project_branch",
            {
                "dataProjectId": grouped_project,
                "branch": "main",
                "message": "enable groups for protocol compatibility",
            },
        )

        graph_status, graph, _ = rest(f"/api/v1/data-projects/{grouped_project}/graph")
        check(
            "grouped fixture graph is readable",
            graph_status == 200 and isinstance(graph, dict),
            status=graph_status,
            code=public_code(graph),
        )
        remote_revision = int(graph["remoteRevision"])
        main_ref = next((row for row in graph.get("refs", []) if row.get("name") == "main"), {})
        main_head = main_ref.get("commitId")
        legacy_commit = str(uuid.uuid4())
        old_tree = {
            "datasets": [{"datasetKey": dataset_key, "datasetId": dataset_key}],
            "entries": [],
            # Deliberately omit projectMetadata: this is the old client shape.
        }
        rejected_status, rejected_payload, _ = rest(
            f"/api/v1/data-projects/{grouped_project}/push",
            "POST",
            {
                "expectedRemoteRevision": remote_revision,
                "commits": [
                    {
                        "commitId": legacy_commit,
                        "parentIds": [main_head] if main_head else [],
                        "message": "legacy client omitted project metadata",
                        "tree": old_tree,
                        "metadata": {},
                    }
                ],
                "refs": [],
            },
        )
        check(
            "legacy push without projectMetadata is explicitly rejected",
            rejected_status == 409
            and public_code(rejected_payload) == "PROJECT_METADATA_REQUIRED",
            status=rejected_status,
            code=public_code(rejected_payload),
        )
        graph_status_after, graph_after, _ = rest(
            f"/api/v1/data-projects/{grouped_project}/graph"
        )
        main_ref_after = next(
            (row for row in graph_after.get("refs", []) if row.get("name") == "main"),
            {},
        ) if isinstance(graph_after, dict) else {}
        check(
            "rejected legacy push preserves remote revision and refs",
            graph_status_after == 200
            and int(graph_after.get("remoteRevision", -1)) == remote_revision
            and main_ref_after.get("commitId") == main_head,
            status=graph_status_after,
            code=public_code(graph_after),
        )

        # A Web-created, otherwise empty project is used so the legacy REST
        # commit has a branch/worktree that the new reader can hydrate.
        legacy_project = identifier(
            object_value(
                web_session.command(
                    "create_data_project",
                    {
                        "name": f"protocol-compat-legacy-{uuid.uuid4().hex[:10]}",
                        "description": "empty legacy snapshot fixture",
                    },
                )
            )
        )
        legacy_graph_status, legacy_graph, _ = rest(
            f"/api/v1/data-projects/{legacy_project}/graph"
        )
        check(
            "legacy snapshot graph is readable before push",
            legacy_graph_status == 200 and isinstance(legacy_graph, dict),
            status=legacy_graph_status,
            code=public_code(legacy_graph),
        )
        legacy_revision = int(legacy_graph["remoteRevision"])
        legacy_snapshot_commit = str(uuid.uuid4())
        legacy_push_status, legacy_push, _ = rest(
            f"/api/v1/data-projects/{legacy_project}/push",
            "POST",
            {
                "expectedRemoteRevision": legacy_revision,
                "commits": [
                    {
                        "commitId": legacy_snapshot_commit,
                        "parentIds": [],
                        "message": "legacy snapshot without projectMetadata",
                        "tree": {"datasetIds": [], "entries": []},
                        "metadata": {},
                    }
                ],
                "refs": [
                    {
                        "name": "main",
                        "expectedCommitId": None,
                        "commitId": legacy_snapshot_commit,
                    }
                ],
            },
        )
        check(
            "legacy snapshot without projectMetadata remains accepted for ungrouped project",
            legacy_push_status == 200,
            status=legacy_push_status,
            code=public_code(legacy_push),
        )
        empty_groups = object_value(
            web_session.command(
                "get_data_project_dataset_groups",
                {"dataProjectId": legacy_project, "branch": "main"},
            )
        )
        check(
            "legacy snapshot reads with empty datasetGroups",
            empty_groups.get("projectMetadata", {}).get("datasetGroups") == []
            and isinstance(empty_groups.get("revision"), int),
        )
        commits = web.list_payload(
            web_session.command(
                "list_data_project_version_commits",
                {"dataProjectId": legacy_project, "branch": "main"},
            ),
            "commits",
            "items",
        )
        legacy_row = next(
            (
                row
                for row in commits
                if isinstance(row, dict)
                and commit_identifier(row) == legacy_snapshot_commit
            ),
            None,
        )
        legacy_metadata = legacy_row.get("projectMetadata") if isinstance(legacy_row, dict) else None
        check(
            "legacy commit remains readable without inventing groups",
            legacy_row is not None
            and (
                legacy_metadata is None
                or legacy_metadata.get("datasetGroups", []) == []
            ),
        )

        report.update(
            {
                "projectIds": {
                    "groupedCompatibility": grouped_project,
                    "legacyEmptySnapshot": legacy_project,
                },
                "remoteRevisionAfterRejectedPush": remote_revision,
                "legacyCommitId": legacy_snapshot_commit,
                "passed": all(item["passed"] for item in checks),
            }
        )
    except Exception as error:
        report["error"] = type(error).__name__
        if getattr(error, "status", None) is not None:
            report["failureHttpStatus"] = error.status
        if getattr(error, "code", None):
            report["failureErrorCode"] = str(error.code)
    finally:
        cleanup: dict[str, int] = {}
        for project_id in (grouped_project, legacy_project):
            if project_id and admin_token:
                status, _, _ = cloud.request(
                    base_url,
                    "DELETE",
                    f"/api/v1/data-projects/{project_id}",
                    admin_token,
                )
                cleanup[project_id] = status
        report["cleanupHttpStatus"] = cleanup
        if web_session is not None:
            try:
                web_session.logout()
            except Exception:
                pass
        if admin_token:
            try:
                cloud.request(base_url, "POST", "/api/v1/auth/logout", admin_token)
            except Exception:
                pass
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(
            json.dumps(
                {
                    "passed": report["passed"],
                    "checks": len(checks),
                    "output": str(args.output),
                    "cleanupCount": len(cleanup),
                }
            )
        )
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
