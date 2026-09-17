# API access control — roles, permissions, and per-person restrictions

**Goal:** one permission model driving three surfaces — the **desktop menu**,
the **mobile app**, and the **API endpoints** — with the roles created and
assigned by a logged-in administrator rather than by a redeploy.

Sections 1–11 are the gate: who may call which endpoint, and how it is
enforced. Sections 12–15 are what the clients render on top of it: the menu and
feature list each surface gets, from one call, from the same tables.

**Status (2026-09-16): the SQL layer is built and applied to `lms_dev`; the
service does not read it yet, and nothing is enforced.**

| Part | State |
|---|---|
| `sql/006_access_control.sql` — 7 tables, the ERP row copy, both functions, 14 endpoint rules | **built**, applied to `lms_dev`, all rules `enforce = false` |
| `docs/attendance_schema.sql` §6 — the same objects (empty), for a fresh deployment | **in step** with the migration; the rows and rules stay in `sql/006` |
| Keeping the copy in step with the ERP's admin screens | **decided: option (c)** — administered here, `POST /ext-api/access/*` + the Access Roles screen ([§17](#17--administering-roles-here)) |
| `tests/access_control_sql.rs` — 20 DB-backed tests | **built**, passing |
| Middleware check (§6) | **built**, running in **audit mode** — asks on every request, refuses nothing |
| `force_reassign` permission point (§5) | **built** — the one check a path cannot express |
| `/ext-api/me/access` handler (§14) | **built** — `src/routes/access.rs`, with ETag/304 |
| `app_users.role_id` backfill (§7.4), `WOW_ACCEPT_DU_TOKEN=false` (§7.1) | not done — **both are preconditions for enforcing** |

