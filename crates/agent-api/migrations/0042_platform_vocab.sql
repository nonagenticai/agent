-- 0042 — the platform vocabularies (B6a T4; plan B6 §1 B6-8, §2). Ports strata_configurator@71aa7af 0001 `axis`,
-- `axis_value`, `axis_rule`, `dimension`, `dimension_value`, `seed_version` without the tenant columns (D8), plus the
-- read-only seed tables P9/P12/P5 show. Forward-only.
--
-- Vocabularies are insert-only and never deleted, so every tag stays resolvable: `agent_app` reads, `agent_platform`
-- inserts, the only UPDATE is `axis_rule (priority)`, nobody but the owner deletes. Not tagged (B6-8).

-- A business-model axis (P12). `object` = the object it classifies (Party, Offering, Order, …).
CREATE TABLE axis (
    key         text PRIMARY KEY CHECK (key <> ''),
    label       text NOT NULL CHECK (btrim(label) <> ''),
    description text NOT NULL DEFAULT '',
    cardinality text NOT NULL CHECK (cardinality IN ('one', 'many')),
    object      text NOT NULL CHECK (object <> ''),
    milestone   text NOT NULL CHECK (milestone IN ('M1', 'M4')),
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- A value of an axis; `key` = `parent.leaf` (UI rule, checked by the API), `parent` another value of the same axis.
CREATE TABLE axis_value (
    axis     text NOT NULL REFERENCES axis (key),
    key      text NOT NULL CHECK (key <> ''),
    label    text NOT NULL CHECK (btrim(label) <> ''),
    parent   text,
    added_at timestamptz NOT NULL DEFAULT now(),
    added_by text NOT NULL CHECK (btrim(added_by) <> ''),
    PRIMARY KEY (axis, key),
    CONSTRAINT axis_value_parent_fk FOREIGN KEY (axis, parent)
        REFERENCES axis_value (axis, key) DEFERRABLE INITIALLY DEFERRED
);

-- An axis rule's definition (its added rows live in its `profile/<key>` layer; an `axis_rule` layer_row in the
-- `owner` family carries it into that family's tags). `condition` = UI AxisCondition JSON.
CREATE TABLE axis_rule (
    key        text PRIMARY KEY CHECK (key <> ''),
    priority   integer NOT NULL,
    condition  jsonb NOT NULL CHECK (jsonb_typeof(condition) = 'object'),
    layer_id   uuid NOT NULL UNIQUE REFERENCES layer (id),
    owner      text NOT NULL DEFAULT 'base' REFERENCES layer (key),
    created_by text NOT NULL CHECK (btrim(created_by) <> ''),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- A standard dimension (P13, D59): `dimension` switches layers on (channel), `grouping` = areas, `placeholder`
-- selects nothing (region). `concerns` = the D59 region split (read-only seed, RegionConcern[]).
CREATE TABLE dimension (
    key         text PRIMARY KEY CHECK (key <> ''),
    label       text NOT NULL CHECK (btrim(label) <> ''),
    description text NOT NULL DEFAULT '',
    kind        text NOT NULL DEFAULT 'dimension' CHECK (kind IN ('dimension', 'grouping', 'placeholder')),
    concerns    jsonb CHECK (concerns IS NULL OR jsonb_typeof(concerns) = 'array'),
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- A standard value of a dimension (no tenant column, D8).
CREATE TABLE dimension_value (
    dimension text NOT NULL REFERENCES dimension (key),
    key       text NOT NULL CHECK (key <> ''),
    label     text NOT NULL CHECK (btrim(label) <> ''),
    note      text,
    added_at  timestamptz NOT NULL DEFAULT now(),
    added_by  text NOT NULL CHECK (btrim(added_by) <> ''),
    PRIMARY KEY (dimension, key)
);

-- Seed only (P12 renewal presets).
CREATE TABLE renewal_preset (
    key        text PRIMARY KEY CHECK (key <> ''),
    label      text NOT NULL CHECK (btrim(label) <> ''),
    example    text NOT NULL DEFAULT '',
    order_type text NOT NULL CHECK (order_type <> ''),
    initiator  text NOT NULL CHECK (initiator <> ''),
    mode       text
);

-- Seed only (P9 "folded": concepts that are deliberately not objects, 11 §2.1).
CREATE TABLE folded_concept (
    concept text PRIMARY KEY CHECK (concept <> ''),
    becomes text NOT NULL,
    object  text NOT NULL,
    cite    text NOT NULL
);

-- Seed only (P5 template starting points; references to live releases are derived, plan §5). `job` NULL = a
-- reference without a job slice (the fixture's `base-only` and `retail-ontology` compile the ontology only).
CREATE TABLE compile_reference_seed (
    key       text PRIMARY KEY CHECK (key <> ''),
    label     text NOT NULL CHECK (btrim(label) <> ''),
    note      text NOT NULL DEFAULT '',
    selection jsonb NOT NULL CHECK (jsonb_typeof(selection) = 'object'),
    job       text CHECK (job IS NULL OR job <> '')
);

-- The platform seed versions applied by the boot seeder (strata_configurator `seed_version`, renamed).
CREATE TABLE platform_seed_version (
    version    integer PRIMARY KEY CHECK (version >= 1),
    sha256     text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    applied_at timestamptz NOT NULL DEFAULT now()
);

REVOKE ALL ON axis, axis_value, axis_rule, dimension, dimension_value, renewal_preset, folded_concept,
    compile_reference_seed, platform_seed_version FROM agent_app;
GRANT SELECT ON axis, axis_value, axis_rule, dimension, dimension_value, renewal_preset, folded_concept,
    compile_reference_seed, platform_seed_version TO agent_app, agent_platform;
GRANT INSERT ON axis, axis_value, axis_rule, dimension, dimension_value, renewal_preset, folded_concept,
    compile_reference_seed, platform_seed_version TO agent_platform;
GRANT UPDATE (priority) ON axis_rule TO agent_platform;
