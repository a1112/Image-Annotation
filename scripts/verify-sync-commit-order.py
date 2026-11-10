#!/usr/bin/env python3
"""Verify that concurrent sync writes commit change events in project order.

This is an intentionally opt-in staging-only check.  It creates two assets in
one project, pauses one device's ``annotation.save`` audit insert inside a
temporary PostgreSQL trigger, and verifies that another device cannot commit a
later annotation event first.  The trigger and function are always removed in
``finally``.

The credentials file has the same shape as ``verify-collaboration.py``.  The
database argument is a PostgreSQL database name used with ``sudo -u postgres
psql``; it must contain ``_staging``.  Reports contain metadata and check
names only and never contain credentials or response bodies.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import threading
import time
import uuid


def load_checks_helper():
    helper_path = Path(__file__).with_name("verify-collaboration.py")
    spec = importlib.util.spec_from_file_location("verify_collaboration", helper_path)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load verify-collaboration.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


_DB_NAME = re.compile(r"^[A-Za-z0-9_]+$")
_IDENTIFIER = re.compile(r"^[A-Za-z0-9_]+$")


class StagingDatabase:
    def __init__(self, name: str):
        if not _DB_NAME.fullmatch(name) or "_staging" not in name:
            raise ValueError("--database must be a PostgreSQL name containing _staging")
        self.name = name

    def execute(self, sql: str, *, tuples: bool = False) -> str:
        command = [
            "sudo",
            "-u",
            "postgres",
            "psql",
            "--no-psqlrc",
            "--dbname",
            self.name,
            "--set",
            "ON_ERROR_STOP=1",
        ]
        if tuples:
            command.extend(["--tuples-only", "--no-align"])
        command.extend(["--command", sql])
        completed = subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
            timeout=15,
        )
        if completed.returncode != 0:
            # Do not include psql output: it may contain deployment details and
            # is not part of the metadata-only report contract.
            raise RuntimeError("isolated staging PostgreSQL command failed")
        return completed.stdout.strip()

    def scalar(self, sql: str) -> str:
        output = self.execute(sql, tuples=True)
        return output.splitlines()[0].strip() if output else ""


def sql_literal(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def sql_identifier(value: str) -> str:
    if not _IDENTIFIER.fullmatch(value):
        raise ValueError("temporary PostgreSQL identifier is invalid")
    return '"' + value + '"'


def install_sleep_trigger(database: StagingDatabase, function_name: str, trigger_name: str, device_id: str) -> None:
    function = sql_identifier(function_name)
    trigger = sql_identifier(trigger_name)
    database.execute(
        f"""
        CREATE FUNCTION public.{function}() RETURNS trigger
        LANGUAGE plpgsql
        AS $sync_order$
        BEGIN
            IF NEW.device_id = {sql_literal(device_id)}
               AND NEW.action = 'annotation.save' THEN
                PERFORM pg_sleep(3);
            END IF;
            RETURN NEW;
        END;
        $sync_order$;
        CREATE TRIGGER {trigger}
        BEFORE INSERT ON public.audit_events
        FOR EACH ROW
        EXECUTE FUNCTION public.{function}();
        """
    )


def remove_sleep_trigger(database: StagingDatabase, function_name: str, trigger_name: str) -> None:
    database.execute(
        f"""
        DROP TRIGGER IF EXISTS {sql_identifier(trigger_name)} ON public.audit_events;
        DROP FUNCTION IF EXISTS public.{sql_identifier(function_name)}();
        """
    )


def pg_sleep_active(database: StagingDatabase) -> bool:
    value = database.scalar(
        """
        SELECT COUNT(*)
        FROM pg_stat_activity
        WHERE datname = current_database()
          AND state = 'active'
          AND wait_event = 'PgSleep'
          AND query ILIKE '%audit_events%'
        """
    )
    return value.isdigit() and int(value) > 0


def upload_asset(checks, token: str, project_id: str, prefix: str, index: int, image_bytes: bytes) -> str:
    content_hash = hashlib.sha256(image_bytes).hexdigest()
    upload = checks.request(
        "POST",
        f"/api/v1/projects/{project_id}/assets/upload-session",
        {
            "clientKey": f"sync-order-{prefix}-{index}",
            "fileName": f"sync-order-{index}.png",
            "contentHash": content_hash,
            "mimeType": "image/png",
            "width": 640,
            "height": 480,
            "byteSize": len(image_bytes),
        },
        token,
    )
    asset_id = upload["assetId"]
    checks.request("PUT", upload["uploadUrl"], image_bytes, expected=(200, 201, 204), raw=True)
    checks.request("POST", f"/api/v1/projects/{project_id}/assets/{asset_id}/complete", token=token)
    return asset_id


def annotation_operation(image_id: str, operation_id: str, label: str, x: int) -> dict:
    return {
        "operationId": operation_id,
        "entityType": "annotation",
        "entityId": image_id,
        "operation": "update",
        "baseRevision": 0,
        "payload": {
            "imageId": image_id,
            "objects": [
                {
                    "id": label,
                    "classId": 0,
                    "label": "同步顺序验收",
                    "type": "bbox",
                    "bbox": {"x": x, "y": 110, "width": 180, "height": 120},
                    "attributes": {},
                }
            ],
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--credentials", type=Path, required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()

    started = time.monotonic()
    checks_module = load_checks_helper()
    checks = checks_module.Checks(args.base_url)
    database = StagingDatabase(args.database)
    credentials = json.loads(args.credentials.read_text())
    project_id = None
    trigger_started = False
    cleanup_error = None
    failure = None
    executor = None

    function_name = f"verify_sync_order_fn_{checks.prefix}"
    trigger_name = f"verify_sync_order_trigger_{checks.prefix}"

    try:
        health = checks.request("GET", "/health")
        checks.check("health", health.get("status") == "ok")

        session_a, device_a = checks.login(credentials, "desktop")
        session_b, device_b = checks.login(credentials, "ios")
        token_a = session_a["accessToken"]
        token_b = session_b["accessToken"]

        project_id = checks.request(
            "POST",
            "/api/v1/projects",
            {
                "name": "提交顺序验收 " + checks.prefix,
                "description": "并发 change_events 顺序验收样本。",
                "mode": "cloud_linked",
            },
            token_a,
        )["id"]
        image_bytes = checks_module.png_fixture()
        image_a = upload_asset(checks, token_a, project_id, checks.prefix, 0, image_bytes)
        image_b = upload_asset(checks, token_a, project_id, checks.prefix, 1, image_bytes)

        bootstrap = checks.request("GET", f"/api/v1/projects/{project_id}/sync-bootstrap", token=token_a)
        cursor = int(str(bootstrap["cursor"]))
        checks.check("bootstrap cursor captured", cursor >= 0)

        # Mark the point after asset creation.  The trigger is installed only
        # after this cursor so the final delta contains exactly the two writes.
        trigger_started = True
        install_sleep_trigger(database, function_name, trigger_name, device_a)

        operation_a = annotation_operation(image_a, str(uuid.uuid4()), "slow-a", 130)
        operation_b = annotation_operation(image_b, str(uuid.uuid4()), "fast-b", 350)
        executor = concurrent.futures.ThreadPoolExecutor(max_workers=2)
        future_a = executor.submit(checks.push, token_a, device_a, project_id, operation_a)

        slow_deadline = time.monotonic() + 10
        slow_seen = False
        while time.monotonic() < slow_deadline:
            if pg_sleep_active(database):
                slow_seen = True
                break
            if future_a.done():
                break
            time.sleep(0.1)
        checks.check("device A entered PgSleep in audit trigger", slow_seen)

        future_b = executor.submit(checks.push, token_b, device_b, project_id, operation_b)
        checks.check("device B request is pending while A sleeps", not future_b.done())

        # These reads intentionally happen while A is sleeping.  An event for
        # B must not be visible, and the repeatable-read bootstrap cursor must
        # still be the cursor captured before either annotation write.
        reads_started_while_sleeping = pg_sleep_active(database)
        mid_changes = checks.request(
            "GET",
            f"/api/v1/projects/{project_id}/changes?cursor={cursor}&limit=100",
            token=token_b,
        )["changes"]
        mid_bootstrap = checks.request(
            "GET", f"/api/v1/projects/{project_id}/sync-bootstrap", token=token_b
        )
        checks.check("mid-flight reads occurred while A slept", reads_started_while_sleeping)
        checks.check(
            "mid-flight changes hide B",
            not any(
                item.get("entityType") == "annotation" and item.get("entityId") == image_b
                for item in mid_changes
            ),
        )
        checks.check(
            "mid-flight bootstrap hides B",
            not any(item.get("imageId") == image_b for item in mid_bootstrap.get("annotations", [])),
        )
        checks.check("mid-flight bootstrap cursor is unchanged", int(str(mid_bootstrap["cursor"])) == cursor)

        # Keep polling independently of the HTTP reads so a prematurely
        # completed B request cannot be hidden by the 3-second trigger window.
        b_done_during_sleep = threading.Event()
        monitor_errors = []

        def monitor_b_until_sleep_ends() -> None:
            deadline = time.monotonic() + 10
            try:
                while time.monotonic() < deadline:
                    sleeping = pg_sleep_active(database)
                    if sleeping:
                        if future_b.done():
                            b_done_during_sleep.set()
                    else:
                        return
                    time.sleep(0.1)
                monitor_errors.append("PgSleep did not end within timeout")
            except Exception:
                monitor_errors.append("PgSleep monitor failed")

        monitor = threading.Thread(target=monitor_b_until_sleep_ends, daemon=True)
        monitor.start()
        monitor.join(timeout=12)
        if monitor.is_alive():
            monitor_errors.append("PgSleep monitor thread did not finish")
        if monitor_errors:
            raise RuntimeError(monitor_errors[0])
        checks.check("device B stayed pending until A committed", not b_done_during_sleep.is_set())

        result_a = future_a.result(timeout=60)
        result_b = future_b.result(timeout=60)
        checks.check("device A annotation applied", result_a.get("status") == "applied")
        checks.check("device B annotation applied", result_b.get("status") == "applied")

        final_changes = checks.request(
            "GET",
            f"/api/v1/projects/{project_id}/changes?cursor={cursor}&limit=100",
            token=token_b,
        )["changes"]
        annotation_events = [
            item
            for item in final_changes
            if item.get("entityType") == "annotation"
            and item.get("entityId") in {image_a, image_b}
        ]
        checks.check("intermediate cursor returns both annotation events", len(annotation_events) == 2)
        by_image = {item["entityId"]: item for item in annotation_events}
        checks.check("intermediate cursor has no lost annotation event", set(by_image) == {image_a, image_b})
        checks.check(
            "slow A sequence precedes B sequence",
            int(by_image[image_a]["sequence"]) < int(by_image[image_b]["sequence"]),
        )
    except Exception as error:  # report only the error type, never its payload
        failure = error
    finally:
        if executor is not None:
            executor.shutdown(wait=True)
        if trigger_started:
            try:
                remove_sleep_trigger(database, function_name, trigger_name)
            except Exception as error:
                cleanup_error = error

    if trigger_started:
        checks.results.append(
            {"name": "temporary PgSleep trigger removed", "passed": cleanup_error is None}
        )
    passed = failure is None and cleanup_error is None
    report = {
        "time": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "baseUrl": args.base_url,
        "database": args.database,
        "projectId": project_id,
        "seconds": round(time.monotonic() - started, 2),
        "checks": checks.results,
        "passed": passed,
    }
    if failure is not None:
        report["failureType"] = type(failure).__name__
    if cleanup_error is not None:
        report["cleanupFailureType"] = type(cleanup_error).__name__
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(
        json.dumps(
            {"passed": passed, "checks": len(checks.results), "report": str(args.report)},
            ensure_ascii=False,
        )
    )
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
