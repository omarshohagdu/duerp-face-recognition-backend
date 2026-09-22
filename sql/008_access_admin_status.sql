-- =====================================================================
-- Account status, role retirement, and account deletion
--
-- Three things the Access Roles screen could show but not change, each of
-- which meant a DBA at a psql prompt:
--
--   * `app_users.status` — THE KILL SWITCH. sql/006 already refuses every
--     request from a non-active account (`ext_api_can_call`, reason
--     `account_inactive`), and the screen already showed the badge. Nothing
--     could set it. Tokens live ~730 hours and there is no revocation list, so
--     "stop that person now" was a manual UPDATE until this file.
--   * `roles.is_active` — NEW COLUMN. A role that is no longer handed out but
--     must keep existing: `is_system` roles cannot be deleted (duerp-api joins
--     on their keys) and a role with members cannot be deleted either, so
--     "retire" was previously expressible only by hiding the role in the
--     frontend's source.
--   * Deleting an account outright.
--
-- WHAT "INACTIVE ROLE" MEANS, and it is deliberately narrow: the role stops
-- being ASSIGNABLE. Everybody already holding it keeps exactly what they had.
--
-- That is why `ext_api_can_call` is NOT touched by this file. Deactivating a
-- role is an administrative tidying-up action — it must never become a bulk
-- revocation that silently changes what a dozen people can do. Cutting access
-- off is what `status` and the deny overrides are for, one person at a time
-- and visibly. The enforcement lives in `admin_user_set_role`, which refuses
-- to hand out an inactive role (`role_inactive`).
--
-- THE GUARDS, in the same spirit as sql/007 — each exists to stop somebody
-- locking the door behind themselves:
--   * The `admin` role cannot be deactivated. It is assignable-only-ness that
--     matters: with it retired, the last admin could never be replaced.
--   * The last active admin cannot be deactivated or deleted. Same failure as
--     demoting them, which sql/007 already refuses.
--   * Nobody can delete their own account. It is never the intended click,
--     and it is the one mistake that cannot be undone from the screen.
--
-- DELETION IS FOREVER, and more final here than it looks: no code path creates
-- `app_users` rows — they arrive by import (sql/006 §seed) — so a deleted
-- account does NOT come back when the person next signs in. They simply have
-- no account, which `ext_api_can_call` answers as `no_account`: denied. Their
-- permission overrides go with them (ON DELETE CASCADE) and any
-- `system_settings` row they touched has its `updated_by` nulled. Deactivating
-- denies them just as completely and keeps the history, which is why the UI
-- offers that first.
--
-- New audit actions: 'user_status_set', 'user_deleted', 'role_active_set'.
--
-- Requires sql/006_access_control.sql and sql/007_access_admin.sql.
-- Idempotent.
--
--     psql "$DATABASE_URL" -f sql/008_access_admin_status.sql
-- =====================================================================

-- ---------------------------------------------------------------------
-- 1 · The column
--
-- Defaults true so every existing role stays exactly as it was: this file
-- changes no behaviour on its own.
-- ---------------------------------------------------------------------

ALTER TABLE attendance.roles
    ADD COLUMN IF NOT EXISTS is_active boolean NOT NULL DEFAULT true;

COMMENT ON COLUMN attendance.roles.is_active IS
    'Assignable? false = retired: no new assignments, existing holders unaffected.';

