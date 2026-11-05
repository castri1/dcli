//! EKS: a cluster whose Kubernetes version is past standard support is billed at the extended
//! support rate (about six times the standard cluster-hour) until it is upgraded. Upgrading
//! moves its hours back to the standard rate.

use super::{Finding, Impact, Scan};
use crate::aws::eks::{self, Cluster, ClusterVersion, Support};
use crate::aws;
use crate::util::{fmt_utc, now_secs};

const MODULE: &str = "EKS";
/// A cluster's hours a month.
const HOURS: f64 = 730.0;
/// Cost Explorer's usage types of a cluster's hours (without the region prefix).
pub const STANDARD_HOURS: &str = "AmazonEKS-Hours:perCluster";
pub const EXTENDED_HOURS: &str = "AmazonEKS-Hours:extendedSupport";

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let overview = eks::overview(scan.aws)?;
    Ok(findings(&overview.clusters, &overview.versions, now_secs()))
}

fn date(t: f64) -> String {
    fmt_utc(t).split(' ').next().unwrap_or_default().to_string()
}

/// The clusters in extended support, one finding each.
pub fn findings(clusters: &[Cluster], versions: &[ClusterVersion], now: f64) -> Vec<Finding> {
    // The oldest version still in standard support: the nearest upgrade that ends the extra.
    let mut standard: Vec<&ClusterVersion> = versions.iter().filter(|v| matches!(Support::of(&v.cluster_version, versions, now), Support::Standard { .. })).collect();
    standard.sort_by_key(|v| eks::version_key(&v.cluster_version));
    let target = standard.first().map(|v| v.cluster_version.clone());
    let mut out = Vec::new();
    for c in clusters {
        let Support::Extended { since, ends } = Support::of(&c.version, versions, now) else { continue };
        let until = ends.map(|e| format!(" until {}", date(e))).unwrap_or_default();
        let to = target.as_deref().map(|t| format!(" to Kubernetes {t} or later")).unwrap_or_default();
        let mut f = Finding::new(
            MODULE,
            c.name.clone(),
            format!("cluster {}", c.name),
            format!("Kubernetes {} is in extended support{until}", c.version),
            format!("Upgrade it{to}: one minor version at a time, control plane first, then node groups and add-ons"),
            format!("{HOURS:.0} cluster-hours a month back at the standard rate"),
            Impact::Medium,
        )
        .kind("eks-extended-support")
        .billed_instead(EXTENDED_HOURS, STANDARD_HOURS, HOURS)
        .score(HOURS)
        .caveat("The upgrade insights of the cluster (dcli's EKS module) list the APIs to move off before each upgrade.");
        if let Some(s) = since {
            f = f.evidence(format!("Standard support for Kubernetes {} ended on {}", c.version, date(s)));
        }
        if let Some(e) = ends {
            f = f.evidence(format!("On {}, when extended support ends, AWS upgrades the cluster itself", date(e)));
        }
        out.push(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clusters_in_extended_support_move_their_hours_to_the_standard_rate() {
        let now = 1_800_000_000.0;
        let day = 86_400.0;
        let versions = vec![
            ClusterVersion { cluster_version: "1.31".into(), version_status: "EXTENDED_SUPPORT".into(), end_of_extended_support_date: Some(now + 20.0 * day), ..Default::default() },
            ClusterVersion { cluster_version: "1.34".into(), version_status: "STANDARD_SUPPORT".into(), ..Default::default() },
            ClusterVersion { cluster_version: "1.33".into(), version_status: "STANDARD_SUPPORT".into(), ..Default::default() },
        ];
        let clusters = vec![
            Cluster { name: "legacy".into(), version: "1.31".into(), ..Default::default() },
            Cluster { name: "shop".into(), version: "1.33".into(), ..Default::default() },
        ];
        let f = findings(&clusters, &versions, now);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].target, "legacy");
        assert!(f[0].recommendation.contains("Kubernetes 1.33 or later"));
        assert_eq!(f[0].billed[0].usage, EXTENDED_HOURS);
        assert_eq!(f[0].billed[0].instead.as_deref(), Some(STANDARD_HOURS));
    }
}
