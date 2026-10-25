//! EC2 and VPC: what "EC2 - Other" and public IPv4 addresses bill for little or nothing. NAT
//! gateways that carry almost no traffic, busy ones in VPCs without the free gateway endpoints
//! to S3 and DynamoDB, EBS volumes attached to nothing, gp2 volumes that cost less as gp3, old
//! snapshots no AMI uses, and Elastic IPs attached to nothing.

use super::{Finding, GB, Impact, MONTH, Scan, billing, metric_data, peak, per_month, qty, query, query_id, total};
use crate::aws::ec2::{self, NatGateway, Snapshot, Volume};
use crate::aws::{self, Aws};
use crate::costs::{self, Snapshot as Costs};
use crate::util::{fmt_utc, now_secs};

/// No dcli module opens these: findings name the resource and open nothing.
const MODULE: &str = "EC2";
const DAY: u32 = 86_400;
/// Hours in an average month.
const HOURS: f64 = MONTH / 3600.0;
/// A NAT gateway processing less than this a month carries almost nothing.
const NAT_IDLE_GB: f64 = 1.0;
/// GB a month through a NAT gateway: medium and high impact from these (each GB is billed).
const NAT_GB_MEDIUM: f64 = 100.0;
const NAT_GB_HIGH: f64 = 1000.0;
/// GB of EBS storage: medium and high impact from these.
const EBS_GB_MEDIUM: f64 = 100.0;
const EBS_GB_HIGH: f64 = 1000.0;
/// gp2 to gp3 saves a part of the rate: medium and high impact from these GB.
const GP3_GB_MEDIUM: f64 = 500.0;
const GP3_GB_HIGH: f64 = 5000.0;
/// Snapshots older than this many days are looked at.
const SNAPSHOT_DAYS: f64 = 90.0;
/// gp3 includes this many IOPS and MiB/s.
const GP3_IOPS: f64 = 3000.0;
const GP3_THROUGHPUT: f64 = 125.0;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let costs = costs::cached(scan.env);
    let mut out = nat(scan)?;
    let volumes = ec2::volumes(scan.aws)?;
    out.extend(volumes.iter().filter_map(unattached));
    out.extend(volumes.iter().filter_map(gp3));
    let in_images = ec2::image_snapshots(scan.aws)?;
    out.extend(old_snapshots(&ec2::snapshots(scan.aws)?, &volumes, &in_images, now_secs()));
    let (usage, service) = idle_ipv4_usage(costs.as_ref());
    out.extend(ec2::addresses(scan.aws)?.iter().filter(|a| a.association_id.is_empty()).map(|a| {
        let name = if a.name.is_empty() { a.public_ip.clone() } else { format!("{} ({})", a.public_ip, a.name) };
        let mut f = Finding::new(MODULE, "", name, "an Elastic IP attached to nothing", "Release it if nothing needs this address (a released address cannot be had back)", format!("{HOURS:.0} address-hours a month"), Impact::Low)
            .kind("ipv4-idle")
            .billed(usage, HOURS)
            .evidence(format!("allocation {}", a.allocation_id))
            .caveat("Every public IPv4 address is billed by the hour; one attached to nothing is billed all the same.");
        if service != "EC2 - Other" {
            f = f.ce_service(service);
        }
        f
    }));
    Ok(out)
}

/// The usage type an idle address is billed as, and its service: `PublicIPv4:IdleAddress` under
/// Amazon VPC since February 2024, `ElasticIP:IdleAddress` under "EC2 - Other" before; the
/// bill says which.
fn idle_ipv4_usage(costs: Option<&Costs>) -> (&'static str, &'static str) {
    let on_bill = |name: &str| costs.and_then(|c| c.usage.iter().find(|u| billing::split_region(&u.usage_type).1 == name).map(|u| u.service.clone()));
    if let Some(service) = on_bill(billing::IPV4_IDLE) {
        return (billing::IPV4_IDLE, if service == "EC2 - Other" { "EC2 - Other" } else { "Amazon Virtual Private Cloud" });
    }
    if on_bill(billing::EIP_IDLE).is_some() {
        return (billing::EIP_IDLE, "EC2 - Other");
    }
    (billing::IPV4_IDLE, "Amazon Virtual Private Cloud")
}

