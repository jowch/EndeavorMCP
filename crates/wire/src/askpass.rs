use serde::{Deserialize, Serialize};

pub const SOCKET_ENV: &str = "ENDEAVOR_ASKPASS_SOCKET";

pub const ADDRESS_ENV: &str = "ENDEAVOR_ASKPASS_ADDRESS";

pub const TOKEN_ENV: &str = "ENDEAVOR_ASKPASS_TOKEN";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Secret,
    YesNo,
    Confirm,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    pub kind: Kind,
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gave_up: bool,
}

impl Ask {
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
