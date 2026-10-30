//! Route 53 (restXml, one global endpoint signed for us-east-1): hosted zones, their record
//! sets, creating and editing records, and following a change until every Route 53 name
//! server answers with it. Alias targets are gathered from CloudFront, load balancers, API
//! Gateway custom domains and the zone's own records.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use super::xml::{El, children};
use super::{Aws, Result, Service, apigateway};
use crate::util::{sanitize, xml_escape};

pub const SERVICE: Service = Service { endpoint: "route53", signing_name: "route53", target_prefix: "" };
const CLOUDFRONT: Service = Service { endpoint: "cloudfront", signing_name: "cloudfront", target_prefix: "" };
const ELB: Service = Service { endpoint: "elasticloadbalancing", signing_name: "elasticloadbalancing", target_prefix: "" };
const API: &str = "2013-04-01";
/// The hosted zone of every CloudFront distribution.
pub const CLOUDFRONT_ZONE: &str = "Z2FDTNDATAQYW2";
/// Pages read at most by a listing.
const MAX_PAGES: usize = 50;
pub const DEFAULT_TTL: i64 = 300;

/// Record types dcli creates, with what each one holds.
pub const TYPES: [(&str, &str); 9] = [
    ("A", "IPv4 addresses"),
    ("AAAA", "IPv6 addresses"),
    ("CNAME", "another domain name (one, not at the zone apex)"),
    ("TXT", "text: domain verification, SPF, DKIM"),
    ("MX", "mail servers: priority and host"),
    ("SRV", "services: priority, weight, port and host"),
    ("CAA", "certificate authorities allowed to issue"),
    ("NS", "name servers of a delegated subdomain"),
    ("PTR", "the name an address points back to"),
];

/// Route 53 and CloudFront answer on one host, signed for us-east-1 (cn-northwest-1 in China).
fn global(aws: &Aws) -> Arc<Aws> {
    aws.in_region(if aws.region.starts_with("cn-") { "cn-northwest-1" } else { "us-east-1" })
}

fn get(aws: &Aws, svc: &Service, segments: &[&str], query: &[(String, String)]) -> Result<String> {
    let resp = aws.raw(svc, "GET", segments, query, Vec::new(), Vec::new())?;
    Ok(String::from_utf8_lossy(&resp.body).into_owned())
}

/// A response's root element.
fn root<'a>(xml: &'a str, name: &str) -> El<'a> {
    El::new(xml).child(name).unwrap_or_else(|| El::new(""))
}

/// Route 53 writes characters other than a–z, 0–9, `-`, `_` and `.` as `\` and three octal
/// digits (`\052` for `*`).
pub fn unescape_name(name: &str) -> String {
    let b = name.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b.get(i + 1..i + 4).filter(|d| b[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal.and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok()) {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    sanitize(&String::from_utf8_lossy(&out))
}

// ---------------------------------------------------------------------------
// Hosted zones
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Zone {
    /// Without the `/hostedzone/` prefix.
    pub id: String,
    /// With the final dot: `example.com.`.
    pub name: String,
    pub private: bool,
    pub comment: Option<String>,
    pub records: i64,
    /// The service that manages it (Cloud Map, for one).
    pub managed_by: Option<String>,
}

impl Zone {
    pub fn kind(&self) -> &'static str {
        if self.private { "private" } else { "public" }
    }

    /// The name as people write it, without the final dot.
    pub fn domain(&self) -> &str {
        self.name.trim_end_matches('.')
    }
}

#[derive(Debug, Clone, Default)]
pub struct ZoneDetail {
    pub name_servers: Vec<String>,
    /// Private zones: (region, VPC id).
    pub vpcs: Vec<(String, String)>,
}

pub fn list_zones(aws: &Aws) -> Result<Vec<Zone>> {
    let aws = global(aws);
    let mut out = Vec::new();
    let mut marker: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut query = vec![("maxitems".to_string(), "100".to_string())];
        if let Some(m) = &marker {
            query.push(("marker".to_string(), m.clone()));
        }
        let xml = get(&aws, &SERVICE, &[API, "hostedzone"], &query)?;
        let list = root(&xml, "ListHostedZonesResponse");
        out.extend(list.list("HostedZones").iter().map(parse_zone));
        marker = if list.flag("IsTruncated") { list.text("NextMarker") } else { None };
        if marker.is_none() {
            break;
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.private.cmp(&b.private)));
    Ok(out)
}

fn parse_zone(z: &El) -> Zone {
    let config = z.child("Config");
    Zone {
        id: z.string("Id").trim_start_matches("/hostedzone/").to_string(),
        name: unescape_name(&z.string("Name")),
        private: config.as_ref().is_some_and(|c| c.flag("PrivateZone")),
        comment: config.and_then(|c| c.text("Comment")),
        records: z.num("ResourceRecordSetCount").unwrap_or(0),
        managed_by: z.child("LinkedService").and_then(|l| l.text("ServicePrincipal")),
    }
}

pub fn zone_detail(aws: &Aws, zone_id: &str) -> Result<ZoneDetail> {
    let xml = get(&global(aws), &SERVICE, &[API, "hostedzone", zone_id], &[])?;
    let zone = root(&xml, "GetHostedZoneResponse");
    Ok(ZoneDetail {
        name_servers: zone.child("DelegationSet").map(|d| d.strings("NameServers")).unwrap_or_default(),
        vpcs: zone.list("VPCs").iter().map(|v| (v.string("VPCRegion"), v.string("VPCId"))).collect(),
    })
}