/// What a NAT gateway did over the window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct NatUse {
    /// Bytes it processed (in from the subnets and in from the destinations).
    bytes: f64,
    /// The most connections it held at once.
    connections: f64,
}

fn nat(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let gateways: Vec<NatGateway> = ec2::nat_gateways(scan.aws)?.into_iter().filter(|n| n.state == "available").collect();
    if gateways.is_empty() {
        return Ok(Vec::new());
    }
    let usage = nat_usage(scan.aws, &gateways, scan.days)?;
    // Unknown when the endpoints cannot be read: the finding says so.
    let endpoints = ec2::gateway_endpoints(scan.aws).ok();
    Ok(gateways
        .iter()
        .zip(usage)
        .filter_map(|(g, u)| {
            let vpc = endpoints.as_ref().map(|list| list.iter().find(|(v, _, _)| *v == g.vpc).map_or((false, false), |(_, s3, ddb)| (*s3, *ddb)));
            nat_finding(g, u, vpc, scan.days)
        })
        .collect())
}

fn nat_usage(aws: &Aws, gateways: &[NatGateway], days: u32) -> aws::Result<Vec<NatUse>> {
    let mut queries = Vec::new();
    for (i, g) in gateways.iter().enumerate() {
        let dims = [("NatGatewayId", g.id.as_str())];
        queries.push(query(query_id("src", i), "AWS/NATGateway", "BytesInFromSource", &dims, "Sum", DAY));
        queries.push(query(query_id("dst", i), "AWS/NATGateway", "BytesInFromDestination", &dims, "Sum", DAY));
        queries.push(query(query_id("con", i), "AWS/NATGateway", "ActiveConnectionCount", &dims, "Maximum", DAY));
    }
    let series = metric_data(aws, &queries, days)?;
    Ok(series
        .chunks(3)
        .map(|s| NatUse { bytes: total(&s[0]) + total(&s[1]), connections: peak(&s[2]).unwrap_or(0.0) })
        .collect())
}

/// `vpc_endpoints`: whether its VPC has gateway endpoints to S3 and DynamoDB (`None`: unknown).
fn nat_finding(g: &NatGateway, u: NatUse, vpc_endpoints: Option<(bool, bool)>, days: u32) -> Option<Finding> {
    let monthly_gb = u.bytes / GB * per_month(days);
    let name = if g.name.is_empty() { g.id.clone() } else { format!("{} ({})", g.id, g.name) };
    let window = super::days_label(days);
    if monthly_gb < NAT_IDLE_GB {
        return Some(
            Finding::new(
                MODULE,
                "",
                name,
                format!("processed {} in {window}", super::size(u.bytes)),
                "Delete it if no private subnet needs to reach the internet through it",
                format!("{HOURS:.0} NAT gateway-hours a month"),
                Impact::Medium,
            )
            .kind("nat-idle")
            .score(1.0)
            .billed(billing::NAT_HOURS, HOURS)
            .evidence(format!("at most {} connections at once", qty(u.connections)))
            .evidence(format!("VPC {} · subnet {}", g.vpc, g.subnet))
            .caveat("Subnets whose route tables send traffic to it lose internet access: check them first.")
            .caveat("One NAT gateway per Availability Zone is often kept on purpose, so a zone's failure does not cut the others."),
        );
    }
    let (s3, ddb) = vpc_endpoints.unwrap_or((false, false));
    if s3 && ddb {
        return None;
    }
    let missing: Vec<&str> = [(!s3).then_some("S3"), (!ddb).then_some("DynamoDB")].into_iter().flatten().collect();
    let mut f = Finding::new(
        MODULE,
        "",
        name,
        format!("processes {} GB a month", qty(monthly_gb)),
        format!("Add gateway endpoints for {} to the VPC (they are free): that traffic stops going through the NAT", missing.join(" and ")),
        format!("up to {} GB a month through the NAT", qty(monthly_gb)),
        Impact::of(monthly_gb, NAT_GB_MEDIUM, NAT_GB_HIGH),
    )
    .kind("nat-endpoints")
    .score(monthly_gb / NAT_GB_HIGH)
    .billed_at_most(billing::NAT_BYTES, monthly_gb)
    .evidence(format!("{} processed in {window}", super::size(u.bytes)))
    .evidence(format!("VPC {}", g.vpc))
    .caveat("The NAT's metrics do not say where the traffic goes: only what goes to S3 or DynamoDB in this region moves to the endpoints, so this is the most it saves.");
    if vpc_endpoints.is_none() {
        f = f.caveat("The VPC's endpoints could not be read: it may have them already.");
    }
    Some(f)
}

