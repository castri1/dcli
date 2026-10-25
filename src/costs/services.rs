//! Cost Explorer's service names next to the dcli module that explores each service and the
//! Savings check that looks at it.

/// (Cost Explorer's name, dcli module, Savings check)
const SERVICES: &[(&str, &str, Option<&str>)] = &[
    ("AWS Lambda", "Lambda", Some("Lambda")),
    ("AmazonCloudWatch", "Logs", Some("CloudWatch Logs")),
    ("Amazon DynamoDB", "DynamoDB", Some("DynamoDB")),
    ("Amazon Simple Storage Service", "S3", Some("S3")),
    ("Amazon Simple Queue Service", "SQS", Some("SQS")),
    ("AWS Step Functions", "Step Functions", Some("Step Functions")),
    ("AWS Secrets Manager", "Secrets", Some("Secrets Manager")),
    ("Amazon API Gateway", "API Gateway", Some("API Gateway")),
    ("Amazon Relational Database Service", "Aurora", Some("Aurora")),
    ("Amazon Redshift", "Redshift", Some("Redshift")),
    ("AWS Database Migration Service", "DMS", Some("DMS")),
    ("AWS CodePipeline", "Pipelines", None),
    ("CodeBuild", "Pipelines", None),
    ("AWS CloudFormation", "CloudFormation", None),
    ("AWS Systems Manager", "Parameters", None),
    ("Amazon EventBridge", "EventBridge", None),
    ("AWS AppConfig", "AppConfig", None),
];

/// Services a Savings check looks at that no dcli module explores: (Cost Explorer's name,
/// check). Each Bedrock model is billed as a service of its own (`Claude Sonnet 4 (Amazon
/// Bedrock Edition)`): any name with "Bedrock" is the Bedrock check's.
const CHECKED: &[(&str, &str)] = &[
    ("EC2 - Other", "EC2"),
    ("Amazon Virtual Private Cloud", "EC2"),
    ("AWS X-Ray", "X-Ray"),
    ("Amazon Bedrock", "Bedrock"),
];

/// The dcli module that explores a Cost Explorer service.
pub fn module(ce_service: &str) -> Option<&'static str> {
    SERVICES.iter().find(|(ce, _, _)| *ce == ce_service).map(|(_, m, _)| *m)
}

/// The Savings check that looks at a Cost Explorer service.
pub fn check(ce_service: &str) -> Option<&'static str> {
    SERVICES
        .iter()
        .find(|(ce, _, _)| *ce == ce_service)
        .and_then(|(_, _, c)| *c)
        .or_else(|| CHECKED.iter().find(|(ce, _)| *ce == ce_service).map(|(_, c)| *c))
        .or_else(|| ce_service.contains("Bedrock").then_some("Bedrock"))
}

/// Cost Explorer's name for the service a Savings check looks at (CloudWatch alarms and logs
/// are both `AmazonCloudWatch`). A finding billed under another name says it
/// ([`crate::savings::Finding::ce_service`]).
pub fn of_check(check: &str) -> Option<&'static str> {
    if check == "CloudWatch alarms" {
        return Some("AmazonCloudWatch");
    }
    SERVICES
        .iter()
        .find(|(_, _, c)| *c == Some(check))
        .map(|(ce, _, _)| *ce)
        .or_else(|| CHECKED.iter().find(|(_, c)| *c == check).map(|(ce, _)| *ce))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_without_a_module() {
        assert_eq!(check("EC2 - Other"), Some("EC2"));
        assert_eq!(check("Claude Sonnet 4 (Amazon Bedrock Edition)"), Some("Bedrock"));
        assert_eq!(module("AWS X-Ray"), None);
        assert_eq!(of_check("EC2"), Some("EC2 - Other"));
        assert_eq!(of_check("X-Ray"), Some("AWS X-Ray"));
        assert_eq!(of_check("Lambda"), Some("AWS Lambda"));
    }
}
