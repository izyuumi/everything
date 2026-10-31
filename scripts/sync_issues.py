import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def command(arguments):
    return subprocess.run(arguments, check=True, capture_output=True, text=True, timeout=180).stdout


def fetch(endpoint):
    output = command(["gh", "api", "--paginate", endpoint, "--jq", ".[] | @json"])
    return [json.loads(line) for line in output.splitlines() if line]


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=False).encode()).hexdigest()


def write_checkpoint(path, value):
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as temporary:
        temporary_path = Path(temporary.name)
        try:
            json.dump(value, temporary, indent=2, sort_keys=True)
            temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        except BaseException:
            temporary_path.unlink(missing_ok=True)
            raise
    try:
        os.replace(temporary_path, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        temporary_path.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description="Refresh the authoritative Vorra issue backlog")
    parser.add_argument("--coordination", type=Path)
    parser.add_argument("--acknowledge", type=int, nargs="*", default=[])
    arguments = parser.parse_args()
    common = Path(command(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"]).strip())
    directory = common / "vorra-orchestration"
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (directory / "sync.lock").open("a") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        synchronize(arguments, directory)


def synchronize(arguments, directory):
    path = directory / "checkpoint.json"
    previous = json.loads(path.read_text()) if path.exists() else {}
    repository = "repos/izyuumi/everything"
    issues = [issue for issue in fetch(repository + "/issues?state=all&per_page=100&sort=updated&direction=desc") if "pull_request" not in issue]
    comments = fetch(repository + "/issues/comments?per_page=100&sort=updated&direction=desc")
    snapshots = {}
    for issue in issues:
        number = str(issue["number"])
        entries = [comment for comment in comments if comment["issue_url"].endswith("/" + number)]
        snapshots[number] = {
            "number": issue["number"],
            "title": issue["title"],
            "body": issue.get("body") or "",
            "state": issue["state"],
            "updated_at": issue["updated_at"],
            "url": issue["html_url"],
            "requirement_digest": digest([issue["title"], issue.get("body") or ""]),
            "comments": [{"id": entry["id"], "body": entry["body"], "updated_at": entry["updated_at"]} for entry in sorted(entries, key=lambda entry: entry["id"])],
        }
    changes = []
    for number, snapshot in snapshots.items():
        original = previous.get("issues", {}).get(number)
        kinds = []
        if original is None:
            kinds.append("new")
        else:
            if original["requirement_digest"] != snapshot["requirement_digest"]:
                kinds.append("requirements")
            if original["state"] != snapshot["state"]:
                kinds.append("reopened" if snapshot["state"] == "open" else "closed")
            if original.get("comments") != snapshot["comments"]:
                kinds.append("comments")
        if kinds:
            changes.append({"number": int(number), "changes": kinds, "title": snapshot["title"], "state": snapshot["state"]})
    coordination = previous.get("coordination", {})
    if arguments.coordination:
        coordination = json.loads(arguments.coordination.read_text())
        for number, assignment in coordination.get("assignments", {}).items():
            if number in snapshots:
                original = previous.get("coordination", {}).get("assignments", {}).get(number, {})
                assignment.setdefault("assigned_requirement_digest", original.get("assigned_requirement_digest", snapshots[number]["requirement_digest"]))
                assignment.setdefault("assigned_comment_digest", original.get("assigned_comment_digest", digest(snapshots[number]["comments"])))
                for field, assigned_digest, current_digest, value in [
                    ("assigned_requirements", "assigned_requirement_digest", snapshots[number]["requirement_digest"], {key: snapshots[number][key] for key in ["title", "body"]}),
                    ("assigned_comments", "assigned_comment_digest", digest(snapshots[number]["comments"]), snapshots[number]["comments"]),
                ]:
                    if field not in assignment:
                        if assignment[assigned_digest] == current_digest:
                            assignment[field] = value
                        elif field in original:
                            assignment[field] = original[field]
    pending = {str(change["number"]): change for change in previous.get("pending_changes", previous.get("changes", []))}
    for number in arguments.acknowledge:
        pending.pop(str(number), None)
    for change in changes:
        number = str(change["number"])
        original = pending.get(number)
        if original:
            change = dict(change, changes=sorted(set(original["changes"]) | set(change["changes"])))
        pending[number] = change
    now = datetime.datetime.now(datetime.timezone.utc)
    checkpoint = {
        "schema_version": 1,
        "repository": "izyuumi/everything",
        "last_successful_sync": now.isoformat(),
        "next_sync_due": (now + datetime.timedelta(minutes=5)).isoformat(),
        "issues": snapshots,
        "changes": changes,
        "pending_changes": sorted(pending.values(), key=lambda change: change["number"]),
        "coordination": coordination,
    }
    write_checkpoint(path, checkpoint)
    print(json.dumps({"checkpoint": str(path), "last_successful_sync": checkpoint["last_successful_sync"], "next_sync_due": checkpoint["next_sync_due"], "open_issues": sorted(int(number) for number, issue in snapshots.items() if issue["state"] == "open"), "changes": changes, "pending_changes": checkpoint["pending_changes"]}, indent=2))


if __name__ == "__main__":
    main()