/// A volume attached to nothing: its storage (and provisioned IOPS) bill all the same.
fn unattached(v: &Volume) -> Option<Finding> {
    if v.state != "available" || v.attached || v.size <= 0.0 {
        return None;
    }
    let name = if v.name.is_empty() { v.id.clone() } else { format!("{} ({})", v.id, v.name) };
    let mut f = Finding::new(
        MODULE,
        "",
        name,
        format!("a {} GiB {} volume attached to nothing", v.size, v.kind),
        "Delete it (snapshot it first if its data may be needed)",
        format!("{} GB-month of {}", qty(v.size), v.kind),
        Impact::of(v.size, EBS_GB_MEDIUM, EBS_GB_HIGH),
    )
    .kind("ebs-unattached")
    .score(v.size / EBS_GB_HIGH)
    .billed(&billing::ebs_volume(&v.kind), v.size);
    let iops = v.iops.unwrap_or(0.0);
    let billed_iops = if v.kind == "gp3" { (iops - GP3_IOPS).max(0.0) } else { iops };
    if let Some(usage) = billing::ebs_iops(&v.kind).filter(|_| billed_iops > 0.0) {
        f = f.billed(&usage, billed_iops).evidence(format!("{} provisioned IOPS billed", qty(billed_iops)));
    }
    if let Some(created) = v.created {
        f = f.evidence(format!("created {}", &fmt_utc(created)[..10]));
    }
    if v.kind == "gp3" && v.throughput.is_some_and(|t| t > GP3_THROUGHPUT) {
        f = f.caveat("Its throughput above 125 MiB/s is billed too: deleting it saves that as well.");
    }
    Some(f.caveat("A snapshot costs less per GB than a volume, and only for the blocks it holds."))
}

/// A gp2 volume in use: gp3 bills less per GB and includes 3,000 IOPS and 125 MiB/s.
fn gp3(v: &Volume) -> Option<Finding> {
    if v.kind != "gp2" || !v.attached || v.size <= 0.0 {
        return None;
    }
    let name = if v.name.is_empty() { v.id.clone() } else { format!("{} ({})", v.id, v.name) };
    let mut f = Finding::new(
        MODULE,
        "",
        name,
        format!("a {} GiB gp2 volume", v.size),
        "Change it to gp3 (online, no downtime: aws ec2 modify-volume --volume-type gp3)",
        format!("{} GB-month billed as gp3 instead of gp2", qty(v.size)),
        Impact::of(v.size, GP3_GB_MEDIUM, GP3_GB_HIGH),
    )
    .kind("ebs-gp3")
    .score(v.size / GP3_GB_HIGH)
    .billed_instead(&billing::ebs_volume("gp2"), &billing::ebs_volume("gp3"), v.size);
    // gp2 gives 3 IOPS per GiB (at least 100), up to 16,000.
    let gp2_iops = (3.0 * v.size).clamp(100.0, 16_000.0);
    let needs_iops = gp2_iops > GP3_IOPS;
    let needs_throughput = v.size >= 170.0;
    if needs_iops {
        f = f.caveat(format!(
            "As gp2 it has {gp2_iops:.0} IOPS; gp3 includes 3000: matching it means provisioning {:.0} more, billed apart.",
            gp2_iops - GP3_IOPS
        ));
    }
    if needs_throughput {
        f = f.caveat("gp2 of this size can reach 250 MiB/s; gp3 includes 125 MiB/s and bills throughput above it.");
    }
    // Matching the performance it has costs IOPS or throughput the figure leaves out: it is the
    // most the change saves (none of it when the volume needs every IOPS and MiB/s it has).
    if needs_iops || needs_throughput {
        f = f.at_most();
    }
    Some(f.caveat("Volumes changed in the last 6 hours cannot be changed again until then."))
}

