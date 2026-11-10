//! Password management from the command line.
//!
//! The portal can change a password, but only for the account that is
//! logged in.  When the password is lost — or a second operator has to be
//! added — there has to be a way in that does not depend on the portal.
//!
//! This mode runs *before* the window is created, so it works even when
//! the app itself cannot start.  It reads the same config file the app
//! uses, so a change takes effect on the next start.
//!
//! Usage:
//!
//!     annotator --password list
//!     annotator --password set <user> [--admin]     # prompts for the password
//!     annotator --password reset <user>             # generates a password
//!     annotator --password generate                 # prints a password
//!
//! The password is read from stdin with echo disabled, so it never
//! appears in the shell history or in `ps` output.

use std::io::{BufRead, Write};

use crate::config::{self, MAX_USERS};

/// What the command line asked for.
#[derive(Debug)]
pub enum Action {
    /// List the accounts.
    List,
    /// Set a password for an account, prompted for on stdin.
    Set { username: String, admin: bool },
    /// Reset an account to a freshly generated password.
    Reset { username: String },
    /// Print a generated password and exit.
    Generate,
}

/// Parse the `--password` arguments.
///
/// Returns `Ok(None)` when the mode was not requested — the ordinary
/// case, where the app starts normally.
pub fn parse(args: &[String]) -> Result<Option<Action>, String> {
    let Some(pos) = args.iter().position(|a| a == "--password") else {
        return Ok(None);
    };
    let rest = &args[pos + 1..];

    // `--password` with nothing after it, or followed by another flag,
    // means "list".
    let verb = rest
        .first()
        .filter(|a| !a.starts_with("--"))
        .map(|s| s.as_str())
        .unwrap_or("list");

    match verb {
        "list" => Ok(Some(Action::List)),
        "generate" => Ok(Some(Action::Generate)),
        "set" => {
            let username = rest
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .ok_or("usage: annotator --password set <username>")?
                .clone();
            let admin = rest.iter().any(|a| a == "--admin");
            Ok(Some(Action::Set { username, admin }))
        }
        "reset" => {
            let username = rest
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .ok_or("usage: annotator --password reset <username>")?
                .clone();
            Ok(Some(Action::Reset { username }))
        }
        other => Err(format!(
            "unknown --password action '{other}' \
             (expected list, set, reset or generate)"
        )),
    }
}

/// Run the requested action.  Returns the process exit code.
pub fn run(action: Action) -> i32 {
    let mut cfg = config::load();

    match action {
        Action::List => {
            println!("Accounts in {}", config::config_path().display());
            if cfg.users.is_empty() {
                println!("  (none)");
            }
            for u in &cfg.users {
                let flags = [
                    if u.is_admin { "admin" } else { "user" },
                    if u.must_change_password {
                        "must change password"
                    } else {
                        ""
                    },
                ];
                let flags: Vec<&str> = flags.into_iter().filter(|f| !f.is_empty()).collect();
                println!("  {:<20} {}", u.username, flags.join(", "));
            }
            println!();
            println!("The password takes effect the next time the app starts.");
            0
        }

        Action::Generate => {
            println!("{}", config::generate_password());
            0
        }

        Action::Set { username, admin } => {
            let password = match read_password("New password: ") {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: {e}");
                    return 1;
                }
            };
            if password.len() < 8 {
                eprintln!("error: the password must be at least 8 characters");
                return 1;
            }
            // Confirm, so a typo does not lock the operator out.
            let again = match read_password("Confirm password: ") {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: {e}");
                    return 1;
                }
            };
            if password != again {
                eprintln!("error: the passwords do not match");
                return 1;
            }

            // An existing account is updated; an unknown one is created,
            // which is how a second operator is added.
            if !cfg.users.iter().any(|u| u.username == username) {
                if cfg.users.len() >= MAX_USERS {
                    eprintln!(
                        "error: the config already holds the maximum of {MAX_USERS} accounts"
                    );
                    return 1;
                }
                cfg.users.push(config::User {
                    username: username.clone(),
                    password_hash: String::new(),
                    is_admin: admin,
                    must_change_password: false,
                });
                println!("Created account '{username}'.");
            } else if admin {
                if let Some(u) = cfg.users.iter_mut().find(|u| u.username == username) {
                    u.is_admin = true;
                }
                println!("'{username}' is now an administrator.");
            }

            if let Err(e) = config::set_password(&mut cfg, &username, &password) {
                eprintln!("error: {e}");
                return 1;
            }
            if let Err(e) = config::save(&cfg) {
                eprintln!("error: {e}");
                return 1;
            }
            println!("Password set for '{username}'.");
            println!("The password takes effect the next time the app starts.");
            0
        }

        Action::Reset { username } => {
            let password = config::generate_password();
            // Create the account if it does not exist, so a reset can
            // also be used to add an operator.
            if !cfg.users.iter().any(|u| u.username == username) && cfg.users.len() < MAX_USERS {
                cfg.users.push(config::User {
                    username: username.clone(),
                    password_hash: String::new(),
                    is_admin: false,
                    must_change_password: false,
                });
            }
            if let Err(e) = config::set_password(&mut cfg, &username, &password) {
                eprintln!("error: {e}");
                return 1;
            }
            if let Err(e) = config::save(&cfg) {
                eprintln!("error: {e}");
                return 1;
            }
            println!("Password for '{username}' reset to:");
            println!("  {password}");
            println!();
            println!("Record it now — it is not stored anywhere else.");
            println!("The password takes effect the next time the app starts.");
            0
        }
    }
}

