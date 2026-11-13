# Runbook — granting and revoking super_admin (D90)

Status: **by hand only.** Owner ruling D90 (spec 13): super_admin is the owner; super_admin registration stays OUT of the
platform. agent has no route, job or handler that writes a Keycloak role mapping (pinned by the tests
`no_route_grants_or_revokes_super_admin` and `keycloak_admin_exposes_no_role_mapping_write` in
`crates/agent-api/tests/roles.rs`). The API only READS the role: from the access token
(`resource_access.agent.roles`) on every request, and through the admin API in add-by-sub (a super_admin cannot be
added as a tenant member: 409 `super_admin_not_member`; a failed read is 502 `keycloak_error`).

What super_admin is: the client role `super_admin` on the client `agent` in the `portal` realm. A super_admin acts on
the platform routes and in any tenant (with the `X-Active-Tenant-Id` header) holding every tenant role (D88), narrowable
with view-as (D79).

## Prerequisites

- An admin login for the Keycloak **master** realm, given to you out of band. Never write it into a file, a commit, a
  PR, a ticket or a chat.
- `kcadm.sh` (ships in the Keycloak distribution under `bin/`; any 26.x client works against the live server).
- The person already has a portal-realm account (they signed in to any app once, or were invited).

Placeholders below: `<KC_ADMIN_USER>` (master admin username), `<EMAIL>` (the person), `<USER_ID>` (their portal
user id = the `sub` of their tokens).

## 1. Log in (master realm)

```bash
kcadm.sh config credentials --server https://auth.nonagentic.ai --realm master --user <KC_ADMIN_USER>
# kcadm prompts for the password; do not pass it with --password on a shared shell (history, ps).
```

The session is stored in `~/.keycloak/kcadm.config`; delete that file when you are done.

## 2. Find the user id

```bash
kcadm.sh get users -r portal -q email=<EMAIL> --fields id,username,email,enabled
# or by username
kcadm.sh get users -r portal -q username=<USERNAME> --exact true --fields id,username,email
```

`id` is `<USER_ID>`. Check that exactly one row comes back and that it is the right person.

## 3. Grant

```bash
kcadm.sh add-roles -r portal --uid <USER_ID> --cclientid agent --rolename super_admin
```

Before granting, check the person is not a member of a tenant (the API resolves a super_admin's home tenant from its
own `users` table; a super_admin should act in tenants by header, not as a member). As the new super_admin, or as
another super_admin: `GET https://agent.nonagentic.ai/api/me` — `tenant` should be `null` without a header.

## 4. Revoke

```bash
kcadm.sh remove-roles -r portal --uid <USER_ID> --cclientid agent --rolename super_admin
```

An access token issued before the revoke keeps the role until it expires (the realm's access-token lifespan); the
next token (refresh or new login) no longer has it. To cut it off at once, also end the person's sessions:

```bash
kcadm.sh create users/<USER_ID>/logout -r portal
```

## 5. Verify

Keycloak side:

```bash
kcadm.sh get-roles -r portal --uid <USER_ID> --cclientid agent --effective
# granted: a row with "name" : "super_admin"; revoked: no such row
```

Token side — the person signs out and in again at https://agent.nonagentic.ai (a token minted before the change does
not carry it), then, with their access token in `$TOKEN` (browser devtools → the `Authorization: Bearer …` header of
any `/api/` call; never paste it anywhere else):

```bash
# decode the JWT payload (base64url, padded for base64 -d) and print the agent client roles
python3 -c 'import base64,json,sys; p=sys.argv[1].split(".")[1]; p+="="*(-len(p)%4); print(json.loads(base64.urlsafe_b64decode(p))["resource_access"].get("agent",{}).get("roles"))' "$TOKEN"
# granted: ['super_admin', ...]   revoked: no 'super_admin'
```

API side:

```bash
curl -s -H "Authorization: Bearer $TOKEN" https://agent.nonagentic.ai/api/me | jq '{sub, super_admin, tenant}'
# granted: "super_admin": true    revoked: "super_admin": false
```

Then `unset TOKEN`, and `rm ~/.keycloak/kcadm.config`.

## Audit

Keycloak records the role-mapping change in its admin events (if admin events are enabled on the `portal` realm). agent
records nothing for a grant (it never sees one); the new super_admin's actions are audited as usual (platform log, and
`platform_actor` rows in a tenant's log).