So today every `/ext-api` request is evaluated and the verdict recorded, and no
request's outcome changes. Turning that into enforcement is two switches and the
preconditions in §7 — the [checklist](#implementation-checklist) has the rest in
order.

Related: [`ARCHITECTURE.md`](ARCHITECTURE.md) (how the two services share one
database) · [`DEPLOYMENT.md`](DEPLOYMENT.md#opening-the-ip-allow-list) (the IP allow-list) ·
[`nfc_card.md`](nfc_card.md#auth) (the auth layers as they stand today).

---

## 1 · The gap, in one sentence

This service **authenticates** callers and does not **authorize** them: every
caller who passes the gate can call every endpoint behind it.

Today a request to any `/ext-api/**` path passes three checks:

| # | Layer | Answers | Where |
|---|---|---|---|
| 1 | `X-App-Id` + `X-App-Password` | "is this a known *client application*?" | `ext_auth_middleware.rs`, values from `.env` |
| 2 | IP allow-list | "is this endpoint reachable from that address?" | `attendance.ext_api_allowed_ips`, per exact path |
| 3 | `Authorization: Bearer` | "is this a real *person*, and who?" | `require_token_caller`, in each handler |

Layer 3 already resolves a person — the token's `sub`, e.g. `2020111007` — and
then **does nothing with it** except write it to the step log. Nothing asks
whether *that* person may call *this* endpoint. So the card desk's token opens
the reassignment endpoint, the reports, and the API logs alike.

Two consequences worth stating plainly, because they are what this work fixes:

- **`save_card_info` with `force_reassign=true` moves a card off another
  student.** Any valid token can do it, from any IP (those rows carry the `'*'`
  wildcard — see [`nfc_card.md`](nfc_card.md#auth)).
- **`logs/login` and `logs/attendance` return step logs** that carry usernames,
  client IPs and DU user objects. Same story.

---

## 2 · What the database already has

**Do not design a role model. One exists**, put there by duerp-api for the ERP
SPA — and its user key is *exactly* the identity this service's token carries.
This service now runs **its own copy of that model**, in `attendance`
([§2.1](#21--this-service-owns-a-copy)); the shapes and the row ids below are
the ERP's, and the tables are ours.

```
attendance.app_users                 16 rows   ← person_id = the token's `sub`
  id, person_id (UNIQUE), username, du_base_role, role_id → roles(id), status

attendance.roles                      8 rows   admin, superadmin, faculty,
  id, key, name, is_system                     officer, staff, student, …

attendance.resources                 18 rows   the permission keys: 12 the ERP
  id, key, name, category                      had, plus this service's six
                                               ('nfc.card.register', …)

attendance.role_permissions          37 rows   role_id × resource_id
attendance.user_permission_overrides  4 rows   user_id × resource_id × effect
                                               ('grant' | 'deny') — per PERSON
attendance.menu_items                 7 rows   the nav tree, gated by
  … + platforms text[]                         resource_id, scoped by platform
```

Every row above was copied from the `ictcell` table of the same name, **ids
preserved** — `user_permission_overrides` references `app_users.id`, so
renumbering would have re-pointed four overrides at the wrong people.

Three things follow, and they are the reason this design is small:

1. **The join from a token to a role already works.** `token.sub` →
   `app_users.person_id` (UNIQUE) → `role_id` → `role_permissions`. No new
   identity table, no mapping to invent.
2. **"Allow one specific person" is already modelled** —
   `user_permission_overrides` with `effect = 'grant'`, which is precisely the
   "allow for some specific person" in the request. Four rows already use it.
3. **Role creation from a logged-in user already has screens.**
   `ictcell.menu_items` carries `/access-roles` ("Access Roles", resource
   `admin.roles.manage`) and `/role_list` ("Users", `admin.users.manage`). The
   ERP admin UI writes these tables; this service does not need to grow a
   role-management API at all.

### 2.1 · This service owns a copy

The tables are **in `attendance`**, created and filled by
`sql/006_access_control.sql`. The `ictcell` originals are untouched and
duerp-api keeps using them.

That is the same move `sql/005` made for the ext-api gate, for the same reason:
a change made for the ERP must not be able to take this service's gate down.
It also removes a negotiation — `menu_items.platforms` ([§13](#13--platform-scoping-for-menus))
needed another team's agreement while the table was theirs, and is simply a
column here.

> **The cost, and it is not small.** The ERP's admin screens (`/access-roles`,
> `/role_list`) write `ictcell`. **A role assigned there no longer reaches this
> service.** One of three things has to happen, and until one does, treat
> `attendance.app_users.role_id` as a thing you edit by hand:
>
> | | What | Trade-off |
> |---|---|---|
> | **a** | Point the ERP's admin screens at `attendance.*` — same database, a schema name in their queries | One source of truth again, owned here. **Recommended**, and needs duerp-api to make a small change |
> | **b** | Re-sync on a schedule (`sql/006` §10 has the statement) | No coordination, but stale between runs — a revoked role keeps working until the next sync |
> | **c** | Administer roles from this service | Needs admin endpoints this service does not have |

A lighter variant worth knowing about if the drift bites: keep `roles`,
`resources`, `role_permissions` and `menu_items` here (the *policy*, which
changes rarely) and read *identity* through a view over `ictcell.app_users`
(the table that changes weekly), the way `attendance.employees` is a view over
`ictcell.employees`. The catch is that `user_permission_overrides.user_id` can
no longer be a foreign key to a view — it would have to key on `person_id`.

### What is missing

Only one link in the chain: **nothing says which permission an ext-api endpoint
requires.** `resources` has ERP screen keys (`attendance.report.view`,
`course.home.view`); it has no notion of
`POST /ext-api/nfc-card/save_card_info`.

---

## 3 · The design

Everything the decision needs lives in `attendance`, owned by this service:

```
identity + roles        →  attendance (copied from the ERP — §2.1)
endpoint → permission   →  attendance (owned by THIS service)  ← the new part
enforcement             →  ExtAuthMiddleware (this service)    ← the new part
```

The request flow becomes:

```
  X-App-Id / X-App-Password        1 · is this a known client app?      401
            ↓
  attendance.ext_api_allowed_ips   2 · is this path open to that IP?    403
            ↓
  Authorization: Bearer <jwt>      3 · which person is this?            401
            ↓
  attendance.ext_api_can_call()    4 · may THAT person call THIS path?  403  ← new
            ↓
  handler
```

### Why it all lives in `attendance`

Same reason the IP allow-list was moved there (see the header of
`sql/005_ext_api_attendance_schema.sql`): `ictcell.ext_api_allowed_ips` was
duerp-api's table, and a change made for the ERP took this service's whole
ext-api surface down with it. An endpoint→permission row is a statement about
*this service's* routes; it belongs to whoever deploys them.

The roles moved for the same reason, and at a real price. **Two systems that
can disagree about who may do what is the worst failure mode available**, and a
copy makes it possible: change a role in the ERP's admin screen and this
service will not notice. That is not a reason the copy is wrong — it is the
reason [§2.1](#21--this-service-owns-a-copy)'s question has to be answered
rather than left. Pick (a), (b) or (c); do not leave two copies drifting.

### Why not put roles in the JWT

Tempting (no per-request lookup) and wrong here, for three reasons:

| | |
|---|---|
| **duerp-api mints tokens too** | Both services share `JWT_SECRET` and accept each other's tokens — that is what makes the split invisible to clients (`utils/jwt.rs`). A token from duerp-api would carry no role claim, so enforcement would have to fall back to a lookup anyway. |
| **Tokens live ~30 days** | `create_jwt` sets `exp` 730 hours out. A role revoked today would keep working for a month. A server-side lookup revokes instantly — including `app_users.status <> 'active'`, which becomes a kill switch. |
| **Claims are the token's, not the truth's** | A stale claim is indistinguishable from a current one. The table is the truth; read the table. |

The cost is one indexed query per gated request, on a connection pool that is
already open. Measure it before optimising it; if it ever matters, cache per
`(person_id, endpoint)` for a few seconds.

---

## 4 · New objects

**These are built.** `sql/006_access_control.sql` contains everything in this
section and in [§14.1](#141--the-function), is idempotent, and is applied to
`lms_dev`. The code below is the design narrative; the file is the source of
truth, and it carries the same comments.

### 4.1 · Endpoint → permission map

```sql
CREATE TABLE IF NOT EXISTS attendance.ext_api_endpoint_permissions (
    id            bigserial   PRIMARY KEY,
    -- FULL request path, matched exactly, like ext_api_allowed_ips.
    -- e.g. '/ext-api/nfc-card/save_card_info'
    endpoint      varchar(255) NOT NULL,
    -- NULL = "any authenticated caller", i.e. today's behaviour, written down.
    -- Otherwise a key from attendance.resources.
    resource_key  text,
    -- false = AUDIT MODE: evaluate, log the verdict, let the request through.
    -- This is how the rollout happens without locking anyone out (§7).
    enforce       boolean     NOT NULL DEFAULT false,
    is_active     boolean     NOT NULL DEFAULT true,
    note          text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ext_api_endpoint_permissions_uq UNIQUE (endpoint)
);
```

**A path with no row is denied once enforcement is on.** Fail closed, and the
same rule the IP allow-list already trains everyone on: a new endpoint is
unreachable until someone says who may reach it. The alternative — unknown
paths open by default — means the next endpoint added ships unprotected and
nobody notices.

### 4.2 · The decision function

One function, so the rule holds for anything that asks — the middleware, a
psql prompt, a future admin screen.

> Both the table and the function below were run against `lms_dev` inside a
> transaction that was rolled back, and every verdict quoted in this document
> is copied from that run — they compile and behave as written. Nothing was
> left behind: none of these objects exist yet.

```sql
CREATE OR REPLACE FUNCTION attendance.ext_api_can_call(
    p_person_id bigint,
    p_endpoint  varchar
) RETURNS jsonb
LANGUAGE plpgsql STABLE
AS $function$
DECLARE
    v_rule     RECORD;
    v_user     RECORD;
    v_resource integer;
    v_effect   text;
    v_allowed  boolean;
BEGIN
    SELECT * INTO v_rule
      FROM attendance.ext_api_endpoint_permissions
     WHERE endpoint = p_endpoint
       AND is_active;

    -- No rule, or a deactivated one. Denied — but `enforce` is reported as
    -- true so the middleware refuses rather than waving it through: an
    -- unconfigured path is exactly the case this must not be lenient about.
    IF NOT FOUND THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', true,
            'reason', 'no_rule', 'endpoint', p_endpoint);
    END IF;

    IF v_rule.resource_key IS NULL THEN
        RETURN jsonb_build_object('allowed', true, 'enforce', v_rule.enforce,
            'reason', 'open_to_authenticated');
    END IF;

    SELECT id INTO v_resource FROM attendance.resources WHERE key = v_rule.resource_key;

    -- A typo'd key must never read as "allowed". It is a config error, and the
    -- reason code says so rather than blaming the caller.
    IF v_resource IS NULL THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', v_rule.enforce,
            'reason', 'unknown_resource', 'resource_key', v_rule.resource_key);
    END IF;

    SELECT au.id, au.role_id, au.status, r.key AS role_key
      INTO v_user
      FROM attendance.app_users au
      LEFT JOIN attendance.roles r ON r.id = au.role_id
     WHERE au.person_id = p_person_id;

    IF NOT FOUND THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', v_rule.enforce,
            'reason', 'no_account', 'person_id', p_person_id);
    END IF;

    -- The kill switch. Tokens live ~730 hours and there is no revocation list,
    -- so this is what makes "stop that person now" possible at all: the next
    -- request is refused whatever their token says.
    IF coalesce(v_user.status, '') <> 'active' THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', v_rule.enforce,
            'reason', 'account_inactive', 'status', v_user.status);
    END IF;

    -- A per-person override beats the role, either way. "Everyone in this role
    -- except her" has to be expressible, and it only is if a person-level deny
    -- outranks a role-level grant. At most one row can match: the table's PK is
    -- (user_id, resource_id), so there is no grant-vs-deny tie to break here.
    SELECT effect INTO v_effect
      FROM attendance.user_permission_overrides
     WHERE user_id = v_user.id AND resource_id = v_resource;

    IF v_effect = 'deny' THEN
        RETURN jsonb_build_object('allowed', false, 'enforce', v_rule.enforce,
            'reason', 'denied_by_override');
    ELSIF v_effect = 'grant' THEN
        RETURN jsonb_build_object('allowed', true, 'enforce', v_rule.enforce,
            'reason', 'granted_by_override');
    END IF;

    v_allowed := EXISTS (
        SELECT 1 FROM attendance.role_permissions rp
         WHERE rp.role_id = v_user.role_id
           AND rp.resource_id = v_resource);

    RETURN jsonb_build_object('allowed', v_allowed, 'enforce', v_rule.enforce,
        'reason', CASE WHEN v_allowed THEN 'granted_by_role' ELSE 'not_in_role' END,
        'role', v_user.role_key,
        'resource_key', v_rule.resource_key);
END;
$function$;
```

It returns a verdict object rather than a boolean on purpose: the caller needs
`enforce` (audit mode vs. deny) and `reason` (what to log, and what to tell an
administrator when someone is locked out).

### 4.3 · Resources for this service's endpoints

`attendance.resources` is the shared permission vocabulary. Adding keys is safe —
the ERP only shows what its menus reference — but **use a distinct prefix**, so
it is obvious which system a key belongs to:

```sql
INSERT INTO attendance.resources (key, name, category) VALUES
  ('nfc.card.read',        'NFC · Read card info',        'Attendance Devices'),
  ('nfc.card.register',    'NFC · Register / update card','Attendance Devices'),
  ('nfc.card.reassign',    'NFC · Reassign a card',       'Attendance Devices'),
  ('nfc.card.status',      'NFC · Check registration',    'Attendance Devices'),
  ('face.enroll',          'Face · Enroll',               'Attendance Devices'),
  ('face.verify',          'Face · Verify / check-in',    'Attendance Devices'),
  ('attendance.report.view','Attendance · Reports',       'Attendance'),  -- exists
  ('admin.logs.view',      'Admin · API & step logs',     'Administration') -- exists
ON CONFLICT (key) DO NOTHING;
```

`attendance.report.view` and `admin.logs.view` already exist and already have
role grants — reuse them rather than minting near-duplicates.

---

## 5 · Suggested endpoint map

A starting point, not a decision: **confirm each line with whoever owns the
process** before enforcing it. `enforce=false` everywhere until §7 says
otherwise.

| Endpoint | Resource | Who should hold it |
|---|---|---|
| `/ext-api/wow-attendance/verify` | `face.verify` | readers/turnstiles, staff |
| `/ext-api/wow-attendance/enroll` | `face.enroll` | enrolment desk |
| `/ext-api/wow-attendance/check`, `/enrolled` | `face.enroll` | enrolment desk |
| `/ext-api/wow-attendance/reports/by-date`, `/by-person` | `attendance.report.view` | admin, management, faculty |
| `/ext-api/wow-attendance/mapping-save` | `attendance.settings` | admin only |
| `/ext-api/wow-attendance/ssl_image_verfiy` | `face.verify` | readers |
| `/ext-api/wow-attendance/logs/login` | `admin.logs.view` | **admin only — PII** |
| `/ext-api/wow-attendance/logs/attendance` | `admin.logs.view` | **admin only — PII** |
| `/ext-api/nfc-card/get_card_info` | `nfc.card.read` | readers/turnstiles |
| `/ext-api/nfc-card/checking_card_reg_status` | `nfc.card.status` | card desk |
| `/ext-api/nfc-card/save_card_info` | `nfc.card.register` | card desk |
| `/ext-api/me/access` | — | **any authenticated caller** — it returns only the caller's own access (§14) |
| `/login` | — | **not gated** (it is what issues the token) |

### `force_reassign` is a second decision inside one endpoint — **built**

Taking a card off another student is the most dangerous thing this service
does, and it is a *field* in `save_card_info`, not a path — so the endpoint map
cannot express it. One person in a hundred needs it; everyone else registering
cards does not. Splitting it is the difference between "a desk clerk can fix a
mis-scan" and "a desk clerk can silently strip any student's card".

It is checked in the handler (`src/routes/nfc_card.rs`), against a row whose
"endpoint" is **not a path**:

```
/ext-api/nfc-card/save_card_info#force_reassign  →  nfc.card.reassign
```

No URL ends in `#force_reassign`, so the middleware — which matches on
`req.path()` — can never hit that row; only the handler that names it asks
about it. It gets its own `enforce` dial as a result, so reassignment can be
locked down before, or after, the endpoint around it.

Three properties worth knowing:

- **It runs before the face-match gate.** That gate is an HTTP round trip to
  another service; a caller who may not reassign should not wait on it, or
  occupy it. The uploaded files are already on disk by then — the multipart
  body has been read — and stay there, referenced by nothing, like every other
  rejected upload here.
- **It obeys the same two switches** as the middleware, through the same
  function (`access_decision`). In `audit` mode the would-be refusal is
  recorded and the save proceeds, so turning the permission on cannot surprise
  a card desk that has been reassigning all along.
- **The refusal uses this module's envelope**, not the middleware's:
  `{"status":"error","code":"forbidden","message":"You may register a card, but
  not reassign one already issued to another student"}` with a `403`. A save
  with no `force_reassign` never asks the question at all.

> **The handler's string and the seeded row must match exactly.** A typo on
> either side reads as `no_rule`, which fails closed — every reassignment
> refused, once enforced. `tests/access_control_sql.rs` pins the row;
> `REASSIGN_POINT` in the handler is the other half.

---

## 6 · Enforcing it in the service

**Built, and running in audit mode.** `src/middleware/ext_auth_middleware.rs`
asks the question on every `/ext-api` request and refuses nothing until two
separate switches say so.

### Where

**In `ExtAuthMiddleware`**, immediately after the IP check — one place, applied
to every route in the scope, impossible to forget when adding an endpoint. The
per-handler alternative is one line per handler and one forgotten line away
from an unprotected route.

The middleware reads the token itself (`token_identity`, a decode with no I/O)
rather than taking the person id from the handler, which has not run yet.
Handlers keep their own `require_token_caller` untouched — it produces the
step-log line and the person id they already use, and a second decode is worth
more than the coupling of passing state through request extensions.

**A missing or unreadable token is not rejected here.** It is simply "no
identity", and the endpoint's rule decides what that is worth — `no_account` on
a gated path, `open_to_authenticated` on an open one. This layer only ever
*adds* a refusal; every 401 still comes from the handler, exactly as before.

### The two switches

Both must agree before any request is refused, which is why the default state
refuses nothing twice over:

| | Where | Says | Default |
|---|---|---|---|
| `EXT_ACCESS_CONTROL` | env var, read per request | whether this layer may refuse **at all** — the panic button | `audit` |
| `enforce` | a column on each rule row | **which endpoints** refuse — the rollout dial | `false` everywhere |

`EXT_ACCESS_CONTROL` takes three values:

- **`off`** — do not even ask. No query, no log line, no cost; the service
  behaves exactly as it did before this layer existed.
- **`audit`** (default) — ask, record, and serve every request whatever the
  answer. **This is where the service is today.**
- **`on`** / `enforce` — ask, record, and refuse when the endpoint's own rule
  says `enforce`.

Anything unrecognised — including a typo — is `audit`: a misspelt value must
not start refusing requests, and must not silently stop collecting the evidence
either.

`EXT_ACCESS_FAIL_CLOSED` (default `false`) covers the other case: the check
itself *failing* — an unapplied migration, a database that is down. That is not
a verdict and does not read as one, so by default the request is served and the
failure is logged. **That default is a deployment property**: a binary shipped
ahead of `sql/006_access_control.sql` behaves exactly as today rather than
403ing every ext-api call. Turn it on once everything is enforcing and "the
check is unavailable" should stop traffic.

### What it writes

Every would-be refusal produces **one log line**, greppable on `[access]`:

```
[access] /ext-api/nfc-card/get_card_info person=2020111007 token=ours reason=not_in_role enforce=false refused=false
[access] /ext-api/nfc-card/get_card_info person=- token=none reason=no_account enforce=false refused=false
```

...and **one row** in `attendance.ext_api_access_audit`, folded by
`(person_id, endpoint, reason)` with a `hits` counter. Folded rather than
appended because during the audit phase *every* request is a denial — nobody
has a role yet — and a row each would be a write per tap for a report nobody
reads line by line. The write is detached (`actix_web::rt::spawn`): the audit
trail is worth a write, never worth delaying a request the verdict already
allowed.

`token_source` on that row is what makes [§7.1](#71--unverifiable-legacy-tokens--the-blocker)
measurable: `legacy` means the caller arrived on a DU token whose signature
**cannot be verified**, so the identity recorded is unproven. Those have to
reach zero before enforcement means anything.

### Response shape

A `403` from this layer sits alongside the two the middleware already returns
(`Invalid App ID or Password`, `IP address not allowed…`), so it keeps their
`{"error": …}` shape rather than either handler envelope:

```json
{ "error": "You do not have permission to use this endpoint", "code": "forbidden" }
```

> **The reason is deliberately not in the response.** `not_in_role` and
> `no_rule` send an operator to completely different places, and both are in
> the log line and the audit row — but telling a caller *why* they were refused
> maps out the permission model for them. Look it up in the audit table.

### Cost

One `SELECT attendance.ext_api_can_call(...)` per `/ext-api` request, on a pool
that is already open, plus a detached upsert on the denial path. `off` removes
even that. If it ever shows up in a latency profile, cache per
`(person_id, endpoint)` for a few seconds — but measure first.

---

## 7 · Preconditions — read this before enabling anything

Authorization is only as good as the identity it rests on. Two things in this
service currently undercut it.

### 7.1 · Unverifiable legacy tokens — **the blocker**

`user_from_token` accepts DU's own Laravel tokens when `WOW_ACCEPT_DU_TOKEN` is
on, and says so itself:

> *"Signed with DU's secret, which this service does not hold, so its signature
> CANNOT be verified here — a well-formed forged token passes."*

A caller who can choose their own `sub` can choose the `sub` of an admin. **Any
role check on top of that is decoration.** So:

```bash
WOW_ACCEPT_DU_TOKEN=false     # in .env, before enforcement goes on
```

The step logs name every caller still arriving on a legacy token — chase those
down first:

```bash
grep -rl "legacy DU token" uploads/log | head
```

If some client genuinely cannot migrate yet, the fallback is to enforce roles
**only** for `TokenSource::Ours` and refuse legacy tokens on gated endpoints
specifically. That is strictly more complexity than turning the flag off; treat
it as a migration crutch with a date on it.

### 7.2 · The app credentials identify an app, not a person

`X-App-Id` / `X-App-Password` are shared by every ext-api client and live in
`.env`. They are a *client* credential — they cannot distinguish two people
using the same reader. All per-person decisions come from the bearer token, and
the app credentials keep doing what they do now: keeping strangers off the
surface entirely.

### 7.3 · Token lifetime is ~30 days

`create_jwt` issues `exp` 730 hours out, and there is no revocation list. The
server-side lookup is what makes revocation immediate: clear `role_id`, add a
`deny` override, or set `app_users.status = 'inactive'` and the next request is
refused whatever the token says. **This is the main practical argument for the
table over a claim** — see [§3](#why-not-put-roles-in-the-jwt).

### 7.4 · The data is not ready yet

Two facts from the live `lms_dev` rows. The first has been half-fixed; the
second is untouched, and between them most people are still denied:

- **`app_users.role_id` was `NULL` for all 16 accounts.** The two whose
  `du_base_role` is `admin` now hold the `admin` role (2026-09-17) — so the
  access-management menus are visible to somebody, which is what makes the
  system administrable at all. **The other 14 still have no role**, including
  every card desk and every reader.
- **`du_base_role` is free text and inconsistent** — `admin`, `Officer`,
  `faculty`, `Guest_Teacher`, `ThirdGeneral`, `ExamControllerUser`, `student` —
  with case that does not match `roles.key`. Only the `admin` spelling happened
  to match; the rest need a decision per role, not a bulk UPDATE.

Pick one before enforcing:

```sql
-- Option A (recommended): fill role_id once, from du_base_role, and let the
-- ERP's Users screen own it from then on. Anything unmatched stays NULL and is
-- handled by hand — do NOT map "unknown" onto a default role.
UPDATE attendance.app_users au
   SET role_id = r.id
  FROM attendance.roles r
 WHERE r.key = lower(au.du_base_role)
   AND au.role_id IS NULL;

SELECT du_base_role, count(*)               -- what did not match
  FROM attendance.app_users WHERE role_id IS NULL GROUP BY 1 ORDER BY 2 DESC;
```

Option B is to teach `ext_api_can_call` to fall back to
`lower(du_base_role) = roles.key` when `role_id IS NULL`. It avoids the
migration, at the cost of two sources of truth for one fact — and the free-text
column wins silently whenever someone forgets to set `role_id`. Prefer A.

> **Check your own account before you flip the switch.** The account this
> service is developed from (`person_id = 2020111007`) is `du_base_role =
> Officer`, so the admin backfill did **not** cover it — and the `officer` role
> holds `dashboard.view` and nothing else. Under any of the maps in §5 it is
> denied everything. Grant yourself first, from a psql prompt, and verify with
> §9's query.

---

## 8 · Rolling it out without locking anyone out

`enforce` exists for this. The verdict is computed and logged for every request
from day one; only the last step changes.

**Stage 1 — build, seed, audit.** Every row `enforce = false`. Nothing is
refused. Let it run for a few days of real traffic.

**Stage 2 — read what would have broken.**

```sql
-- Who would have been denied, and why. One row per (person, endpoint, reason),
-- with a hit counter, so this is the whole report.
SELECT endpoint, reason, count(*) AS people, sum(hits) AS calls,
       max(last_seen) AS most_recent
  FROM attendance.ext_api_access_audit
 WHERE last_seen > now() - interval '7 days'
 GROUP BY 1, 2
 ORDER BY calls DESC;

-- Ready to enforce ONE endpoint? Everything left here is somebody who breaks.
SELECT person_id, reason, hits, last_seen
  FROM attendance.ext_api_access_audit
 WHERE endpoint = '/ext-api/wow-attendance/logs/login'
 ORDER BY hits DESC;

-- The §7.1 blocker, measured: calls still arriving on unverifiable tokens.
SELECT endpoint, count(*), sum(hits)
  FROM attendance.ext_api_access_audit
 WHERE token_source = 'legacy' GROUP BY 1;
```

Clear the table (`DELETE FROM attendance.ext_api_access_audit`) after fixing a
batch, so what is left is always "still broken" rather than history.

Expect two classes, and treat them differently: `reason=not_in_role` is a real
person who needs a grant, `reason=no_account` is a client with **no ERP account
at all** — usually a device (§10).

**Stage 3 — enforce, one endpoint at a time.** This needs BOTH switches: set
`EXT_ACCESS_CONTROL=on` in `.env` and restart (that alone refuses nothing,
since every rule is still `enforce = false`), then flip rules one at a time.
Start where a mistake is cheapest to undo and the risk is highest — the log
readers:

```sql
UPDATE attendance.ext_api_endpoint_permissions
   SET enforce = true
 WHERE endpoint IN ('/ext-api/wow-attendance/logs/login',
                    '/ext-api/wow-attendance/logs/attendance');
```

**Stage 4 — the rest**, once each has run a week clean. The turnstile paths
(`verify`, `get_card_info`) go last: a mistake there stops people getting
through doors.

**Rollback is one statement, no redeploy** — the same property the IP allow-list
has, and the reason both live in tables:

```sql
UPDATE attendance.ext_api_endpoint_permissions SET enforce = false
 WHERE endpoint = '/ext-api/…';
```

---

## 9 · Managing roles and people (the day-to-day)

### From the ERP UI — the intended path

The screens already exist and already write these tables:

| Screen | Route | Gated by | Use it to |
|---|---|---|---|
| Access Roles | `/access-roles` | `admin.roles.manage` | create a role, tick which resources it holds |
| Users | `/role_list` | `admin.users.manage` | assign a role to a person, activate/deactivate |

So "create an access role from the logged-in user" needs **no new endpoint in
this service** — it needs this service's resource keys (§4.3) to exist, so they
appear as tick-boxes on that screen. Confirm with whoever maintains duerp-api
that the Access Roles screen lists all of `attendance.resources` and not a
hard-coded subset; if it filters by `category`, use a category it already shows.

> **Those screens currently write `ictcell`, which this service no longer
> reads.** Until [§2.1](#21--this-service-owns-a-copy)'s question is answered,
> the psql recipes below are the only way to change what this service enforces.

### Who holds what today

```sql
-- Roles, and how many people are in each. 14 of 16 accounts still have none.
SELECT coalesce(r.key, '(no role)') AS role, count(*)
  FROM attendance.app_users au
  LEFT JOIN attendance.roles r ON r.id = au.role_id
 GROUP BY 1 ORDER BY 2 DESC;

-- Everything one role may do.
SELECT res.key FROM attendance.role_permissions rp
  JOIN attendance.roles r      ON r.id  = rp.role_id
  JOIN attendance.resources res ON res.id = rp.resource_id
 WHERE r.key = 'admin' ORDER BY 1;
```

`admin` and `superadmin` hold **every** permission this service defines,
including the six `nfc.*` / `face.*` keys and `admin.logs.view`
(`sql/006_access_control.sql` §4b) — right for administrators, wrong for
everyone else. Three working roles exist for the rest (§4c), and what they do
**not** hold is the point:

| Role | Holds | Deliberately not |
|---|---|---|
| `card_desk` | `nfc.card.register`, `nfc.card.status`, `nfc.card.read`, `dashboard.view` | **`nfc.card.reassign`** — taking a card off another student. The one clerk who needs it gets a per-person `grant` override, not a wider role |
| `reader` | `nfc.card.read`, `face.verify`, `dashboard.view` | registration, enrolment, reports, logs. A device's credentials sit in a machine on a corridor, so this is the role worth keeping narrowest |
| `enrollment_desk` | `face.enroll`, `dashboard.view` | `face.verify` — marking somebody present is the door's job, and a desk confirming an enrolment worked asks `check` instead. Also not `attendance.report.view`: enrolling people is no reason to read their history |

All three are `is_system = false`, so the Access Roles screen may edit them.
**Nobody is in any of them yet** — the desks need people, and the readers need
accounts at all (§10). `tests/access_control_sql.rs` pins all three sets
exactly, so widening one is a deliberate diff rather than a quiet grant.

It also asserts that **every endpoint is reachable by some role other than
`admin`** — a completeness check, not a security one: an endpoint only an admin
can reach means the people who actually do that job have nowhere to be put, and
enforcing it would stop the work. Two exceptions are recorded there with their
reasons:

- **`#force_reassign`** — handed out per person with a `grant` override.
- **`logs/login` and `logs/attendance`** — the step logs carry usernames,
  client IPs, GPS, employee ids and whole request bodies. "Which role needs to
  read everyone's requests?" has one honest answer, and it is not a desk.

`tests/access_control_sql.rs` pins the invariant that an admin is never locked
out of a permission point this service defines: add an endpoint rule naming a
new key and forget to grant it, and that test fails rather than the
administrators discovering it on the day enforcement starts.

### From psql — the equivalents

```sql
-- 1 · Create a role for the card desk
INSERT INTO attendance.roles (key, name, is_system)
VALUES ('card_desk', 'Card Desk', false)
ON CONFLICT (key) DO NOTHING;

-- 2 · Give it permissions
INSERT INTO attendance.role_permissions (role_id, resource_id)
SELECT r.id, res.id
  FROM attendance.roles r, attendance.resources res
 WHERE r.key = 'card_desk'
   AND res.key IN ('nfc.card.read', 'nfc.card.register', 'nfc.card.status')
ON CONFLICT DO NOTHING;

-- 3 · Put a person in it (person_id is the token `sub`: emp_id for staff)
UPDATE attendance.app_users
   SET role_id = (SELECT id FROM attendance.roles WHERE key = 'card_desk')
 WHERE person_id = 2020111007;

-- 4 · Allow ONE person something their role does not have.
--     The PK is (user_id, resource_id), so a person has at most one override
--     per permission — write these as upserts and flipping grant↔deny is the
--     same statement with the other word in it.
INSERT INTO attendance.user_permission_overrides (user_id, resource_id, effect)
SELECT au.id, res.id, 'grant'
  FROM attendance.app_users au, attendance.resources res
 WHERE au.person_id = 2020111007 AND res.key = 'nfc.card.reassign'
ON CONFLICT (user_id, resource_id) DO UPDATE SET effect = EXCLUDED.effect;

-- 5 · Take it away from one person whose role has it (an override beats the role)
INSERT INTO attendance.user_permission_overrides (user_id, resource_id, effect)
SELECT au.id, res.id, 'deny'
  FROM attendance.app_users au, attendance.resources res
 WHERE au.person_id = 2020111007 AND res.key = 'nfc.card.reassign'
ON CONFLICT (user_id, resource_id) DO UPDATE SET effect = EXCLUDED.effect;

-- 5b · Back to "whatever the role says" — drop the override rather than
--      writing the opposite one.
DELETE FROM attendance.user_permission_overrides
 WHERE user_id = (SELECT id FROM attendance.app_users WHERE person_id = 2020111007)
   AND resource_id = (SELECT id FROM attendance.resources WHERE key = 'nfc.card.reassign');

-- 6 · Suspend an account entirely (every gated endpoint, immediately)
UPDATE attendance.app_users SET status = 'inactive' WHERE person_id = 2020111007;
```

### Answering "why was I refused?"

One query, the same verdict the middleware saw:

```sql
SELECT attendance.ext_api_can_call(2020111007,
                                   '/ext-api/nfc-card/save_card_info');
-- {"allowed": false, "enforce": true, "reason": "not_in_role",
--  "role": "officer", "resource_key": "nfc.card.register"}
```

And the whole picture for one person:

```sql
SELECT p.endpoint, p.resource_key, p.enforce,
       attendance.ext_api_can_call(au.person_id, p.endpoint) ->> 'allowed' AS allowed,
       attendance.ext_api_can_call(au.person_id, p.endpoint) ->> 'reason'  AS reason
  FROM attendance.ext_api_endpoint_permissions p
  CROSS JOIN attendance.app_users au
 WHERE au.person_id = 2020111007
 ORDER BY p.endpoint;
```

---

## 10 · Devices have no person

The card readers and turnstiles authenticate with a bearer token like everyone
else, which means each one is **somebody's login**. Under role enforcement that
becomes visible: either the device has an account with a role, or every tap is
`reason=no_account`.

Three options, in order of preference:

| Option | Shape | Trade-off |
|---|---|---|
| **Service account per device class** | One DU account (`nfc-reader@du.ac.bd`), one `app_users` row, role `nfc_reader` holding only `nfc.card.read` | Needs DU to issue the account; gives the narrowest possible permission set and a name in every log |
| Service account per device | As above, one per reader | Best audit trail ("reader 3 did this"); more accounts to manage than DU will probably want |
| Leave the device paths open | `resource_key = NULL` on those rows | Honest about what it is — the IP allow-list and app credentials stay the only gate on them, exactly as today |

Whichever is chosen, **say it in the row's `note`**, because the next person to
read the table will otherwise assume the `NULL` is an oversight.

---

## 11 · Testing

**Built: `tests/access_control_sql.rs`, 20 tests, all passing against
`lms_dev`.** It follows the repo pattern — a transaction per test, rolled back,
every fixture key carrying a random suffix, so a run against a database holding
the copied production rows cannot collide with them. The fixtures write only
`attendance`; `ictcell` is never touched, and the suite was checked for
leftovers afterwards.

The rules it pins, one test each:

- no rule row → denied, `reason=no_rule` (**the fail-closed test**; if this one
  is wrong, everything else is theatre)
- `resource_key IS NULL` → allowed for any person
- role holds the resource → allowed, `granted_by_role`
- role does not → denied, `not_in_role`
- `grant` override on a person whose role lacks it → allowed
- `deny` override on a person whose role has it → **denied** (precedence)
- `status <> 'active'` → denied even with the role
- unknown `person_id` → denied, `no_account`
- unknown `resource_key` → denied, `unknown_resource`, not allowed-by-accident
- `enforce=false` → verdict still `allowed:false`, and the caller is let through

For `access_profile` (§14.1), the tests that matter are the ones where the UI
would otherwise lie to someone:

- a `deny` override removes the menu item *and* the permission key, not one of
  the two
- a parent whose children are all denied is **absent**, not an empty dropdown
- `platform='mobile'` returns only rows marked for mobile; the same person on
  `desktop` gets more
- an inactive account gets an empty profile and a `200`, not an error
- `version` changes when the role changes, and does not when nothing did
- **every `route` in a returned menu resolves to an endpoint rule** — the drift
  test, and the one that keeps §12's rule true:

```sql
-- Menu routes whose API side nobody has configured. Should be empty.
SELECT m.label, m.route, res.key AS menu_permission
  FROM attendance.menu_items m
  JOIN attendance.resources res ON res.id = m.resource_id
 WHERE m.is_active
   AND NOT EXISTS (SELECT 1 FROM attendance.ext_api_endpoint_permissions p
                    WHERE p.resource_key = res.key AND p.is_active);
```

Plus a middleware-level check that a `403` from this layer is distinguishable
from the IP one, and a manual matrix before flipping each endpoint:

```bash
set -a; . ./.env; set +a
AUTH=(-H "X-App-Id: $EXT_APP_ID" -H "X-App-Password: $EXT_APP_PASSWORD")
# same request, two people: one with the role, one without
curl -s -o /dev/null -w '%{http_code}\n' "${AUTH[@]}" -H "Authorization: Bearer $ADMIN_TOKEN" \
  -X POST "$BASE/ext-api/wow-attendance/logs/login?limit=1"
curl -s -o /dev/null -w '%{http_code}\n' "${AUTH[@]}" -H "Authorization: Bearer $DESK_TOKEN" \
  -X POST "$BASE/ext-api/wow-attendance/logs/login?limit=1"     # expect 403
```

---

## 12 · One model, three surfaces

The same question — *may this person do this?* — is asked in three places:

| Surface | Asks | Consequence of getting it wrong |
|---|---|---|
| **Desktop menu** (React SPA) | which menu items and routes to render | a user sees a screen that 403s, or misses one they need |
| **Mobile app** | which tabs, buttons and flows to show | same, plus a cached answer that outlives the change |
| **API endpoints** | may this request proceed | **data is read or written that should not have been** |

They must be driven by **one vocabulary — `attendance.resources` keys — and one
source of truth.** A menu item, a mobile tab and the endpoints behind them name
the *same* resource key, so a role change moves all three together.

> ### The rule that makes this safe
>
> **The API is the gate. Menus are a view.**
>
> Hiding a menu item is a courtesy to the user, not a security control. Anyone
> can read the token out of their own browser and curl the endpoint; a mobile
> binary is even easier — the app's code, its URLs and any credential compiled
> into it are readable by whoever installs it.
>
> So: **never** protect something by hiding it in the UI alone. Every route the
> menu can reach must also have a row in
> `attendance.ext_api_endpoint_permissions` ([§4.1](#41--endpoint--permission-map)).
> The menu answer and the endpoint answer come from the same tables precisely so
> they cannot disagree — but only the endpoint one is enforced.

### What each surface gets

```
                   attendance.resources  (one vocabulary of permission keys)
                              │
        ┌─────────────────────┼──────────────────────┐
        │                     │                      │
  menu_items            (feature flags)     ext_api_endpoint_permissions
  platforms[]            in the profile         endpoint → resource_key
        │                     │                      │
   desktop nav          mobile tabs /           middleware, every request
   (SPA renders)        buttons (app renders)   (the actual gate)
        └──────── both read POST /ext-api/me/access ───────┘
```

---

## 13 · Platform scoping for menus

`attendance.menu_items` — copied from the ERP — is desktop-shaped: a two-level tree of SPA routes
(`/dashboard`, `/role_list`, `/access-roles`, `/menu-list`, `/api-logs`,
`/create-class-operation`). A mobile app does not want that tree — it wants four
or five tabs and a short "more" list.

**One column solves it**, and keeps one table as the source of truth:

```sql
-- What the mobile app shows: the dashboard and the two attendance actions
UPDATE attendance.menu_items SET platforms = '{desktop,mobile}' WHERE route = '/dashboard';

INSERT INTO attendance.menu_items (label, icon, route, resource_id, sort_order, is_active, platforms)
SELECT v.label, v.icon, v.route, res.id, v.sort, true, '{mobile}'
  FROM (VALUES
        ('Check in', 'camera',  '/verify',   'face.verify', 10),
        ('My face',  'user',    '/enroll',   'face.enroll', 20)
       ) AS v(label, icon, route, perm, sort)
  JOIN attendance.resources res ON res.key = v.perm;
```

The default `'{desktop}'` means **every existing row keeps behaving exactly as
it does now** — a mobile client asking for its menu gets only rows that were
deliberately marked for it, rather than a desktop tree squeezed onto a phone.

The column is plain DDL on a table this service owns, so the question that used
to attach to it — whether duerp-api would accept the change — no longer arises.
That is one of the things [§2.1](#21--this-service-owns-a-copy)'s move bought.

The **permission key stays shared** across surfaces. Two tables of *menu rows*
is survivable; two tables of *permissions* is the failure mode this whole
document exists to avoid — which is exactly why the copy needs a chosen
direction and not a shrug.

---

## 14 · The access endpoint

**Built: `src/routes/access.rs`.** One call, made right after login by both
clients, that answers "who am I, what may I do, what do I show".

```
POST /ext-api/me/access          (GET also)
  X-App-Id / X-App-Password + Authorization: Bearer <token>
  ?platform=desktop | mobile | kiosk        (default: desktop)
```

`platform` is also accepted in a JSON, form-urlencoded or multipart body — the
same four encodings the NFC lookups take, through the same parser, so a client
that already talks to this service needs no second way of sending one scalar.

**An unknown platform is a `400`, not an empty menu.** A typo like
`?platform=mobil` matches no menu row, and returning `200` with an empty menu
renders as "you have no access" and sends somebody hunting through role tables.
The accepted values are named in the error.

It is **not** an admin endpoint: it returns the caller's *own* access and
nothing else. The person id comes from the token, never from a parameter — an
endpoint that lets you ask about somebody else is an org-chart leak, and a
tempting one.

### Response

```json
{
  "status": "success",
  "message": "Access profile",
  "data": {
    "person_id": 2020111007,
    "username": "omarfaruk@du.ac.bd",
    "role": "admin",
    "role_name": "Admin",
    "platform": "desktop",
    "permissions": [
      "admin.logs.view", "admin.menu.manage", "admin.roles.manage",
      "admin.users.manage", "attendance.report.view", "attendance.settings",
      "attendance.take", "dashboard.view"
    ],
    "menu": [
      { "id": 1, "label": "Dashboard", "icon": "GridIcon",
        "route": "/dashboard", "permission": "dashboard.view", "children": [] },
      { "id": 4, "label": "User Management", "icon": "UserCircleIcon",
        "route": null, "permission": null, "children": [
          { "id": 5, "label": "Users", "route": "/role_list",
            "permission": "admin.users.manage" },
          { "id": 6, "label": "Access Roles", "route": "/access-roles",
            "permission": "admin.roles.manage" }
      ]}
    ],
    "version": "8f14e45fceea167a5a36dedd4bea2543"
  }
}
```

Three fields earn their place:

- **`permissions`** — the flat list, for `can('nfc.card.reassign')` checks on
  buttons and fields the menu cannot express (`force_reassign` is a checkbox,
  not a route).
- **`menu`** — already filtered by platform *and* permission, and **already
  pruned**: a parent whose children are all denied does not come back as an
  empty dropdown. The client renders what it is given; it does not filter.
- **`version`** — a hash of the payload, returned as the `ETag` header too.
  The client sends it back as `If-None-Match` (what a browser does by itself)
  or as `?version=` (what a mobile app does, having stored it) and gets a
  **`304` with no body**. Both forms are accepted, quoted or bare, because both
  happen. It is also the cheapest possible "your access changed" signal.

### Where it lives

The SQL function is the contract (`attendance.access_profile(person_id,
platform)` — [§14.1](#141--the-function)); the HTTP handler is a wrapper around
it that validates the platform, applies the 304 check and writes the step log.
If duerp-api later wants to serve the same profile to the ERP SPA, it calls the
same function and returns the same shape. **Two hosts, one answer** — the same
reason every rule in this service lives in SQL.

### What a caller sees when they have nothing

A person with no account, or a suspended one, gets **`200` with an empty
profile** — no role, no permissions, no menu — not a `403`. The client renders a
bare shell, and every endpoint still refuses them; an error here would only make
the app look broken to someone who is merely unprivileged.

`500` is reserved for the profile genuinely failing to build (an unapplied
migration, a database that is down), precisely so a client can tell "you may do
nothing" from "we could not find out".

### Errors

| Status | `code` | Case |
|---|---|---|
| 400 | `invalid_platform` | Not one of `desktop`, `mobile`, `kiosk` |
| 401 | — | Bad app credentials, or missing/invalid/expired token |
| 403 | — | Caller IP not allow-listed for this path |
| 500 | `internal_error` | `attendance.access_profile` failed — including not being applied yet |

### 14.1 · The function

```sql
CREATE OR REPLACE FUNCTION attendance.access_profile(
    p_person_id bigint,
    p_platform  text DEFAULT 'desktop'
) RETURNS jsonb
LANGUAGE plpgsql STABLE
AS $function$
DECLARE
    v_user  RECORD;
    v_perms text[];
    v_menu  jsonb;
BEGIN
    SELECT au.id, au.person_id, au.username, au.status, au.role_id,
           r.key AS role_key, r.name AS role_name
      INTO v_user
      FROM attendance.app_users au
      LEFT JOIN attendance.roles r ON r.id = au.role_id
     WHERE au.person_id = p_person_id;

    -- No account, or a suspended one: an EMPTY profile, not an error. The
    -- client renders a bare shell and every endpoint still refuses them; a
    -- failure here would only make the app look broken to someone who is
    -- merely unprivileged.
    IF NOT FOUND OR coalesce(v_user.status, '') <> 'active' THEN
        RETURN jsonb_build_object(
            'status',  'success',
            'message', 'No access profile for this user',
            'data', jsonb_build_object(
                'person_id',   p_person_id,
                'username',    NULL,
                'role',        NULL,
                'role_name',   NULL,
                'platform',    p_platform,
                'permissions', '[]'::jsonb,
                'menu',        '[]'::jsonb,
                'version',     md5('')));
    END IF;

    SELECT coalesce(array_agg(DISTINCT res.key ORDER BY res.key), '{}')
      INTO v_perms
      FROM attendance.resources res
     WHERE (EXISTS (SELECT 1 FROM attendance.role_permissions rp
                     WHERE rp.role_id = v_user.role_id
                       AND rp.resource_id = res.id)
            OR EXISTS (SELECT 1 FROM attendance.user_permission_overrides o
                        WHERE o.user_id = v_user.id
                          AND o.resource_id = res.id
                          AND o.effect = 'grant'))
       AND NOT EXISTS (SELECT 1 FROM attendance.user_permission_overrides o
                        WHERE o.user_id = v_user.id
                          AND o.resource_id = res.id
                          AND o.effect = 'deny');

    WITH visible AS (
        SELECT m.*, res.key AS resource_key
          FROM attendance.menu_items m
          LEFT JOIN attendance.resources res ON res.id = m.resource_id
         WHERE m.is_active
           AND p_platform = ANY(m.platforms)
           -- A row with no resource_id is a container ("User Management"). It
           -- carries no permission of its own and survives only if something
           -- under it did — see the final WHERE.
           AND (m.resource_id IS NULL OR res.key = ANY(v_perms))
    ),
    children AS (
        SELECT parent_id,
               jsonb_agg(jsonb_build_object(
                   'id', id, 'label', label, 'icon', icon,
                   'route', route, 'permission', resource_key)
                   ORDER BY sort_order) AS items
          FROM visible
         WHERE parent_id IS NOT NULL
         GROUP BY parent_id
    )
    SELECT coalesce(jsonb_agg(jsonb_build_object(
               'id', v.id, 'label', v.label, 'icon', v.icon, 'route', v.route,
               'permission', v.resource_key,
               'children', coalesce(c.items, '[]'::jsonb))
               ORDER BY v.sort_order), '[]'::jsonb)
      INTO v_menu
      FROM visible v
      LEFT JOIN children c ON c.parent_id = v.id
     WHERE v.parent_id IS NULL
       -- An empty dropdown is worse than no dropdown: it tells the user there
       -- is something there and then refuses to open.
       AND (v.resource_id IS NOT NULL OR c.items IS NOT NULL);

    RETURN jsonb_build_object(
        'status',  'success',
        'message', 'Access profile',
        'data', jsonb_build_object(
            'person_id',   v_user.person_id,
            'username',    v_user.username,
            'role',        v_user.role_key,
            'role_name',   v_user.role_name,
            'platform',    p_platform,
            'permissions', to_jsonb(v_perms),
            'menu',        v_menu,
            -- Cheap change-detection for the client, and the cheapest possible
            -- "your access changed" signal. Covers both halves of the payload,
            -- so a menu edit invalidates it as surely as a role change does.
            'version',     md5(v_perms::text || v_menu::text)));
END;
$function$;
```

Verified against `lms_dev` in a rolled-back transaction: as `admin` on
`desktop` it returns the four top-level items with User Management's two
children nested; the **same person** on `mobile` gets only the rows marked for
mobile; as `faculty` the User Management container disappears entirely rather
than rendering empty.

---

## 15 · Mobile-specific concerns

### 15.1 · The app credentials are not secret once they ship

`ExtAuthMiddleware` requires `X-App-Id` / `X-App-Password` on every `/ext-api`
call. **A mobile app that calls this API directly has to carry them in its
binary, where anyone who installs it can read them.** Treat a shipped app
credential as public and plan for it:

| Option | What it buys | Cost |
|---|---|---|
| **A per-surface app id** (`duerp-mobile`, `duerp-web`, `nfc-reader`) | the mobile credential can be rotated without breaking the readers, and the call logs say which surface a request came from | needs the middleware to accept a set of id/password pairs instead of one, and a table to hold them |
| Proxy mobile traffic through a backend that holds the credential | the secret never ships | a second hop, and the backend becomes the thing to secure |
| Ship the shared credential | nothing to build | one leak rotates every client at once — including the card readers |

The first is the recommended one, and it is a small change: the credential pair
moves from `.env` into a table next to `ext_api_allowed_ips`, keyed by app id.
**None of this weakens the model** — the app credential was never the thing
identifying a *person*. It keeps strangers off the surface; the token and the
role decide what happens next.

### 15.2 · Caching, and the lag it introduces

| | Desktop | Mobile |
|---|---|---|
| Fetch profile | on login, and on hard refresh | on login, and on app foreground |
| Cache | in memory | encrypted storage, so the UI draws before the network answers |
| Max staleness | a page load | **set a TTL (15 min is reasonable) and refresh on resume** |
| On a `403` | refresh the profile once, then show the error | identical |

A cached profile can only make the UI *wrong*, never the API *permissive* — the
endpoint check re-reads the tables on every request. That is the entire reason
the menu is allowed to be cached at all. Say it in the mobile code review
checklist: **a cached permission is a rendering hint, never an authorization.**

### 15.3 · Offline

The attendance flows need the network anyway (face verification is a server
call), so "offline mode" means *a queue*, not *offline permissions*. Queue the
intent, replay it when the network returns, and let the server decide then. A
queued action from a person whose role was revoked meanwhile must fail on
replay — which it will, for free, because the check happens at replay time.

### 15.4 · Kill switches worth having in the profile

Cheap to add while designing the payload, expensive to retrofit:

```json
"app": { "min_version": "2.1.0", "message": "Update required to check in" }
```

A per-platform minimum version lets a broken release be switched off centrally
rather than waiting for stores. Keep it in the same table as the menus, and it
is an UPDATE, not a deploy.

---

## 17 · Administering roles here

**Built.** `sql/007_access_admin.sql` + `src/routes/access_admin.rs` +
`duerp-face-recognition-frontend` → **`/access-roles`**.

This is [§2.1](#21--this-service-owns-a-copy) option (c): the roles this
service enforces are edited in this service. The ERP's own `/access-roles`
still exists and still writes `ictcell` — it does **not** reach the gate here,
and the two screens have the same name on purpose only in the sense that they
do the same job for different systems.

| Endpoint | Permission | Does |
|---|---|---|
| `POST /ext-api/access/roles` | `admin.roles.manage` | roles, their permissions, member counts |
| `POST /ext-api/access/resources` | `admin.roles.manage` | the permission vocabulary, with the endpoints each key opens |
| `POST /ext-api/access/role-save` | `admin.roles.manage` | create/update a role and **replace** its permission set |
| `POST /ext-api/access/role-delete` | `admin.roles.manage` | delete an unused, non-system role |
| `POST /ext-api/access/users?search=&limit=&offset=` | `admin.users.manage` | accounts, their role, overrides, and **effective** permissions |
| `POST /ext-api/access/user-role` | `admin.users.manage` | put a person in a role, or take them out |
| `POST /ext-api/access/user-override` | `admin.users.manage` | one person's exception: `grant` / `deny` / `clear` |

### These enforce from day one

The one deliberate exception to the audit-mode rollout in §6/§8. That rollout
is cautious because every other endpoint has existing callers who would break;
these have none, and they are the API that **hands out permissions**. Serving
them in audit mode would let any token holder make themselves an admin — a
strictly worse failure than a new screen 403ing on its first day. So the
handler reads the verdict and ignores both `EXT_ACCESS_CONTROL` and the rule
row's `enforce`.

### The guards, and why each one exists

All four are in SQL, so they hold for a DBA at a psql prompt as well as for the
screen:

| Refusal | `code` | Why |
|---|---|---|
| Removing `admin.roles.manage` from `admin` | `would_lock_out` | It is the permission that reaches this API. Remove it and nobody can give it back — a psql prompt becomes the only way home |
| Demoting the last active admin | `last_admin` | The same lockout, from the other end |
| Deleting a role somebody holds | `role_in_use` | Their `role_id` would go `NULL`, which reads as "denied everything" once enforcement is on, and nobody would connect the two |
| Renaming or deleting a system role's key | `system_role` | duerp-api still joins on those keys |

Plus the ordinary ones: an invalid key (`invalid_key` — it ends up in queries
and guards), an unknown permission (`unknown_permission`, refused rather than
silently dropped, which would leave the screen showing a permission the role
does not hold), and no such account (`no_account`).

### `permissions` is the whole set

`role-save` **replaces**. A key left out is a revocation — that is what
unticking a box means — so a client that sends a partial list silently strips
the rest. The audit row records both sides.

### Every write is audited

`attendance.access_admin_audit` holds the actor (the token's `sub`), the
action, the target, and a `detail` with before and after. "Who gave them that?"
is the first question asked after an incident, and the step logs answer "who
called what", not "what did it change".

```sql
SELECT created_at, actor, action, target, detail
  FROM attendance.access_admin_audit ORDER BY id DESC LIMIT 20;
```

### The screen

`/access-roles` in the attendance panel, admin-only in the nav — **hiding it
hides a link; the API is the gate.** Two tabs: *Roles & permissions* (pick a
role, tick what it may do, save) and *People* (search, assign a role, see each
account's effective permissions and overrides).

---

## 16 · Decisions to confirm before building

1. ~~Reuse `ictcell` roles, or keep this service's own copy?~~ **Decided: own
   copy**, in `attendance` (§2.1). The open half is what replaces the ERP
   screens as the way roles get assigned — option (a), (b) or (c) in §2.1.
   Nothing else in this list matters as much: until it is answered, every role
   change is a manual UPDATE here.
2. **Who gets each endpoint** (§5) — needs the process owner, not a developer.
3. **`force_reassign`: separate permission or not?** Recommended yes (§5).
4. **Device accounts** (§10) — needs DU to issue accounts, so start early.
5. **`WOW_ACCEPT_DU_TOKEN=false`** — who is still on legacy tokens, and when can
   they move? Enforcement is meaningless until they have (§7.1).
6. **`role_id` backfill** — Option A or B in §7.4.
7. **Does the ERP's Access Roles screen list all resources?** If it filters, the
   new keys need a category it shows, or a small change on the duerp-api side.
8. ~~Will duerp-api accept `menu_items.platforms`?~~ **Moot** — the table is
   this service's now, and the column is already on it (§13).
9. **Who serves `/ext-api/me/access`** — this service, duerp-api, or both from
   the one SQL function? (§14) Both is fine; two *implementations* is not.
10. **What is actually on the mobile menu?** Product decision, and it is the
    input to §13's seed. "The desktop menu, smaller" is the answer to avoid.
11. **Per-surface app credentials** (§15.1) — one `X-App-Id` per client type, or
    keep the single shared pair that ships inside the mobile binary?

---

## Implementation checklist

Nothing here is built yet. In order — each step is useful on its own, and
nothing before step 8 can refuse a request:

| # | Step | Where | |
|---|---|---|---|
| 0 | **Decide how roles get assigned now that the copy exists** — §2.1 (a)/(b)/(c) | with duerp-api | |
| 1 | Backfill `app_users.role_id`; agree the role list | `attendance.app_users` (§7.4) | **partly** — 3 admins; `card_desk` and `reader` exist but are empty |
| 2 | Seed this service's resource keys | `attendance.resources` (§4.3) | **done** |
| 3 | `ext_api_endpoint_permissions` table + `ext_api_can_call()` | `sql/006_access_control.sql` | **done** |
| 4 | Seed one row per endpoint, all `enforce = false` | same file (§5) | **done** |
| 5 | DB-backed tests | `tests/access_control_sql.rs` (§11) | **done** |
| 6 | Middleware check, audit-only, logging verdicts | `ext_auth_middleware.rs` (§6) | **done** |
| 7 | `force_reassign` check in the handler | `routes/nfc_card.rs` (§5) | **done** |
| 8 | `WOW_ACCEPT_DU_TOKEN=false` | `.env` (§7.1) — **before any enforce=true** | |
| 9 | `EXT_ACCESS_CONTROL=on`, then flip `enforce` per endpoint, logs first | `.env` + SQL (§8) | |
| 10 | Document the final map here, and in `nfc_card.md` / `wow_attendance.md` | docs | |

Then the client-facing half — it needs nothing from the list above except step
2, so it can be built in parallel:

| # | Step | Where | |
|---|---|---|---|
| 11 | `menu_items.platforms` column, defaulting to `{desktop}` | `attendance.menu_items` (§13) | **done** |
| 12 | `attendance.access_profile()` | `sql/006_access_control.sql` (§14.1) | **done** |
| 13 | `POST /ext-api/me/access` handler (GET too) | `src/routes/access.rs` (§14) | **done** |
| 14 | Seed the mobile menu rows | `attendance.menu_items` (§13) — needs the product decision first | |
| 15 | Desktop SPA renders from the profile instead of its own map | `duerp-ui` | |
| 16 | Mobile app: fetch on login/foreground, cache with TTL, refresh on 403 | mobile (§15.2) | |
| 17 | Drift test: every menu route has an endpoint rule | CI (§11) | |

Steps 3–7 and 11–14 are a day or two of work each half. Steps 1, 2 and 8 are
where the calendar time goes, because they need other people — which is the
argument for starting them today and building the code around them.

**Build order that never leaves a hole:** the endpoint gate (1–9) before the
menus (11–16). A menu that hides what the API still serves is a false sense of
security; an API that refuses what the menu still shows is merely an ugly error
message. If only one half can ship, ship the gate.
