use serde::{Deserialize, Serialize};

pub const INTERVAL_RUN_KEY: &str = "_flow_interval_last_run";

#[derive(Serialize, Deserialize)]
pub struct AirbyteIntervalCheckpoint {
    #[serde(rename = "_flow_interval_last_run")]
    pub flow_interval_last_run: Option<String>
}

