# ADR-271: RuView as a Cognitum OAuth resource server

- **Status**: accepted
- **Date**: 2026-07-22
- **Deciders**: RuView maintainers
- **Tags**: auth, oauth, cognitum, security, sensing-server
- **Related**: ADR-055 (integrated sensing server), ADR-102 (edge module registry), ADR-066 (ESP32 seed pairing), cognitum-one/dashboard ADR-060 (OAuth scopes beyond `inference`), cognitum-one/meta-llm ADR-045 (Bearer at completions)

## Context

`/api/v1/*` on `wifi-densepose-sensing-server` is gated by `RUVIEW_API_TOKEN`
(`bearer_auth.rs`): a single shared secret, compared in constant time, with no
expiry, no rotation and no per-user attribution. `homecore-api` has a second,
unrelated scheme (`LongLivedTokenStore` over `HOMECORE_TOKENS`) whose own doc
comment describes it as "no expiry, no rotation, no per-user attribution yet".

That is proportionate for the ADR-055 topology — server bundled in the desktop
app, spawned as a child, localhost only. It is not proportionate for the other
deployment RuView actually has: a sensing server on a Pi or hub, reachable on a
LAN, potentially serving more than one person, exposing live presence, pose,
breathing and heart-rate data plus destructive operations (model training,
model delete, recording delete).

Cognitum operates a live OAuth 2.1 authorization server at `auth.cognitum.one`.
Users of RuView are already Cognitum account holders. The obvious question is
whether RuView can accept that identity instead of a shared string.

### The direction of the integration is the thing most likely to be misread

Every existing Cognitum OAuth integration in the org — meta-proxy, musica,
metaharness, the dashboard CLI — is an OAuth **client**: it obtains a token so
the application can *call* a Cognitum service (the completions plane).

RuView is the opposite. It makes **no authenticated calls to any Cognitum API**.
Its only outbound Cognitum dependency is the ADR-102 registry fetch, which is an
anonymous GET against a public GCS bucket. What RuView wants is to be a
**resource server**: a user signs in to their *own* RuView instance with their
Cognitum identity, and RuView verifies the token they present.

So the client-side prior art in the org, while useful for a future `ruview
login` command, addresses a plane RuView does not have. The only relevant
precedent is `meta-llm/src/auth/oauthBearer.ts` (ADR-045) — the org's sole
resource-server-side verifier of these tokens. It is TypeScript; **RuView is the
first Rust one.**

### Facts about the tokens, verified against a live production token

- **ES256 JWT**, signed by a single P-256 key published at
  `https://auth.cognitum.one/.well-known/jwks.json`.
- **15-minute lifetime**, with an opaque refresh token that **rotates with reuse
  detection** (presenting a spent one ends the session).
- Claims: `typ`, `sub`, `account_id`, `org_id`, `workspace_id`, `client_id`,
  `scope`, `family_id`, `jti`, `iat`, `exp`, `setup`, `workload`.
- **No `aud` claim.** No `/oauth/introspect`. No `/userinfo`. It is an OAuth 2.1
  authorization server, not an OpenID Provider, deliberately.

## Decision

Verify Cognitum access tokens **offline**, in a new `ruview-auth` crate, and
gate RuView's own API surface on the **scope** they carry.

### 1. Offline verification is a requirement, not an optimisation

RuView runs on Pi-class hardware that loses WAN, and there is no introspection
endpoint to call even when the network is up. Verification is therefore an
ES256 signature check against a `kid`-indexed JWKS cache. Two consequences we
accept explicitly:

- **Revocation window = token lifetime.** A compromised access token stays
  usable until `exp`. This is the same position meta-llm takes, for the same
  reason, and it is why §3 refuses long-lived credentials.
- **A JWKS refetch failure is survivable while a key set is cached.** A key that
  verified a minute ago has not stopped being valid because the network blipped;
  failing closed there would log every user out of their own sensing server
  whenever their internet wobbled. We fail closed in exactly one case: no key
  set has *ever* been fetched.

### 2. The accept-rule is ported from meta-llm, not designed

```
typ == "access"  AND  NOT setup  AND  NOT workload
AND account_id is a non-empty string
AND exp is in the future
AND iss matches the configured issuer verbatim
AND the scope required by the route is held
```

Divergence from `oauthBearer.ts` would be a bug rather than a preference: a
token meta-llm rejects must not be one RuView accepts. The algorithm is **fixed
to ES256 by our code** — the header's `alg` is only ever compared against that
allowlist, never used to select an algorithm.

### 3. Long-lived setup and workload credentials are refused outright

Identity also issues 365-day *setup* and machine *workload* credentials. Their
revocation state lives in identity's `oauth_setup_tokens` table. RuView — like
meta-llm — has no database and no way to check it, so accepting one would mean
honouring a credential that may already have been revoked. A 15-minute token
needs no revocation round-trip because it expires faster than revocation
propagates; a 365-day one does.

### 4. Scope is the capability boundary, because nothing else can be

Tokens carry no `aud`, so RuView cannot verify a token was minted *for* RuView.
`client_id` cannot substitute: clients borrow each other's registrations when
their own has not been deployed (musica ships `DEFAULT_CLIENT_ID = "meta-proxy"`).

This is not a defect to route around. Cross-product **identity** is intended —
one Cognitum account, every Cognitum product. Cross-product **capability** is
not, and scope is what carries the difference.

RuView registers two scopes (dashboard ADR-060, identity migration `0016`):

| Scope | Grants |
|---|---|
| `sensing:read` | sensing/pose streams, one-shot inference, reading model and recording metadata |
| `sensing:admin` | `POST /api/v1/train/*`, `DELETE /api/v1/models/{id}`, `DELETE /api/v1/recording/{id}` |

