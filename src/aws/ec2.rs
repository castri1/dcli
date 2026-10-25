//! EC2 (awsQuery XML, Version 2016-11-15), read only, for the Savings checks of what "EC2 -
//! Other" bills: NAT gateways, EBS volumes and snapshots, the VPC endpoints that spare NAT
//! traffic, and Elastic IPs. EC2 answers `<XResponse>` with lists of `<item>` and pages with
//! `NextToken`, unlike the `<XResult>` / `Marker` of the other query APIs.

use super::xml::El;
use super::{Aws, Result, Service};

pub const SERVICE: Service = Service { endpoint: "ec2", signing_name: "ec2", target_prefix: "" };
pub const VERSION: &str = "2016-11-15";

/// Pages read at most per listing.
const MAX_PAGES: usize = 20;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Volume {
    pub id: String,
    /// GiB.
    pub size: f64,
    /// `gp2`, `gp3`, `io1`, `io2`, `st1`, `sc1`, `standard`.
    pub kind: String,
    /// `available` (attached to nothing), `in-use`…
    pub state: String,
    pub iops: Option<f64>,
    /// MiB/s (gp3).
    pub throughput: Option<f64>,
    pub created: Option<f64>,
    pub attached: bool,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NatGateway {
    pub id: String,
    pub vpc: String,
    pub subnet: String,
    /// `available`, `pending`, `deleting`…
    pub state: String,
    /// `public` or `private`.
    pub connectivity: String,
    pub created: Option<f64>,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Address {
    pub public_ip: String,
    pub allocation_id: String,
    /// Set when it is associated with an instance or a network interface.
    pub association_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub id: String,
    pub volume: String,
    /// The size of the volume it was taken from (GiB): what it stores is at most that.
    pub volume_size: f64,
    /// What a full copy of it takes, when EC2 says (bytes).
    pub full_size: Option<f64>,
    pub started: Option<f64>,
    pub description: String,
    /// `standard` or `archive`.
    pub tier: String,
    pub name: String,
    /// Made by AWS Backup or Data Lifecycle Manager: their policies delete it.
    pub managed: bool,
}

/// The response element of `action`, read through its children (it carries an `xmlns`).
fn response<'a>(xml: &'a str, action: &str) -> El<'a> {
    El::new(El::new(xml).raw(&format!("{action}Response")).unwrap_or(""))
}

/// The `Name` tag of an item.
fn name_tag(item: &El) -> String {
    item.list("tagSet").iter().find(|t| t.string("key") == "Name").map(|t| t.string("value")).unwrap_or_default()
}

/// Every page of `action`, each item of the set `list` through `parse`.
fn pages<T>(aws: &Aws, action: &str, params: &[(&str, &str)], list: &str, parse: impl Fn(&El) -> Option<T>) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut page: Vec<(&str, &str)> = params.to_vec();
        if let Some(t) = &token {
            page.push(("NextToken", t));
        }
        let xml = aws.query(&SERVICE, action, VERSION, &page)?;
        let r = response(&xml, action);
        out.extend(r.list(list).iter().filter_map(&parse));
        token = r.text("nextToken");
        if token.is_none() {
            break;
        }
    }
    Ok(out)
}

pub fn volumes(aws: &Aws) -> Result<Vec<Volume>> {
    pages(aws, "DescribeVolumes", &[("MaxResults", "500")], "volumeSet", |v| {
        Some(Volume {
            id: v.text("volumeId")?,
            size: v.num("size").unwrap_or(0.0),
            kind: v.string("volumeType"),
            state: v.string("status"),
            iops: v.num("iops"),
            throughput: v.num("throughput"),
            created: v.time("createTime"),
            attached: !v.list("attachmentSet").is_empty(),
            name: name_tag(v),
        })
    })
}

pub fn nat_gateways(aws: &Aws) -> Result<Vec<NatGateway>> {
    pages(aws, "DescribeNatGateways", &[("MaxResults", "1000")], "natGatewaySet", |n| {
        Some(NatGateway {
            id: n.text("natGatewayId")?,
            vpc: n.string("vpcId"),
            subnet: n.string("subnetId"),
            state: n.string("state"),
            connectivity: n.text("connectivityType").unwrap_or_else(|| "public".into()),
            created: n.time("createTime"),
            name: name_tag(n),
        })
    })
}

