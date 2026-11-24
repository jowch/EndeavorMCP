use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use crate::log::Log;

pub struct Injection {
    spec: Value,
    work: PathBuf,
    endeavor: PathBuf,
    project: PathBuf,
}

impl Injection {
    pub fn load(work: &Path, endeavor: &Path, project: &Path) -> Option<Injection> {
        let spec: Value = serde_json::from_str(&std::fs::read_to_string(work.join("inject.json")).ok()?).ok()?;
        Some(Injection { spec, work: work.to_owned(), endeavor: endeavor.to_owned(), project: project.to_owned() })
    }

    pub fn matches(&self, when: &str, tool: &str) -> bool {
        self.spec["when"] == when && self.spec["tools"].as_array().into_iter().flatten().any(|t| t == tool) && !self.marker().exists()
    }

    fn marker(&self) -> PathBuf {
        self.work.join("injected")
    }

    pub fn run(&self, log: &Mutex<Log>) {
        let _ = std::fs::write(self.marker(), "");
        let result = (|| -> Result<(), String> {
            let mut session = crate::mcp::Session::start(&self.endeavor, &self.work, &self.project)?;
            let id = crate::mcp::notebook_id(&mut session, self.spec["notebook"].as_str().unwrap_or_default())?;
            for step in self.spec["steps"].as_array().into_iter().flatten() {
                if let Some(page) = step.get("page") {
                    let reply = move_on_page(&mut session, &id, page);
                    log.lock().unwrap().write("inject", &json!({ "page": page, "ok": reply.is_ok(), "reply": reply.clone().err() }).to_string());
                    reply?;
                    continue;
                }
                let tool = step["tool"].as_str().unwrap_or_default();
                let mut args = step["args"].clone();
                args["notebook_id"] = json!(id);
                let reply = session.tool(tool, args.clone());
                let entry = match &reply {
                    Ok(r) => json!({ "tool": tool, "args": args, "ok": true, "reply": r }),
                    Err(e) => json!({ "tool": tool, "args": args, "ok": false, "reply": e }),
                };
                log.lock().unwrap().write("inject", &entry.to_string());
                reply?;
            }
            crate::mcp::wait_until_done(&mut session, &id, Duration::from_secs(300))
        })();
        if let Err(e) = result {
            log.lock().unwrap().write("inject", &json!({ "ok": false, "reply": e }).to_string());
        }
    }
}

fn move_on_page(session: &mut crate::mcp::Session, id: &str, page: &Value) -> Result<(), String> {
    use base64::Engine;
    let bond = page["bond"].as_str().ok_or("page: bond must be a string")?;
    let socket = session.view("notebook_socket_open", json!({ "notebook_id": id }))?["socket"].clone();
    let message = |kind: &str, body: Value| json!({ "type": kind, "client_id": "smoke", "request_id": kind, "notebook_id": id, "body": body });
    let messages = [
        message("connect", json!({})),
        message("reset_shared_state", json!({})),
        message("update_notebook", json!({ "updates": [{ "op": "add", "path": ["bonds", bond], "value": { "value": page["value"] } }] })),
    ];
    for m in messages {
        let data = base64::engine::general_purpose::STANDARD.encode(msgpack(&m));
        session.view("notebook_socket_send", json!({ "socket": socket, "data": data }))?;
        session.view("notebook_socket_recv", json!({ "socket": socket, "wait_ms": 1000 }))?;
    }
    std::thread::sleep(Duration::from_secs(1));
    let _ = session.view("notebook_socket_close", json!({ "socket": socket }));
    Ok(())
}

fn msgpack(value: &Value) -> Vec<u8> {
    fn put(out: &mut Vec<u8>, value: &Value) {
        let len = |out: &mut Vec<u8>, n: usize, fix: u8, small: usize, codes: [u8; 2]| {
            if n < small {
                out.push(fix | n as u8);
            } else if n < 65536 {
                out.push(codes[0]);
                out.extend((n as u16).to_be_bytes());
            } else {
                out.push(codes[1]);
                out.extend((n as u32).to_be_bytes());
            }
        };
        match value {
            Value::Null => out.push(0xc0),
            Value::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
            Value::Number(n) => match (n.as_i64(), n.as_f64()) {
                (Some(i), _) => {
                    out.push(0xd3);
                    out.extend(i.to_be_bytes());
                }
                (None, Some(f)) => {
                    out.push(0xcb);
                    out.extend(f.to_be_bytes());
                }
                _ => out.push(0xc0),
            },
            Value::String(s) => {
                if s.len() < 32 {
                    out.push(0xa0 | s.len() as u8);
                } else if s.len() < 256 {
                    out.extend([0xd9, s.len() as u8]);
                } else {
                    len(out, s.len(), 0, 0, [0xda, 0xdb]);
                }
                out.extend(s.as_bytes());
            }
            Value::Array(a) => {
                len(out, a.len(), 0x90, 16, [0xdc, 0xdd]);
                a.iter().for_each(|v| put(out, v));
            }
            Value::Object(m) => {
                len(out, m.len(), 0x80, 16, [0xde, 0xdf]);
                for (k, v) in m {
                    put(out, &Value::String(k.clone()));
                    put(out, v);
                }
            }
        }
    }
    let mut out = Vec::new();
    put(&mut out, value);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn msgpack_writes_what_plutos_socket_reads() {
        let m = super::msgpack(&serde_json::json!({ "a": [1, "b", true, null, 1.5] }));
        assert_eq!(m, [0x81, 0xa1, b'a', 0x95, 0xd3, 0, 0, 0, 0, 0, 0, 0, 1, 0xa1, b'b', 0xc3, 0xc0, 0xcb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0]);
    }
}