**No hierarchy**: `sensing:admin` does not imply `sensing:read`. Consent means
exactly what it said, and a token needing both must have consented to both.
`client_id` is retained on the principal for logging and attribution only —
never as an authorization input.

### 5. Additive and fail-closed, never a silent downgrade

`RUVIEW_API_TOKEN` and `HOMECORE_TOKENS` deployments keep working unchanged.
OAuth is opt-in; with it unconfigured, behaviour is byte-identical to today.
When OAuth *is* configured but unusable (JWKS unreachable at boot, required
scope not registered), the server must refuse to serve `/api/v1/*` rather than
fall through to an open or single-secret state.

### 6. `ureq`, and a transport seam

`wifi-densepose-sensing-server` deliberately chose `ureq` as "the smallest" HTTP
client. Introducing `reqwest` for a JWKS fetch would silently reverse that for
the whole dependency graph. The fetch sits behind a `JwksFetcher` trait — the
`ureq` implementation is a default-on feature, and a host may supply its own and
take no HTTP dependency at all.

## Consequences

- Requests become attributable: `sub`, `account_id`, `org_id`, `workspace_id`,
  `jti`. This closes the gap `homecore-api`'s `tokens.rs` has been deferring as
  "P3", using claims rather than new RuView machinery.
- Destructive operations can be separated from observation for the first time.
- **The 15-minute lifetime is the main operational cost.** A long-running client
  must refresh, and because refresh tokens rotate with reuse detection, a
  concurrent or naively retried refresh **ends the session** — single-flight is a
  correctness requirement, not an optimisation. This lands with the login flow,
  not this crate.
- Hosts without a battery-backed clock will fail `exp`/`iat` until NTP lands.
  The verifier reports that distinguishably so it is diagnosable rather than
  presenting as a generic 401.
- A new dependency, `jsonwebtoken` — the same crate, same major version, that
  identity itself uses to sign these tokens.

## Alternatives considered

**Keep `RUVIEW_API_TOKEN` only.** Zero work, and adequate for a single-user
localhost install. Rejected because it cannot express who did what, cannot be
revoked without a restart, and cannot separate "watch the stream" from "delete
the model" — all of which matter the moment the server is on a LAN.

**Exchange the OAuth token for a `cog_` key.** The pattern ADR-316 (meta-proxy)
and ADR-119 (metaharness) originally described. Rejected: it cannot work.
`/v1/me/keys` requires a *Firebase* ID token, not an OAuth token — meta-proxy
hit the resulting 401 in production, replaced the approach with Bearer-direct
under ADR-045, and deleted `mint.rs` as dead code.

**Call identity to introspect each token.** Rejected: no introspection endpoint
exists, and a network round-trip per request would be wrong for an edge sensing
server regardless.

**Wait for an `aud` claim before shipping.** Rejected as sequencing. `aud` would
touch every issued token and every verifier in the org; scope is additive and
independently correct. Tracked separately; adding `aud` later strengthens this
design rather than invalidating it.

**Use OAuth for the ESP32 device plane too.** Rejected as a category error.
Devices have no browser, no user and no human present; they already pair with a
`seed_token` bearer (ADR-066) plus a device-bound PSK. Cognitum OAuth is for the
API plane only.

## Implementation

`v2/crates/ruview-auth` — `jwks` (fetch, TTL cache, `kid` index, one
rate-limited forced refetch on an unknown `kid` so rotation is picked up without
waiting out the TTL), `verify` (the §2 accept-rule), `principal` (the verified
caller and its scopes).

41 tests pass under both `cargo test --no-default-features` (the repo's
canonical gate) and default features. The matrix signs real ES256 tokens with a
runtime-generated key — no key material is committed — and covers `alg:none`,
forged signatures, spliced payloads, unknown `kid`, expiry on both sides of the
leeway, issuer mismatch including a trailing-slash-only difference, `typ`
confusion, `setup`/`workload` smuggled onto a `typ=access` token, missing and
empty `account_id`, and scope escalation.

The load-bearing case is
`g2_a_genuinely_valid_token_from_another_cognitum_product_cannot_reach_the_sensing_surface`:
a correctly signed, unexpired, right-issuer, right-`typ` token bearing
`client_id=meta-proxy` and `scope=inference` is rejected. Nothing about its
signature or identity claims distinguishes it — only scope does. A naive
verifier accepts it, and an `inference` token becomes a key to someone's home
sensor.

**Not in this crate**: WebSocket authentication (ADR-272) and any outbound
Cognitum call.

### Amendment, 2026-07-22 — the login flow lives here after all, behind a feature

The paragraph above originally also excluded the login flow. That was written
to keep the sensing server lean, which is the right goal but not a reason to put
the code somewhere else: the Tauri desktop app needs the same flow, and a second
copy of a PKCE + rotating-refresh implementation is exactly the kind of
duplication that drifts apart and then disagrees about something subtle.

So `login` is a **non-default feature** of this crate. A server built with
default features gets the verifier and nothing more — no `reqwest`, no tokio
networking, no browser launcher. The CLI opts in with
`features = ["login"]`, and the desktop app can do the same.

Shipped as `wifi-densepose login` / `logout` / `whoami`. Two properties worth
restating because they are easy to get wrong:

* **Refresh is serialised and never retried.** Identity rotates refresh tokens
  with reuse detection, so a concurrent refresh looks like replay and a retry
  *is* replay — either revokes the session family. `Session::ensure_fresh`
  holds an async mutex across the network call, re-checks expiry after
  acquiring it, and persists the rotated token before returning it.
* **Least scope by default.** `login` requests `sensing:read`; `--admin` is an
  explicit escalation and requests both scopes, since there is no hierarchy
  server-side.