/// The account's Elastic IPs (not paged).
pub fn addresses(aws: &Aws) -> Result<Vec<Address>> {
    let xml = aws.query(&SERVICE, "DescribeAddresses", VERSION, &[])?;
    Ok(response(&xml, "DescribeAddresses")
        .list("addressesSet")
        .iter()
        .filter_map(|a| {
            Some(Address {
                public_ip: a.text("publicIp")?,
                allocation_id: a.string("allocationId"),
                association_id: a.string("associationId"),
                name: name_tag(a),
            })
        })
        .collect())
}

/// The snapshots the account owns.
pub fn snapshots(aws: &Aws) -> Result<Vec<Snapshot>> {
    pages(aws, "DescribeSnapshots", &[("Owner.1", "self"), ("MaxResults", "1000")], "snapshotSet", |s| {
        let tags = s.list("tagSet");
        let tag = |k: &str| tags.iter().any(|t| t.string("key").starts_with(k));
        let description = s.string("description");
        Some(Snapshot {
            id: s.text("snapshotId")?,
            volume: s.string("volumeId"),
            volume_size: s.num("volumeSize").unwrap_or(0.0),
            full_size: s.num("fullSnapshotSizeInBytes"),
            started: s.time("startTime"),
            tier: s.text("storageTier").unwrap_or_else(|| "standard".into()),
            name: name_tag(s),
            // An AMI's snapshot left after the AMI was deregistered is not managed: it is
            // one to look at.
            managed: tag("aws:backup:") || tag("dlm:") || description.contains("AWS Backup"),
            description,
        })
    })
}

/// The snapshots the account's own AMIs are made of (deleting them would break the image).
pub fn image_snapshots(aws: &Aws) -> Result<Vec<String>> {
    let images = pages(aws, "DescribeImages", &[("Owner.1", "self")], "imagesSet", |i| {
        let ids: Vec<String> = i.list("blockDeviceMapping").iter().filter_map(|m| m.child("ebs")?.text("snapshotId")).collect();
        Some(ids)
    })?;
    Ok(images.into_iter().flatten().collect())
}

/// The VPCs with a gateway endpoint to S3 and to DynamoDB: (VPC, S3, DynamoDB).
pub fn gateway_endpoints(aws: &Aws) -> Result<Vec<(String, bool, bool)>> {
    let params = [("Filter.1.Name", "vpc-endpoint-type"), ("Filter.1.Value.1", "Gateway"), ("MaxResults", "1000")];
    let endpoints = pages(aws, "DescribeVpcEndpoints", &params, "vpcEndpointSet", |e| Some((e.string("vpcId"), e.string("serviceName"))))?;
    let mut out: Vec<(String, bool, bool)> = Vec::new();
    for (vpc, service) in endpoints {
        let i = match out.iter().position(|(v, _, _)| *v == vpc) {
            Some(i) => i,
            None => {
                out.push((vpc, false, false));
                out.len() - 1
            }
        };
        if service.ends_with(".s3") {
            out[i].1 = true;
        } else if service.ends_with(".dynamodb") {
            out[i].2 = true;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_items_and_tags() {
        let xml = r#"<DescribeVolumesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/"><requestId>r</requestId>
            <volumeSet><item><volumeId>vol-1</volumeId><size>500</size><volumeType>gp2</volumeType><status>available</status>
            <attachmentSet/><tagSet><item><key>Name</key><value>old-data</value></item></tagSet></item>
            <item><volumeId>vol-2</volumeId><size>8</size><volumeType>gp3</volumeType><status>in-use</status>
            <attachmentSet><item><instanceId>i-1</instanceId></item></attachmentSet></item></volumeSet>
            <nextToken>t2</nextToken></DescribeVolumesResponse>"#;
        let r = response(xml, "DescribeVolumes");
        let items = r.list("volumeSet");
        assert_eq!(items.len(), 2);
        assert_eq!(name_tag(&items[0]), "old-data");
        assert!(items[0].list("attachmentSet").is_empty() && !items[1].list("attachmentSet").is_empty());
        assert_eq!(r.text("nextToken").as_deref(), Some("t2"));
    }
}
