---
name: skill-store
description: Browse, install, update, and manage skill packages from the registry.
version: 1.0.0
author: octos
always: true
---

# Skill Store

Use the `manage_skills` tool to browse, install, and remove skills. Do NOT use shell commands — skills must be managed via the tool.

## Browse Available Packages

```
manage_skills(action="search")
manage_skills(action="search", query="voice")
```

## Install a Package

```
manage_skills(action="install", repo="mofa-org/mofa-skills/mofa-fm")
manage_skills(action="install", repo="mofa-org/mofa-skills/mofa-fm", force=true)
```

Use `force=true` to overwrite an existing skill. Use `branch` for a specific version.

## List Installed Skills

```
manage_skills(action="list")
```

## Update a Skill

```
manage_skills(action="update", name="mofa-fm")
```

Reads the original source repo from `.source` and reinstalls with the latest version.

## Remove a Skill

```
manage_skills(action="remove", name="mofa-fm")
```

Tell the user what was installed and any requirements (e.g. API keys, binaries).
