# Helm fixtures

Release payloads shaped like the JSON Helm writes, and the storage objects
that hold them, as `kubectl get -o json` shows them.

- `helm3-release.json`: a Helm 3 payload. Zero times are written out
  (`deleted: 0001-01-01T00:00:00Z`), hooks are present.
- `helm4-release.json`: a Helm 4 payload. Zero times are left out, and it
  carries `apply_method`, file `modtime` and `schemamodtime`.
- `helm3-secret.json` and `helm4-configmap.json`: those payloads stored the
  way Helm's secrets and configmap drivers store them. Helm's text is
  base64 of gzip of compact JSON, with `<`, `>` and `&` escaped as Go does.
  A Secret adds the API's own base64 on top.

To rebuild the storage objects after editing a payload:

```sh
python3 build.py
```
