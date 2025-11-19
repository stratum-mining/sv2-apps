//! File-based persistence handler implementation.
//!
//! This module provides a simple file-based persistence handler that appends
//! events to a log file using Debug formatting. Events are written in the background
//! via an async channel to ensure the hot path remains unblocked.

use crate::task_manager::TaskManager;

use super::{PersistenceBackend, PersistenceEvent};
use async_channel::{Receiver, Sender};
use std::{fmt::Debug, path::PathBuf, sync::Arc};
use tokio::io::AsyncWriteExt;

/// File-based persistence handler that appends events to a log file.
///
/// Events are sent through an async channel and written by a background thread,
/// ensuring non-blocking operation for the caller. The file is opened in append
/// mode and events are written using Debug format.
///
/// # Example
///
/// ```rust,no_run
/// use std::{path::PathBuf, sync::Arc};
/// use stratum_apps::persistence::{FileBackend, PersistenceBackend};
/// use stratum_apps::task_manager::TaskManager;
///
/// // Create a file handler with buffer size 1000
/// let task_manager = Arc::new(TaskManager::new());
/// let handler = FileBackend::new(PathBuf::from("events.log"), 1000, task_manager).unwrap();
///
/// // Persist events (non-blocking) - handler uses Debug format internally
/// // handler.persist_event(share_event);
/// ```
#[derive(Debug, Clone)]
pub struct FileBackend {
    sender: Sender<FileCommand>,
}

#[derive(Debug)]
enum FileCommand {
    Write(String),
    Flush,
    Shutdown,
}

impl FileBackend {
    /// Create a new file handler that will write to the specified path.
    ///
    /// This will spawn a background thread that handles all file I/O operations.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the log file
    /// * `channel_size` - The size of the async channel buffer
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created or opened.
    pub fn new(
        path: PathBuf,
        channel_size: usize,
        task_manager: Arc<TaskManager>,
    ) -> std::io::Result<Self> {
        // Ensure the parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let (sender, receiver) = async_channel::bounded(channel_size);

        // Spawn background worker task
        task_manager.spawn(Self::worker_loop(path, receiver));

        tracing::info!("Initialized file persistence handler");
        Ok(Self { sender })
    }

    /// Worker loop that runs as an async task and handles file writes.
    async fn worker_loop(path: PathBuf, receiver: Receiver<FileCommand>) {
        // Open file with tokio async file operations
        let file_result = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await;

        let mut file = match file_result {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("Failed to open file for persistence: {}", e);
                return;
            }
        };

        loop {
            // Use async receive
            match receiver.recv().await {
                Ok(FileCommand::Write(text)) => {
                    let line = format!("{}\n", text);
                    if let Err(e) = file.write_all(line.as_bytes()).await {
                        tracing::error!("Failed to write to file: {}", e);
                    }
                }
                Ok(FileCommand::Flush) => {
                    if let Err(e) = file.flush().await {
                        tracing::error!("Failed to flush file: {}", e);
                    }
                }
                Ok(FileCommand::Shutdown) => {
                    // Drain remaining events
                    while let Ok(cmd) = receiver.try_recv() {
                        match cmd {
                            FileCommand::Write(text) => {
                                let line = format!("{}\n", text);
                                let _ = file.write_all(line.as_bytes()).await;
                            }
                            FileCommand::Flush => {
                                let _ = file.flush().await;
                            }
                            FileCommand::Shutdown => break,
                        }
                    }
                    let _ = file.flush().await;
                    tracing::info!("File persistence worker shutdown complete");
                    break;
                }
                Err(_) => {
                    // Channel closed, shutdown
                    let _ = file.flush().await;
                    tracing::info!("File persistence channel closed, shutting down");
                    break;
                }
            }
        }
    }

    /// Get the number of events waiting in the channel.
    pub fn pending_events(&self) -> usize {
        self.sender.len()
    }
}

impl PersistenceBackend for FileBackend {
    fn persist_event(&self, event: PersistenceEvent) {
        // Format using Debug - handler decides serialization format
        let formatted = format!("{:?}", event);

        // Send is non-blocking when channel has capacity
        // If channel is full, try_send will fail and we log an error
        if let Err(e) = self.sender.try_send(FileCommand::Write(formatted)) {
            tracing::error!("Failed to send event to file persistence: {}", e);
        }
    }