/// The zone named `query` (with or without the final dot, any case) or with that id. Public
/// and private zones may share a name; then only the id tells them apart.
pub fn find_zone<'a>(zones: &'a [Zone], query: &str) -> std::result::Result<&'a Zone, String> {
    let q = query.trim().trim_start_matches("/hostedzone/");
    if let Some(z) = zones.iter().find(|z| z.id == q) {
        return Ok(z);
    }
    let name = format!("{}.", q.trim_end_matches('.').to_lowercase());
    let found: Vec<&Zone> = zones.iter().filter(|z| z.name.to_lowercase() == name).collect();
    match found.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no hosted zone {q}")),
        many => Err(format!(
            "{} hosted zones are named {name}: use the id ({})",
            many.len(),
            many.iter().map(|z| format!("{} {}", z.id, z.kind())).collect::<Vec<_>>().join(", ")
        )),
    }
}

pub fn console_url(zone_id: &str) -> String {
    format!("https://us-east-1.console.aws.amazon.com/route53/v2/hostedzones#ListRecordSets/{zone_id}")
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Alias {
    pub zone_id: String,
    /// With the final dot, as Route 53 keeps it.
    pub dns_name: String,
    pub evaluate_health: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Record {
    /// Unescaped (`*.example.com.`), with the final dot.
    pub name: String,
    pub kind: String,
    pub ttl: Option<i64>,
    pub values: Vec<String>,
    pub alias: Option<Alias>,
    /// Tells apart records that share a name and type under a routing policy.
    pub set_id: Option<String>,
    /// A routing policy other than simple: `weighted 10`, `failover PRIMARY`, …
    pub routing: Option<String>,
    pub health_check: Option<String>,
    /// Created by a traffic policy, which owns it.
    pub traffic_policy: Option<String>,
    /// The record as Route 53 sent it. Deleting a record must name it exactly, so an edit
    /// sends this back unchanged, and fails if the record changed since it was read.
    raw: String,
}

impl Record {
    /// What the record answers, on one line.
    pub fn summary(&self) -> String {
        match &self.alias {
            Some(a) => format!("alias → {}", a.dns_name.trim_end_matches('.')),
            None => self.values.join(", "),
        }
    }

    /// The name relative to the zone: `api`, or `@` for the apex.
    pub fn relative(&self, zone: &str) -> String {
        relative(&self.name, zone)
    }

    /// Why dcli does not edit it, if it does not.
    pub fn locked(&self, zone: &str) -> Option<&'static str> {
        if self.kind == "SOA" || (self.kind == "NS" && self.name.eq_ignore_ascii_case(zone)) {
            return Some("Route 53 manages the zone's SOA and NS records");
        }
        self.traffic_policy.is_some().then_some("A traffic policy manages this record")
    }

    /// The same record answering `answer`: routing policy, health check and the rest as read.
    fn with_answer(&self, answer: &Answer) -> String {
        let mut out = String::new();
        let mut placed = false;
        for (tag, raw) in children(&self.raw) {
            if matches!(tag, "TTL" | "ResourceRecords" | "AliasTarget") {
                if !placed {
                    out.push_str(&answer.xml());
                    placed = true;
                }
                continue;
            }
            out.push_str(&format!("<{tag}>{raw}</{tag}>"));
        }
        if !placed {
            out.push_str(&answer.xml());
        }
        out
    }

    #[cfg(test)]
    pub fn sample(name: &str, kind: &str, ttl: i64, values: &[&str]) -> Record {
        let answer = Answer::Values { ttl, values: values.iter().map(|v| v.to_string()).collect() };
        parse_record(&format!("<Name>{}</Name><Type>{kind}</Type>{}", xml_escape(name), answer.xml()))
    }
}

/// `api.example.com.` in `example.com.` → `api`; the apex is `@`.
pub fn relative(name: &str, zone: &str) -> String {
    let (n, z) = (name.to_lowercase(), zone.to_lowercase());
    if n == z {
        return "@".into();
    }
    match n.strip_suffix(&format!(".{z}")) {
        Some(r) => name[..r.len()].to_string(),
        None => name.to_string(),
    }
}

fn parse_record(raw: &str) -> Record {
    let r = El::new(raw);
    let routing = if let Some(w) = r.num::<i64>("Weight") {
        Some(format!("weighted {w}"))
    } else if let Some(f) = r.text("Failover") {
        Some(format!("failover {f}"))
    } else if let Some(region) = r.text("Region") {
        Some(format!("latency {region}"))
    } else if let Some(g) = r.child("GeoLocation") {
        let place = [g.text("CountryCode"), g.text("SubdivisionCode")].into_iter().flatten().collect::<Vec<_>>().join("-");
        Some(format!("geolocation {}", if place.is_empty() { g.string("ContinentCode") } else { place }))
    } else if let Some(g) = r.child("GeoProximityLocation") {
        let place = g.text("AWSRegion").or_else(|| g.text("LocalZoneGroup")).unwrap_or_else(|| "coordinates".into());
        Some(format!("geoproximity {place}"))
    } else if let Some(c) = r.child("CidrRoutingConfig") {
        Some(format!("ip-based {}", c.string("LocationName")))
    } else if r.flag("MultiValueAnswer") {
        Some("multivalue".into())
    } else {
        None
    };
    Record {
        name: unescape_name(&r.string("Name")),
        kind: r.string("Type"),
        ttl: r.num("TTL"),
        values: r.list("ResourceRecords").iter().map(|v| v.string("Value")).collect(),
        alias: r.child("AliasTarget").map(|a| Alias {
            zone_id: a.string("HostedZoneId"),
            dns_name: unescape_name(&a.string("DNSName")),
            evaluate_health: a.flag("EvaluateTargetHealth"),
        }),
        set_id: r.text("SetIdentifier"),
        routing,
        health_check: r.text("HealthCheckId"),
        traffic_policy: r.text("TrafficPolicyInstanceId"),
        raw: raw.to_string(),
    }
}

/// Where a listing of records goes on: the next record's name, type and set identifier.
#[derive(Debug, Clone)]
pub struct Next {
    name: String,
    kind: String,
    set_id: Option<String>,
}

/// One page of a zone's records (300, Route 53's maximum), in Route 53's order.
pub fn list_records(aws: &Aws, zone_id: &str, from: Option<&Next>) -> Result<(Vec<Record>, Option<Next>)> {
    let mut query = vec![("maxitems".to_string(), "300".to_string())];
    if let Some(n) = from {
        query.push(("name".to_string(), n.name.clone()));
        query.push(("type".to_string(), n.kind.clone()));
        if let Some(id) = &n.set_id {
            query.push(("identifier".to_string(), id.clone()));
        }
    }
    let xml = get(&global(aws), &SERVICE, &[API, "hostedzone", zone_id, "rrset"], &query)?;
    let list = root(&xml, "ListResourceRecordSetsResponse");
    let records = list.raw("ResourceRecordSets").map(|x| children(x).into_iter().map(|(_, raw)| parse_record(raw)).collect()).unwrap_or_default();
    let next = match (list.flag("IsTruncated"), list.text("NextRecordName"), list.text("NextRecordType")) {
        (true, Some(name), Some(kind)) => Some(Next { name, kind, set_id: list.text("NextRecordIdentifier") }),
        _ => None,
    };
    Ok((records, next))
}

/// Every record of a zone, up to `max`.
pub fn all_records(aws: &Aws, zone_id: &str, max: usize) -> Result<Vec<Record>> {
    let (mut all, mut next) = (Vec::new(), None::<Next>);
    loop {
        let (page, more) = list_records(aws, zone_id, next.as_ref())?;
        all.extend(page);
        match more {
            Some(n) if all.len() < max => next = Some(n),
            _ => return Ok(all),
        }
    }
}

/// The record named `name` of type `kind` (and set identifier `set_id`, under a routing policy).
pub fn find_record<'a>(records: &'a [Record], name: &str, kind: &str, set_id: Option<&str>) -> Option<&'a Record> {
    records.iter().find(|r| r.name.eq_ignore_ascii_case(name) && r.kind == kind && r.set_id.as_deref() == set_id)
}

