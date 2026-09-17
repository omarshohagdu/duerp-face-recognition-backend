-- =====================================================================
-- Role administration API — the functions behind /ext-api/access/*
--
-- Design: docs/access_control.md. This is option (c) from §2.1: the roles this
-- service enforces are administered HERE, rather than in the ERP's screens
-- (which write `ictcell` and no longer reach this service).
--
-- Requires sql/006_access_control.sql — these read and write the tables it
-- creates.
--
-- WHY THE RULES ARE IN SQL, as everywhere else in this service: the handler is
-- a wrapper. A DBA at a psql prompt gets the same guards, the same audit row
-- and the same envelope the HTTP caller does, which matters more here than
-- anywhere — this is the API that hands out permissions.
--
-- THE GUARDS, and why each exists:
--   * `admin.roles.manage` cannot be taken off the `admin` role. It is the
--     permission that reaches these endpoints, so removing it locks every
--     administrator out of the tool that could put it back. One UPDATE at a
--     psql prompt is then the only way home.
--   * The last account holding `admin` cannot be demoted. Same failure, from
--     the other direction.
--   * A role with members cannot be deleted. The members would silently become
--     role-less, which reads as "denied everything" once enforcement is on.
--   * `is_system` roles keep their key. They came from the ERP and duerp-api
--     still joins on those keys; their NAME and permissions are editable.
--
-- EVERY WRITE IS AUDITED to `attendance.access_admin_audit`, with the actor.
-- "Who gave them that?" is the first question asked after an incident, and the
-- step logs answer "who called what", not "what did it change".
--
-- Idempotent: CREATE ... IF NOT EXISTS / CREATE OR REPLACE throughout.
--
--     psql "$DATABASE_URL" -f sql/007_access_admin.sql
-- =====================================================================

-- ---------------------------------------------------------------------
-- 1 · The trail
-- ---------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS attendance.access_admin_audit (
    id           bigserial   PRIMARY KEY,
    -- The token's `sub` for whoever made the change. NOT the person changed.
    actor        bigint,
    -- 'role_saved' | 'role_deleted' | 'user_role_set' | 'override_set'
    action       varchar(32) NOT NULL,
    -- What was changed: a role key, or a person id as text.
    target       text        NOT NULL,
    -- Before and after, as far as it matters: the permission sets, the old and
    -- new role, the effect. Enough to answer "what did this change" without a
    -- second table per action.
    detail       jsonb       NOT NULL DEFAULT '{}'::jsonb,
    client_ip    varchar(45),
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS access_admin_audit_created_idx
    ON attendance.access_admin_audit (created_at DESC);
CREATE INDEX IF NOT EXISTS access_admin_audit_target_idx
    ON attendance.access_admin_audit (target, created_at DESC);

COMMENT ON TABLE attendance.access_admin_audit IS
    'Every change made through /ext-api/access/*: who changed what, when, and to what.';

-- ---------------------------------------------------------------------
-- 2 · Read: roles, with their permissions and how many people hold them
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_roles_list()
RETURNS jsonb
LANGUAGE sql STABLE
AS $function$
    SELECT jsonb_build_object(
        'status',  'success',
        'message', 'Roles',
        'data', jsonb_build_object('roles', coalesce(jsonb_agg(r ORDER BY r->>'key'), '[]'::jsonb)))
      FROM (
        SELECT jsonb_build_object(
                   'id',          ro.id,
                   'key',         ro.key,
                   'name',        ro.name,
                   -- A system role's key is fixed (duerp-api joins on it); the
                   -- UI greys the field rather than letting the save fail.
                   'is_system',   ro.is_system,
                   'members',     (SELECT count(*) FROM attendance.app_users au WHERE au.role_id = ro.id),
                   'permissions', (SELECT coalesce(jsonb_agg(res.key ORDER BY res.key), '[]'::jsonb)
                                     FROM attendance.role_permissions rp
                                     JOIN attendance.resources res ON res.id = rp.resource_id
                                    WHERE rp.role_id = ro.id)
               ) AS r
          FROM attendance.roles ro
      ) roles;
$function$;

-- ---------------------------------------------------------------------
-- 3 · Read: the permission vocabulary, for the tick-boxes
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_resources_list()
RETURNS jsonb
LANGUAGE sql STABLE
AS $function$
    SELECT jsonb_build_object(
        'status',  'success',
        'message', 'Permissions',
        'data', jsonb_build_object(
            'resources', coalesce(jsonb_agg(jsonb_build_object(
                'id',       res.id,
                'key',      res.key,
                'name',     res.name,
                'category', coalesce(res.category, 'Other'),
                -- Which endpoints this key opens. The screen shows it so
                -- "nfc.card.reassign" is not a guess about what it does.
                'endpoints', (SELECT coalesce(jsonb_agg(p.endpoint ORDER BY p.endpoint), '[]'::jsonb)
                                FROM attendance.ext_api_endpoint_permissions p
                               WHERE p.resource_key = res.key AND p.is_active)
            ) ORDER BY coalesce(res.category, 'Other'), res.key), '[]'::jsonb)))
      FROM attendance.resources res;
$function$;

-- ---------------------------------------------------------------------
-- 4 · Write: create or update a role, and replace its permission set
--
-- One call for what the screen calls Save: the name and the ticked boxes
-- arrive together, so they are applied together or not at all.
--
-- `p_permissions` REPLACES the set. A missing key is a revocation — that is
-- what a tick-box screen means by unticking one — so the caller must always
-- send the full list, and the audit row records both sides.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_role_save(
    p_key         varchar,
    p_name        varchar,
    p_permissions text[],
    p_actor       bigint  DEFAULT NULL,
    p_client_ip   varchar DEFAULT NULL
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    v_key      varchar := lower(btrim(coalesce(p_key, '')));
    v_name     varchar := btrim(coalesce(p_name, ''));
    v_wanted   text[]  := coalesce(p_permissions, '{}');
    v_role     RECORD;
    v_before   text[];
    v_unknown  text[];
    v_created  boolean := false;
BEGIN
    IF v_key = '' OR v_name = '' THEN
        RETURN jsonb_build_object('status','error','code','missing_fields',
            'message','key and name are required','data','{}'::jsonb);
    END IF;

    -- The key is an identifier: it ends up in queries, in this file's guards,
    -- and in duerp-api's joins. Keep it to what an identifier may be.
    IF v_key !~ '^[a-z][a-z0-9_]{1,30}$' THEN
        RETURN jsonb_build_object('status','error','code','invalid_key',
            'message','key must be 2-31 characters: lower-case letters, digits and underscore, starting with a letter',
            'data','{}'::jsonb);
    END IF;

    -- Every key must exist. Silently dropping an unknown one would make the
    -- screen show a permission the role does not actually hold.
    SELECT coalesce(array_agg(w), '{}') INTO v_unknown
      FROM unnest(v_wanted) w
     WHERE NOT EXISTS (SELECT 1 FROM attendance.resources res WHERE res.key = w);

    IF array_length(v_unknown, 1) IS NOT NULL THEN
        RETURN jsonb_build_object('status','error','code','unknown_permission',
            'message','no such permission: ' || array_to_string(v_unknown, ', '),
            'data', jsonb_build_object('unknown', to_jsonb(v_unknown)));
    END IF;

    -- THE LOCKOUT GUARD. `admin.roles.manage` is what reaches this endpoint;
    -- taking it off `admin` leaves nobody able to give it back.
    IF v_key = 'admin' AND NOT ('admin.roles.manage' = ANY(v_wanted)) THEN
        RETURN jsonb_build_object('status','error','code','would_lock_out',
            'message','admin.roles.manage cannot be removed from the admin role — it is the permission that reaches this screen',
            'data','{}'::jsonb);
    END IF;

    SELECT * INTO v_role FROM attendance.roles WHERE key = v_key;

    IF NOT FOUND THEN
        INSERT INTO attendance.roles (key, name, is_system)
        VALUES (v_key, v_name, false)
        RETURNING * INTO v_role;
        v_created := true;
        v_before  := '{}';
    ELSE
        SELECT coalesce(array_agg(res.key ORDER BY res.key), '{}') INTO v_before
          FROM attendance.role_permissions rp
          JOIN attendance.resources res ON res.id = rp.resource_id
         WHERE rp.role_id = v_role.id;

        UPDATE attendance.roles SET name = v_name WHERE id = v_role.id;
    END IF;

    -- Replace the set: delete what is no longer wanted, add what is new. Not
    -- "delete all, re-insert", so a concurrent reader never sees a role with
    -- no permissions at all.
    DELETE FROM attendance.role_permissions rp
     USING attendance.resources res
     WHERE rp.role_id = v_role.id
       AND res.id = rp.resource_id
       AND NOT (res.key = ANY(v_wanted));

    INSERT INTO attendance.role_permissions (role_id, resource_id)
    SELECT v_role.id, res.id FROM attendance.resources res
     WHERE res.key = ANY(v_wanted)
    ON CONFLICT DO NOTHING;

    INSERT INTO attendance.access_admin_audit (actor, action, target, detail, client_ip)
    VALUES (p_actor, 'role_saved', v_key,
            jsonb_build_object('created', v_created, 'name', v_name,
                               'before', to_jsonb(v_before), 'after', to_jsonb(v_wanted)),
            p_client_ip);

    RETURN jsonb_build_object('status','success',
        'message', CASE WHEN v_created THEN 'Role created' ELSE 'Role updated' END,
        'data', jsonb_build_object('key', v_key, 'name', v_name, 'created', v_created,
                                   'permissions', to_jsonb(v_wanted)));
END;
$function$;

-- ---------------------------------------------------------------------
-- 5 · Write: delete a role
--
-- Refused while anybody holds it: their `role_id` would go NULL and they would
-- silently be denied everything the next time enforcement asked.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_role_delete(
    p_key       varchar,
    p_actor     bigint  DEFAULT NULL,
    p_client_ip varchar DEFAULT NULL
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    v_role    RECORD;
    v_members integer;
BEGIN
    SELECT * INTO v_role FROM attendance.roles WHERE key = lower(btrim(coalesce(p_key,'')));
    IF NOT FOUND THEN
        RETURN jsonb_build_object('status','error','code','not_found',
            'message','No such role','data','{}'::jsonb);
    END IF;

    IF v_role.is_system THEN
        RETURN jsonb_build_object('status','error','code','system_role',
            'message','System roles cannot be deleted — duerp-api joins on their keys','data','{}'::jsonb);
    END IF;

    SELECT count(*) INTO v_members FROM attendance.app_users WHERE role_id = v_role.id;
    IF v_members > 0 THEN
        RETURN jsonb_build_object('status','error','code','role_in_use',
            'message', format('%s account(s) still hold this role — reassign them first', v_members),
            'data', jsonb_build_object('members', v_members));
    END IF;

    DELETE FROM attendance.roles WHERE id = v_role.id;

    INSERT INTO attendance.access_admin_audit (actor, action, target, detail, client_ip)
    VALUES (p_actor, 'role_deleted', v_role.key,
            jsonb_build_object('name', v_role.name), p_client_ip);

    RETURN jsonb_build_object('status','success','message','Role deleted',
        'data', jsonb_build_object('key', v_role.key));
END;
$function$;

-- ---------------------------------------------------------------------
-- 6 · Read: the accounts, and what each one may do
--
-- Paged and searchable, because `app_users` grows with the ERP and a screen
-- that loads all of them is a screen that stops loading one day.
--
-- `effective` is what the person ACTUALLY holds — role grants, plus their
-- grant overrides, minus their deny overrides — because that is the question
-- the screen is open to answer, and working it out from two lists is how
-- mistakes get made.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_users_list(
    p_search text    DEFAULT NULL,
    p_limit  integer DEFAULT 50,
    p_offset integer DEFAULT 0
) RETURNS jsonb
LANGUAGE plpgsql STABLE
AS $function$
DECLARE
    v_search text    := nullif(btrim(coalesce(p_search, '')), '');
    v_limit  integer := least(greatest(coalesce(p_limit, 50), 1), 200);
    v_offset integer := greatest(coalesce(p_offset, 0), 0);
    v_total  bigint;
    v_rows   jsonb;
BEGIN
    SELECT count(*) INTO v_total
      FROM attendance.app_users au
     WHERE v_search IS NULL
        OR au.person_id::text ILIKE '%' || v_search || '%'
        OR coalesce(au.username, '') ILIKE '%' || v_search || '%';

    SELECT coalesce(jsonb_agg(u ORDER BY u->>'username'), '[]'::jsonb) INTO v_rows
      FROM (
        SELECT jsonb_build_object(
                   'person_id',    au.person_id,
                   'username',     au.username,
                   'du_base_role', au.du_base_role,
                   'role',         r.key,
                   'role_name',    r.name,
                   'status',       au.status,
                   'overrides',    (SELECT coalesce(jsonb_agg(jsonb_build_object(
                                               'permission', res.key, 'effect', o.effect)
                                               ORDER BY res.key), '[]'::jsonb)
                                      FROM attendance.user_permission_overrides o
                                      JOIN attendance.resources res ON res.id = o.resource_id
                                     WHERE o.user_id = au.id),
                   -- Role grants + grant overrides − deny overrides. The same
                   -- arithmetic `access_profile` and `ext_api_can_call` do, so
                   -- the screen cannot show something the gate disagrees with.
                   'effective',    (SELECT coalesce(jsonb_agg(res.key ORDER BY res.key), '[]'::jsonb)
                                      FROM attendance.resources res
                                     WHERE (EXISTS (SELECT 1 FROM attendance.role_permissions rp
                                                     WHERE rp.role_id = au.role_id AND rp.resource_id = res.id)
                                            OR EXISTS (SELECT 1 FROM attendance.user_permission_overrides o
                                                        WHERE o.user_id = au.id AND o.resource_id = res.id
                                                          AND o.effect = 'grant'))
                                       AND NOT EXISTS (SELECT 1 FROM attendance.user_permission_overrides o
                                                        WHERE o.user_id = au.id AND o.resource_id = res.id
                                                          AND o.effect = 'deny'))
               ) AS u
          FROM attendance.app_users au
          LEFT JOIN attendance.roles r ON r.id = au.role_id
         WHERE v_search IS NULL
            OR au.person_id::text ILIKE '%' || v_search || '%'
            OR coalesce(au.username, '') ILIKE '%' || v_search || '%'
         ORDER BY coalesce(au.username, au.person_id::text)
         LIMIT v_limit OFFSET v_offset
      ) users;

    RETURN jsonb_build_object('status','success','message','Accounts',
        'data', jsonb_build_object('total', v_total, 'limit', v_limit,
                                   'offset', v_offset, 'users', v_rows));
END;
$function$;

-- ---------------------------------------------------------------------
-- 7 · Write: put a person in a role, or take them out of one
--
-- `p_role_key = NULL` clears it, which is "no role" — denied everything once
-- enforcement is on, and the state every copied account is in today.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_user_set_role(
    p_person_id bigint,
    p_role_key  varchar,
    p_actor     bigint  DEFAULT NULL,
    p_client_ip varchar DEFAULT NULL
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    v_key      varchar := nullif(lower(btrim(coalesce(p_role_key, ''))), '');
    v_user     RECORD;
    v_role_id  integer;
    v_old_key  varchar;
    v_admins   integer;
BEGIN
    SELECT au.id, au.person_id, au.role_id, r.key AS role_key
      INTO v_user
      FROM attendance.app_users au
      LEFT JOIN attendance.roles r ON r.id = au.role_id
     WHERE au.person_id = p_person_id;

    IF NOT FOUND THEN
        RETURN jsonb_build_object('status','error','code','no_account',
            'message','No account for that person id','data','{}'::jsonb);
    END IF;
    v_old_key := v_user.role_key;

    IF v_key IS NOT NULL THEN
        SELECT id INTO v_role_id FROM attendance.roles WHERE key = v_key;
        IF v_role_id IS NULL THEN
            RETURN jsonb_build_object('status','error','code','no_such_role',
                'message','No such role','data','{}'::jsonb);
        END IF;
    END IF;

    -- THE OTHER LOCKOUT GUARD: demoting the last administrator leaves nobody
    -- who can promote anyone. Reported as a refusal, not fixed silently.
    IF v_old_key = 'admin' AND coalesce(v_key, '') <> 'admin' THEN
        SELECT count(*) INTO v_admins
          FROM attendance.app_users au
          JOIN attendance.roles r ON r.id = au.role_id
         WHERE r.key = 'admin' AND au.status = 'active';
        IF v_admins <= 1 THEN
            RETURN jsonb_build_object('status','error','code','last_admin',
                'message','This is the only active admin — promote somebody else first',
                'data','{}'::jsonb);
        END IF;
    END IF;

    UPDATE attendance.app_users SET role_id = v_role_id WHERE id = v_user.id;

    INSERT INTO attendance.access_admin_audit (actor, action, target, detail, client_ip)
    VALUES (p_actor, 'user_role_set', p_person_id::text,
            jsonb_build_object('from', v_old_key, 'to', v_key), p_client_ip);

    RETURN jsonb_build_object('status','success',
        'message', CASE WHEN v_key IS NULL THEN 'Role cleared' ELSE 'Role assigned' END,
        'data', jsonb_build_object('person_id', p_person_id, 'from', v_old_key, 'to', v_key));
END;
$function$;

-- ---------------------------------------------------------------------
-- 8 · Write: one person's exception to their role
--
-- `grant` gives somebody something their role lacks; `deny` takes away
-- something it has; `clear` removes the exception and lets the role decide
-- again. Deny beats the role, which is what makes "everyone in this role
-- except her" expressible.
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_user_override(
    p_person_id    bigint,
    p_resource_key varchar,
    p_effect       varchar,
    p_actor        bigint  DEFAULT NULL,
    p_client_ip    varchar DEFAULT NULL
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    v_effect   varchar := lower(btrim(coalesce(p_effect, '')));
    v_user_id  integer;
    v_res_id   integer;
    v_before   text;
BEGIN
    IF v_effect NOT IN ('grant', 'deny', 'clear') THEN
        RETURN jsonb_build_object('status','error','code','invalid_effect',
            'message','effect must be grant, deny or clear','data','{}'::jsonb);
    END IF;

    SELECT id INTO v_user_id FROM attendance.app_users WHERE person_id = p_person_id;
    IF v_user_id IS NULL THEN
        RETURN jsonb_build_object('status','error','code','no_account',
            'message','No account for that person id','data','{}'::jsonb);
    END IF;

    SELECT id INTO v_res_id FROM attendance.resources WHERE key = btrim(coalesce(p_resource_key,''));
    IF v_res_id IS NULL THEN
        RETURN jsonb_build_object('status','error','code','unknown_permission',
            'message','No such permission','data','{}'::jsonb);
    END IF;

    SELECT effect INTO v_before FROM attendance.user_permission_overrides
     WHERE user_id = v_user_id AND resource_id = v_res_id;

    IF v_effect = 'clear' THEN
        DELETE FROM attendance.user_permission_overrides
         WHERE user_id = v_user_id AND resource_id = v_res_id;
    ELSE
        INSERT INTO attendance.user_permission_overrides (user_id, resource_id, effect)
        VALUES (v_user_id, v_res_id, v_effect)
        ON CONFLICT (user_id, resource_id) DO UPDATE SET effect = EXCLUDED.effect;
    END IF;

    INSERT INTO attendance.access_admin_audit (actor, action, target, detail, client_ip)
    VALUES (p_actor, 'override_set', p_person_id::text,
            jsonb_build_object('permission', p_resource_key, 'from', v_before,
                               'to', nullif(v_effect, 'clear')),
            p_client_ip);

    RETURN jsonb_build_object('status','success','message','Override saved',
        'data', jsonb_build_object('person_id', p_person_id, 'permission', p_resource_key,
                                   'effect', nullif(v_effect, 'clear')));
END;
$function$;

COMMENT ON FUNCTION attendance.admin_role_save(varchar, varchar, text[], bigint, varchar) IS
    'Create/update a role and REPLACE its permission set. Refuses to strip admin.roles.manage from admin.';
COMMENT ON FUNCTION attendance.admin_user_set_role(bigint, varchar, bigint, varchar) IS
    'Assign or clear a person''s role. Refuses to demote the last active admin.';

-- ---------------------------------------------------------------------
-- 9 · The endpoints, and who may reach them
--
-- Seeded `enforce = true`, unlike every rule in sql/006. These endpoints have
-- no existing callers to break, and they are the API that hands out
-- permissions — the handlers refuse without the permission whatever
-- `EXT_ACCESS_CONTROL` says (src/routes/access_admin.rs), and these rows make
-- the middleware agree rather than contradict it.
--
-- Split across the two management permissions the ERP already defines, so the
-- person who may edit roles is not automatically the person who may assign
-- them to people.
-- ---------------------------------------------------------------------

INSERT INTO attendance.ext_api_endpoint_permissions (endpoint, resource_key, enforce, note)
SELECT v.endpoint, v.resource_key, true, v.note
  FROM (VALUES
    ('/ext-api/access/roles',         'admin.roles.manage', 'Access Roles screen — read.'),
    ('/ext-api/access/resources',     'admin.roles.manage', 'The permission vocabulary, for the tick-boxes.'),
    ('/ext-api/access/role-save',     'admin.roles.manage', 'Creates roles and REPLACES their permission sets.'),
    ('/ext-api/access/role-delete',   'admin.roles.manage', 'Refused while anybody holds the role.'),
    ('/ext-api/access/users',         'admin.users.manage', 'Accounts, their role, overrides and effective permissions.'),
    ('/ext-api/access/user-role',     'admin.users.manage', 'Puts a person in a role. Refuses to demote the last admin.'),
    ('/ext-api/access/user-override', 'admin.users.manage', 'One person''s exception to their role: grant | deny | clear.')
  ) AS v(endpoint, resource_key, note)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_endpoint_permissions p WHERE p.endpoint = v.endpoint
 );

-- `ExtAuthMiddleware` matches the full path against `ext_api_allowed_ips`
-- before any handler runs, so each of these needs a row or it is a 403 that
-- never reaches the permission check. Open to every IP, like the rest: an
-- administrator's browser is on whatever network they are on, and the app
-- credentials plus the bearer token plus `admin.*.manage` are the gate.
INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{*}'::text[]
  FROM (VALUES
    ('/ext-api/access/roles'), ('/ext-api/access/resources'),
    ('/ext-api/access/role-save'), ('/ext-api/access/role-delete'),
    ('/ext-api/access/users'), ('/ext-api/access/user-role'),
    ('/ext-api/access/user-override')
  ) AS v(endpoint)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_allowed_ips a WHERE a.endpoint = v.endpoint
 );