    fn flush(&self) {
        if let Err(e) = self.sender.try_send(FileCommand::Flush) {
            tracing::error!("Failed to send flush command: {}", e);
        }
    }

    fn shutdown(&self) {
        if let Err(e) = self.sender.try_send(FileCommand::Shutdown) {
            tracing::error!("Failed to send shutdown command: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, io::Read};

    #[tokio::test]
    async fn test_file_handler_basic_operations() {
        use super::super::ShareEvent;
        use std::time::SystemTime;

        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!(
            "test_persistence_{}_{}.log",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        // Clean up any existing test file
        let _ = std::fs::remove_file(&test_file);

        let task_manager = Arc::new(crate::task_manager::TaskManager::new());
        let handler = FileBackend::new(test_file.clone(), 100, task_manager).unwrap();

        // Create share hash
        use stratum_core::bitcoin::hashes::{sha256d::Hash, Hash as HashTrait};
        let share_hash = Some(Hash::from_byte_array([0xab; 32]));

        // Write some events
        let event1 = ShareEvent {
            error_code: None,
            extranonce_prefix: vec![0x01, 0x02],
            is_block_found: false,
            is_valid: true,
            nominal_hash_rate: 100.0,
            nonce: 987654321,
            ntime: 1234567890,
            rollable_extranonce_size: None,
            share_hash,
            share_work: 1000.0,
            target: [0xff; 32],
            template_id: Some(5000),
            timestamp: SystemTime::now(),
            user_identity: "miner1".to_string(),
            version: 536870912,
        };

        use super::super::PersistenceEvent;
        handler.persist_event(PersistenceEvent::Share(event1.clone()));
        handler.persist_event(PersistenceEvent::Share(event1.clone()));
        handler.persist_event(PersistenceEvent::Share(event1));
        handler.flush();

        // Give the worker thread time to process and flush
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Read back the file
        let mut file = File::open(&test_file).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();

        assert!(contents.contains("miner1"));
        // Count non-empty lines (writeln! adds trailing newline)
        let line_count = contents.lines().filter(|l| !l.is_empty()).count();
        assert_eq!(line_count, 3);

        // Clean up
        handler.shutdown();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        std::fs::remove_file(&test_file).unwrap();
    }

    #[tokio::test]
    async fn test_file_handler_creates_parent_directories() {
        let temp_dir = std::env::temp_dir();
        let nested_path = temp_dir
            .join(format!("test_nested_{}", std::process::id()))
            .join("subdir")
            .join("persistence.log");

        let task_manager = Arc::new(crate::task_manager::TaskManager::new());
        let handler = FileBackend::new(nested_path.clone(), 100, task_manager).unwrap();

        // Verify that parent directories were created synchronously
        assert!(nested_path.parent().unwrap().exists());

        // Clean up
        handler.shutdown();
        if let Some(parent) = nested_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap());
        }
    }

    #[tokio::test]
    async fn test_file_handler_shutdown() {
        use super::super::ShareEvent;
        use std::time::SystemTime;

        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_shutdown_{}.log", std::process::id()));

        let _ = std::fs::remove_file(&test_file);

        let task_manager = Arc::new(crate::task_manager::TaskManager::new());
        let handler = FileBackend::new(test_file.clone(), 100, task_manager).unwrap();

        use stratum_core::bitcoin::hashes::{sha256d::Hash, Hash as HashTrait};
        let share_hash = Some(Hash::from_byte_array([0u8; 32]));

        let event = ShareEvent {
            error_code: None,
            extranonce_prefix: vec![],
            is_block_found: false,
            is_valid: true,
            nominal_hash_rate: 1.0,
            nonce: 1,
            ntime: 1,
            rollable_extranonce_size: None,
            share_hash,
            share_work: 1.0,
            target: [0; 32],
            template_id: None,
            timestamp: SystemTime::now(),
            user_identity: "test".to_string(),
            version: 1,
        };

        use super::super::PersistenceEvent;
        handler.persist_event(PersistenceEvent::Share(event));
        handler.shutdown();

        // Give worker time to shutdown
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify file was flushed
        let metadata = std::fs::metadata(&test_file).unwrap();
        assert!(metadata.len() > 0);

        // Clean up
        let _ = std::fs::remove_file(&test_file);
    }
}
