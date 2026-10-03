//! Server settings and the command line that fills them.
//!
//! Every setting has a default, so the server starts with no flag at all.

use std::path::PathBuf;
use std::str::FromStr;

const GIB: u64 = 1024 * 1024 * 1024;

/// The memory limit that [NFR10] sets by default.
const DEFAULT_MEM_LIMIT: u64 = GIB;

/// The WAL size that the memory budget in `design.md` assumes.
const DEFAULT_WAL_MAX_BYTES: u64 = 8 * GIB;

/// The connection count that [NFR14] asks for.
const DEFAULT_MAX_CONNECTIONS: usize = 100;

/// The settings of one server process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The directory that holds one directory for each database.
    pub data_dir: PathBuf,
    pub port: u16,
    /// The memory limit in bytes. See [NFR10].
    pub mem_limit: u64,
    /// The WAL size in bytes at which a checkpoint runs.
    pub wal_max_bytes: u64,
    /// The wait in milliseconds after which a lock fails with `LOCK_TIMEOUT`.
    pub lock_timeout_ms: u64,
    /// The connection count that the server accepts. See [NFR14].
    pub max_connections: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: PathBuf::from("./data"),
            port: 5432,
            mem_limit: DEFAULT_MEM_LIMIT,
            wal_max_bytes: DEFAULT_WAL_MAX_BYTES,
            lock_timeout_ms: 5000,
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }
}

impl Config {
    /// Reads the command line of this process. The program name is dropped.
    pub fn from_args() -> Result<Config, String> {
        Config::parse(std::env::args().skip(1))
    }

    /// Reads flags, each one followed by its value. Every other flag is an
    /// error. A setting that no flag names keeps its default.
    fn parse(flags: impl IntoIterator<Item = String>) -> Result<Config, String> {
        let mut config = Config::default();
        let mut flags = flags.into_iter();
        while let Some(flag) = flags.next() {
            match flag.as_str() {
                "--data-dir" => config.data_dir = PathBuf::from(value(&flag, flags.next())?),
                "--port" => config.port = number(&flag, flags.next())?,
                "--max-connections" => config.max_connections = number(&flag, flags.next())?,
                _ => return Err(format!("unknown flag `{flag}`")),
            }
        }
        Ok(config)
    }
}

fn value(flag: &str, arg: Option<String>) -> Result<String, String> {
    arg.ok_or_else(|| format!("flag `{flag}` needs a value"))
}

fn number<T: FromStr>(flag: &str, arg: Option<String>) -> Result<T, String> {
    let text = value(flag, arg)?;
    text.parse()
        .map_err(|_| format!("flag `{flag}` needs a number, got `{text}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, String> {
        Config::parse(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn the_default_config_holds_the_documented_values() {
        let c = Config::default();
        assert_eq!(c.data_dir, PathBuf::from("./data"));
        assert_eq!(c.port, 5432);
        assert_eq!(c.mem_limit, 1024 * 1024 * 1024);
        assert_eq!(c.wal_max_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(c.lock_timeout_ms, 5000);
        assert_eq!(c.max_connections, 100);
    }

    #[test]
    fn no_flag_gives_the_defaults() {
        assert_eq!(parse(&[]).unwrap(), Config::default());
    }

    #[test]
    fn data_dir_takes_the_next_argument() {
        assert_eq!(
            parse(&["--data-dir", "/var/lib/garibaldb"])
                .unwrap()
                .data_dir,
            PathBuf::from("/var/lib/garibaldb")
        );
    }

    #[test]
    fn port_takes_the_next_argument() {
        assert_eq!(parse(&["--port", "6000"]).unwrap().port, 6000);
    }

    #[test]
    fn max_connections_takes_the_next_argument() {
        assert_eq!(
            parse(&["--max-connections", "7"]).unwrap().max_connections,
            7
        );
    }

    #[test]
    fn every_flag_reads_in_one_command_line() {
        let c = parse(&[
            "--port",
            "6000",
            "--data-dir",
            "/tmp/db",
            "--max-connections",
            "7",
        ])
        .unwrap();
        assert_eq!(
            (c.port, c.data_dir, c.max_connections),
            (6000, PathBuf::from("/tmp/db"), 7)
        );
    }

    #[test]
    fn a_setting_that_no_flag_names_keeps_its_default() {
        assert_eq!(parse(&["--port", "6000"]).unwrap().mem_limit, GIB);
    }

    #[test]
    fn the_last_flag_wins() {
        assert_eq!(
            parse(&["--port", "6000", "--port", "7000"]).unwrap().port,
            7000
        );
    }

    #[test]
    fn a_flag_with_no_value_is_an_error() {
        assert_eq!(
            parse(&["--port"]).unwrap_err(),
            "flag `--port` needs a value"
        );
    }

    #[test]
    fn a_value_that_is_not_a_number_is_an_error() {
        assert_eq!(
            parse(&["--port", "http"]).unwrap_err(),
            "flag `--port` needs a number, got `http`"
        );
    }

    #[test]
    fn a_port_above_the_u16_range_is_an_error() {
        assert!(parse(&["--port", "65536"]).is_err());
    }

    #[test]
    fn an_unknown_flag_is_an_error() {
        assert_eq!(
            parse(&["--verbose"]).unwrap_err(),
            "unknown flag `--verbose`"
        );
    }

    #[test]
    fn an_unknown_flag_with_no_value_names_the_flag() {
        assert_eq!(parse(&["--wat"]).unwrap_err(), "unknown flag `--wat`");
    }

    #[test]
    fn a_value_that_stands_alone_is_an_unknown_flag() {
        assert_eq!(parse(&["6000"]).unwrap_err(), "unknown flag `6000`");
    }
}
