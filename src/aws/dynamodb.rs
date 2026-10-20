//! DynamoDB (awsJson1_0): tables, Query, Scan, GetItem; DynamoDB JSON ↔ plain JSON.
//!
//! Items travel as DynamoDB JSON (`{"name": {"S": "Ana"}}`); the UI and the CLI show them
//! as plain JSON (`{"name": "Ana"}`). Key values typed by the user are converted back
//! using the table's attribute definitions (S, N or B).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service { endpoint: "dynamodb", signing_name: "dynamodb", target_prefix: "DynamoDB_20120810" };

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct KeyElement {
    pub attribute_name: String,
    /// `HASH` (partition) or `RANGE` (sort).
    pub key_type: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct AttributeDefinition {
    pub attribute_name: String,
    /// `S`, `N` or `B`.
    pub attribute_type: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Projection {
    pub projection_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Index {
    pub index_name: String,
    pub key_schema: Vec<KeyElement>,
    pub projection: Option<Projection>,
    pub index_status: Option<String>,
    pub item_count: Option<i64>,
    pub index_size_bytes: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct BillingModeSummary {
    pub billing_mode: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Throughput {
    pub read_capacity_units: Option<i64>,
    pub write_capacity_units: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct StreamSpecification {
    pub stream_enabled: Option<bool>,
    pub stream_view_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TableDescription {
    pub table_name: String,
    pub table_status: Option<String>,
    pub table_arn: Option<String>,
    pub key_schema: Vec<KeyElement>,
    pub attribute_definitions: Vec<AttributeDefinition>,
    pub global_secondary_indexes: Vec<Index>,
    pub local_secondary_indexes: Vec<Index>,
    pub item_count: Option<i64>,
    pub table_size_bytes: Option<i64>,
    pub billing_mode_summary: Option<BillingModeSummary>,
    pub provisioned_throughput: Option<Throughput>,
    pub stream_specification: Option<StreamSpecification>,
    pub creation_date_time: Option<f64>,
    pub deletion_protection_enabled: Option<bool>,
}

/// Partition and (optional) sort key attribute names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNames {
    pub partition: String,
    pub sort: Option<String>,
}

fn key_names(schema: &[KeyElement]) -> Option<KeyNames> {
    let partition = schema.iter().find(|k| k.key_type == "HASH")?.attribute_name.clone();
    let sort = schema.iter().find(|k| k.key_type == "RANGE").map(|k| k.attribute_name.clone());
    Some(KeyNames { partition, sort })
}

impl TableDescription {
    /// Keys of the table (`index` = None) or of one of its indexes.
    pub fn keys(&self, index: Option<&str>) -> Option<KeyNames> {
        match index {
            None => key_names(&self.key_schema),
            Some(name) => {
                let i = self.indexes().find(|(_, i)| i.index_name == name)?.1;
                key_names(&i.key_schema)
            }
        }
    }

    /// Every index with its kind (`GSI` / `LSI`).
    pub fn indexes(&self) -> impl Iterator<Item = (&'static str, &Index)> {
        self.global_secondary_indexes
            .iter()
            .map(|i| ("GSI", i))
            .chain(self.local_secondary_indexes.iter().map(|i| ("LSI", i)))
    }

    /// `S`, `N` or `B` for a key attribute (`S` when not defined).
    pub fn attr_type(&self, name: &str) -> &str {
        self.attribute_definitions
            .iter()
            .find(|a| a.attribute_name == name)
            .map_or("S", |a| a.attribute_type.as_str())
    }

    pub fn billing(&self) -> String {
        match self.billing_mode_summary.as_ref().and_then(|b| b.billing_mode.as_deref()) {
            Some("PAY_PER_REQUEST") => "on demand".into(),
            _ => match &self.provisioned_throughput {
                Some(t) => format!(
                    "provisioned ({} RCU / {} WCU)",
                    t.read_capacity_units.unwrap_or(0),
                    t.write_capacity_units.unwrap_or(0)
                ),
                None => "provisioned".into(),
            },
        }
    }

    /// `orderId (S) + createdAt (S)`.
    pub fn key_label(&self, index: Option<&str>) -> String {
        let Some(k) = self.keys(index) else { return "—".into() };
        let mut text = format!("{} ({})", k.partition, self.attr_type(&k.partition));
        if let Some(s) = &k.sort {
            text.push_str(&format!(" + {s} ({})", self.attr_type(s)));
        }
        text
    }
}

/// One page of table names, from `start` (the last name of the previous page).
pub fn list_tables(aws: &Aws, start: Option<&str>) -> Result<(Vec<String>, Option<String>)> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        table_names: Vec<String>,
        last_evaluated_table_name: Option<String>,
    }
    let mut input = json!({ "Limit": 100 });
    if let Some(s) = start {
        input["ExclusiveStartTableName"] = json!(s);
    }
    let out: Out = aws.call_json10(&SERVICE, "ListTables", &input)?;
    Ok((out.table_names, out.last_evaluated_table_name.filter(|t| !t.is_empty())))
}

pub fn describe_table(aws: &Aws, name: &str) -> Result<TableDescription> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        table: TableDescription,
    }
    let out: Out = aws.call_json10(&SERVICE, "DescribeTable", &json!({ "TableName": name }))?;
    Ok(out.table)
}

/// A condition on the sort key of a query.
#[derive(Debug, Clone, PartialEq)]
pub enum SortCondition {
    Eq(String),
    BeginsWith(String),
    Between(String, String),
    Lt(String),
    Le(String),
    Gt(String),
    Ge(String),
}

/// The operators the UI and the CLI offer, in that order.
pub const SORT_OPERATORS: [&str; 7] = ["=", "begins_with", "between", "<", "<=", ">", ">="];

impl SortCondition {
    /// From an operator of [`SORT_OPERATORS`] and its value(s).
    pub fn new(op: &str, value: &str, value2: &str) -> std::result::Result<SortCondition, String> {
        let v = value.to_string();
        Ok(match op {
            "=" => SortCondition::Eq(v),
            "begins_with" => SortCondition::BeginsWith(v),
            "between" => SortCondition::Between(v, value2.to_string()),
            "<" => SortCondition::Lt(v),
            "<=" => SortCondition::Le(v),
            ">" => SortCondition::Gt(v),
            ">=" => SortCondition::Ge(v),
            other => return Err(format!("unknown sort condition '{other}' (use one of {})", SORT_OPERATORS.join(", "))),
        })
    }

    /// `OP:VALUE`, as `dcli ddb --sk` takes it: `begins_with:2026-`, `between:a,b`, `=:x`.
    pub fn parse(text: &str) -> std::result::Result<SortCondition, String> {
        let (op, value) = text.split_once(':').ok_or("--sk takes OP:VALUE, e.g. begins_with:2026- or between:a,b")?;
        match op {
            "between" => {
                let (a, b) = value.split_once(',').ok_or("between takes two values: between:a,b")?;
                Ok(SortCondition::Between(a.to_string(), b.to_string()))
            }
            _ => SortCondition::new(op, value, ""),
        }
    }

    fn values(&self) -> Vec<&str> {
        match self {
            SortCondition::Between(a, b) => vec![a, b],
            SortCondition::Eq(v)
            | SortCondition::BeginsWith(v)
            | SortCondition::Lt(v)
            | SortCondition::Le(v)
            | SortCondition::Gt(v)
            | SortCondition::Ge(v) => vec![v],
        }
    }

    /// The expression on `#sk` (one of the few shapes the mock understands too).
    fn expression(&self) -> &'static str {
        match self {
            SortCondition::Eq(_) => "#sk = :sk",
            SortCondition::BeginsWith(_) => "begins_with(#sk, :sk)",
            SortCondition::Between(..) => "#sk BETWEEN :sk1 AND :sk2",
            SortCondition::Lt(_) => "#sk < :sk",
            SortCondition::Le(_) => "#sk <= :sk",
            SortCondition::Gt(_) => "#sk > :sk",
            SortCondition::Ge(_) => "#sk >= :sk",
        }
    }

    /// `createdAt begins_with "2026-"`, for the screen.
    pub fn describe(&self, name: &str) -> String {
        match self {
            SortCondition::Between(a, b) => format!("{name} between \"{a}\" and \"{b}\""),
            SortCondition::BeginsWith(v) => format!("{name} begins with \"{v}\""),
            SortCondition::Eq(v) => format!("{name} = \"{v}\""),
            SortCondition::Lt(v) => format!("{name} < \"{v}\""),
            SortCondition::Le(v) => format!("{name} <= \"{v}\""),
            SortCondition::Gt(v) => format!("{name} > \"{v}\""),
            SortCondition::Ge(v) => format!("{name} >= \"{v}\""),
        }
    }
}

/// A key lookup: partition key value, optional sort key condition, on the table or an index.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyQuery {
    pub index: Option<String>,
    pub partition: String,
    pub sort: Option<SortCondition>,
}

