use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransferDirection {
    Send,
    Receive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransferStatus {
    Queued,
    Transferring,
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferProgress {
    pub transfer_id: String,
    pub direction: TransferDirection,
    pub peer_device_id: String,
    pub peer_device_name: String,
    pub current_file: String,
    pub file_index: u32,
    pub total_files: u32,
    pub bytes_transferred: u64,
    pub total_bytes: u64,
    pub progress_percent: f32,
    pub speed_bytes_per_sec: u64,
    pub status: TransferStatus,
}

impl TransferProgress {
    pub fn speed_formatted(&self) -> String {
        let mb = (self.speed_bytes_per_sec as f64) / (1024.0 * 1024.0);
        format!("{:.1} MB/s", mb)
    }
}
