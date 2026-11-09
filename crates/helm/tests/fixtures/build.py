"""Builds the storage-object fixtures from the readable payloads.

Run from this folder: python3 build.py
"""

import base64
import gzip
import json


def helm_text(path):
    """Encodes a payload the way Helm's storage drivers do."""
    with open(path) as f:
        release = json.load(f)
    compact = json.dumps(release, separators=(",", ":"), ensure_ascii=False)
    # Go's json.Marshal escapes these for HTML safety; mirror it.
    for char, escaped in (("&", "\\u0026"), ("<", "\\u003c"), (">", "\\u003e")):
        compact = compact.replace(char, escaped)
    packed = gzip.compress(compact.encode(), compresslevel=9, mtime=0)
    return base64.b64encode(packed).decode()


def labels(release, revision, status, created, modified):
    return {
        "createdAt": str(created),
        "modifiedAt": str(modified),
        "name": release,
        "owner": "helm",
        "status": status,
        "version": str(revision),
    }


def write(path, obj):
    with open(path, "w") as f:
        json.dump(obj, f, indent=2)
        f.write("\n")


write(
    "helm3-secret.json",
    {
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "creationTimestamp": "2026-11-03T21:04:50Z",
            "labels": labels("ledger", 3, "deployed", 1793739890, 1793739910),
            "name": "sh.helm.release.v1.ledger.v3",
            "namespace": "payments",
            "resourceVersion": "184467",
            "uid": "3f0e4c1a-8a49-4d0e-9a7e-2f3f5d7c1b20",
        },
        "type": "helm.sh/release.v1",
        "data": {
            "release": base64.b64encode(helm_text("helm3-release.json").encode()).decode()
        },
    },
)

write(
    "helm4-configmap.json",
    {
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "creationTimestamp": "2026-11-09T00:02:17Z",
            "labels": labels("edge", 2, "failed", 1794182537, 1794182837),
            "name": "sh.helm.release.v1.edge.v2",
            "namespace": "ingress",
            "resourceVersion": "90211",
            "uid": "b7d1a3f2-5c0e-4f6b-8d2a-9e4c7f1a6d33",
        },
        "data": {"release": helm_text("helm4-release.json")},
    },
)
