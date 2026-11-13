-- 0041 — the platform layer store (B6a T4; plan B6 §1 B6-1..B6-5, B6-9, §2, §3). Ports strata_configurator@71aa7af
-- 0001 `layer` / `overlay_row` (+ 0008 kinds) without the tenant arm, widened to the 24 row kinds, plus the
-- immutable tags (`layer_tag` + `layer_tag_row`: a membership snapshot) and the two read views. Forward-only.
--
-- Platform data: no tenant column, no RLS. `agent_app` reads, `agent_platform` (crate::platform_db) inserts and
-- updates only the named columns, nobody but the owner deletes. A platform edit is a new `layer_row` revision
-- (flip the current one, insert rev + 1); a tag freezes the current rows of a family. The trigger messages below are
-- invariant violations (a bug in the caller), not client errors, so they are not in crate::error::KNOWN_DB_CODES.

-- A layer (spec 13 / plan §2). `key` is the web `layerLabel` (`base`, `profile/bundles`, `industry/retail`,
-- `industry_dept/retail/customer_service`, `job/customer_care_agent`, `job/customer_care_agent/channel=email`); a
-- CHECK keeps it equal to the label of the key parts. `family` is the template layer whose tag carries this layer:
-- itself for base / industry / industry_dept / job heads, the owning head for profile and job×dimension layers.
CREATE TABLE layer (
    id                 uuid PRIMARY KEY DEFAULT uuidv7(),
    key                text NOT NULL,
    scope              text NOT NULL CHECK (scope IN ('base', 'profile', 'industry', 'industry_dept', 'job')),
    industry           text CHECK (industry ~ '^[^/=[:space:]]+$'),
    department         text CHECK (department ~ '^[^/=[:space:]]+$'),
    job                text CHECK (job ~ '^[^/=[:space:]]+$'),
    rule               text CHECK (rule ~ '^[^/=[:space:]]+$'),
    dimension_key      text CHECK (dimension_key ~ '^[^/=[:space:]]+$'),
    dimension_value    text CHECK (dimension_value ~ '^[^/=[:space:]]+$'),
    family             text NOT NULL,
    -- metadata (B6-9, not versioned; the only columns agent_platform may UPDATE)
    label              text CHECK (label IS NULL OR char_length(btrim(label)) BETWEEN 1 AND 120),
    rollout_wave       smallint CHECK (rollout_wave BETWEEN 1 AND 3),
    wave_label         text CHECK (wave_label IS NULL OR char_length(btrim(wave_label)) BETWEEN 1 AND 60),
    extends            text CHECK (extends IS NULL OR extends <> ''),
    areas              text[] NOT NULL DEFAULT '{}',
    template_selection jsonb CHECK (template_selection IS NULL OR jsonb_typeof(template_selection) = 'object'),
    created_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT layer_key UNIQUE (key),
    CONSTRAINT layer_family_fk FOREIGN KEY (family) REFERENCES layer (key),
    CONSTRAINT layer_key_unique UNIQUE NULLS NOT DISTINCT
        (scope, industry, department, job, rule, dimension_key, dimension_value),
    CONSTRAINT layer_dimension_pair CHECK ((dimension_key IS NULL) = (dimension_value IS NULL)),
    -- strata_configurator 0001 `layer_scope_keys` minus the tenant arm (D8: no tenant layer).
    CONSTRAINT layer_scope_keys CHECK (
        CASE scope
            WHEN 'base' THEN industry IS NULL AND department IS NULL AND job IS NULL
                AND rule IS NULL AND dimension_key IS NULL
            WHEN 'profile' THEN rule IS NOT NULL AND industry IS NULL AND department IS NULL
                AND job IS NULL AND dimension_key IS NULL
            WHEN 'industry' THEN industry IS NOT NULL AND department IS NULL AND job IS NULL
                AND rule IS NULL AND dimension_key IS NULL
            WHEN 'industry_dept' THEN industry IS NOT NULL AND department IS NOT NULL
                AND job IS NULL AND rule IS NULL AND dimension_key IS NULL
            WHEN 'job' THEN job IS NOT NULL AND industry IS NULL AND department IS NULL
                AND rule IS NULL
            ELSE false
        END
    ),
    -- key = web layerLabel: [scope, industry, department, job, rule, dimension.join('=')].filter(Boolean).join('/')
    CONSTRAINT layer_key_label CHECK (
        key = scope || coalesce('/' || industry, '') || coalesce('/' || department, '') || coalesce('/' || job, '')
            || coalesce('/' || rule, '') || coalesce('/' || dimension_key || '=' || dimension_value, '')
    ),
    -- B6-3: heads are their own family; profile and job×dimension layers belong to another one.
    CONSTRAINT layer_family_head CHECK ((scope <> 'profile' AND dimension_key IS NULL) = (family = key)),
    CONSTRAINT layer_wave_label_scope CHECK (wave_label IS NULL OR scope = 'industry'),
    CONSTRAINT layer_extends_scope CHECK (extends IS NULL OR scope = 'industry_dept'),
    CONSTRAINT layer_areas_scope CHECK (areas = '{}' OR scope = 'industry_dept'),
    CONSTRAINT layer_template_selection_scope
        CHECK (template_selection IS NULL OR scope IN ('job', 'industry_dept'))
);
CREATE INDEX layer_family ON layer (family);

