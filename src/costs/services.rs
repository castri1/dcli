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

/// The dcli module that explores a Cost Explorer service.
pub fn module(ce_service: &str) -> Option<&'static str> {
    SERVICES.iter().find(|(ce, _, _)| *ce == ce_service).map(|(_, m, _)| *m)
}

/// The Savings check that looks at a Cost Explorer service.
pub fn check(ce_service: &str) -> Option<&'static str> {
    SERVICES.iter().find(|(ce, _, _)| *ce == ce_service).and_then(|(_, _, c)| *c)
}

/// Cost Explorer's name for the service a Savings check looks at (CloudWatch alarms and logs
/// are both `AmazonCloudWatch`).
pub fn of_check(check: &str) -> Option<&'static str> {
    if check == "CloudWatch alarms" {
        return Some("AmazonCloudWatch");
    }
    SERVICES.iter().find(|(_, _, c)| *c == Some(check)).map(|(ce, _, _)| *ce)
}
