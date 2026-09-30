//! The command line of the client.
//!
//! Every setting has a default, so the client runs with no flag at all. See
//! [FR71] and [FR77].

use std::str::FromStr;

/// Where to connect, and what to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub host: String,
    pub port: u16,
    /// The database that `Startup` names. Milestone 5 gives the name a meaning.
    pub database: String,
    /// The statement of `-c`. `None` opens the prompt. See [FR77].
    pub command: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            host: "127.0.0.1".to_string(),
            port: 5432,
            database: "default".to_string(),
            command: None,
        }
    }
}

impl Args {
    /// Reads flags, each one followed by its value. Every other flag is an
    /// error. A setting that no flag names keeps its default.
    pub fn parse(flags: impl IntoIterator<Item = String>) -> Result<Args, String> {
        let mut args = Args::default();
        let mut flags = flags.into_iter();
        while let Some(flag) = flags.next() {
            match flag.as_str() {
                "--host" => args.host = value(&flag, flags.next())?,
                "--port" => args.port = number(&flag, flags.next())?,
                "--database" => args.database = value(&flag, flags.next())?,
                "-c" => args.command = Some(value(&flag, flags.next())?),
                _ => return Err(format!("unknown flag `{flag}`")),
            }
        }
        Ok(args)
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

    fn parse(args: &[&str]) -> Result<Args, String> {
        Args::parse(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn the_default_args_hold_the_documented_values() {
        let a = Args::default();
        assert_eq!(a.host, "127.0.0.1");
        assert_eq!(a.port, 5432);
        assert_eq!(a.database, "default");
        assert_eq!(a.command, None);
    }

    #[test]
    fn no_flag_gives_the_defaults() {
        assert_eq!(parse(&[]).unwrap(), Args::default());
    }

    #[test]
    fn every_flag_reads_in_one_command_line() {
        let a = parse(&[
            "--host",
            "db.example",
            "--port",
            "6000",
            "--database",
            "shop",
            "-c",
            "SELECT 1",
        ])
        .unwrap();
        assert_eq!(
            (a.host, a.port, a.database, a.command),
            (
                "db.example".to_string(),
                6000,
                "shop".to_string(),
                Some("SELECT 1".to_string())
            )
        );
    }

    #[test]
    fn a_setting_that_no_flag_names_keeps_its_default() {
        assert_eq!(parse(&["--port", "6000"]).unwrap().host, "127.0.0.1");
    }

    #[test]
    fn the_last_flag_wins() {
        assert_eq!(
            parse(&["--port", "6000", "--port", "7000"]).unwrap().port,
            7000
        );
    }

    #[test]
    fn a_statement_keeps_its_spaces_and_its_semicolon() {
        let sql = "SELECT a, b FROM t WHERE a = 'x';";
        assert_eq!(parse(&["-c", sql]).unwrap().command, Some(sql.to_string()));
    }

    #[test]
    fn a_flag_with_no_value_is_an_error() {
        for flag in ["--host", "--port", "--database", "-c"] {
            assert_eq!(
                parse(&[flag]).unwrap_err(),
                format!("flag `{flag}` needs a value")
            );
        }
    }

    #[test]
    fn a_port_that_is_not_a_number_is_an_error() {
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
        assert_eq!(parse(&["6000"]).unwrap_err(), "unknown flag `6000`");
    }
}