-- One revision of one layered item. Exactly one revision per (layer, kind, key) is current; a row never changes
-- except `is_current` true → false (trigger below). `fields_hash` = `sha256:<hex>` of the canonical JSON of
-- `fields` (agent-model canonical.rs; B2 `base_hash`).
CREATE TABLE layer_row (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    layer_id    uuid NOT NULL REFERENCES layer (id),
    kind        text NOT NULL,
    key         text NOT NULL CHECK (key <> ''),
    op          text NOT NULL CHECK (op IN ('add', 'patch', 'tombstone')),
    fields      jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(fields) = 'object'),
    rev         integer NOT NULL CHECK (rev >= 1),
    is_current  boolean NOT NULL,
    author      text NOT NULL CHECK (btrim(author) <> ''),
    created_at  timestamptz NOT NULL DEFAULT now(),
    fields_hash text NOT NULL CHECK (fields_hash ~ '^sha256:[0-9a-f]{64}$'),
    -- The 24 row kinds of plan §3, in one named place: B8 widens it by DROP/ADD of this name.
    CONSTRAINT layer_row_kind_check CHECK (
        kind IN (
            'entity', 'attribute', 'relation', 'job',
            'context_space', 'procedure', 'connector_slot', 'knowledge_store',
            'object', 'operation', 'error_state', 'gate_question', 'agent_role', 'profile', 'axis_rule',
            'department_archetype',
            'journey_theme', 'journey_stage', 'theme_stage_prior', 'stage_knowledge_kind', 'activity',
            'activity_object', 'operation_expectation',
            'job_setting'
        )
    ),
    CONSTRAINT layer_row_rev_unique UNIQUE (layer_id, kind, key, rev)
);
CREATE UNIQUE INDEX layer_row_current_unique ON layer_row (layer_id, kind, key) WHERE is_current;
CREATE INDEX layer_row_kind_current ON layer_row (kind, key) WHERE is_current;

-- UPDATE may only retire the current revision (is_current true → false); every other column is unchanged. Holds for
-- the owner too. Compared as the row's JSON text minus is_current, so a column added later is covered and `1` →
-- `1.0` in `fields` counts as a change.
CREATE FUNCTION layer_row_immutable() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
BEGIN
    IF OLD.is_current AND NOT NEW.is_current
       AND (to_jsonb(NEW) - 'is_current')::text = (to_jsonb(OLD) - 'is_current')::text THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'layer_row_immutable'
        USING DETAIL = format('layer_row %s: only is_current may change, from true to false', OLD.id);
END $$;
CREATE TRIGGER layer_row_immutable BEFORE UPDATE ON layer_row
    FOR EACH ROW EXECUTE FUNCTION layer_row_immutable();

-- A tag of a family (B6-4): `version` ≥ 1, or the one provisional `v0` (a generator draft, B8 replaces it).
-- `stack` = `[{layer, version}]`, the latest non-provisional tag of every family the head composes on at tag time.
-- `tagged_at` is the authored time (the seed keeps the fixture's); `created_at` is the inserting transaction's time
-- and is what the membership guard below compares against.
CREATE TABLE layer_tag (
    family       text NOT NULL REFERENCES layer (key),
    version      integer NOT NULL CHECK (version >= 0),
    content_hash text NOT NULL CHECK (content_hash ~ '^sha256:[0-9a-f]{64}$'),
    note         text NOT NULL DEFAULT '' CHECK (char_length(note) <= 2000),
    provisional  boolean NOT NULL DEFAULT false,
    stack        jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(stack) = 'array'),
    tagged_by    text NOT NULL CHECK (btrim(tagged_by) <> ''),
    tagged_at    timestamptz NOT NULL DEFAULT now(),
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (family, version),
    CONSTRAINT layer_tag_provisional_v0 CHECK (provisional = (version = 0))
);

