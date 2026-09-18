//! Remote sync functionality over SSH.
//!
//! Provides fetching and bidirectional merging of hstry databases across machines.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::config::RemoteConfig;
use crate::db::{Database, SearchOptions};
use crate::error::{Error, Result};
use crate::models::{Conversation, ConversationWithMessages, Message, Source};
use crate::recall::SearchReport;

/// Default remote database path (XDG standard).
pub const DEFAULT_REMOTE_DB_PATH: &str = "~/.local/share/hstry/hstry.db";