// ---------------------------------------------------------------------------
// Changes
// ---------------------------------------------------------------------------

/// What a record answers: values with a TTL, or an alias to another name.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Values { ttl: i64, values: Vec<String> },
    Alias(Alias),
}

impl Answer {
    fn xml(&self) -> String {
        match self {
            Answer::Values { ttl, values } => {
                let records: String =
                    values.iter().map(|v| format!("<ResourceRecord><Value>{}</Value></ResourceRecord>", xml_escape(v))).collect();
                format!("<TTL>{ttl}</TTL><ResourceRecords>{records}</ResourceRecords>")
            }
            Answer::Alias(a) => format!(
                "<AliasTarget><HostedZoneId>{}</HostedZoneId><DNSName>{}</DNSName><EvaluateTargetHealth>{}</EvaluateTargetHealth></AliasTarget>",
                xml_escape(&a.zone_id),
                xml_escape(&a.dns_name),
                a.evaluate_health
            ),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Answer::Values { ttl, values } => format!("{} · TTL {ttl}", values.join(", ")),
            Answer::Alias(a) => {
                format!("alias → {}{}", a.dns_name.trim_end_matches('.'), if a.evaluate_health { " · evaluates health" } else { "" })
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Change {
    /// Without the `/change/` prefix.
    pub id: String,
    /// PENDING, then INSYNC once every Route 53 name server answers with it.
    pub status: String,
}

impl Change {
    pub fn in_sync(&self) -> bool {
        self.status == "INSYNC"
    }
}

fn parse_change(xml: &str, response: &str) -> Change {
    let info = root(xml, response).child("ChangeInfo").unwrap_or_else(|| El::new(""));
    Change { id: info.string("Id").trim_start_matches("/change/").to_string(), status: info.string("Status") }
}

/// Creates record `name` of type `kind`. Route 53 refuses it if the record exists: creating
/// never overwrites.
pub fn create_record(aws: &Aws, zone_id: &str, name: &str, kind: &str, answer: &Answer) -> Result<Change> {
    let set = format!("<Name>{}</Name><Type>{}</Type>{}", xml_escape(name), xml_escape(kind), answer.xml());
    change(aws, zone_id, &[("CREATE", set)], "Created with dcli")
}

/// Changes what `old` answers, in one atomic change: Route 53 deletes `old` exactly as it was
/// read and creates it again answering `answer`, with its routing policy and health check. If
/// the record changed meanwhile the delete does not match and nothing changes.
pub fn edit_record(aws: &Aws, zone_id: &str, old: &Record, answer: &Answer) -> Result<Change> {
    change(aws, zone_id, &[("DELETE", old.raw.clone()), ("CREATE", old.with_answer(answer))], "Edited with dcli")
}

fn change(aws: &Aws, zone_id: &str, changes: &[(&str, String)], comment: &str) -> Result<Change> {
    let changes: String = changes
        .iter()
        .map(|(action, set)| format!("<Change><Action>{action}</Action><ResourceRecordSet>{set}</ResourceRecordSet></Change>"))
        .collect();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ChangeResourceRecordSetsRequest xmlns=\"https://route53.amazonaws.com/doc/{API}/\">\
         <ChangeBatch><Comment>{}</Comment><Changes>{changes}</Changes></ChangeBatch></ChangeResourceRecordSetsRequest>",
        xml_escape(comment)
    );
    let headers = vec![("content-type".to_string(), "application/xml".to_string())];
    // Never sent twice: a create that ran would then fail as existing, an edit as changed.
    let resp = global(aws).raw_once(&SERVICE, "POST", &[API, "hostedzone", zone_id, "rrset", ""], headers, body.into_bytes())?;
    Ok(parse_change(&String::from_utf8_lossy(&resp.body), "ChangeResourceRecordSetsResponse"))
}

pub fn change_status(aws: &Aws, change_id: &str) -> Result<Change> {
    let xml = get(&global(aws), &SERVICE, &[API, "change", change_id], &[])?;
    Ok(parse_change(&xml, "GetChangeResponse"))
}

// ---------------------------------------------------------------------------
// Checking what was typed
// ---------------------------------------------------------------------------

/// `api`, `api.example.com` or `api.example.com.` in zone `example.com.` → `api.example.com.`;
/// empty or `@` is the zone apex. Lowercased, as Route 53 keeps names.
pub fn full_name(input: &str, zone: &str) -> std::result::Result<String, String> {
    let input = input.trim().to_lowercase();
    let zone = zone.to_lowercase();
    let bare = zone.trim_end_matches('.');
    let name = if input.is_empty() || input == "@" {
        zone.clone()
    } else if input.ends_with('.') {
        input
    } else if input == bare || input.ends_with(&format!(".{bare}")) {
        format!("{input}.")
    } else {
        format!("{input}.{zone}")
    };
    if name != zone && !name.ends_with(&format!(".{zone}")) {
        return Err(format!("{name} is not in the zone {zone}"));
    }
    if name.len() > 255 {
        return Err("Names have at most 255 characters".into());
    }
    for (i, label) in name.trim_end_matches('.').split('.').enumerate() {
        if label.is_empty() {
            return Err(format!("{name} has an empty label"));
        }
        if label.len() > 63 {
            return Err(format!("{label}: labels have at most 63 characters"));
        }
        let ok = (label == "*" && i == 0) || label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return Err(format!("{label}: use letters, digits, - and _ (and * alone as the first label)"));
        }
    }
    Ok(name)
}

/// What was typed, as values: commas (or line breaks) separate them, except inside quotes.
pub fn split_values(text: &str) -> Vec<String> {
    let (mut out, mut current, mut quoted, mut escaped) = (Vec::new(), String::new(), false, false);
    for c in text.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' | '\n' | '\r' if !quoted => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    out.push(current);
    out.into_iter().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect()
}

pub fn check_ttl(text: &str) -> std::result::Result<i64, String> {
    match text.trim().parse::<i64>() {
        Ok(t) if (0..=2_147_483_647).contains(&t) => Ok(t),
        _ => Err(format!("TTL: seconds, 0 to 2147483647 (not {})", text.trim())),
    }
}

/// Checks the values of a `kind` record named `name` and writes them as Route 53 wants them:
/// TXT values in quotes (split in 255-character strings), CAA values quoted.
pub fn check_values(kind: &str, name: &str, zone: &str, values: &[String]) -> std::result::Result<Vec<String>, String> {
    if values.is_empty() {
        return Err("Write at least one value".into());
    }
    if kind == "CNAME" {
        if name.eq_ignore_ascii_case(zone) {
            return Err("A CNAME cannot be at the zone apex: use an alias".into());
        }
        if values.len() > 1 {
            return Err("A CNAME has a single value".into());
        }
    }
    let mut out: Vec<String> = Vec::new();
    for v in values {
        let v = check_value(kind, v.trim())?;
        if !out.contains(&v) {
            out.push(v);
        }
    }
    Ok(out)
}

fn check_value(kind: &str, v: &str) -> std::result::Result<String, String> {
    let words: Vec<&str> = v.split_whitespace().collect();
    let number = |w: &str, what: &str, max: u32| match w.parse::<u32>() {
        Ok(n) if n <= max => Ok(n),
        _ => Err(format!("{v}: {what} is a number from 0 to {max}")),
    };
    match kind {
        "A" => v.parse::<Ipv4Addr>().map(|_| v.to_string()).map_err(|_| format!("{v} is not an IPv4 address")),
        "AAAA" => v.parse::<Ipv6Addr>().map(|_| v.to_string()).map_err(|_| format!("{v} is not an IPv6 address")),
        "CNAME" | "NS" | "PTR" => host(v).map(str::to_string),
        "MX" => match words.as_slice() {
            [p, h] => Ok(format!("{} {}", number(p, "the priority", 65535)?, host(h)?)),
            _ => Err(format!("{v}: write the priority and the host, like 10 mail.example.com")),
        },
        "SRV" => match words.as_slice() {
            [p, w, port, h] => Ok(format!(
                "{} {} {} {}",
                number(p, "the priority", 65535)?,
                number(w, "the weight", 65535)?,
                number(port, "the port", 65535)?,
                host(h)?
            )),
            _ => Err(format!("{v}: write priority, weight, port and host, like 10 5 5060 sip.example.com")),
        },
        "CAA" => match v.splitn(3, ' ').collect::<Vec<_>>().as_slice() {
            [flags, tag, value] if !value.trim().is_empty() => {
                let tag_ok = !tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphanumeric());
                if !tag_ok {
                    return Err(format!("{v}: the tag is a word like issue, issuewild or iodef"));
                }
                Ok(format!("{} {} {}", number(flags, "the flag", 255)?, tag.to_lowercase(), quote(value.trim())))
            }
            _ => Err(format!("{v}: write flag, tag and value, like 0 issue \"amazon.com\"")),
        },
        "TXT" | "SPF" => Ok(txt(v)),
        _ => Ok(v.to_string()),
    }
}

/// A host name, with or without the final dot (`.` alone for a null MX).
fn host(v: &str) -> std::result::Result<&str, String> {
    let ok = v == "."
        || (!v.is_empty()
            && v.len() <= 255
            && v.trim_end_matches('.').split('.').all(|l| {
                !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '*')
            }));
    if ok { Ok(v) } else { Err(format!("{v} is not a host name")) }
}

