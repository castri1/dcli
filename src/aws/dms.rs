//! Database Migration Service (awsJson1_1, `X-Amz-Target: AmazonDMSv20160101.<Operation>`):
//! replication instances, tasks, their table statistics, endpoints and connections.

use super::Service;

#[allow(dead_code)] // used by the DMS module
pub const SERVICE: Service = Service { endpoint: "dms", signing_name: "dms", target_prefix: "AmazonDMSv20160101" };