/// Read a line from stdin with echo disabled.
///
/// Without a terminal — a pipe, say — echo cannot be turned off, but the
/// line is still read.  That makes the mode scriptable.
fn read_password(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();

    let restore = disable_echo();
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    restore_terminal(restore);
    println!();

    read.map_err(|e| format!("could not read the password: {e}"))?;
    let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
    if trimmed.is_empty() {
        return Err("the password must not be empty".into());
    }
    Ok(trimmed)
}

/// The terminal settings, saved so they can be restored exactly.
#[cfg(unix)]
type SavedTermios = libc::termios;

/// Turn off terminal echo, returning the settings to restore afterwards.
///
/// `None` means the terminal could not be changed — a pipe, for
/// instance — in which case the line is still read, just echoed.
fn disable_echo() -> Option<SavedTermios> {
    #[cfg(unix)]
    {
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } != 0 {
            return None;
        }
        let saved = term;
        term.c_lflag &= !(libc::ECHO | libc::ECHONL);
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) };
        Some(saved)
    }
    #[cfg(not(unix))]
    None
}

/// Restore the terminal settings, if they were changed.
fn restore_terminal(saved: Option<SavedTermios>) {
    #[cfg(unix)]
    if let Some(term) = saved {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) };
    }
    #[cfg(not(unix))]
    let _ = saved;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_password_flag_means_no_mode() {
        assert!(parse(&args(&["annotator"])).unwrap().is_none());
        assert!(parse(&args(&[])).unwrap().is_none());
    }

    #[test]
    fn the_list_action_is_recognised() {
        assert!(matches!(
            parse(&args(&["annotator", "--password", "list"])).unwrap(),
            Some(Action::List)
        ));
        // A bare --password lists too.
        assert!(matches!(
            parse(&args(&["annotator", "--password"])).unwrap(),
            Some(Action::List)
        ));
    }

    #[test]
    fn the_generate_action_is_recognised() {
        assert!(matches!(
            parse(&args(&["annotator", "--password", "generate"])).unwrap(),
            Some(Action::Generate)
        ));
    }

    #[test]
    fn the_set_action_carries_the_username() {
        match parse(&args(&["annotator", "--password", "set", "alice"])).unwrap() {
            Some(Action::Set { username, admin }) => {
                assert_eq!(username, "alice");
                assert!(!admin);
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn the_set_action_can_make_an_administrator() {
        match parse(&args(&[
            "annotator",
            "--password",
            "set",
            "alice",
            "--admin",
        ]))
        .unwrap()
        {
            Some(Action::Set { username, admin }) => {
                assert_eq!(username, "alice");
                assert!(admin);
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn the_reset_action_carries_the_username() {
        match parse(&args(&["annotator", "--password", "reset", "bob"])).unwrap() {
            Some(Action::Reset { username }) => assert_eq!(username, "bob"),
            other => panic!("expected Reset, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_username_is_an_error() {
        assert!(parse(&args(&["annotator", "--password", "set"])).is_err());
        assert!(parse(&args(&["annotator", "--password", "reset"])).is_err());
    }

    #[test]
    fn an_unknown_action_is_an_error() {
        assert!(parse(&args(&["annotator", "--password", "frobnicate"])).is_err());
    }

    #[test]
    fn the_flags_before_the_verb_are_ignored() {
        match parse(&args(&[
            "annotator",
            "--verbose",
            "--password",
            "reset",
            "bob",
        ]))
        .unwrap()
        {
            Some(Action::Reset { username }) => assert_eq!(username, "bob"),
            other => panic!("expected Reset, got {other:?}"),
        }
    }
}