/// Snapshots older than [`SNAPSHOT_DAYS`] that no AMI uses and no backup policy manages, one
/// finding per source volume.
fn old_snapshots(snapshots: &[Snapshot], volumes: &[Volume], in_images: &[String], now: f64) -> Vec<Finding> {
    let old: Vec<&Snapshot> = snapshots
        .iter()
        .filter(|s| !s.managed && s.tier == "standard" && !in_images.contains(&s.id))
        .filter(|s| s.started.is_some_and(|t| (now - t) / 86_400.0 >= SNAPSHOT_DAYS))
        .collect();
    let mut sources: Vec<&str> = Vec::new();
    for s in &old {
        if !sources.contains(&s.volume.as_str()) {
            sources.push(&s.volume);
        }
    }
    sources
        .into_iter()
        .map(|volume| {
            let group: Vec<&&Snapshot> = old.iter().filter(|s| s.volume == volume).collect();
            let gb: f64 = group.iter().map(|s| s.full_size.map_or(s.volume_size, |b| b / GB)).sum();
            let oldest = group.iter().filter_map(|s| s.started).reduce(f64::min).unwrap_or(now);
            let exists = volumes.iter().any(|v| v.id == volume);
            let resource = if volume.is_empty() || volume == "vol-ffffffff" { "snapshots of no volume".to_string() } else { format!("snapshots of {volume}") };
            let mut f = Finding::new(
                MODULE,
                "",
                resource,
                format!(
                    "{} snapshot{} older than {SNAPSHOT_DAYS:.0} days{}",
                    group.len(),
                    if group.len() == 1 { "" } else { "s" },
                    if exists { "" } else { ", of a volume that no longer exists" }
                ),
                "Delete the ones nobody will restore (or archive them: the archive tier costs less to keep)",
                format!("up to {} GB-month", qty(gb)),
                Impact::of(gb, EBS_GB_MEDIUM * 5.0, EBS_GB_HIGH * 5.0),
            )
            .kind("ebs-snapshots")
            .score(gb / (EBS_GB_HIGH * 5.0))
            .billed_at_most(billing::EBS_SNAPSHOTS, gb)
            .evidence(format!("oldest from {}", &fmt_utc(oldest)[..10]))
            .evidence(format!("ids: {}", group.iter().take(5).map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ") + if group.len() > 5 { "…" } else { "" }))
            .caveat("Snapshots are incremental: deleting one frees only the blocks no other snapshot holds, so this is the most it frees.");
            if exists {
                f = f.caveat("Its volume still exists: keep at least its most recent snapshot if it is the backup.");
            }
            f
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_790_000_000.0;

    fn gateway() -> NatGateway {
        NatGateway { id: "nat-1".into(), vpc: "vpc-1".into(), subnet: "subnet-1".into(), state: "available".into(), ..Default::default() }
    }

    #[test]
    fn idle_nat_gateways_bill_their_hours() {
        let f = nat_finding(&gateway(), NatUse { bytes: 0.2 * GB, connections: 2.0 }, Some((true, true)), 14).unwrap();
        assert_eq!(f.kind, "nat-idle");
        assert_eq!(f.billed[0].usage, billing::NAT_HOURS);
        assert!((f.billed[0].quantity - HOURS).abs() < 1e-9);
    }

    #[test]
    fn busy_nat_gateways_without_endpoints_save_at_most_their_bytes() {
        let busy = NatUse { bytes: 700.0 * GB, connections: 300.0 };
        let f = nat_finding(&gateway(), busy, Some((true, false)), 14).unwrap();
        assert_eq!(f.kind, "nat-endpoints");
        assert!(f.recommendation.contains("for DynamoDB") && !f.recommendation.contains("S3"));
        assert!(f.billed[0].at_most && f.billed[0].usage == billing::NAT_BYTES);
        assert!((f.billed[0].quantity - 700.0 * per_month(14)).abs() < 1e-6);
        assert_eq!(f.impact, Impact::High);
        // With both endpoints there is nothing to add.
        assert!(nat_finding(&gateway(), busy, Some((true, true)), 14).is_none());
        assert!(nat_finding(&gateway(), busy, None, 14).unwrap().caveats.iter().any(|c| c.contains("could not be read")));
    }

    #[test]
    fn unattached_volumes_and_gp2() {
        let v = Volume { id: "vol-1".into(), size: 500.0, kind: "io1".into(), state: "available".into(), iops: Some(4000.0), ..Default::default() };
        let f = unattached(&v).unwrap();
        assert_eq!((f.billed[0].usage.as_str(), f.billed[0].quantity), ("EBS:VolumeUsage.piops", 500.0));
        assert_eq!((f.billed[1].usage.as_str(), f.billed[1].quantity), ("EBS:VolumeP-IOPS.piops", 4000.0));
        let gp3_vol = Volume { kind: "gp3".into(), iops: Some(3000.0), ..v.clone() };
        assert_eq!(unattached(&gp3_vol).unwrap().billed.len(), 1, "gp3's first 3,000 IOPS are free");
        assert!(unattached(&Volume { state: "in-use".into(), attached: true, ..v.clone() }).is_none());
        let used = Volume { id: "vol-2".into(), size: 2000.0, kind: "gp2".into(), state: "in-use".into(), attached: true, ..Default::default() };
        let f = gp3(&used).unwrap();
        assert_eq!(f.billed[0].instead.as_deref(), Some("EBS:VolumeUsage.gp3"));
        assert!(f.caveats.iter().any(|c| c.contains("6000 IOPS") && c.contains("3000 more")), "{:?}", f.caveats);
        // Matching its IOPS and throughput costs extra: the figure is a ceiling.
        assert!(f.billed[0].at_most);
        // A small volume gets the same performance from gp3's baseline: a firm figure.
        let small = gp3(&Volume { size: 100.0, ..used.clone() }).unwrap();
        assert!(!small.billed[0].at_most && small.caveats.len() == 1, "{:?}", small.caveats);
        assert!(gp3(&Volume { kind: "gp3".into(), ..used.clone() }).is_none());
        assert!(gp3(&Volume { attached: false, state: "available".into(), ..used }).is_none(), "unattached: delete it instead");
    }

    #[test]
    fn old_snapshots_group_by_volume_and_skip_amis_and_backups() {
        let snap = |id: &str, volume: &str, days: f64| Snapshot {
            id: id.into(),
            volume: volume.into(),
            volume_size: 100.0,
            started: Some(NOW - days * 86_400.0),
            tier: "standard".into(),
            ..Default::default()
        };
        let list = vec![
            snap("snap-1", "vol-gone", 400.0),
            snap("snap-2", "vol-gone", 200.0),
            snap("snap-3", "vol-live", 120.0),
            snap("snap-4", "vol-live", 10.0),
            snap("snap-5", "vol-ami", 300.0),
            Snapshot { managed: true, ..snap("snap-6", "vol-live", 500.0) },
        ];
        let volumes = vec![Volume { id: "vol-live".into(), ..Default::default() }];
        let found = old_snapshots(&list, &volumes, &["snap-5".to_string()], NOW);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].resource, "snapshots of vol-gone");
        assert!(found[0].summary.contains("2 snapshots") && found[0].summary.contains("no longer exists"));
        assert_eq!(found[0].billed[0].quantity, 200.0);
        assert!(found[1].caveats.iter().any(|c| c.contains("still exists")));
    }

    #[test]
    fn idle_addresses_follow_the_bill() {
        let usage = |service: &str, usage_type: &str| crate::costs::UsageCost { service: service.into(), usage_type: usage_type.into(), ..Default::default() };
        let vpc = Costs { usage: vec![usage("Amazon Virtual Private Cloud", "USE1-PublicIPv4:IdleAddress")], ..Default::default() };
        assert_eq!(idle_ipv4_usage(Some(&vpc)), (billing::IPV4_IDLE, "Amazon Virtual Private Cloud"));
        let old = Costs { usage: vec![usage("EC2 - Other", "USE1-ElasticIP:IdleAddress")], ..Default::default() };
        assert_eq!(idle_ipv4_usage(Some(&old)), (billing::EIP_IDLE, "EC2 - Other"));
        assert_eq!(idle_ipv4_usage(None), (billing::IPV4_IDLE, "Amazon Virtual Private Cloud"));
    }
}
