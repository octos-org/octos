---
name: nas
description: Browse, read and search files on a Synology NAS (network attached storage). Triggers: NAS, Synology, DiskStation, network drive, shared folder, my files, my photos, NAS文件, 群晖, list files on the nas, read a file from the nas, search the nas.
version: 1.0.0
author: octos
always: false
---

# NAS (Synology FileStation)

Access files stored on a Synology NAS through the official FileStation Web API.
The skill logs in fresh on each call, performs the operation, then logs out.

Credentials are read from environment variables (nothing is hardcoded):

- `NAS_URL` (required): base URL of the NAS including scheme and port, e.g.
  `https://nas.example.com:5001` (HTTPS) or `http://192.168.1.10:5000` (HTTP).
  Do not include `/webapi`.
- `NAS_USER` (required): DSM account name.
- `NAS_PASS` (required): DSM account password.
- `NAS_VERIFY_TLS` (optional): set to `false` to accept self-signed HTTPS
  certificates, which are common on home NAS boxes. Default is to verify.

Limitations: accounts with 2-step verification (2FA) cannot be used by the Web
API — use a dedicated account with 2FA disabled.

## Tools

### nas_list_folder

List a folder's contents. With no `path` (or `/`) it lists the shared folders.

```json
{"path": "/photo"}
```

**Parameters:**
- `path` (optional): Folder path starting with a share name (e.g. `/photo`,
  `/video/2024`). Omit or pass `/` to list the available shared folders.

### nas_read_file

Read a text file and return its contents. Refuses files larger than ~1 MB or
files that are not valid UTF-8 text (binary files).

```json
{"path": "/photo/notes.txt"}
```

**Parameters:**
- `path` (required): Full file path starting with a share name.

### nas_search

Recursively search a folder for files whose name matches a glob pattern.

```json
{"folder": "/photo", "pattern": "*.jpg"}
```

**Parameters:**
- `folder` (required): Folder to search under, starting with a share name.
- `pattern` (required): Filename glob pattern, e.g. `*.jpg` or `report`.