/// A TXT value as DNS stores it: quoted strings of at most 255 characters. Quoted input is
/// taken as written.
fn txt(v: &str) -> String {
    if v.starts_with('"') {
        return v.to_string();
    }
    let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
    let mut parts = Vec::new();
    let mut current = String::new();
    for c in escaped.chars() {
        // Never split between a backslash and the character it escapes.
        if current.len() + c.len_utf8() > 255 && !current.ends_with('\\') {
            parts.push(std::mem::take(&mut current));
        }
        current.push(c);
    }
    parts.push(current);
    parts.iter().map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(" ")
}

fn quote(v: &str) -> String {
    if v.starts_with('"') { v.to_string() } else { format!("\"{}\"", v.replace('"', "\\\"")) }
}

/// What a value of `kind` looks like, for a form's placeholder.
pub fn example(kind: &str) -> &'static str {
    match kind {
        "A" => "192.0.2.10, 192.0.2.11",
        "AAAA" => "2001:db8::10",
        "CNAME" => "app.example.net",
        "TXT" => "v=spf1 include:amazonses.com ~all",
        "MX" => "10 mail.example.com, 20 mail2.example.com",
        "SRV" => "10 5 5060 sip.example.com",
        "CAA" => "0 issue \"amazon.com\"",
        "NS" => "ns-1.awsdns-01.org, ns-2.awsdns-02.com",
        "PTR" => "host.example.com",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Alias targets
// ---------------------------------------------------------------------------

/// Something an alias record can point at.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// CloudFront, load balancer, API Gateway or record.
    pub source: &'static str,
    pub label: String,
    pub detail: String,
    pub alias: Alias,
    /// Record types it answers for.
    pub kinds: Vec<String>,
    /// The names it serves (CloudFront alternate domain names, an API Gateway custom
    /// domain); requests for other names fail there.
    pub serves: Vec<String>,
}

