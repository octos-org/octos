# UPCR-2026-076 — Admin Folder Sticky Ownership

Status: proposed (implemented for review)
Task: T-0202

## Decision record

This is an authorization behavior correction for existing `launch/resolve` and
`session/open`; it adds no OUP method, parameter, result field, notification,
capability, or error variant.

An implicit admin launch may follow only a folder sticky marker written by an
admin connection. It otherwise uses the existing default profile. Store-mtime
recency, legacy plain-id markers, unknown formats, and user-owned markers are
not admin-owned and are not migrated. An explicit admin `params.profile_id`
remains the supported cross-profile operation and is unchanged. Ordinary-user
resolution keeps its existing behavior.

New session/open writes use a versioned local marker containing `profile_id`
and the opener kind (`admin` or authenticated `user` plus owner profile id).
This is a local persistence format, not a wire DTO. Legacy markers remain
readable for ordinary-user behavior but fail closed for implicit admin use.
Admin and user writes may replace the shared marker, but an admin write never
rewrites another user's profile store.

The launch result schema and `session.workspace_cwd.v1` capability remain
unchanged. Existing tenant-Host and session/open scope checks are not widened.
