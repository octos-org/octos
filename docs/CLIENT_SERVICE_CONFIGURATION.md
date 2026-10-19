# BYOK service configuration and browser relay

Octos can manage caller-owned credentials for services that run directly in an
application's browser or native client. This REST surface stores configuration
and provides a restricted authenticated relay when browser CORS prevents direct
provider calls. It never grants or substitutes a platform key.
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
| `POST /api/my/client-services/sync` | Explicitly return the owner's configured client credentials |
| `POST /api/my/client-services/{capability}/relay` | Relay a client-initiated request using its own credential |

Capabilities are `asr`, `tts`, and `admission`. The compatibility field
`execution_target: "client"` denotes application-initiated service use; the
application chooses native direct calls or the browser relay. Providers are identifiers; parameters are non-secret string fields
(`auth_mode`, `app_id`, `resource_id`, `voice`, `model`, `region`, `language`).
Application prompts, questions, learning thresholds, microphone permission,
device readiness and transport selection belong to the application.

A PUT accepts `provider`, `enabled`, `params`, optional `key`, and
`expected_revision`. New records use `expected_revision: null`. Existing records
require their exact revision; mismatches return 409. Omitting `key` retains this
capability's existing credential. An empty key explicitly clears it and cannot
be saved with `enabled: true`. Changing provider requires an explicit new key;
credentials are never implicitly borrowed from another capability.

DELETE requires `{ "expected_revision": "..." }`. Responses use the envelope
`{ "profile_id": "...", "services": { ... } }`. Ordinary reads and mutations
return metadata only, without key previews. Only explicit authenticated POST
sync returns raw keys, including disabled services for explicit draft tests. The enabled flag is
preserved; ordinary runtime snapshots must exclude disabled services and may
not activate a service merely because a credential exists. All successful
responses carry `Cache-Control: no-store, max-age=0` and `Pragma: no-cache`.
Do not log or cache sync bodies. TLS is the host deployment's responsibility.

A saved configuration is not proof that a particular device can call its
provider. Clients synchronize at login/configuration lifecycle boundaries,
prepare in-memory snapshots, and use their supported provider transport. They must clear
identity-bound data and cancel requests on sign-out, discard late responses,
and distinguish missing configuration from CORS, permissions and network
errors. A missing credential must never trigger a host-funded fallback.

## Restricted browser relay

The relay accepts `{provider, key, params, payload}`. The credential is explicitly
supplied from the caller's synchronized snapshot or unsaved test draft; relay
execution never searches profiles, environment variables, parent keys, or
platform defaults for a replacement. Authentication and authoritative owner
scope are checked before sending any supplier request. The endpoint also works
for a draft that has not been saved, without mutating the account configuration.

Supported pairs are `asr/volcano`, `tts/volcano`, and `admission/typesafe`, mapped
to fixed official HTTPS endpoints in `client_services.rs`. Volcano uses only
`X-Api-Key`; legacy authentication is rejected. Clients cannot supply URLs,
forwarded headers, or another owner's credential reference. Octos auth tokens
and cookies are never forwarded. Application payloads remain opaque: no
learning-specific prompts or decisions are implemented here.

Requests have an 8 MiB route limit; upstream response bodies are capped at
12 MiB. The shared client disables redirects and retries, uses a five-second
connect timeout and a 30-second total timeout. At most 16 relay requests execute
concurrently. Responses are never cached. The envelope is
`{status, body, code}`, where `status` is the supplier HTTP status and `code` is
the validated numeric `X-Api-Status-Code`, if supplied. Supplier HTTP failures
return an empty body in the envelope, avoiding raw error/credential disclosure
and avoiding confusion between supplier 401 and Octos session authentication.
Connection and size failures produce generic 502 errors without supplier error
text. Clients cancel on identity change and ignore late results; cancellation
after supplier processing starts does not guarantee avoidance of supplier cost.
The relay has no persistent request/audio cache. Hosts must not log request
bodies or credentials and must allow the configured request size and timeout
through their reverse proxy.

## Storage and backup

Secrets live under `<registry root>/client-service-secrets/`, outside the profile
runtime workspace. Records use ChaCha20-Poly1305 with the owner as associated
data and atomic replacement. An OS file lock covers revision checks and
replacement across separate Octos instances sharing a registry; contention
returns 503 for an explicit retry. The directory is 0700; encrypted record files and
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
of another user's saved LLM key during provider probing. Additional tests cover
fixed relay destinations, new-Key-only headers, forbidden URL overrides,
response status preservation, sanitized errors, timeouts and oversized bodies. The tests use synthetic
keys and make no paid supplier calls. This API does not establish any provider's
browser compatibility or the application's end-to-end acceptance.
