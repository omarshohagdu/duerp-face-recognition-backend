# NFC Card Module

NFC card ↔ student mapping. A card desk assigns a physical card to a student
and stores a photo of the card plus a selfie; a reader later taps the card and
gets back who it belongs to.

Three endpoints:

| Method | Path | Purpose |
|---|---|---|
| `POST` (or `GET`) | `/ext-api/nfc-card/get_card_info` | Scan lookup → applicant id, card number, and the two flags with readable labels. Nothing else: it is called on every tap, so the payload stays small and free of PII. |
| `POST` | `/ext-api/nfc-card/save_card_info` | Create or update a student's card record. Always carries a card photo **and** a selfie, and saves only if a face-match service says they are the same person. |
| `POST` (or `GET`) | `/ext-api/nfc-card/checking_card_reg_status` | **Does this student already hold a card?** Asked with `registration_no`, not a card number. Returns the whole registration — images, timestamps, `is_registered` — and a `data` object on *every* response. For a desk deciding what to do next, not for a turnstile. |

The first and the third read the same table from opposite ends — one by card,
one by student; [which to call](#which-lookup-should-i-call) is a one-line
decision.

Every call needs **three** things: the app credentials, an allow-listed IP, and
a bearer token. If you are looking at an error right now, jump to
[Troubleshooting](#troubleshooting) — each layer fails with a distinguishable
message.

This is the **endpoint reference**. The original requirement is
[`nfc-card-reader-api-spec.md`](nfc-card-reader-api-spec.md); differences from
it are listed under [Deviations from the spec](#deviations-from-the-spec).

Source: `src/routes/nfc_card.rs` (crate `duerp-attendance`, default `:8083`)
Stored procedures: `sql/004_nfc_card.sql` (also folded into `docs/attendance_schema.sql` §5)
Tests: `cargo test --bin duerp-attendance` (unit) · `cargo test --test nfc_card_sql` (DB-backed)

---

## Response envelope

All three endpoints answer in one shape:

```json
{ "status": "success", "message": "…", "data": { … } }
{ "status": "error",   "message": "…", "code": "…"    }
```

| Key | Always | Notes |
|---|---|---|
| `status` | yes | `"success"` or `"error"`. **This is the field to branch on.** |
| `message` | yes | Human-readable. Prose — may be reworded, so don't match on it. |
| `data` | success only — **but always on `checking_card_reg_status`** | Plus `data.assigned_to` on a `card_conflict`, naming the current holder. |
| `code` | errors only | Stable machine-readable reason. Match on this. |
| `warnings` | `save_card_info` | Non-fatal notices; see [below](#behaviour-worth-knowing). |

> **One deliberate difference between the three.**
> `checking_card_reg_status` carries `data` on *every* response — `{}` when
> there is no registration — so a client can read it without branching on
> `status` first. The other two omit `data` on failure, and their callers are
> written against that. Both shapes were requested; neither is drift, and
> "harmonising" them would break one set of clients.

> **This module is the only one in the service that uses `status`.** Everything
> under `/ext-api/wow-attendance/*` still answers with a `success` boolean, and
> there is **no** `success` field here — a client cannot share one response
> parser across the two modules. The divergence is deliberate.

The envelope is built in SQL, so a `psql` caller sees exactly what the HTTP
client does; the handler only maps `code` onto an HTTP status.

---

## Where the rules live

Almost every rule below is enforced in **`attendance.nfc_card_save_info`**, not
in the Rust handler. That function is the only writer, so the rules hold whether
the caller is the handler or a DBA at a psql prompt — a second caller cannot
bypass the forced `is_verified`, skip the audit row, or steal a card.

The handler owns the HTTP shape: parse the multipart body, store images on disk,
map the function's error `code` onto a status, and turn stored paths into URLs.

---

## Auth

All three endpoints sit under `/ext-api`, so `ExtAuthMiddleware` applies first:

| Layer | Requirement | Failure |
|---|---|---|
| 1 · App credentials | `X-App-Id` + `X-App-Password` matching `EXT_APP_ID` / `EXT_APP_PASSWORD` | `401 Invalid App ID or Password` |
| 2 · IP allow-list | **Open to every IP.** All three rows carry the `'*'` wildcard, so the check passes for any caller; the row must still exist and be `is_active` | `403 IP address not allowed for this endpoint` |
| 3 · Bearer token | `Authorization: Bearer <token>` from `POST /login` (signature + expiry verified) | `401` |

**The app credentials and the bearer token are the whole authorization**, and
the app credentials are shared by every ext-api client. Be aware of what that
means for `force_reassign`: **any** holder of a valid token, calling from
**anywhere that can reach the service**, can take a card off another student.
The IP allow-list used to scope that to trusted readers; it no longer does. See
[Reassigning a card](#reassigning-a-card).

> **Each endpoint still needs its own allow-list row** — the wildcard lives
> *in* the row, so a missing or `is_active = false` row is still a `403` before
> the handler runs. `sql/004_nfc_card.sql` seeds and opens all three:
>
> ```sql
> -- what the migration leaves in place
> SELECT endpoint, ip_address, is_active
>   FROM attendance.ext_api_allowed_ips
>  WHERE endpoint LIKE '/ext-api/nfc-card/%';   -- ip_address = {*}
> ```
>
> To scope them back to real readers, replace the wildcard with their IPs:
>
> ```sql
> UPDATE attendance.ext_api_allowed_ips
>    SET ip_address = '{203.0.113.10,203.0.113.11}'::text[]
>  WHERE endpoint IN ('/ext-api/nfc-card/get_card_info',
>                     '/ext-api/nfc-card/save_card_info',
>                     '/ext-api/nfc-card/checking_card_reg_status');
> ```

The token's `sub` is a DU user id. It is recorded as `performed_by` on every
audit row, but it says only *which login* performed the write — it carries no
applicant identity, so it is **not** checked against `student_applicant_id`. A
self-registration is therefore "a self-registration someone recorded", not
"proof this student registered their own card".

---

## Card numbers are normalised

One physical card must never occupy two rows. `attendance.nfc_card_normalize`
strips every non-alphanumeric character and upper-cases the rest, on **both**
save and lookup:

| Sent | Stored / matched |
|---|---|
| `04:a1:b2:c3:d4:e5` | `04A1B2C3D4E5` |
| `04-A1-B2-C3-D4-E5` | `04A1B2C3D4E5` |
| `  04a1b2c3d4e5 ` | `04A1B2C3D4E5` |
| `---`, `""`, whitespace | `NULL` (= "no card") |

So the reader that enrolls a card and the turnstile that reads it need not agree
on formatting. After normalisation a card number must be **4–64 characters**;
below that it is a failed scan, not a card, and is rejected.

---

## 1 · `get_card_info`

**`POST /ext-api/nfc-card/get_card_info`** — `GET` also accepted.

Use **POST**: every other endpoint in this service is POST, including the
read-only ones, so a client posting to all of them need not special-case this
path. `GET` is still honoured because the spec documents it and it is the honest
method for a lookup that changes nothing. One `ext_api_allowed_ips` row covers
both — the allow-list matches on path, not method. Any other method is a `404`.

### Where `card_number` can go

| Source | Example |
|---|---|
| Query string | `?card_number=04A1B2C3D4E5` |
| JSON body | `{"card_number":"04A1B2C3D4E5"}` — a bare number works too |
| Form-urlencoded body | `card_number=04A1B2C3D4E5` |
| Multipart body | Postman's default POST body |

The query string wins if both are present. All four are accepted because a
lookup carrying one scalar arrives every one of these ways in practice, and a
client that guessed differently should not get a `400` that reads as if the card
were unknown. A body with **no** `Content-Type` is also parsed, since embedded
HTTP stacks routinely omit it.

Any separator style works for the value itself — see
[normalisation](#card-numbers-are-normalised).

### Success — `200 OK`

```json
{
  "status": "success",
  "message": "Data Found",
  "data": {
    "student_applicant_id": "APP-2026-00123",
    "card_number": "04A1B2C3D4E5",
    "is_verified": 1,
    "is_verified_label": "Verified",
    "registration_type": 1,
    "registration_type_label": "Admin"
  }
}
```

The four contract fields plus a readable label for each enum, and nothing else.
No image paths, no timestamps, no other PII.

| Field | `1` | `2` |
|---|---|---|
| `is_verified` / `is_verified_label` | `Verified` | `Not Verified` |
| `registration_type` / `registration_type_label` | `Admin` | `Self` |

Branch on the **number**; show the label. The labels are generated by
`attendance.nfc_card_labels`, so both endpoints word them identically, and a
value outside `1`/`2` yields `null` rather than a wrong label.

**Unverified students are returned normally**, with `is_verified: 2`. The flag
is the answer; the caller decides what an unreviewed mapping may do. This
endpoint reports the mapping, it does not police it.

### Errors

| Status | `code` | Case |
|---|---|---|
| 400 | `missing_card_number` | Absent, blank, or normalises to nothing |
| 404 | `card_not_found` | No student holds this card |
| 401 | — | Bad app credentials, or missing/invalid/expired token |
| 403 | — | Caller IP not allow-listed for this path |
| 500 | `internal_error` | Database error |

---

## 2 · `save_card_info`

**`POST /ext-api/nfc-card/save_card_info`**
**Content-Type:** `multipart/form-data`

### Body

| Field | Type | Required | Notes |
|---|---|---|---|
| `student_applicant_id` | string (≤64) | Yes | The key a save upserts on — one card record per student |
| `card_number` | string | Yes | Normalised before storing |
| `is_verified` | `1` \| `2` | No, default `2` | `1 = Verified`, `2 = Not Verified`. Forced to `2` when `registration_type = 2` |
| `registration_type` | `1` \| `2` | Yes | `1 = Admin`, `2 = Self` |
| `force_reassign` | `true`/`1`/`yes`/`on` | No, default off | Move a card off its current holder — see below |
| `card_image` | file | **Yes** | Photo of the physical card — the face on it is one half of the match |
| `student_selfie` | file | **Yes** | Student's selfie — the other half |

The scalar fields are also accepted as **query parameters**, because several
HTTP clients drop the query string on a multipart POST and some send the scalars
there anyway. The body wins when both are present. The two files have no query
equivalent — they must be in the body.

### The face-match gate

**Both images are required on every save, and they must be the same person.**
Before anything is written, the pair is sent to the face-match service:

```
POST $NFC_FACE_VERIFY_URL          # e.g. http://10.224.224.101:8089/verify
X-API-Key: $NFC_FACE_VERIFY_API_KEY
card_image=@…  selfie_image=@…     # multipart/form-data
```

It answers with a verdict:

```json
{ "match": true, "similarity": 0.562791, "threshold": 0.36,
  "model": "insightface", "message": "MATCH — same person" }
```

Only an explicit `"match": true` lets the save proceed. Everything else stops
it, and **nothing is written** — the card record is untouched and no row is
created. The uploaded files stay on disk, referenced by nothing, the same way a
rejected upload is kept for audit.

The gate **fails closed**. A mismatch, a photo with no detectable face, a
service that is down, a 5xx, a rejected API key, a reply that carries no
`match` field, or an unset `NFC_FACE_VERIFY_URL` all mean "no verdict", and no
verdict means no save. This is deliberate: a card mapping that was never
face-checked is exactly what the gate exists to prevent, so an outage stops
card registration rather than quietly letting unchecked pairs through. **If
saves start failing with 503 `face_verify_unavailable`, the face service — not
this one — is what to look at.**

Two consequences worth planning for:

- **A metadata-only update is no longer possible.** Flipping `is_verified` to
  `1` means re-sending both photos, because a save with no files cannot be
  face-checked. (The SQL function still keeps a stored image when one is
  omitted, but that path is no longer reachable through this endpoint.)
- **The check is in the handler, not in SQL.** Postgres cannot make the HTTP
  call, so — unlike normalisation, the `1`/`2` enums and the card-ownership
  rules — a DBA calling `attendance.nfc_card_save_info` directly bypasses it.

### Success — `200 OK`

```json
{
  "status": "success",
  "message": "Card info saved successfully",
  "data": {
    "student_applicant_id": "APP-2026-00123",
    "card_number": "04A1B2C3D4E5",
    "is_verified": 1,
    "is_verified_label": "Verified",
    "registration_type": 1,
    "registration_type_label": "Admin",
    "card_image": "https://api.atten.du.ac.bd/uploads/nfc_card/cards/e9d816f4-…_card.png",
    "student_selfie": "https://api.atten.du.ac.bd/uploads/nfc_card/selfies/f8fca930-…_selfie.png",
    "created": true,
    "reassigned_from": null,
    "changed_fields": ["card_number", "is_verified", "registration_type", "card_image", "student_selfie"]
  },
  "warnings": []
}
```

- `is_verified_label` / `registration_type_label` — the same labels
  `get_card_info` returns, from the same SQL function, so the two endpoints can
  never word them differently.
- `created` — `true` on the first save for an applicant id, `false` on updates.
- `changed_fields` — which columns this write actually changed. Empty for a
  re-save that altered nothing.
- `warnings` — non-fatal things the caller should see: a forced `is_verified`,
  or a card taken from another student. **Surface these in the UI**; they report
  writes that did not do what was asked.
- Image values are **absolute URLs**, rebuilt per response from
  `WOW_PUBLIC_BASE_URL` (or the request's own origin). Rows store filesystem
  paths, so a hostname change does not strand them. `null` means no image on
  file, or a file stored outside the served tree.

### Errors

| Status | `code` | Case |
|---|---|---|
| 400 | `missing_fields` | `student_applicant_id`, `card_number` or `registration_type` absent |
| 400 | `invalid_value` | `is_verified`/`registration_type` not `1` or `2`, or applicant id over 64 chars |
| 400 | `invalid_card_number` | Under 4 or over 64 chars after normalisation |
| 400 | `invalid_image` | Upload's magic bytes are not a known image format |
| 400 | `missing_images` | `card_image` and/or `student_selfie` not sent — both are required |
| 400 | `face_mismatch` | The two photos are different people. `data` carries `similarity`, `threshold` and the service's own `detail` |
| 400 | `face_not_comparable` | The service could not compare them — usually no detectable face in one. Its wording is passed through as the `message` |
| 400 | `bad_multipart` | Malformed multipart body |
| 409 | `card_conflict` | Card is assigned to a **different** student — `data.assigned_to` names them |
| 413 | `image_too_large` | An image exceeds `WOW_MAX_UPLOAD_MB` (default 25 MB) |
| 401 / 403 | — | Auth layers above |
| 500 | `internal_error` | Database or file-storage error |
| 503 | `face_verify_unavailable` | No verdict was obtained — service down, timed out, 5xx, API key rejected, unreadable reply, or `NFC_FACE_VERIFY_URL` unset. **Not the caller's fault**; nothing was written |

A rejected pair looks like this:

```json
{
  "status": "error",
  "code": "face_mismatch",
  "message": "The student selfie does not match the face on the card image; card not saved",
  "data": {
    "match": false,
    "similarity": 0.187816,
    "threshold": 0.36,
    "detail": "NO MATCH — different people"
  }
}
```

`code` is the stable part — match on it. `message` and `data.detail` are prose;
`data.detail` comes from the face service and may be reworded there.

### Behaviour worth knowing

**Self-registration cannot mark itself verified.** `registration_type = 2`
forces `is_verified = 2` whatever was passed in, and reports it in `warnings`.
Enforced in SQL, so it holds for every caller.

**Omitting an image is rejected, not tolerated.** Both files are required —
see [the face-match gate](#the-face-match-gate). Underneath, the SQL function
still keeps a stored image when one is omitted (`COALESCE`, not overwrite), so
a direct caller cannot wipe a photo by leaving it out, and there is **no way to
clear an image** through this endpoint either.

**A repeated save is an update, not a second row** — the upsert is on
`student_applicant_id`, and re-scanning the card you already hold is idempotent.

**Rejected uploads are kept on disk.** A file that fails the image check or the
size ceiling stays where it was written, for audit, and is not referenced by any
row. Same as the face-capture flow. Superseded images are also kept rather than
deleted, so `uploads/nfc_card/` grows monotonically — prune it on a schedule if
that matters.

**Images are validated by magic bytes**, not by extension or the part's
`Content-Type`: both are client-supplied, and a renamed PDF must not end up
stored as somebody's card photo. Client filenames are reduced to a sanitised
basename and prefixed with a UUID.

---

## 3 · `checking_card_reg_status`

**`POST /ext-api/nfc-card/checking_card_reg_status`** — `GET` also accepted.

*Does this student already hold a registered card?* Asked at the desk before a
card is issued, so the question is about the **student**, and the answer is the
**whole registration**: which card, both photos, when it was registered and when
it last changed.

### Request

| Parameter | Required | Notes |
|---|---|---|
| `registration_no` | Yes | The student's registration number, e.g. `2017001010`. Matched against `nfc_student_cards.student_applicant_id` — **the same key `save_card_info` upserts on**, so whatever value the desk registered the student under is the value to ask with. |

It goes in any of the four places
[`get_card_info` accepts its parameter](#where-card_number-can-go) — query
string, JSON body, form-urlencoded body or multipart body — with the query
string winning if both are present. The requested form works as-is:

```json
{ "registration_no": 2017001010 }
```

An **unquoted number is accepted**, as are quoted strings.

> **The value is trimmed, and nothing else.** Card numbers are
> [normalised](#card-numbers-are-normalised) — case-folded, separators stripped
> — but a registration number is **not**: it is an opaque key, and matching more
> loosely here than `save_card_info` does when it writes would report one
> student's card under another student's id. `2017001010` and ` 2017001010 `
> are the same student; `app-2026-00123` and `APP-2026-00123` are not.

There is no student registry to validate against — `student_applicant_id` has
[no foreign key](#student_applicant_id-has-no-foreign-key) — so a typo'd number
and a genuinely unregistered student give the same answer.

### Registered — `200 OK`

```json
{
  "status": "success",
  "message": "Registration found for registration no 2017001010",
  "data": {
    "is_registered": true,
    "registration_no": "2017001010",
    "student_applicant_id": "2017001010",
    "card_number": "04A1B2C3D4E4",
    "is_verified": 1,
    "is_verified_label": "Verified",
    "registration_type": 1,
    "registration_type_label": "Admin",
    "card_image": "https://api.atten.du.ac.bd/uploads/nfc_card/cards/e9d816f4-…_card.png",
    "student_selfie": "https://api.atten.du.ac.bd/uploads/nfc_card/selfies/f8fca930-…_selfie.png",
    "registered_at": "2026-09-09T11:02:41.882374+06:00",
    "updated_at": "2026-09-15T09:18:03.114902+06:00"
  }
}
```

| Field | Meaning |
|---|---|
| `is_registered` | Always `true` when it appears. Redundant with `status` by construction, and there for a UI that would rather bind one boolean than branch on a string. **There is no `is_registered: false`** — an unregistered student has no `data` object to put it in, so read `status` / `code` for that case. |
| `registration_no` / `student_applicant_id` | The same column under both names: what you asked with, and what the other two endpoints call it. |
| `card_number` | The card this student currently holds, in normalised form. |
| `card_image` / `student_selfie` | Absolute URLs, rebuilt per response like `save_card_info`'s. `null` means no image on file, or a file stored outside the served tree. |
| `registered_at` / `updated_at` | When the record was created, and when it last changed. |

### Not registered — `404`

```json
{
  "status": "error",
  "code": "card_not_found",
  "message": "No Data Found",
  "data": {}
}
```

**`data` is an empty object, not absent** — that is this endpoint's contract, so
a client can read `data` unconditionally. The HTTP status is `404`, the same one
`get_card_info` returns for the same `code`. If your client treats any non-2xx
as a transport failure, branch on `status` / `code` rather than on the status
line — "not registered" is a perfectly good answer here, not an outage.

**A student whose card was re-issued to someone else reads as not
registered.** Their record survives, images and all, with a `NULL` card number
(see [Reassigning a card](#reassigning-a-card)) — but they hold no card, so the
answer is no. Reporting them as registered would stop the desk issuing the
replacement they came for.

### Errors

| Status | `code` | Case |
|---|---|---|
| 400 | `missing_registration_no` | Absent or blank |
| 404 | `card_not_found` | This student holds no card — i.e. **not registered**, whether the number is unknown or their card was re-issued |
| 401 | — | Bad app credentials, or missing/invalid/expired token |
| 403 | — | Caller IP not allow-listed for this path |
| 500 | `internal_error` | Database error — including `attendance.nfc_card_reg_status` not being applied yet |

Every one of them carries `"data": {}`.

### Which lookup should I call?

|  | `get_card_info` | `checking_card_reg_status` |
|---|---|---|
| Question | *Who does this card belong to?* | *Does this student already hold a card?* |
| Asked with | `card_number` | `registration_no` |
| Caller | Turnstile / reader, on every tap | Registration desk, before issuing a card |
| Payload | 4 fields + 2 labels | The whole row: images, timestamps, `is_registered` |
| `data` on failure | absent | `{}` |

Both read `nfc_student_cards` and never disagree about it — they differ in which
end they come at it from, and in what the caller is entitled to see. A turnstile
has no use for a student's selfie, so it does not get one: **don't call
`checking_card_reg_status` on every tap.**

> **"Registered" is not "verified".** A card with `is_verified: 2` is still
> registered — it is taken, awaiting review. Reading an unverified record as
> "not registered" would issue the student a second card.

---

## Reassigning a card

By default, presenting a card that belongs to someone else is a **`409`**:

```json
{
  "status": "error",
  "code": "card_conflict",
  "message": "Card already assigned to another student",
  "data": { "assigned_to": "APP-2026-00123" }
}
```

`assigned_to` is what lets a desk resolve the clash without a DBA.

Re-issuing the same physical card to a new student needs
**`force_reassign=true`**, which:

1. Clears `card_number` on the current holder — their record and images stay;
   only the card is gone.
2. Writes an `unassigned` audit row against them.
3. Assigns the card to the new student.
4. Returns `data.reassigned_from` and a warning naming who lost the card.

A student left with a `NULL` card number can be given a new one normally, so the
card-loss path works end to end.

---

## Audit trail

Every write leaves a row in **`attendance.nfc_card_audit`**: `action`
(`created` / `updated` / `unassigned`), the card number, the flags,
`changed_fields`, `performed_by` (the token's `sub`) and `client_ip`.

The step logs and `attendance.ext_api_call_logs` already record the *calls*; this
table answers the question they cannot — **who held this card before, and when
did it move**. A rejected save writes nothing, so the trail never implies a
change that did not happen.

```sql
-- What happened to one student's card
SELECT created_at, action, card_number, changed_fields, performed_by, client_ip
  FROM attendance.nfc_card_audit
 WHERE student_applicant_id = 'APP-2026-00123'
 ORDER BY id;

-- Everyone who has ever held one card
SELECT created_at, student_applicant_id, action, performed_by
  FROM attendance.nfc_card_audit
 WHERE card_number = '04A1B2C3D4E5'
 ORDER BY id;
```

---

## Data model

```
attendance.nfc_student_cards
  id                    bigserial PK
  student_applicant_id  varchar(64)  UNIQUE NOT NULL   -- one card record per student
  card_number           varchar(64)  UNIQUE            -- NULL = no card; NULLs don't collide
  card_image            text                           -- filesystem path
  student_selfie        text                           -- filesystem path
  is_verified           smallint NOT NULL DEFAULT 2     CHECK (1,2)
  registration_type     smallint NOT NULL               CHECK (1,2)
  created_at / updated_at timestamptz

attendance.nfc_card_audit
  card_id → nfc_student_cards(id) ON DELETE SET NULL
  student_applicant_id, action, card_number, is_verified,
  registration_type, changed_fields text[], performed_by, client_ip, created_at
```

### `student_applicant_id` has no foreign key

The spec describes a `student_table` the applicant id points at. **No such table
exists in this database** — `ictcell.lms_student` carries `id` / `name` /
`reg_no` / `roll_no` and no applicant identifier, and applicants are not students
yet, so it could not hold them anyway.

`nfc_student_cards` is therefore itself the registry: the applicant id is an
opaque external key, and the first save for an id creates its row. The
consequence is deliberate and worth knowing: **a typo'd applicant id creates a
new card record rather than returning a 404**, and nothing here can catch it. Add
the FK — and the spec's `404 Student not found` — the day an applicant registry
lands.

---

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `NFC_UPLOAD_DIR` | `./uploads/nfc_card` | Card photos and selfies. `cards/` and `selfies/` are created beneath it. A **sibling** of `WOW_UPLOAD_DIR`, not a child. |
| `WOW_UPLOADS_SERVE_DIR` | `./uploads` | Shared with the face module; must contain `NFC_UPLOAD_DIR` or saved images have no URL. |
| `WOW_PUBLIC_BASE_URL` | request origin | Origin prefixed onto image URLs. Set it behind a reverse proxy. |
| `WOW_MAX_UPLOAD_MB` | 25 | Per-image ceiling. Shared with the face module. |
| `WOW_MAX_IMAGE_MB` | 5 | Images above this are compressed down in place after upload. |
| `WOW_LOG_DIR` | `./uploads/log` | Per-call step logs, one file per call. |
| `NFC_FACE_VERIFY_URL` | *(unset)* | Face-match endpoint for `save_card_info`, e.g. `http://10.224.224.101:8089/verify`. **Unset means every save is rejected** with 503 — the gate fails closed. |
| `NFC_FACE_VERIFY_API_KEY` | *(empty)* | Sent as `X-API-Key` to that service. A rejected key is a 503, not a 400. |
| `NFC_FACE_VERIFY_TIMEOUT_SECS` | 30 | Timeout for the match call. It bounds a save, so keep it under the proxy read timeout in front of this service. |

These are not NFC-specific but every call fails without them:

| Variable | Purpose |
|---|---|
| `EXT_APP_ID` | Must equal the `X-App-Id` header, byte for byte. |
| `EXT_APP_PASSWORD` | Must equal the `X-App-Password` header, byte for byte. **In `.env` this value is single-quoted; `dotenvy` strips the quotes, so the header takes the INNER value.** See [Troubleshooting](#401-invalid-app-id-or-password). |
| `JWT_SECRET` | Verifies the bearer token. Must be byte-identical to duerp-api's. |

---

## Deploying

```bash
psql "$DATABASE_URL" -f sql/000_ext_api_infra.sql   # allow-list rows
psql "$DATABASE_URL" -f sql/004_nfc_card.sql        # 2 tables + 5 functions
# then add the reader's real IP to all three allow-list rows (see Auth)
```

`sql/004_nfc_card.sql` is idempotent — every statement is
`CREATE ... IF NOT EXISTS` or `CREATE OR REPLACE`, so re-running it changes
nothing.

**Restart the service after rebuilding.** The route table is compiled in, so a
running process keeps serving the old methods and paths — a `404` on a route you
just added is almost always a stale binary. Note that `cargo test` does **not**
rebuild the plain binary; run `cargo build`.

### Deployment status

Applied to `lms_dev` on 2026-09-09: `attendance.nfc_student_cards`,
`attendance.nfc_card_audit`, the four `nfc_card_*` functions
(`_normalize`, `_labels`, `_get_info`, `_save_info`), and both
`ext_api_allowed_ips` rows. `dev_team` (the role the service connects as) owns
the schema's default privileges, so the grants were inherited on creation — no
separate `GRANT` was needed.

The `status`/`message` envelope and the enum labels were applied the same day,
as a re-run of `sql/004_nfc_card.sql` (`CREATE OR REPLACE`, no data change).

> **The SQL and the binary must ship together.** The envelope lives in the SQL
> functions, and the handler reads `status` to decide the HTTP code. A binary
> from before that change returns **`400` on a successful lookup** — the body is
> right, the status code is wrong, because the old code looks for a `success`
> field that no longer exists. Deploy both, and restart.

**Applied to `lms_dev` on 2026-09-16:** `attendance.nfc_card_reg_status` and
the `/ext-api/nfc-card/checking_card_reg_status` allow-list row, backing the new
third endpoint. Any other database needs the same re-run:

```sql
-- after: psql "$DATABASE_URL" -f sql/004_nfc_card.sql
SELECT proname, pg_get_function_arguments(p.oid)
  FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
 WHERE n.nspname = 'attendance' AND proname = 'nfc_card_reg_status';
 --> nfc_card_reg_status | p_registration_no character varying
SELECT endpoint, ip_address, is_active FROM attendance.ext_api_allowed_ips
 WHERE endpoint = '/ext-api/nfc-card/checking_card_reg_status';
```

> **That function is `DROP`ped and recreated, not replaced.** It first shipped
> keyed on `p_card_number`, and Postgres refuses to rename an input parameter
> through `CREATE OR REPLACE`. `sql/004_nfc_card.sql` therefore carries a
> `DROP FUNCTION IF EXISTS attendance.nfc_card_reg_status(varchar)` ahead of
> it. The file stays idempotent and nothing else depends on the function — but
> between the drop and the create, that one endpoint is a `500`. Keep the SQL
> and the binary shipping together, as ever.

Still outstanding: the allow-list rows carry `127.0.0.1`/`::1` only, so the card
reader's real IP has to be added before it can call any of the endpoints.

---

## Troubleshooting

The three auth layers fail with different messages, so the response tells you
which one you are stuck on. Work down the list in order.

| Response | Layer | Meaning |
|---|---|---|
| `{"error":"Invalid App ID or Password"}` | 1 · app credentials | Headers don't match the env values |
| `{"error":"IP address not allowed..."}` | 2 · IP allow-list | No row for this path with your IP |
| `{"message":"Missing or empty token..."}` | 3 · bearer token | No `Authorization: Bearer` header |
| `{"message":"Token has expired"}` | 3 · bearer token | Log in again |
| `{"status":"success",...}` or `card_not_found` | — | Handler reached; auth is fine |

### `401 Invalid App ID or Password`

The single most common cause: **`EXT_APP_PASSWORD` is single-quoted in `.env`,
and the quotes were copied along with the value.** `dotenvy` strips them, so the
process expects the inner value — 20 characters, not the 22 that appear in the
file. Send the value *without* the surrounding `'`.

Leave `.env` as it is; the quoting there is correct. The reliable fix is to stop
copying the secret by hand and let the shell read it:

```bash
set -a; . ./.env; set +a     # same quote handling as dotenvy
curl -s -X POST "$BASE/ext-api/nfc-card/get_card_info?card_number=04A1B2C3D4E5" \
  -H "X-App-Id: $EXT_APP_ID" \
  -H "X-App-Password: $EXT_APP_PASSWORD" \
  -H "Authorization: Bearer $TOKEN"
```

Other causes, in rough order of likelihood: a trailing space or newline in the
header value (the comparison is exact, with no trimming — see
`middleware/ext_auth_middleware.rs`); a stale process still holding the previous
`.env` (the values are read once at startup, so **editing `.env` requires a
restart**); or the header sent on the wrong request — `POST /login` does not take
these headers, only the two `/ext-api/nfc-card/*` calls do.

### `403 IP address not allowed for this endpoint`

All three NFC rows are open to every IP, so this should not be reachable on
these paths. If it is, the row is missing or `is_active = false` — the check
matches the **full path**, and each endpoint needs its own row, so a newly added
endpoint 403s until its row exists:

```sql
UPDATE attendance.ext_api_allowed_ips
   SET ip_address = ip_address || '{*}'::text[], is_active = true
 WHERE endpoint IN ('/ext-api/nfc-card/get_card_info',
                    '/ext-api/nfc-card/save_card_info',
                    '/ext-api/nfc-card/checking_card_reg_status');
```

Behind a reverse proxy the recorded IP is whatever `X-Forwarded-For` resolves
to, so the proxy must set it — otherwise every request looks like it came from
the proxy and the allow-list stops meaning anything.

### `404` with an empty body

Not an application error — the route didn't match. Either the path is wrong, or
the method is (`GET` and `POST` on `get_card_info` and
`checking_card_reg_status`, only `POST` on `save_card_info`), or **the running
binary predates the route** and needs a restart — the usual cause on
`checking_card_reg_status`, which is newer than the other two. A `404` that
carries `{"code":"card_not_found"}` is the opposite case: the handler ran, and
the card (or the student) is genuinely unknown.

### `400 card_number is required` / `400 registration_no is required` on a request that sent one

The value went somewhere the endpoint didn't look, or normalised away to
nothing. Both lookups accept their parameter in the query string, a JSON body, a
form-urlencoded body, or a multipart body — but they read **different field
names**: `card_number` for `get_card_info`, `registration_no` for
`checking_card_reg_status`. Sending one to the other is the most common cause,
and it fails with the 400 that names the parameter it wanted. On
`get_card_info`, a value of `---` or `""` also normalises to `NULL`, which
counts as absent. The step log settles it: on
failure it records the method, `Content-Type` and body byte count in the
`Steps:` section, and when the value *is* found in the body it appears as a
`body:` line under `Params:`. Absence of that line with a non-zero body size
means the encoding wasn't recognised.

### `503 face_verify_unavailable` on every save

The face-match gate got no verdict, so nothing was written. In order of how
often it is the cause:

```bash
# 1 · Is the URL configured at all? Unset = every save rejected, by design.
grep NFC_FACE_VERIFY .env

# 2 · Is the service reachable from THIS host? (the reader's network is
#     irrelevant — this service makes the call, not the reader)
curl -s -m 30 -H "X-API-Key: $NFC_FACE_VERIFY_API_KEY" "$NFC_FACE_VERIFY_URL" \
  -F "card_image=@card.jpg" -F "selfie_image=@selfie.jpg"
```

- `{"detail":"Invalid or missing API key."}` → `NFC_FACE_VERIFY_API_KEY` is
  wrong. The gate reports this as a 503 rather than a 400 on purpose: it is
  this service's credential, and no photo the student retakes will fix it.
- A hang or connection refused → the face service is down, or a firewall sits
  between it and this host. Card registration stays stopped until it is back;
  that is the fail-closed behaviour, not a bug.
- A verdict comes back fine here but saves still 503 → read the step log for
  the call (below). The `Response (AI /verify)` line holds exactly what the
  service answered this service.

A **400** `face_mismatch` or `face_not_comparable` is a different thing: the
gate worked and the photos are the problem. `face_not_comparable` usually means
no detectable face — a glare-washed card photo or a cropped selfie.

### Reading the step log for one call

Every call writes one file to `WOW_LOG_DIR`, named `{id}_{timestamp}.log`, with
the params received, each step, and the response returned. For
`get_card_info` the `{id}` is the scanned card number; for `save_card_info` and
`checking_card_reg_status` it is the applicant id / registration number, so the
two calls a desk makes about one student land next to each other.

```bash
ls -t uploads/log | head
cat "uploads/log/$(ls -t uploads/log | head -1)"
```

These files carry tokens, IPs and applicant ids, and are **deliberately not
reachable over HTTP** — `main.rs` 404s `/uploads/log` ahead of the static route.
Read them on the box, or through the gated
`POST /ext-api/wow-attendance/logs/attendance` endpoint.

---

## Deviations from the spec

| Spec | Implementation | Why |
|---|---|---|
| `get_card_info` is `GET` only | `POST` **and** `GET` | Every other endpoint in this service is POST, including read-only ones; a reader posting to all of them shouldn't special-case one path. GET is kept so the spec's documented form still works. |
| Table `student_table` with `Student Applicant ID`, `Card Number`, … | `attendance.nfc_student_cards`, snake_case | No `student_table` exists; column names with spaces would need quoting in every query. Snake_case matches the rest of the schema. |
| `404 Student not found` when the applicant id is unknown | Not implemented | There is no applicant registry to check against — see [above](#student_applicant_id-has-no-foreign-key). |
| Errors as `{ "success": false, "error": "…" }` | `{ "status": "error", "message": "…", "code": "…" }` | Superseded by a later request: this module reports outcome as `status` (`"success"`/`"error"`) with a `message`, and carries no `success` boolean and no `error` key. `code` is the stable machine-readable form — match on that, not on the prose. |
| — (not in the spec) | `checking_card_reg_status`, asked with `registration_no` | Requested later: a desk needs to know whether a student already has a card *before* it registers one, and wants the record it would be clashing with. `get_card_info` could not simply grow the payload — a turnstile calls it on every tap and should not carry images or timestamps — so it is a third endpoint over the same row, entered from the student side. |
| — (not in the spec) | `is_verified_label` / `registration_type_label` | Requested: the response carries each enum's meaning alongside the number, so a screen need not hard-code the mapping. |
| Image URLs like `.../cards/APP-2026-00123.jpg` | UUID-prefixed filenames | A deterministic name would clobber the previous image on re-save and lose the audit trail. |
| Card reassignment "silently / `force_reassign` / blocked" (open question) | `409` by default, `force_reassign=true` to override | Blocking outright would need a DBA for every re-issue; silent overwrite lets a typo'd scan quietly unassign a card. |
| `save_card_info` auth "admin/staff role" | Bearer token only | This service has no role to check — the token carries only `sub` and `exp`. See the warning under [Auth](#auth). |
| Attendance check-in on scan (open question) | Not implemented | `get_card_info` is a pure lookup; check-in stays a separate downstream call. |
| Image storage backend (open question) | Local disk under the served `uploads` tree | Matches how the face module already stores images; no S3/GCS credentials exist in this service. |
| `card_image` / `student_selfie` are just stored | Both **required**, and face-matched against each other before the row is written | Requested: storing a card photo and a selfie proves nothing if nobody compares them. The pair goes to `NFC_FACE_VERIFY_URL` and only `match: true` saves — see [the face-match gate](#the-face-match-gate). |

---

## Examples

```bash
BASE=https://api.atten.du.ac.bd
AUTH=(-H "X-App-Id: $EXT_APP_ID" -H "X-App-Password: $EXT_APP_PASSWORD" \
      -H "Authorization: Bearer $TOKEN")

# Scan lookup
curl -s "${AUTH[@]}" -X POST "$BASE/ext-api/nfc-card/get_card_info?card_number=04:A1:B2:C3:D4:E5"

# ...or with the card number in the body, any of these three:
curl -s "${AUTH[@]}" -X POST "$BASE/ext-api/nfc-card/get_card_info" \
  -H 'Content-Type: application/json' -d '{"card_number":"04A1B2C3D4E5"}'
curl -s "${AUTH[@]}" -X POST "$BASE/ext-api/nfc-card/get_card_info" \
  -d 'card_number=04A1B2C3D4E5'                       # form-urlencoded
curl -s "${AUTH[@]}" -X POST "$BASE/ext-api/nfc-card/get_card_info" \
  -F 'card_number=04A1B2C3D4E5'                       # multipart

# GET still works, for a client written against the spec
curl -s "${AUTH[@]}" "$BASE/ext-api/nfc-card/get_card_info?card_number=04A1B2C3D4E5"

# Does this student already hold a card? Asked by registration number, in any
# of the four encodings — the whole registration comes back.
curl -s "${AUTH[@]}" -X POST "$BASE/ext-api/nfc-card/checking_card_reg_status" \
  -H 'Content-Type: application/json' -d '{"registration_no":2017001010}'
curl -s "${AUTH[@]}" -X POST \
  "$BASE/ext-api/nfc-card/checking_card_reg_status?registration_no=2017001010"

# "Not registered" is a 404 carrying {"code":"card_not_found","data":{}} — the
# answer to the question, not a failure. Branch on `status`/`code`, not on 200.
curl -s -o /dev/null -w '%{http_code}\n' "${AUTH[@]}" -X POST \
  "$BASE/ext-api/nfc-card/checking_card_reg_status?registration_no=9999999999"

# Assign a card, as an admin. Both images are required on EVERY save: they
# are what the face-match gate compares before anything is written.
curl -s "${AUTH[@]}" -X POST "$BASE/ext-api/nfc-card/save_card_info" \
  -F "student_applicant_id=APP-2026-00123" \
  -F "card_number=04:A1:B2:C3:D4:E5" \
  -F "is_verified=1" \
  -F "registration_type=1" \
  -F "card_image=@card.jpg" \
  -F "student_selfie=@selfie.jpg"

# Mark an existing record verified — still needs both photos, because a save
# with no files cannot be face-checked. Sending them again is harmless: the
# upsert is on student_applicant_id, so this updates the row rather than adding one.
curl -s "${AUTH[@]}" -X POST "$BASE/ext-api/nfc-card/save_card_info" \
  -F "student_applicant_id=APP-2026-00123" \
  -F "card_number=04A1B2C3D4E5" \
  -F "is_verified=1" -F "registration_type=1" \
  -F "card_image=@card.jpg" \
  -F "student_selfie=@selfie.jpg"

# Re-issue the same physical card to a different student
curl -s "${AUTH[@]}" -X POST "$BASE/ext-api/nfc-card/save_card_info" \
  -F "student_applicant_id=APP-2026-00777" \
  -F "card_number=04A1B2C3D4E5" \
  -F "registration_type=1" \
  -F "force_reassign=true" \
  -F "card_image=@card.jpg" \
  -F "student_selfie=@selfie.jpg"

# What the face-match service is asked, in isolation — useful when a save is
# failing and you want to know whether the gate or this service is the problem.
curl -s -H "X-API-Key: $NFC_FACE_VERIFY_API_KEY" "$NFC_FACE_VERIFY_URL" \
  -F "card_image=@card.jpg" \
  -F "selfie_image=@selfie.jpg"
```
