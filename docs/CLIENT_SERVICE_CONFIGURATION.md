# Client-executed service configuration

Octos can manage caller-owned credentials for services that run directly in an
application's browser or native client. This REST surface stores configuration;
it never calls ASR, TTS, or admission providers and never grants a platform key.
It is separate from OUP's server-executed LLM profile routes.

## Authentication and scope

These routes belong to the existing authenticated `/api/my/*` router. They use
its identity and authoritative host scope, ignore a caller-supplied profile
selection header, and read only the resolved profile's own record. They do not
inherit operator defaults, environment variables, parent credentials, or LLM
secrets. JSON bodies deny unknown fields; there is no arbitrary credential or
environment-variable export operation.

| Route | Meaning |
| --- | --- |
| `GET /api/my/client-services` | Metadata: provider, parameters, enabled, revision, has_credential, execution_target |
| `PUT /api/my/client-services/{capability}` | Save a capability with an expected revision |
| `DELETE /api/my/client-services/{capability}` | Delete with an expected revision |
| `POST /api/my/client-services/sync` | Explicitly return the owner's enabled client credentials |

Capabilities are `asr`, `tts`, and `admission`. Execution target is always
`client`. Providers are identifiers; parameters are non-secret string fields
(`auth_mode`, `app_id`, `resource_id`, `voice`, `model`, `region`, `language`).
Application prompts, questions, learning thresholds, microphone permission,
provider networking and device readiness belong to the application.

A PUT accepts `provider`, `enabled`, `params`, optional `key`, and
`expected_revision`. New records use `expected_revision: null`. Existing records
require their exact revision; mismatches return 409. Omitting `key` retains this
capability's existing credential. An empty key explicitly clears it and cannot
be saved with `enabled: true`. Changing provider requires an explicit new key;
credentials are never implicitly borrowed from another capability.

DELETE requires `{ "expected_revision": "..." }`. Responses use the envelope
`{ "profile_id": "...", "services": { ... } }`. Ordinary reads and mutations
return metadata only, without key previews. Only explicit authenticated POST
sync returns raw keys, and only for enabled client services. All successful
responses carry `Cache-Control: no-store, max-age=0` and `Pragma: no-cache`.
Do not log or cache sync bodies. TLS is the host deployment's responsibility.

A saved configuration is not proof that a particular device can call its
provider. Clients synchronize at login/configuration lifecycle boundaries,
prepare in-memory snapshots, and call providers directly. They must clear
identity-bound data and cancel requests on sign-out, discard late responses,
and distinguish missing configuration from CORS, permissions and network
errors. A missing credential must never trigger a host-funded fallback.

## Storage and backup

Secrets live under `<registry root>/client-service-secrets/`, outside the profile
runtime workspace. Records use ChaCha20-Poly1305 with the owner as associated
data and atomic replacement. The directory is 0700; encrypted record files and
`master.key` are created with 0600. A master key with unsafe permissions is
refused. Platforms without enforced Unix file permissions fail explicitly.
This is an encrypted file vault, not an OS Keychain integration. The master key
resides on the same host; server owner/root compromise can expose credentials.
Back up the master key and records together with owner-only permissions. Loss
of the master key makes the encrypted records unrecoverable. Disabling or
removing an Octos record cannot revoke a supplier key already copied to an
offline device; supplier-side revocation remains necessary.

## Validation

`cargo test -p octos-cli --no-default-features --features api client_services --lib`
checks encrypted storage, metadata redaction, cross-owner ciphertext rejection,
optimistic revisions, explicit owner synchronization, deletion, and rejection
of another user's saved LLM key during provider probing. The tests use synthetic
keys and make no paid supplier calls. This API does not establish any provider's
browser compatibility or the application's end-to-end acceptance.