impl KeyQuery {
    /// True when it names one item of the table: the whole primary key, sort key with `=`.
    /// Those go through GetItem.
    pub fn is_get(&self, table: &TableDescription) -> bool {
        if self.index.is_some() {
            return false;
        }
        matches!(
            (table.keys(None).and_then(|k| k.sort), &self.sort),
            (None, None) | (Some(_), Some(SortCondition::Eq(_)))
        )
    }

    /// `orderId = "A-1" and createdAt begins with "2026-" · index status-index`.
    pub fn describe(&self, table: &TableDescription) -> String {
        let Some(keys) = table.keys(self.index.as_deref()) else { return "—".into() };
        let mut text = format!("{} = \"{}\"", keys.partition, self.partition);
        if let (Some(c), Some(sort)) = (&self.sort, &keys.sort) {
            text.push_str(&format!(" and {}", c.describe(sort)));
        }
        if let Some(i) = &self.index {
            text.push_str(&format!(" · index {i}"));
        }
        text
    }
}

/// A page of items (as plain JSON) and where the next one starts.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub items: Vec<Value>,
    /// DynamoDB JSON key to pass as `start` for the next page.
    pub next: Option<Value>,
    /// Items DynamoDB read for this page (more than returned when filtering).
    pub scanned: i64,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct ItemsOut {
    items: Vec<Map<String, Value>>,
    last_evaluated_key: Option<Value>,
    scanned_count: Option<i64>,
}