impl Candidate {
    /// Why a record named `name` pointing here would not work, if it would not.
    pub fn warning(&self, name: &str) -> Option<String> {
        let name = name.trim_end_matches('.').to_lowercase();
        // A wildcard covers one label.
        let served = |s: &String| {
            let s = s.trim_end_matches('.').to_lowercase();
            match s.strip_prefix("*.") {
                Some(base) => name.split_once('.').is_some_and(|(_, rest)| rest == base),
                None => s == name,
            }
        };
        if self.serves.is_empty() || self.serves.iter().any(served) {
            return None;
        }
        Some(match self.source {
            "CloudFront" => format!("The distribution does not list {name} among its alternate domain names: CloudFront answers 403"),
            _ => format!("{} serves {}, not {name}", self.source, self.serves.join(", ")),
        })
    }
}

/// What a record points at, from its alias.
pub fn alias_kind(alias: &Alias, zone_id: &str) -> &'static str {
    let dns = alias.dns_name.to_lowercase();
    if alias.zone_id == zone_id {
        "a record in this zone"
    } else if alias.zone_id == CLOUDFRONT_ZONE || dns.ends_with(".cloudfront.net.") {
        "CloudFront"
    } else if dns.contains(".execute-api.") {
        "API Gateway"
    } else if dns.contains(".elb.") {
        "load balancer"
    } else if dns.contains(".s3-website") {
        "S3 website"
    } else if dns.contains(".elasticbeanstalk.com") {
        "Elastic Beanstalk"
    } else if dns.contains(".vpce.") {
        "VPC endpoint"
    } else if dns.contains("awsglobalaccelerator.com") {
        "Global Accelerator"
    } else {
        "AWS resource"
    }
}