-- ---------------------------------------------------------------------
-- 2 · Read: roles now carry `is_active`
--
-- Same shape as sql/007 §2 with one field added. Replaced whole rather than
-- patched, because CREATE OR REPLACE FUNCTION is the only way to change it and
-- a diff against sql/007 is how the two are kept comparable.
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
                   -- Retired roles are still returned: the screen lists them
                   -- behind a "show retired" toggle so one can be brought back.
                   'is_active',   ro.is_active,
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
-- 3 · Write: retire or restore a role
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_role_set_active(
    p_key       varchar,
    p_is_active boolean,
    p_actor     bigint  DEFAULT NULL,
    p_client_ip varchar DEFAULT NULL
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    v_role RECORD;
BEGIN
    IF p_is_active IS NULL THEN
        RETURN jsonb_build_object('status','error','code','invalid_value',
            'message','is_active must be true or false','data','{}'::jsonb);
    END IF;

    SELECT * INTO v_role FROM attendance.roles WHERE key = lower(btrim(coalesce(p_key,'')));
    IF NOT FOUND THEN
        RETURN jsonb_build_object('status','error','code','not_found',
            'message','No such role','data','{}'::jsonb);
    END IF;

    -- THE LOCKOUT GUARD. Retiring `admin` leaves nobody who could ever be made
    -- one again — the same dead end sql/007 refuses when the last admin is
    -- demoted, reached from a third direction.
    IF v_role.key = 'admin' AND NOT p_is_active THEN
        RETURN jsonb_build_object('status','error','code','system_role',
            'message','The admin role cannot be retired — nobody could be made an admin again',
            'data','{}'::jsonb);
    END IF;

    IF v_role.is_active = p_is_active THEN
        RETURN jsonb_build_object('status','success',
            'message', CASE WHEN p_is_active THEN 'Role already active' ELSE 'Role already retired' END,
            'data', jsonb_build_object('key', v_role.key, 'is_active', p_is_active));
    END IF;

    UPDATE attendance.roles SET is_active = p_is_active WHERE id = v_role.id;

    INSERT INTO attendance.access_admin_audit (actor, action, target, detail, client_ip)
    VALUES (p_actor, 'role_active_set', v_role.key,
            jsonb_build_object('from', v_role.is_active, 'to', p_is_active,
                               'members', (SELECT count(*) FROM attendance.app_users au
                                            WHERE au.role_id = v_role.id)),
            p_client_ip);

    RETURN jsonb_build_object('status','success',
        'message', CASE WHEN p_is_active THEN 'Role restored' ELSE 'Role retired' END,
        'data', jsonb_build_object('key', v_role.key, 'is_active', p_is_active));
END;
$function$;

COMMENT ON FUNCTION attendance.admin_role_set_active(varchar, boolean, bigint, varchar) IS
    'Retire or restore a role. Retired = not assignable; existing holders keep it.';

-- ---------------------------------------------------------------------
-- 4 · Assignment now respects it
--
-- sql/007 §8 verbatim, plus one guard. This is the ONLY place "retired" is
-- enforced, and it is enforced at the moment of handing the role out — never
-- retroactively against somebody who already holds it.
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
    v_role     RECORD;
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
        SELECT * INTO v_role FROM attendance.roles WHERE key = v_key;
        IF NOT FOUND THEN
            RETURN jsonb_build_object('status','error','code','no_such_role',
                'message','No such role','data','{}'::jsonb);
        END IF;
        v_role_id := v_role.id;

        -- Retired roles are not handed out. Re-affirming the role somebody
        -- ALREADY holds is allowed through, so that a person on a retired role
        -- is not stuck: any other edit to their row would otherwise fail.
        IF NOT v_role.is_active AND coalesce(v_old_key, '') <> v_key THEN
            RETURN jsonb_build_object('status','error','code','role_inactive',
                'message','That role is retired — restore it before assigning it',
                'data', jsonb_build_object('key', v_key));
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

COMMENT ON FUNCTION attendance.admin_user_set_role(bigint, varchar, bigint, varchar) IS
    'Assign or clear a person''s role. Refuses retired roles and the last active admin.';

-- ---------------------------------------------------------------------
-- 5 · Write: the kill switch
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_user_set_status(
    p_person_id bigint,
    p_status    varchar,
    p_actor     bigint  DEFAULT NULL,
    p_client_ip varchar DEFAULT NULL
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    v_status varchar := lower(btrim(coalesce(p_status, '')));
    v_user   RECORD;
    v_admins integer;
BEGIN
    -- Two values only. `status` is free text in the table, and the gate treats
    -- ANYTHING that is not 'active' as inactive — so a typo'd status would
    -- lock the person out just as effectively as the intended word, which is
    -- not a thing an API should let a caller do by accident.
    IF v_status NOT IN ('active', 'inactive') THEN
        RETURN jsonb_build_object('status','error','code','invalid_status',
            'message','status must be active or inactive','data','{}'::jsonb);
    END IF;

    SELECT au.id, au.person_id, au.username, au.status, r.key AS role_key
      INTO v_user
      FROM attendance.app_users au
      LEFT JOIN attendance.roles r ON r.id = au.role_id
     WHERE au.person_id = p_person_id;

    IF NOT FOUND THEN
        RETURN jsonb_build_object('status','error','code','no_account',
            'message','No account for that person id','data','{}'::jsonb);
    END IF;

    IF v_user.role_key = 'admin' AND v_status <> 'active' AND v_user.status = 'active' THEN
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

    IF v_user.status = v_status THEN
        RETURN jsonb_build_object('status','success',
            'message', format('Account already %s', v_status),
            'data', jsonb_build_object('person_id', p_person_id, 'status', v_status));
    END IF;

    UPDATE attendance.app_users SET status = v_status WHERE id = v_user.id;

    INSERT INTO attendance.access_admin_audit (actor, action, target, detail, client_ip)
    VALUES (p_actor, 'user_status_set', p_person_id::text,
            jsonb_build_object('from', v_user.status, 'to', v_status,
                               'username', v_user.username, 'role', v_user.role_key),
            p_client_ip);

    RETURN jsonb_build_object('status','success',
        'message', CASE WHEN v_status = 'active'
                        THEN 'Account activated'
                        ELSE 'Account deactivated — every request is refused from now on' END,
        'data', jsonb_build_object('person_id', p_person_id, 'status', v_status));
END;
$function$;

COMMENT ON FUNCTION attendance.admin_user_set_status(bigint, varchar, bigint, varchar) IS
    'The kill switch: active | inactive. Refuses to deactivate the last active admin.';

-- ---------------------------------------------------------------------
-- 6 · Write: delete an account
-- ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION attendance.admin_user_delete(
    p_person_id bigint,
    p_actor     bigint  DEFAULT NULL,
    p_client_ip varchar DEFAULT NULL
) RETURNS jsonb
LANGUAGE plpgsql
AS $function$
DECLARE
    v_user      RECORD;
    v_admins    integer;
    v_overrides integer;
BEGIN
    SELECT au.id, au.person_id, au.username, au.status, au.du_base_role, r.key AS role_key
      INTO v_user
      FROM attendance.app_users au
      LEFT JOIN attendance.roles r ON r.id = au.role_id
     WHERE au.person_id = p_person_id;

    IF NOT FOUND THEN
        RETURN jsonb_build_object('status','error','code','no_account',
            'message','No account for that person id','data','{}'::jsonb);
    END IF;

    -- Never your own. It is the one click on this screen with no way back:
    -- the account that could restore it is the account being deleted.
    IF p_actor IS NOT NULL AND p_actor = p_person_id THEN
        RETURN jsonb_build_object('status','error','code','self_delete',
            'message','You cannot delete your own account','data','{}'::jsonb);
    END IF;

    IF v_user.role_key = 'admin' AND v_user.status = 'active' THEN
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

    SELECT count(*) INTO v_overrides
      FROM attendance.user_permission_overrides WHERE user_id = v_user.id;

    -- The audit row goes in BEFORE the delete: it is the only record that this
    -- person ever had an account, and it must survive the row it describes.
    INSERT INTO attendance.access_admin_audit (actor, action, target, detail, client_ip)
    VALUES (p_actor, 'user_deleted', p_person_id::text,
            jsonb_build_object('username', v_user.username, 'role', v_user.role_key,
                               'status', v_user.status, 'du_base_role', v_user.du_base_role,
                               'overrides_removed', v_overrides),
            p_client_ip);

    -- Overrides go with it (ON DELETE CASCADE); `system_settings.updated_by`
    -- is nulled (ON DELETE SET NULL). Both are declared in sql/006.
    DELETE FROM attendance.app_users WHERE id = v_user.id;

    RETURN jsonb_build_object('status','success','message','Account deleted',
        'data', jsonb_build_object('person_id', p_person_id,
                                   'overrides_removed', v_overrides));
END;
$function$;

COMMENT ON FUNCTION attendance.admin_user_delete(bigint, bigint, varchar) IS
    'Delete an account and its overrides. Refuses self-deletion and the last active admin.';

-- ---------------------------------------------------------------------
-- 7 · The new endpoints, and who may reach them
--
-- Same split as sql/007 §9: roles are one permission, people another.
-- ---------------------------------------------------------------------

INSERT INTO attendance.ext_api_endpoint_permissions (endpoint, resource_key, enforce, note)
SELECT v.endpoint, v.resource_key, true, v.note
  FROM (VALUES
    ('/ext-api/access/role-active',  'admin.roles.manage', 'Retire or restore a role. Existing holders are unaffected.'),
    ('/ext-api/access/user-status',  'admin.users.manage', 'The kill switch: active | inactive.'),
    ('/ext-api/access/user-delete',  'admin.users.manage', 'Deletes an account and its overrides. Irreversible.')
  ) AS v(endpoint, resource_key, note)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_endpoint_permissions p WHERE p.endpoint = v.endpoint
 );

INSERT INTO attendance.ext_api_allowed_ips (endpoint, ip_address)
SELECT v.endpoint, '{*}'::text[]
  FROM (VALUES
    ('/ext-api/access/role-active'), ('/ext-api/access/user-status'),
    ('/ext-api/access/user-delete')
  ) AS v(endpoint)
 WHERE NOT EXISTS (
    SELECT 1 FROM attendance.ext_api_allowed_ips a WHERE a.endpoint = v.endpoint
 );

-- ---------------------------------------------------------------------
-- 8 · Retire the ERP roles nobody holds
--
-- These five came from the ERP's vocabulary and have never been assigned in
-- this service. They were previously hidden in the frontend's source, which
-- put a deployment between an administrator and a list they own; this moves
-- that decision into the data where it belongs and where it can be undone
-- from the screen.
--
-- Guarded on `members = 0` and run once (`is_active` is only flipped where it
-- is still true), so re-running this file never retires a role somebody has
-- since been given.
-- ---------------------------------------------------------------------

UPDATE attendance.roles ro
   SET is_active = false
 WHERE ro.key IN ('academic_provc', 'management_panel', 'officer', 'staff', 'student')
   AND ro.is_active
   AND NOT EXISTS (SELECT 1 FROM attendance.app_users au WHERE au.role_id = ro.id);