impl From<ItemsOut> for Page {
    fn from(out: ItemsOut) -> Page {
        let scanned = out.scanned_count.unwrap_or(out.items.len() as i64);
        Page {
            items: out.items.iter().map(item_to_plain).collect(),
            next: out.last_evaluated_key.filter(|k| k.as_object().is_some_and(|m| !m.is_empty())),
            scanned,
        }
    }
}

/// A typed key value: `{"S": "x"}`, `{"N": "12"}` or `{"B": "base64"}`.
pub fn typed(value: &str, attr_type: &str) -> std::result::Result<Value, String> {
    match attr_type {
        "N" => {
            let v = value.trim();
            if v.parse::<f64>().is_err() {
                return Err(format!("'{value}' is not a number"));
            }
            Ok(json!({ "N": v }))
        }
        "B" => {
            let v = value.trim();
            let base64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=');
            if v.is_empty() || !v.chars().all(base64) {
                return Err(format!("'{value}' is not base64 (binary keys are typed as base64)"));
            }
            Ok(json!({ "B": v }))
        }
        _ => Ok(json!({ "S": value })),
    }
}

/// Query (or GetItem, for a whole primary key) of up to `limit` items from `start`.
pub fn query(aws: &Aws, table: &TableDescription, q: &KeyQuery, limit: u32, start: Option<&Value>) -> Result<Page> {
    let invalid = |m: String| super::AwsError::Config(m);
    let keys = table.keys(q.index.as_deref()).ok_or_else(|| invalid("no key schema for that index".into()))?;
    let pk = typed(&q.partition, table.attr_type(&keys.partition)).map_err(invalid)?;

    if q.is_get(table) {
        let mut key = Map::new();
        key.insert(keys.partition.clone(), pk);
        if let (Some(sort), Some(c)) = (&keys.sort, &q.sort) {
            key.insert(sort.clone(), typed(c.values()[0], table.attr_type(sort)).map_err(invalid)?);
        }
        return Ok(Page { items: get_item(aws, &table.table_name, key)?.into_iter().collect(), next: None, scanned: 1 });
    }

    let mut names = json!({ "#pk": keys.partition });
    let mut values = json!({ ":pk": pk });
    let mut expression = "#pk = :pk".to_string();
    if let Some(c) = &q.sort {
        let sort = keys.sort.as_ref().ok_or_else(|| invalid("this key has no sort key".into()))?;
        names["#sk"] = json!(sort);
        let kind = table.attr_type(sort);
        match c {
            SortCondition::Between(a, b) => {
                values[":sk1"] = typed(a, kind).map_err(invalid)?;
                values[":sk2"] = typed(b, kind).map_err(invalid)?;
            }
            _ => values[":sk"] = typed(c.values()[0], kind).map_err(invalid)?,
        }
        expression = format!("#pk = :pk AND {}", c.expression());
    }
    let mut input = json!({
        "TableName": table.table_name,
        "KeyConditionExpression": expression,
        "ExpressionAttributeNames": names,
        "ExpressionAttributeValues": values,
        "Limit": limit,
    });
    if let Some(i) = &q.index {
        input["IndexName"] = json!(i);
    }
    if let Some(s) = start {
        input["ExclusiveStartKey"] = s.clone();
    }
    let out: ItemsOut = aws.call_json10(&SERVICE, "Query", &input)?;
    Ok(out.into())
}

