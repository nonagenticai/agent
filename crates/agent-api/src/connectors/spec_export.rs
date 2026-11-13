//! F-36 (B7 T13a): the integration field-spec export — one row per operation × field × env, so a client's IT can see
//! what the job needs and which system and mapping answers it. The renderer is the Rust port of TS
//! `web/src/lib/connectors/spec-export.ts` (`specRows`, `specCsv`, `specMd`); [`load`] builds its [`SpecInput`] from the
//! same sources as `jobOperations`. The input has no place for a base URL, env ids, credential fields, a fingerprint
//! or a sample body, so the export can never carry them.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::model::OpRef;
use super::opkey::op_key;
use super::readiness::{RouteCell, cells_of};
use super::resolve::RouteTarget;
use super::routes_store::{self, DesignOf};
use crate::design::Registry;
use crate::error::ApiResult;

/// One expected operation with its catalogue inputs and outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecOp {
    pub object: String,
    pub verb: String,
    pub variant: String,
    pub side_effect: String,
    pub identity_level: String,
    #[serde(default)]
    pub input_fields: Vec<String>,
    #[serde(default)]
    pub output_fields: Vec<String>,
}

impl SpecOp {
    fn op_ref(&self) -> OpRef {
        OpRef {
            object: self.object.clone(),
            verb: self.verb.clone(),
            variant: self.variant.clone(),
        }
    }
}

/// One object field as the catalogue declares it (`ObjectField` subset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecField {
    pub field: String,
    #[serde(default)]
    pub data_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

/// One pinned template op of a routed system: paths only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecTemplateOp {
    pub op: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub request: String,
    #[serde(default)]
    pub field_map: Vec<SpecFieldMap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecFieldMap {
    pub path: String,
    pub field: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecOverride {
    pub op: String,
    pub field: String,
    pub path: String,
}

/// One routed system: its pinned template ops and the tenant's mapping overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecInstance {
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub template_key: String,
    pub template_version: String,
    #[serde(default)]
    pub ops: Vec<SpecTemplateOp>,
    #[serde(default)]
    pub overrides: Vec<SpecOverride>,
}

/// TS `SpecInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecInput {
    pub job: String,
    /// `draft` | `live`.
    pub of: String,
    pub ops: Vec<SpecOp>,
    /// object → its fields.
    #[serde(default)]
    pub object_fields: BTreeMap<String, Vec<SpecField>>,
    /// `object.verb{variant}@env` → the D67 cell.
    #[serde(default)]
    pub cells: BTreeMap<String, RouteCell>,
    #[serde(default)]
    pub instances: Vec<SpecInstance>,
}

/// The columns, in order (TS `SPEC_COLUMNS`).
pub const SPEC_COLUMNS: [&str; 14] = [
    "operation",
    "side_effect",
    "identity_level",
    "direction",
    "field",
    "type",
    "required",
    "description",
    "env",
    "system",
    "template",
    "mapping_path",
    "mapping_source",
    "status",
];

/// The envs in export order.
pub const SPEC_ENVS: [&str; 2] = ["test", "prod"];

/// One row: the values of [`SPEC_COLUMNS`] in order.
pub type SpecRow = [String; 14];

fn target_label(t: &RouteTarget) -> &'static str {
    match t {
        RouteTarget::Builtin => "built-in (agent)",
        RouteTarget::AgentCases => "agent_cases (fallback)",
        RouteTarget::DeepLink { .. } => "deep link",
        RouteTarget::NotOffered => "not offered",
        RouteTarget::Instance { .. } => "instance",
    }
}