/// What an alias in zone `zone` can point at: CloudFront distributions, load balancers and
/// API Gateway custom domains of the account (in parallel), and the zone's own `records`.
/// Sources that could not be read are named in the second list ("CloudFront: AccessDenied").
pub fn candidates(aws: &Aws, zone: &Zone, records: &[Record]) -> (Vec<Candidate>, Vec<String>) {
    type Lister = fn(&Aws) -> Result<Vec<Candidate>>;
    let listers: [(&str, Lister); 3] =
        [("CloudFront", distributions), ("Load balancers", load_balancers), ("API Gateway", api_domains)];
    let (mut out, mut failed) = (Vec::new(), Vec::new());
    let results: Vec<(&str, Result<Vec<Candidate>>)> = std::thread::scope(|s| {
        let jobs: Vec<_> = listers.iter().map(|(what, list)| (*what, s.spawn(move || list(aws)))).collect();
        jobs.into_iter().map(|(what, j)| (what, j.join().unwrap_or_else(|_| Ok(Vec::new())))).collect()
    });
    for (what, result) in results {
        match result {
            Ok(list) => out.extend(list),
            Err(e) => failed.push(format!("{what}: {}", e.short())),
        }
    }
    let mut seen: Vec<(String, String)> = Vec::new();
    for r in records.iter().filter(|r| !matches!(r.kind.as_str(), "SOA" | "NS")) {
        let key = (r.name.to_lowercase(), r.kind.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(Candidate {
            source: "record",
            label: r.name.trim_end_matches('.').to_string(),
            detail: format!("record in this zone · {} · {}", r.kind, r.summary()),
            alias: Alias { zone_id: zone.id.clone(), dns_name: r.name.clone(), evaluate_health: false },
            kinds: vec![r.kind.clone()],
            serves: Vec::new(),
        });
    }
    (out, failed)
}

fn kinds(ipv6: bool) -> Vec<String> {
    if ipv6 { vec!["A".into(), "AAAA".into()] } else { vec!["A".into()] }
}

fn distributions(aws: &Aws) -> Result<Vec<Candidate>> {
    let aws = global(aws);
    let (mut out, mut marker) = (Vec::new(), None::<String>);
    for _ in 0..MAX_PAGES {
        let mut query = vec![("MaxItems".to_string(), "100".to_string())];
        if let Some(m) = &marker {
            query.push(("Marker".to_string(), m.clone()));
        }
        let xml = get(&aws, &CLOUDFRONT, &["2020-05-31", "distribution"], &query)?;
        let list = root(&xml, "DistributionList");
        for d in list.list("Items") {
            let domain = d.string("DomainName").to_lowercase();
            let names = d.child("Aliases").map(|a| a.strings("Items")).unwrap_or_default();
            let mut detail = format!("CloudFront · {}", d.string("Id"));
            if !names.is_empty() {
                detail.push_str(&format!(" · {}", names.join(", ")));
            }
            if let Some(c) = d.text("Comment") {
                detail.push_str(&format!(" · {c}"));
            }
            out.push(Candidate {
                source: "CloudFront",
                label: domain.clone(),
                detail,
                alias: Alias { zone_id: CLOUDFRONT_ZONE.into(), dns_name: format!("{domain}."), evaluate_health: false },
                kinds: kinds(d.flag("IsIPV6Enabled")),
                serves: names,
            });
        }
        marker = if list.flag("IsTruncated") { list.text("NextMarker") } else { None };
        if marker.is_none() {
            break;
        }
    }
    Ok(out)
}

fn load_balancers(aws: &Aws) -> Result<Vec<Candidate>> {
    let (mut out, mut marker) = (Vec::new(), None::<String>);
    for _ in 0..MAX_PAGES {
        let mut params = vec![("PageSize", "400")];
        if let Some(m) = &marker {
            params.push(("Marker", m));
        }
        let xml = aws.query(&ELB, "DescribeLoadBalancers", "2015-12-01", &params)?;
        let result = super::xml::result_of(&xml, "DescribeLoadBalancers");
        for lb in result.list("LoadBalancers") {
            let dns = lb.string("DNSName").to_lowercase();
            if dns.is_empty() {
                continue;
            }
            out.push(Candidate {
                source: "load balancer",
                label: dns.clone(),
                detail: format!("load balancer · {} · {} · {}", lb.string("Type"), lb.string("LoadBalancerName"), lb.string("Scheme")),
                alias: Alias { zone_id: lb.string("CanonicalHostedZoneId"), dns_name: format!("{dns}."), evaluate_health: true },
                kinds: kinds(lb.string("IpAddressType").starts_with("dualstack")),
                serves: Vec::new(),
            });
        }
        marker = result.text("NextMarker");
        if marker.is_none() {
            break;
        }
    }
    Ok(out)
}

fn api_domains(aws: &Aws) -> Result<Vec<Candidate>> {
    Ok(apigateway::domain_targets(aws)?
        .into_iter()
        .map(|d| Candidate {
            source: "API Gateway",
            label: d.name.clone(),
            detail: format!("API Gateway · {} · {}", d.endpoint_type, d.target),
            alias: Alias { zone_id: d.hosted_zone_id, dns_name: format!("{}.", d.target.to_lowercase()), evaluate_health: false },
            kinds: kinds(d.dualstack),
            serves: vec![d.name],
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = r#"<?xml version="1.0"?>
<ListResourceRecordSetsResponse xmlns="https://route53.amazonaws.com/doc/2013-04-01/"><ResourceRecordSets>
<ResourceRecordSet><Name>example.com.</Name><Type>NS</Type><TTL>172800</TTL><ResourceRecords><ResourceRecord><Value>ns-1.awsdns-01.org.</Value></ResourceRecord></ResourceRecords></ResourceRecordSet>
<ResourceRecordSet><Name>\052.example.com.</Name><Type>A</Type><TTL>60</TTL><ResourceRecords><ResourceRecord><Value>192.0.2.1</Value></ResourceRecord><ResourceRecord><Value>192.0.2.2</Value></ResourceRecord></ResourceRecords></ResourceRecordSet>
<ResourceRecordSet><Name>api.example.com.</Name><Type>A</Type><SetIdentifier>blue</SetIdentifier><Weight>90</Weight><AliasTarget><HostedZoneId>Z35SXDOTRQ7X7K</HostedZoneId><DNSName>dualstack.web-1.us-east-1.elb.amazonaws.com.</DNSName><EvaluateTargetHealth>true</EvaluateTargetHealth></AliasTarget><HealthCheckId>hc-1</HealthCheckId></ResourceRecordSet>
<ResourceRecordSet><Name>txt.example.com.</Name><Type>TXT</Type><TTL>300</TTL><ResourceRecords><ResourceRecord><Value>"a &amp; b"</Value></ResourceRecord></ResourceRecords></ResourceRecordSet>
</ResourceRecordSets><IsTruncated>true</IsTruncated><NextRecordName>www.example.com.</NextRecordName><NextRecordType>A</NextRecordType><MaxItems>4</MaxItems></ListResourceRecordSetsResponse>"#;

    fn records() -> Vec<Record> {
        let list = root(LISTING, "ListResourceRecordSetsResponse");
        children(list.raw("ResourceRecordSets").unwrap()).into_iter().map(|(_, raw)| parse_record(raw)).collect()
    }

    #[test]
    fn reads_records_as_route53_lists_them() {
        let r = records();
        assert_eq!(r.len(), 4);
        assert_eq!(r[1].name, "*.example.com.", "octal escapes are decoded");
        assert_eq!((r[1].ttl, r[1].values.clone()), (Some(60), vec!["192.0.2.1".to_string(), "192.0.2.2".to_string()]));
        assert_eq!(r[1].relative("example.com."), "*");
        assert_eq!(r[0].relative("example.com."), "@");
        assert!(r[0].locked("example.com.").is_some() && r[1].locked("example.com.").is_none());
        let api = &r[2];
        assert_eq!(api.routing.as_deref(), Some("weighted 90"));
        assert_eq!(api.set_id.as_deref(), Some("blue"));
        assert_eq!(api.health_check.as_deref(), Some("hc-1"));
        let alias = api.alias.as_ref().unwrap();
        assert!(alias.evaluate_health && api.values.is_empty() && api.ttl.is_none());
        assert_eq!(alias_kind(alias, "ZONE"), "load balancer");
        assert_eq!(r[3].values, ["\"a & b\""]);
        assert!(find_record(&r, "API.example.com.", "A", Some("blue")).is_some() && find_record(&r, "api.example.com.", "A", None).is_none());
    }

    #[test]
    fn an_edit_keeps_the_rest_of_the_record() {
        let r = records();
        let new = r[2].with_answer(&Answer::Values { ttl: 60, values: vec!["192.0.2.9".into()] });
        assert_eq!(
            new,
            "<Name>api.example.com.</Name><Type>A</Type><SetIdentifier>blue</SetIdentifier><Weight>90</Weight><TTL>60</TTL>\
             <ResourceRecords><ResourceRecord><Value>192.0.2.9</Value></ResourceRecord></ResourceRecords><HealthCheckId>hc-1</HealthCheckId>"
        );
        let txt = r[3].with_answer(&Answer::Values { ttl: 300, values: vec!["\"a & b\"".into(), "\"<c>\"".into()] });
        assert!(txt.contains("<Value>&quot;a &amp; b&quot;</Value>") && txt.contains("<Value>&quot;&lt;c&gt;&quot;</Value>"), "{txt}");
        let alias = Alias { zone_id: CLOUDFRONT_ZONE.into(), dns_name: "d1.cloudfront.net.".into(), evaluate_health: false };
        let swapped = r[1].with_answer(&Answer::Alias(alias));
        assert!(!swapped.contains("<TTL>") && swapped.ends_with("<EvaluateTargetHealth>false</EvaluateTargetHealth></AliasTarget>"));
        assert!(swapped.starts_with("<Name>\\052.example.com.</Name>"), "the name goes back as Route 53 wrote it");
    }

    #[test]
    fn names_are_completed_within_the_zone() {
        let z = "example.com.";
        assert_eq!(full_name("api", z).unwrap(), "api.example.com.");
        assert_eq!(full_name("API.Example.com", z).unwrap(), "api.example.com.");
        assert_eq!(full_name("api.example.com.", z).unwrap(), "api.example.com.");
        assert_eq!(full_name("", z).unwrap(), "example.com.");
        assert_eq!(full_name("@", z).unwrap(), "example.com.");
        assert_eq!(full_name("*.dev", z).unwrap(), "*.dev.example.com.");
        assert_eq!(full_name("other.org", z).unwrap(), "other.org.example.com.", "relative unless it ends with the zone");
        assert!(full_name("other.org.", z).unwrap_err().contains("not in the zone"));
        assert!(full_name("a..b", z).is_err() && full_name("dev.*", z).is_err() && full_name("a b", z).is_err());
        assert!(full_name(&"x".repeat(64), z).is_err());
    }

    #[test]
    fn values_are_checked_and_written_as_route53_wants() {
        let z = "example.com.";
        let check = |kind: &str, text: &str| check_values(kind, "x.example.com.", z, &split_values(text));
        assert_eq!(check("A", "192.0.2.1, 192.0.2.2\n192.0.2.1").unwrap(), ["192.0.2.1", "192.0.2.2"]);
        assert!(check("A", "192.0.2.300").unwrap_err().contains("not an IPv4"));
        assert!(check("AAAA", "2001:db8::1").is_ok() && check("AAAA", "192.0.2.1").is_err());
        assert!(check("CNAME", "a.example.net, b.example.net").unwrap_err().contains("single"));
        assert!(check_values("CNAME", z, z, &["a.example.net".into()]).unwrap_err().contains("apex"));
        assert_eq!(check("MX", "10 mail.example.com").unwrap(), ["10 mail.example.com"]);
        assert!(check("MX", "mail.example.com").is_err() && check("MX", "70000 m.example.com").is_err());
        assert_eq!(check("SRV", "10 5 5060 sip.example.com").unwrap(), ["10 5 5060 sip.example.com"]);
        assert_eq!(check("CAA", "0 issue amazon.com").unwrap(), ["0 issue \"amazon.com\""]);
        assert_eq!(check("TXT", "v=spf1 -all, \"a, b\"").unwrap(), ["\"v=spf1 -all\"", "\"a, b\""]);
        assert_eq!(check("TXT", "say \"hi\"").unwrap(), ["\"say \\\"hi\\\"\""]);
        let long = check("TXT", &"k".repeat(300)).unwrap();
        assert_eq!(long[0], format!("\"{}\" \"{}\"", "k".repeat(255), "k".repeat(45)), "long TXT values are split in 255-character strings");
        assert!(check("A", " , ").unwrap_err().contains("at least one"));
        assert_eq!(check_ttl("60"), Ok(60));
        assert!(check_ttl("-1").is_err() && check_ttl("soon").is_err());
    }

    #[test]
    fn zones_are_found_by_name_or_id() {
        let zone = |id: &str, name: &str, private: bool| Zone { id: id.into(), name: name.into(), private, ..Zone::default() };
        let zones = [zone("Z1", "example.com.", false), zone("Z2", "example.com.", true), zone("Z3", "shop.dev.", false)];
        assert_eq!(find_zone(&zones, "shop.dev").unwrap().id, "Z3");
        assert_eq!(find_zone(&zones, "/hostedzone/Z2").unwrap().id, "Z2");
        assert!(find_zone(&zones, "Example.com.").unwrap_err().contains("Z1 public, Z2 private"));
        assert!(find_zone(&zones, "nope.dev").is_err());
    }

    #[test]
    fn warns_when_the_target_does_not_serve_the_name() {
        let alias = Alias::default();
        let cf = Candidate {
            source: "CloudFront",
            label: "d1.cloudfront.net".into(),
            detail: String::new(),
            alias,
            kinds: kinds(true),
            serves: vec!["www.example.com".into(), "*.cdn.example.com".into()],
        };
        assert!(cf.warning("www.example.com.").is_none());
        assert!(cf.warning("img.cdn.example.com.").is_none());
        assert!(cf.warning("a.b.cdn.example.com.").is_some(), "a wildcard covers one label");
        assert!(cf.warning("api.example.com.").unwrap().contains("403"));
    }

    #[test]
    fn changes_and_errors_are_read() {
        let xml = r#"<?xml version="1.0"?><ChangeResourceRecordSetsResponse xmlns="x"><ChangeInfo><Id>/change/C2682N5HXP0BZ4</Id><Status>PENDING</Status></ChangeInfo></ChangeResourceRecordSetsResponse>"#;
        let c = parse_change(xml, "ChangeResourceRecordSetsResponse");
        assert_eq!((c.id.as_str(), c.in_sync()), ("C2682N5HXP0BZ4", false));
        let resp = super::super::http::RawResponse {
            status: 400,
            headers: Vec::new(),
            body: br#"<?xml version="1.0"?><InvalidChangeBatch xmlns="x"><Messages><Message>Tried to create resource record set [name='a.example.com.', type='A'] but it already exists</Message></Messages></InvalidChangeBatch>"#.to_vec(),
            error_type: None,
        };
        let e = super::super::http::api_error(&resp);
        assert!(e.short().contains("InvalidChangeBatch") && e.short().contains("already exists"), "{}", e.short());
    }
}