/// One page of a Scan: reads the table in storage order, `limit` items at a time.
pub fn scan(aws: &Aws, table: &str, limit: u32, start: Option<&Value>) -> Result<Page> {
    let mut input = json!({ "TableName": table, "Limit": limit });
    if let Some(s) = start {
        input["ExclusiveStartKey"] = s.clone();
    }
    let out: ItemsOut = aws.call_json10(&SERVICE, "Scan", &input)?;
    Ok(out.into())
}

/// The item with this (DynamoDB JSON) key, as plain JSON.
pub fn get_item(aws: &Aws, table: &str, key: Map<String, Value>) -> Result<Option<Value>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Out {
        item: Option<Map<String, Value>>,
    }
    let out: Out = aws.call_json10(&SERVICE, "GetItem", &json!({ "TableName": table, "Key": key }))?;
    Ok(out.item.as_ref().map(item_to_plain))
}

/// A DynamoDB number as JSON: a number when it converts exactly, else the string.
fn number(n: &str) -> Value {
    let n = n.trim();
    if let Ok(i) = n.parse::<i64>() {
        return json!(i);
    }
    if let Ok(u) = n.parse::<u64>() {
        return json!(u);
    }
    // Decimals: f64 holds 15 significant digits exactly.
    let digits = n.chars().take_while(|c| *c != 'e' && *c != 'E').filter(char::is_ascii_digit).count();
    let significant = n
        .trim_start_matches(['-', '+'])
        .chars()
        .take_while(|c| *c != 'e' && *c != 'E')
        .filter(|c| *c != '.')
        .skip_while(|c| *c == '0')
        .count();
    match n.parse::<f64>() {
        Ok(f) if f.is_finite() && digits > 0 && significant <= 15 => {
            serde_json::Number::from_f64(f).map_or_else(|| json!(n), Value::Number)
        }
        _ => json!(n),
    }
}

/// One attribute value (`{"S": "x"}`, `{"M": {...}}`…) as plain JSON. Binary values stay
/// base64 strings; sets become arrays.
pub fn to_plain(value: &Value) -> Value {
    let Some((kind, v)) = value.as_object().and_then(|m| m.iter().next()) else { return value.clone() };
    match (kind.as_str(), v) {
        ("S" | "B", Value::String(s)) => json!(s),
        ("N", Value::String(n)) => number(n),
        ("BOOL", Value::Bool(b)) => json!(b),
        ("NULL", _) => Value::Null,
        ("M", Value::Object(m)) => item_to_plain(m),
        ("L", Value::Array(items)) => Value::Array(items.iter().map(to_plain).collect()),
        ("SS" | "BS", Value::Array(items)) => Value::Array(items.clone()),
        ("NS", Value::Array(items)) => Value::Array(items.iter().map(|n| n.as_str().map_or(n.clone(), number)).collect()),
        _ => value.clone(),
    }
}

/// A whole item (attribute name → DynamoDB JSON value) as a plain JSON object.
pub fn item_to_plain(item: &Map<String, Value>) -> Value {
    Value::Object(item.iter().map(|(k, v)| (k.clone(), to_plain(v))).collect())
}