/// TS `specRows`: op order, inputs then outputs, field order, `test` then `prod`.
pub fn spec_rows(input: &SpecInput) -> Vec<SpecRow> {
    let mut rows = Vec::new();
    let no_fields: Vec<SpecField> = Vec::new();
    for op in &input.ops {
        let r = op.op_ref();
        let key = op_key(&r);
        let star = op_key(&OpRef {
            object: r.object.clone(),
            verb: r.verb.clone(),
            variant: "*".to_owned(),
        });
        let fields = input.object_fields.get(&op.object).unwrap_or(&no_fields);
        for (direction, names) in [("input", &op.input_fields), ("output", &op.output_fields)] {
            for name in names {
                let def = fields.iter().find(|f| f.field == *name);
                for env in SPEC_ENVS {
                    let cell = input.cells.get(&format!("{key}@{env}"));
                    let t = cell.and_then(|c| c.target.as_ref());
                    let mut system = String::new();
                    let mut template = String::new();
                    let mut mapping_path = String::new();
                    let mut mapping_source = "unmapped";
                    match t {
                        Some(RouteTarget::Instance { instance_id, .. }) => {
                            let inst = input.instances.iter().find(|i| i.id == *instance_id);
                            system = match inst {
                                Some(i) => format!("{} ({})", i.id, i.label),
                                None => instance_id.clone(),
                            };
                            if let Some(inst) = inst {
                                template =
                                    format!("{}@{}", inst.template_key, inst.template_version);
                                let top = inst
                                    .ops
                                    .iter()
                                    .find(|o| o.op == key)
                                    .or_else(|| inst.ops.iter().find(|o| o.op == star));
                                if let Some(top) = top {
                                    if direction == "output" {
                                        let ov = inst
                                            .overrides
                                            .iter()
                                            .find(|o| o.op == top.op && o.field == *name);
                                        let fm = top.field_map.iter().find(|f| f.field == *name);
                                        if let Some(ov) = ov {
                                            mapping_path = ov.path.clone();
                                            mapping_source = "override";
                                        } else if let Some(fm) = fm {
                                            mapping_path = fm.path.clone();
                                            mapping_source = "default";
                                        }
                                    } else {
                                        let ph = format!("{{{{canonical.{name}}}}}");
                                        if top.path.contains(&ph) {
                                            mapping_path = "path".to_owned();
                                            mapping_source = "default";
                                        } else if top.request.contains(&ph) {
                                            mapping_path = "request".to_owned();
                                            mapping_source = "default";
                                        }
                                    }
                                }
                            }
                        }
                        Some(other) => system = target_label(other).to_owned(),
                        None => {}
                    }
                    rows.push([
                        key.clone(),
                        op.side_effect.clone(),
                        op.identity_level.clone(),
                        direction.to_owned(),
                        name.clone(),
                        def.map(|d| d.data_type.clone()).unwrap_or_default(),
                        def.map(|d| (if d.required { "yes" } else { "no" }).to_owned())
                            .unwrap_or_default(),
                        def.map(|d| d.description.clone()).unwrap_or_default(),
                        env.to_owned(),
                        system,
                        template,
                        mapping_path,
                        mapping_source.to_owned(),
                        cell.map_or_else(|| "unrouted".to_owned(), |c| c.state.clone()),
                    ]);
                }
            }
        }
    }
    rows
}

