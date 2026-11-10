#!/usr/bin/env python3
"""Black-box staging verification for the project-level cloud repository.

The script deliberately prints only safe identifiers and HTTP/error codes.
Access and refresh tokens stay in memory and are never included in reports.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Any


def request(
    base_url: str,
    method: str,
    path: str,
    token: str | None = None,
    body: Any | None = None,
    headers: dict[str, str] | None = None,
) -> tuple[int, Any, dict[str, str]]:
    request_headers = {"Accept": "application/json"}
    if token:
        request_headers["Authorization"] = f"Bearer {token}"
    if headers:
        request_headers.update(headers)
    payload = None
    if body is not None:
        if isinstance(body, (bytes, bytearray)):
            payload = bytes(body)
        else:
            payload = json.dumps(body, separators=(",", ":")).encode()
            request_headers.setdefault("Content-Type", "application/json")
    request = urllib.request.Request(
        f"{base_url.rstrip('/')}{path}",
        data=payload,
        headers=request_headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            raw = response.read()
            content_type = response.headers.get("Content-Type", "")
            if "application/json" in content_type:
                return response.status, json.loads(raw.decode()), dict(response.headers)
            return response.status, raw, dict(response.headers)
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            parsed: Any = json.loads(raw.decode())
        except (UnicodeDecodeError, json.JSONDecodeError):
            parsed = {"message": raw[:200].decode(errors="replace")}
        return error.code, parsed, dict(error.headers)


def login(base_url: str, email: str, password: str, device_id: str) -> dict[str, Any]:
    status, payload, _ = request(
        base_url,
        "POST",
        "/api/v1/auth/login",
        body={
            "email": email,
            "password": password,
            "deviceId": device_id,
            "platform": "desktop",
            "deviceName": "data-project-staging-verifier",
        },
    )
    if status != 200 or not isinstance(payload, dict) or not payload.get("accessToken"):
        raise RuntimeError(f"login failed: HTTP {status} code={safe_code(payload)}")
    return payload


def safe_code(payload: Any) -> str | None:
    if isinstance(payload, dict):
        code = payload.get("code")
        return str(code) if code is not None else None
    return None


def check(
    checks: list[dict[str, Any]],
    name: str,
    status: int,
    expected: int,
    payload: Any,
) -> None:
    checks.append(
        {
            "name": name,
            "passed": status == expected,
            "httpStatus": status,
            "expectedStatus": expected,
            "errorCode": safe_code(payload),
        }
    )
    if status != expected:
        raise RuntimeError(
            f"{name}: expected HTTP {expected}, got {status} code={safe_code(payload)}"
        )


def require_dict(payload: Any, field: str) -> Any:
    if not isinstance(payload, dict) or field not in payload:
        raise RuntimeError(f"response missing {field}")
    return payload[field]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default="http://127.0.0.1:16425")
    parser.add_argument(
        "--credentials",
        default="/home/ubuntu/projects/.deployment/image-annotation-data-projects-20261026/staging-credentials.json",
    )
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    checks: list[dict[str, Any]] = []
    report: dict[str, Any] = {
        "schema": "data-project-cloud-verification/v1",
        "baseUrl": args.base_url,
        "passed": False,
        "checks": checks,
    }
    admin_token = None
    member_token = None
    try:
        credentials = json.loads(Path(args.credentials).read_text())
        admin_session = login(
            args.base_url,
            credentials["email"],
            credentials["password"],
            f"data-project-admin-{uuid.uuid4()}",
        )
        admin_token = admin_session["accessToken"]
        admin_user = require_dict(admin_session, "user")

        legacy_status, legacy, _ = request(
            args.base_url,
            "POST",
            "/api/v1/projects",
            admin_token,
            {"name": f"staging invitation {uuid.uuid4()}", "description": "verification"},
        )
        check(checks, "legacy project for second-member invitation", legacy_status, 200, legacy)
        legacy_id = str(require_dict(legacy, "id"))
        member_email = f"data-project-member-{uuid.uuid4().hex[:12]}@example.invalid"
        member_password = f"StagingMember-{uuid.uuid4()}"
        invite_status, invite, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/projects/{legacy_id}/invitations",
            admin_token,
            {"email": member_email, "role": "viewer"},
        )
        check(checks, "create same-organization member invitation", invite_status, 200, invite)
        invite_token = require_dict(invite, "token")
        accept_status, accepted, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/auth/invitations/{invite_token}/accept",
            body={
                "displayName": "Data Project Staging Member",
                "password": member_password,
                "deviceId": f"data-project-member-{uuid.uuid4()}",
                "platform": "desktop",
            },
        )
        check(checks, "accept second-member invitation", accept_status, 200, accepted)
        member_token = require_dict(accepted, "accessToken")
        member_user = require_dict(accepted, "user")
        member_id = str(require_dict(member_user, "id"))

        create_status, project, _ = request(
            args.base_url,
            "POST",
            "/api/v1/data-projects",
            admin_token,
            {
                "name": f"staging data project {uuid.uuid4()}",
                "description": "cloud repository verification",
                "organizationId": admin_user.get("organizationId"),
            },
        )
        check(checks, "create data project", create_status, 200, project)
        project_id = str(require_dict(project, "id"))
        revision = int(require_dict(project, "revision"))
        report["projectId"] = project_id
        report["memberId"] = member_id

        member_add_status, member_add, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/members",
            admin_token,
            {"userId": member_id, "role": "viewer"},
        )
        check(checks, "add viewer member", member_add_status, 200, member_add)
        members_status, members, _ = request(
            args.base_url,
            "GET",
            f"/api/v1/data-projects/{project_id}/members",
            member_token,
        )
        check(checks, "viewer can read member list", members_status, 200, members)

        graph_status, graph, _ = request(
            args.base_url,
            "GET",
            f"/api/v1/data-projects/{project_id}/graph",
            member_token,
        )
        check(checks, "viewer can read graph", graph_status, 200, graph)
        # Adding the member is itself a project revision.  Read the revision
        # after all ACL setup so the first repository push exercises the
        # normal CAS path rather than failing for an intentionally stale
        # revision.
        revision = int(require_dict(graph, "remoteRevision"))

        viewer_push_status, viewer_push, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/push",
            member_token,
            {"expectedRemoteRevision": revision, "commits": [], "refs": []},
        )
        check(checks, "viewer cannot push", viewer_push_status, 403, viewer_push)

        content = b"data-project-staging-object-v1"
        content_hash = hashlib.sha256(content).hexdigest()
        wrong_status, wrong, _ = request(
            args.base_url,
            "PUT",
            f"/api/v1/data-projects/{project_id}/objects/{'0' * 64}",
            admin_token,
            content,
            {
                "Content-Type": "application/octet-stream",
                "X-Object-Size": str(len(content)),
                "X-Object-Kind": "other",
            },
        )
        check(checks, "reject blob hash mismatch", wrong_status, 400, wrong)
        blob_status, blob, _ = request(
            args.base_url,
            "PUT",
            f"/api/v1/data-projects/{project_id}/objects/{content_hash}",
            admin_token,
            content,
            {
                "Content-Type": "application/octet-stream",
                "X-Object-Size": str(len(content)),
                "X-Object-Kind": "other",
            },
        )
        check(checks, "upload verified blob", blob_status, 200, blob)
        blob_read_status, blob_read, _ = request(
            args.base_url,
            "GET",
            f"/api/v1/data-projects/{project_id}/objects/{content_hash}",
            member_token,
        )
        check(checks, "viewer can read project-scoped blob", blob_read_status, 200, blob_read)
        if blob_read != content:
            raise RuntimeError("downloaded blob bytes differ")

        isolation_status, isolation_project, _ = request(
            args.base_url,
            "POST",
            "/api/v1/data-projects",
            admin_token,
            {
                "name": f"staging object isolation {uuid.uuid4()}",
                "description": "object scope verification",
                "organizationId": admin_user.get("organizationId"),
            },
        )
        check(checks, "create second project for object isolation", isolation_status, 200, isolation_project)
        isolation_project_id = str(require_dict(isolation_project, "id"))
        isolated_read_status, isolated_read, _ = request(
            args.base_url,
            "GET",
            f"/api/v1/data-projects/{isolation_project_id}/objects/{content_hash}",
            admin_token,
        )
        check(checks, "reject cross-project blob lookup", isolated_read_status, 404, isolated_read)
        isolation_delete_status, isolation_deleted, _ = request(
            args.base_url,
            "DELETE",
            f"/api/v1/data-projects/{isolation_project_id}",
            admin_token,
        )
        check(checks, "delete object-isolation project", isolation_delete_status, 200, isolation_deleted)

        commit_1 = str(uuid.uuid4())
        commit_2 = str(uuid.uuid4())
        commit_sibling = str(uuid.uuid4())
        tree = {"objectHash": content_hash, "entries": []}
        push_1_status, push_1, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/push",
            admin_token,
            {
                "expectedRemoteRevision": revision,
                "commits": [
                    {
                        "commitId": commit_1,
                        "parentIds": [],
                        "message": "baseline",
                        "tree": tree,
                        "metadata": {},
                    }
                ],
                "refs": [
                    {"name": "main", "expectedCommitId": None, "commitId": commit_1}
                ],
            },
        )
        check(checks, "push baseline commit", push_1_status, 200, push_1)
        revision = int(require_dict(push_1, "remoteRevision"))

        immutable_status, immutable, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/push",
            admin_token,
            {
                "expectedRemoteRevision": revision,
                "commits": [
                    {
                        "commitId": commit_1,
                        "parentIds": [],
                        "message": "tampered",
                        "tree": tree,
                        "metadata": {},
                    }
                ],
                "refs": [],
            },
        )
        check(checks, "reject immutable commit mutation", immutable_status, 409, immutable)
        if safe_code(immutable) != "IMMUTABLE_COMMIT_MISMATCH":
            raise RuntimeError("immutable commit rejection used the wrong error code")

        cas_status, cas, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/push",
            admin_token,
            {"expectedRemoteRevision": 1, "commits": [], "refs": []},
        )
        check(checks, "reject stale remote revision", cas_status, 409, cas)
        if safe_code(cas) != "REMOTE_REVISION_MISMATCH":
            raise RuntimeError("stale revision rejection used the wrong error code")

        push_2_status, push_2, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/push",
            admin_token,
            {
                "expectedRemoteRevision": revision,
                "commits": [
                    {
                        "commitId": commit_2,
                        "parentIds": [commit_1],
                        "message": "fast-forward",
                        "tree": tree,
                        "metadata": {},
                    }
                ],
                "refs": [
                    {
                        "name": "main",
                        "expectedCommitId": commit_1,
                        "commitId": commit_2,
                    }
                ],
            },
        )
        check(checks, "push fast-forward commit", push_2_status, 200, push_2)
        revision = int(require_dict(push_2, "remoteRevision"))

        nonancestor_status, nonancestor, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/push",
            admin_token,
            {
                "expectedRemoteRevision": revision,
                "commits": [
                    {
                        "commitId": commit_sibling,
                        "parentIds": [],
                        "message": "sibling",
                        "tree": tree,
                        "metadata": {},
                    }
                ],
                "refs": [
                    {
                        "name": "main",
                        "expectedCommitId": commit_2,
                        "commitId": commit_sibling,
                    }
                ],
            },
        )
        check(checks, "reject non-fast-forward ref", nonancestor_status, 409, nonancestor)
        if safe_code(nonancestor) != "NON_FAST_FORWARD":
            raise RuntimeError("non-fast-forward rejection used the wrong error code")

        promote_status, promote, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/members",
            admin_token,
            {"userId": member_id, "role": "editor"},
        )
        check(checks, "promote member to editor", promote_status, 200, promote)
        status_status, project_status, _ = request(
            args.base_url,
            "GET",
            f"/api/v1/data-projects/{project_id}/status",
            admin_token,
        )
        check(checks, "read revision after membership change", status_status, 200, project_status)
        revision = int(require_dict(project_status, "remoteRevision"))

        commit_3 = str(uuid.uuid4())
        editor_status, editor, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/push",
            member_token,
            {
                "expectedRemoteRevision": revision,
                "commits": [
                    {
                        "commitId": commit_3,
                        "parentIds": [commit_2],
                        "message": "editor update",
                        "tree": tree,
                        "metadata": {},
                    }
                ],
                "refs": [
                    {
                        "name": "main",
                        "expectedCommitId": commit_2,
                        "commitId": commit_3,
                    }
                ],
            },
        )
        check(checks, "editor can push fast-forward", editor_status, 200, editor)
        revision = int(require_dict(editor, "remoteRevision"))

        commit_feature = str(uuid.uuid4())
        feature_status, feature, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/push",
            member_token,
            {
                "expectedRemoteRevision": revision,
                "commits": [
                    {
                        "commitId": commit_feature,
                        "parentIds": [commit_3],
                        "message": "feature-only update",
                        "tree": tree,
                        "metadata": {},
                    }
                ],
                "refs": [
                    {
                        "name": "feature",
                        "expectedCommitId": None,
                        "commitId": commit_feature,
                    }
                ],
            },
        )
        check(checks, "push non-main feature ref", feature_status, 200, feature)
        revision = int(require_dict(feature, "remoteRevision"))

        commit_merge = str(uuid.uuid4())
        merge_status, merge, _ = request(
            args.base_url,
            "POST",
            f"/api/v1/data-projects/{project_id}/push",
            member_token,
            {
                "expectedRemoteRevision": revision,
                "commits": [
                    {
                        "commitId": commit_merge,
                        "parentIds": [commit_3, commit_feature],
                        "message": "two-parent merge",
                        "tree": tree,
                        "metadata": {},
                    }
                ],
                "refs": [
                    {
                        "name": "main",
                        "expectedCommitId": commit_3,
                        "commitId": commit_merge,
                    }
                ],
            },
        )
        check(checks, "push double-parent merge", merge_status, 200, merge)
        revision = int(require_dict(merge, "remoteRevision"))
        graph_after_merge_status, graph_after_merge, _ = request(
            args.base_url,
            "GET",
            f"/api/v1/data-projects/{project_id}/graph",
            admin_token,
        )
        check(checks, "read feature and merge graph", graph_after_merge_status, 200, graph_after_merge)
        if isinstance(graph_after_merge, dict):
            refs = graph_after_merge.get("refs", [])
            commits = graph_after_merge.get("commits", [])
            feature_ref = next(
                (item for item in refs if item.get("name") == "feature"), None
            )
            merge_commit = next(
                (item for item in commits if item.get("commitId") == commit_merge), None
            )
            if not feature_ref or feature_ref.get("commitId") != commit_feature:
                raise RuntimeError("feature ref was not retained in graph")
            if not merge_commit or merge_commit.get("parentIds") != [commit_3, commit_feature]:
                raise RuntimeError("double-parent merge was not retained in graph")

        protected_status, protected, _ = request(
            args.base_url, "POST", f"/api/v1/data-projects/{project_id}/push",
            admin_token,
            {"expectedRemoteRevision": revision, "commits": [],
             "refs": [{"name": "main", "expectedCommitId": commit_merge, "commitId": None}]},
        )
        check(checks, "main branch cannot be cleared", protected_status, 409, protected)

        revoke_status, revoked, _ = request(
            args.base_url,
            "DELETE",
            f"/api/v1/data-projects/{project_id}/members/{member_id}",
            admin_token,
        )
        check(checks, "revoke member", revoke_status, 200, revoked)
        revoked_graph_status, revoked_graph, _ = request(
            args.base_url,
            "GET",
            f"/api/v1/data-projects/{project_id}/graph",
            member_token,
        )
        check(checks, "revoked member loses graph access", revoked_graph_status, 403, revoked_graph)

        delete_status, deleted, _ = request(
            args.base_url,
            "DELETE",
            f"/api/v1/data-projects/{project_id}",
            admin_token,
        )
        check(checks, "delete data project", delete_status, 200, deleted)
        deleted_graph_status, deleted_graph, _ = request(
            args.base_url,
            "GET",
            f"/api/v1/data-projects/{project_id}/graph",
            admin_token,
        )
        # Deleted rows are ACL-hidden by the same permission guard as an
        # unknown project, so callers receive a non-disclosing 403.
        check(checks, "deleted project is inaccessible", deleted_graph_status, 403, deleted_graph)

        report["passed"] = all(item["passed"] for item in checks)
    except Exception as error:
        report["error"] = str(error)
        report["passed"] = False
    finally:
        # Logout is best effort and never prints the response.
        if admin_token:
            request(args.base_url, "POST", "/api/v1/auth/logout", admin_token)
        if member_token:
            request(args.base_url, "POST", "/api/v1/auth/logout", member_token)
        output = Path(args.output)
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