/// A plain JSON value in one short line, for table cells.
pub fn cell(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_dynamodb_json_to_plain_json() {
        let item: Map<String, Value> = serde_json::from_value(json!({
            "id": {"S": "u-1"},
            "age": {"N": "42"},
            "score": {"N": "1.5"},
            "big": {"N": "123456789012345678901234567890"},
            "precise": {"N": "0.12345678901234567"},
            "active": {"BOOL": true},
            "nickname": {"NULL": true},
            "avatar": {"B": "aGVsbG8="},
            "address": {"M": {"city": {"S": "Bogotá"}, "zip": {"N": "110111"}}},
            "roles": {"L": [{"S": "admin"}, {"N": "7"}, {"M": {"x": {"BOOL": false}}}]},
            "tags": {"SS": ["a", "b"]},
            "lucky": {"NS": ["3", "7.25"]},
            "blobs": {"BS": ["AQI="]},
        }))
        .unwrap();
        assert_eq!(
            item_to_plain(&item),
            json!({
                "id": "u-1",
                "age": 42,
                "score": 1.5,
                "big": "123456789012345678901234567890",
                "precise": "0.12345678901234567",
                "active": true,
                "nickname": null,
                "avatar": "aGVsbG8=",
                "address": {"city": "Bogotá", "zip": 110111},
                "roles": ["admin", 7, {"x": false}],
                "tags": ["a", "b"],
                "lucky": [3, 7.25],
                "blobs": ["AQI="],
            })
        );
    }

    #[test]
    fn types_key_values() {
        assert_eq!(typed("x", "S").unwrap(), json!({"S": "x"}));
        assert_eq!(typed(" 12 ", "N").unwrap(), json!({"N": "12"}));
        assert!(typed("abc", "N").is_err());
        assert_eq!(typed("AQI=", "B").unwrap(), json!({"B": "AQI="}));
        assert!(typed("not base64!", "B").is_err());
    }

    #[test]
    fn parses_sort_conditions() {
        assert_eq!(SortCondition::parse("begins_with:2026-"), Ok(SortCondition::BeginsWith("2026-".into())));
        assert_eq!(SortCondition::parse("between:a,b"), Ok(SortCondition::Between("a".into(), "b".into())));
        assert_eq!(SortCondition::parse("=:x:y"), Ok(SortCondition::Eq("x:y".into())));
        assert_eq!(SortCondition::parse(">=:5"), Ok(SortCondition::Ge("5".into())));
        assert!(SortCondition::parse("between:a").is_err());
        assert!(SortCondition::parse("like:x").is_err());
        assert!(SortCondition::parse("nocolon").is_err());
    }

    fn table(sort: bool) -> TableDescription {
        let mut key_schema = vec![KeyElement { attribute_name: "pk".into(), key_type: "HASH".into() }];
        if sort {
            key_schema.push(KeyElement { attribute_name: "sk".into(), key_type: "RANGE".into() });
        }
        TableDescription {
            table_name: "t".into(),
            key_schema,
            global_secondary_indexes: vec![Index {
                index_name: "by-status".into(),
                key_schema: vec![KeyElement { attribute_name: "status".into(), key_type: "HASH".into() }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn whole_primary_keys_use_get_item() {
        let q = |index: Option<&str>, sort: Option<SortCondition>| KeyQuery {
            index: index.map(str::to_string),
            partition: "a".into(),
            sort,
        };
        assert!(q(None, None).is_get(&table(false)));
        assert!(!q(None, None).is_get(&table(true)));
        assert!(q(None, Some(SortCondition::Eq("x".into()))).is_get(&table(true)));
        assert!(!q(None, Some(SortCondition::BeginsWith("x".into()))).is_get(&table(true)));
        assert!(!q(Some("by-status"), None).is_get(&table(false)));
        assert_eq!(table(true).keys(Some("by-status")), Some(KeyNames { partition: "status".into(), sort: None }));
        assert_eq!(
            q(None, Some(SortCondition::Between("1".into(), "2".into()))).describe(&table(true)),
            "pk = \"a\" and sk between \"1\" and \"2\""
        );
    }
}