/// One CSV cell: formula-injection guard (a cell starting `=`, `+`, `-` or `@` gets a leading `'`), then RFC 4180
/// quoting when the value holds `"`, `,`, CR or LF.
pub fn csv_cell(v: &str) -> String {
    let s = if v.starts_with(['=', '+', '-', '@']) {
        format!("'{v}")
    } else {
        v.to_owned()
    };
    if s.contains(['"', ',', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

/// RFC 4180: header row, CRLF line ends (also after the last row). `text/csv; charset=utf-8`.
pub fn spec_csv(input: &SpecInput) -> String {
    let mut out = SPEC_COLUMNS.join(",");
    out.push_str("\r\n");
    for r in spec_rows(input) {
        let cells: Vec<String> = r.iter().map(|c| csv_cell(c)).collect();
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }
    out
}

/// One Markdown table cell: backslashes and pipes escaped, line breaks flattened to a space.
pub fn md_cell(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace("\r\n", " ")
        .replace(['\r', '\n'], " ")
}

/// The Markdown table columns: every column but the three the section heading carries.
const MD_SKIP: [usize; 3] = [0, 1, 2];

/// One section per operation (`## op`, side effect + identity level), then its table; `_No fields._` when empty.
/// `text/markdown; charset=utf-8`.
pub fn spec_md(input: &SpecInput) -> String {
    let rows = spec_rows(input);
    let cols: Vec<usize> = (0..SPEC_COLUMNS.len())
        .filter(|i| !MD_SKIP.contains(i))
        .collect();
    let mut out = format!(
        "# Integration spec: {} ({})\n",
        md_cell(&input.job),
        input.of
    );
    for op in &input.ops {
        let key = op_key(&op.op_ref());
        out.push_str(&format!(
            "\n## {}\n\nSide effect {} \u{b7} identity level {}\n\n",
            md_cell(&key),
            md_cell(&op.side_effect),
            md_cell(&op.identity_level)
        ));
        let mine: Vec<&SpecRow> = rows.iter().filter(|r| r[0] == key).collect();
        if mine.is_empty() {
            out.push_str("_No fields._\n");
            continue;
        }
        let header: Vec<&str> = cols.iter().map(|i| SPEC_COLUMNS[*i]).collect();
        out.push_str(&format!("| {} |\n", header.join(" | ")));
        let sep: Vec<&str> = cols.iter().map(|_| "---").collect();
        out.push_str(&format!("|{}|\n", sep.join("|")));
        for r in mine {
            let cells: Vec<String> = cols.iter().map(|i| md_cell(&r[*i])).collect();
            out.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
    }
    out
}

/// `<job_key>-integration-spec-<of>.<csv|md>`.
pub fn spec_filename(job: &str, of: &str, format: &str) -> String {
    format!("{job}-integration-spec-{of}.{format}")
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The [`SpecInput`] of `job` `of` the draft (working rows) or Production (its current release snapshot: the
/// expected ops of its pins, its routes and mapping overrides): the expected ops with their catalogue inputs/outputs,
/// the fields of their objects, the D67 cells, and every system a route lands on (pinned template ops + overrides).
/// A job never released has an empty Live view; the route answers 409 `no_release` before calling this.
pub async fn load(
    conn: &mut PgConnection,
    reg: &Registry,
    job: Uuid,
    of: DesignOf,
) -> ApiResult<SpecInput> {
    let view = routes_store::route_data(&mut *conn, reg, job, of).await?;
    let overrides = routes_store::overrides_of(&mut *conn, reg, &view.job.job_ref(), of).await?;
    let cells = cells_of(&view.data);
    let ops: Vec<SpecOp> = view
        .expected
        .ops
        .iter()
        .zip(view.expected.defs.iter())
        .map(|(o, d)| SpecOp {
            object: o.object.clone(),
            verb: o.verb.clone(),
            variant: o.variant.clone(),
            side_effect: o.side_effect.clone(),
            identity_level: o.identity_level.clone(),
            input_fields: strings(d.attrs.get("input_fields")),
            output_fields: strings(d.attrs.get("output_fields")),
        })
        .collect();
    let mut object_fields = BTreeMap::new();
    for o in &ops {
        if object_fields.contains_key(&o.object) {
            continue;
        }
        let fields: Vec<SpecField> = view
            .expected
            .object(&o.object)
            .and_then(|item| item.fields.get("fields"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|f| SpecField {
                        field: f["field"].as_str().unwrap_or_default().to_owned(),
                        data_type: f["data_type"].as_str().unwrap_or_default().to_owned(),
                        required: f["required"].as_bool().unwrap_or(false),
                        description: f["description"].as_str().unwrap_or_default().to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        object_fields.insert(o.object.clone(), fields);
    }
    let mut routed: Vec<String> = Vec::new();
    for r in &view.data.routes {
        if let RouteTarget::Instance { instance_id, .. } = &r.target
            && !routed.contains(instance_id)
        {
            routed.push(instance_id.clone());
        }
    }
    let instances = view
        .instances
        .iter()
        .filter(|i| routed.contains(&i.id))
        .map(|i| SpecInstance {
            id: i.id.clone(),
            label: i.label.clone(),
            template_key: i.template_key.clone(),
            template_version: i.template_version.clone(),
            ops: view
                .templates
                .get(&i.id)
                .map(|t| {
                    t.ops
                        .iter()
                        .map(|o| SpecTemplateOp {
                            op: op_key(&o.op_ref()),
                            path: o.path.clone(),
                            request: o.request.clone(),
                            field_map: o
                                .field_map
                                .iter()
                                .map(|f| SpecFieldMap {
                                    path: f.path.clone(),
                                    field: f.field.clone(),
                                })
                                .collect(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            overrides: overrides
                .iter()
                .filter(|o| o.instance_id == i.id)
                .map(|o| SpecOverride {
                    op: o.op.clone(),
                    field: o.field.clone(),
                    path: o.path.clone(),
                })
                .collect(),
        })
        .collect();
    Ok(SpecInput {
        job: view.job.key.clone(),
        of: match of {
            DesignOf::Draft => "draft",
            DesignOf::Live => "live",
        }
        .to_owned(),
        ops,
        object_fields,
        cells,
        instances,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_guards_formulas_and_quotes() {
        assert_eq!(csv_cell("=1+1"), "'=1+1");
        assert_eq!(csv_cell("+1"), "'+1");
        assert_eq!(csv_cell("-2"), "'-2");
        assert_eq!(csv_cell("@x"), "'@x");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("=\"x\""), "\"'=\"\"x\"\"\"");
        assert_eq!(csv_cell("plain"), "plain");
    }

    #[test]
    fn md_cells_escape_pipes_and_flatten_lines() {
        assert_eq!(md_cell("a|b\\c"), "a\\|b\\\\c");
        assert_eq!(md_cell("one\r\ntwo\rthree\nfour"), "one two three four");
    }
}
