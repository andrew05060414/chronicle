//! Rolling compressed checkpoints of a hub SQLite database.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc, Weekday};
use serde::{Deserialize, Serialize};

use crate::config::CheckpointConfig;
use crate::db::Database;
use crate::error::{Error, Result};

const MANIFEST_VERSION: u32 = 1;
