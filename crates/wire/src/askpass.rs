//! `ssh` asking for a password, a two-factor code or a yes/no, through the
//! helper's askpass mode: ssh runs the app (or the `endeavor` helper) as `SSH_ASKPASS`, which
//! sends one [`Ask`] line to the app's Unix socket (path in [`SOCKET_ENV`]) and
//! prints the [`Answer`] for ssh.

use serde::{Deserialize, Serialize};

pub const SOCKET_ENV: &str = "ENDEAVOR_ASKPASS_SOCKET";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// Type something in (password, verification code); never shown.
    Secret,
    /// ssh wants the word "yes" or "no" (an unknown host key).
    YesNo,
    /// ssh only reads the exit status (`SSH_ASKPASS_PROMPT=confirm`).
    Confirm,
}

/// Helper → app: one JSON line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    pub kind: Kind,
    pub prompt: String,
}

/// App → helper: one JSON line. `None` means the user cancelled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub text: Option<String>,
}

impl Ask {
    /// What ssh's askpass call means, from its prompt and `SSH_ASKPASS_PROMPT`.
    pub fn from_ssh(prompt: &str, hint: Option<&str>) -> Ask {
        let kind = if hint == Some("confirm") {
            Kind::Confirm
        } else if prompt.contains("(yes/no") {
            Kind::YesNo
        } else {
            Kind::Secret
        };
        Ask { kind, prompt: prompt.trim_end().to_owned() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_ssh_is_asking() {
        let host_key = "The authenticity of host 'lab (10.0.0.2)' can't be established.\nED25519 key fingerprint is SHA256:abc.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";
        assert_eq!(Ask::from_ssh(host_key, None).kind, Kind::YesNo);
        assert!(Ask::from_ssh(host_key, None).prompt.ends_with("[fingerprint])?"));
        assert_eq!(Ask::from_ssh("jc@lab's password: ", None).kind, Kind::Secret);
        assert_eq!(Ask::from_ssh("Allow use of key id_ed25519?", Some("confirm")).kind, Kind::Confirm);
    }
}