-- A tag never changes and is never deleted, unless it is the provisional v0 (holds for the owner too).
CREATE FUNCTION layer_tag_immutable() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
BEGIN
    IF OLD.provisional THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'layer_tag_immutable'
        USING DETAIL = format('%s@v%s is a tag: it never changes', OLD.family, OLD.version);
END $$;
CREATE TRIGGER layer_tag_immutable BEFORE UPDATE OR DELETE ON layer_tag
    FOR EACH ROW EXECUTE FUNCTION layer_tag_immutable();

-- The members of a tag: every row of the family current at tag time (history rows stay members of older tags).
CREATE TABLE layer_tag_row (
    family  text NOT NULL,
    version integer NOT NULL,
    row_id  uuid NOT NULL REFERENCES layer_row (id),
    PRIMARY KEY (family, version, row_id),
    CONSTRAINT layer_tag_row_tag_fk FOREIGN KEY (family, version) REFERENCES layer_tag (family, version)
);
CREATE INDEX layer_tag_row_row ON layer_tag_row (row_id);

-- Membership is as immutable as its tag: UPDATE / DELETE only while the parent tag is provisional, and a member joins
-- a non-provisional tag only in the transaction that created that tag (so an old tag's snapshot cannot grow). A
-- member must be a row of a layer of the tag's family.
CREATE FUNCTION layer_tag_row_immutable() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
DECLARE
    prov boolean;
    made timestamptz;
    fam  text;
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        SELECT t.provisional INTO prov FROM layer_tag t WHERE t.family = OLD.family AND t.version = OLD.version;
        IF prov IS NOT TRUE THEN
            RAISE EXCEPTION 'layer_tag_immutable'
                USING DETAIL = format('%s@v%s is a tag: its members never change', OLD.family, OLD.version);
        END IF;
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
    END IF;
    SELECT t.provisional, t.created_at INTO prov, made
      FROM layer_tag t WHERE t.family = NEW.family AND t.version = NEW.version;
    -- (no such tag: the foreign key rejects the row)
    IF FOUND AND NOT prov AND made <> now() THEN
        RAISE EXCEPTION 'layer_tag_immutable'
            USING DETAIL = format('%s@v%s is a tag: members join only in the transaction that tags',
                                  NEW.family, NEW.version);
    END IF;
    SELECT l.family INTO fam FROM layer_row r JOIN layer l ON l.id = r.layer_id WHERE r.id = NEW.row_id;
    IF FOUND AND fam <> NEW.family THEN
        RAISE EXCEPTION 'layer_tag_row_family'
            USING DETAIL = format('row %s belongs to family %s, not %s', NEW.row_id, fam, NEW.family);
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER layer_tag_row_immutable BEFORE INSERT OR UPDATE OR DELETE ON layer_tag_row
    FOR EACH ROW EXECUTE FUNCTION layer_tag_row_immutable();

-- The catalogue read path: current rows with their layer. security_invoker, so the caller's own grants on the base
-- tables apply (a plain view would read with the owner's rights).
CREATE VIEW layer_head WITH (security_invoker = true) AS
    SELECT r.id, r.layer_id, l.key AS layer_key, l.family, l.scope, r.kind, r.key, r.op, r.fields, r.fields_hash,
           r.rev, r.author, r.created_at
      FROM layer_row r
      JOIN layer l ON l.id = r.layer_id
     WHERE r.is_current;

-- B6-5: current rows of a family that are not members of its latest non-provisional tag (every current row when the
-- family has no tag yet). P4 `untagged`, P6 `draft_revisions`, P10 `pending_edits`.
CREATE VIEW layer_untagged WITH (security_invoker = true) AS
    SELECT h.*
      FROM layer_head h
     WHERE NOT EXISTS (
               SELECT 1
                 FROM layer_tag_row m
                WHERE m.row_id = h.id
                  AND m.family = h.family
                  AND m.version = (SELECT max(t.version) FROM layer_tag t
                                    WHERE t.family = h.family AND NOT t.provisional));

REVOKE ALL ON layer, layer_row, layer_tag, layer_tag_row, layer_head, layer_untagged FROM agent_app;
GRANT SELECT ON layer, layer_row, layer_tag, layer_tag_row, layer_head, layer_untagged TO agent_app, agent_platform;
GRANT INSERT ON layer, layer_row, layer_tag, layer_tag_row TO agent_platform;
GRANT UPDATE (label, rollout_wave, wave_label, areas, template_selection) ON layer TO agent_platform;
GRANT UPDATE (is_current) ON layer_row TO agent_platform;
