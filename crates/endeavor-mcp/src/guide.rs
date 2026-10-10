//! Endeavor's skills (`plugin/skills`) for agents that don't load the
//! Claude Code plugin: the `notebook_guide` tool serves them, and the server's
//! MCP `instructions` say to call it first. A session whose agent loads the
//! plugin says so with `X-Endeavor-Skills: plugin` and gets neither.

use serde_json::{Value, json};

pub const TOOL: &str = "notebook_guide";

/// Each skill file by its path under `plugin/skills`.
const FILES: [(&str, &str); 6] = [
    (NOTEBOOKS, include_str!("../../../plugin/skills/endeavor-notebooks/SKILL.md")),
    (PLUTO, include_str!("../../../plugin/skills/endeavor-notebooks/reference/pluto.md")),
    ("endeavor-notebooks/reference/app.md", include_str!("../../../plugin/skills/endeavor-notebooks/reference/app.md")),
    ("endeavor-notebooks/reference/errors.md", include_str!("../../../plugin/skills/endeavor-notebooks/reference/errors.md")),
    ("endeavor-machines/SKILL.md", include_str!("../../../plugin/skills/endeavor-machines/SKILL.md")),
    ("endeavor-machines/reference/machine-tools.md", include_str!("../../../plugin/skills/endeavor-machines/reference/machine-tools.md")),
];

/// The skill the guide is, when asked for no topic.
const NOTEBOOKS: &str = "endeavor-notebooks/SKILL.md";

/// The engine reference the guide includes while Pluto is the only engine.
const PLUTO: &str = "endeavor-notebooks/reference/pluto.md";

/// Points an agent without the plugin's skills to the guide.
const READ_GUIDE: &str = "Before your first notebook tool call in a session, call `notebook_guide` once with no arguments and follow what it says: \
how to find this session's notebook, the read-edit-run loop, and the rules for a Pluto cell.";

/// Where the guide keeps what only the app needs, for an agent in the app without the skills.
const APP_GUIDE: &str = "The rules for runs the user must approve are in the topic `endeavor-notebooks/reference/app.md`.";

/// What a runtime with the app tells an agent without the skills.
const APP: &str = "These tools edit and run a live Pluto (Julia) notebook that the user sees in Endeavor, next to this chat.";

/// What a runtime without the app (`endeavor serve` or `mcp`) tells every
/// agent: the skill keeps what holds only in the app apart, and this says
/// which side the agent is on.
pub const STANDALONE: &str = "These tools edit and run live Pluto (Julia) notebooks without the Endeavor app: \
the user watches them in a web browser, on Pluto's own page, and there is no notebook pane next to this chat. \
`new_notebook` and `open_notebook` return `browser_url`. When the result has `opened_in_browser` true, the notebook is already open in the user's browser: tell them. Otherwise give them `browser_url`. \
Endeavor's skills (or `notebook_guide`) and these tools' descriptions say where something holds only in the Endeavor app, \
such as the reference `app.md`: skip those parts.";

/// What `endeavor mcp` adds to what it tells every agent: it has the machine tools.
pub const MACHINES: &str = "This server also has `list_machines`, `add_machine`, `use_machine` and `stop_machine`, which put this session's notebooks on a server or a Slurm cluster \
that the user reaches over ssh. `list_machines` only reads and is fine any time; call the other three only when the user asks.";

/// What an agent without the skills is told about the machine tools on top of `MACHINES`.
const MACHINES_GUIDE: &str = "Before your first `add_machine`, `use_machine` or `stop_machine` call, \
call `notebook_guide` with `topic` set to `endeavor-machines/SKILL.md` and follow it.";

/// The server's MCP `instructions` for an agent with the plugin's skills or
/// without (`has_skills`), on a runtime with the app or without (`standalone`);
/// `machines`: from `endeavor mcp`, which has the machine tools.
pub fn instructions(standalone: bool, has_skills: bool, machines: bool) -> Option<String> {
    let machines_text = match (machines, has_skills) {
        (false, _) => String::new(),
        (true, true) => format!(" {MACHINES}"),
        (true, false) => format!(" {MACHINES} {MACHINES_GUIDE}"),
    };
    match (standalone, has_skills) {
        (false, true) => None,
        (false, false) => Some(format!("{APP} {READ_GUIDE} {APP_GUIDE}")),
        (true, true) => Some(format!("{STANDALONE}{machines_text}")),
        (true, false) => Some(format!("{STANDALONE}{machines_text}\n\n{READ_GUIDE}")),
    }
}

/// `tools/list`'s entry for the guide.
pub fn schema() -> Value {
    json!({
        "name": TOOL,
        "description": "How to work in Endeavor's notebooks with these tools. Call it once, with no arguments, before your first other notebook tool call in a session, and follow it. \
It names further topics (paths ending in .md): call it again with `topic` set to one when it says to read it.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "A topic the guide names, such as \"endeavor-notebooks/reference/pluto.md\". Leave it out for the guide itself." }
            },
        },
    })
}

/// The guide, or one of its topics.
pub fn read(arguments: &Value) -> Result<String, String> {
    let topic = arguments["topic"].as_str().map(str::trim).filter(|t| !t.is_empty());
    let Some(topic) = topic else { return Ok(whole()) };
    let path = if topic.ends_with(".md") { topic.to_owned() } else { format!("{}/SKILL.md", topic.trim_end_matches('/')) };
    let path = path.trim_start_matches("./");
    match FILES.iter().find(|(p, _)| *p == path) {
        Some((path, text)) => Ok(served(path, text)),
        None => Err(format!(
            "ArgumentError: not_found::No guide topic '{topic}'. Topics: {}",
            FILES.iter().map(|(p, _)| *p).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The notebook skill and the Pluto reference, and how to ask for what they and the machine tools point to.
fn whole() -> String {
    let file = |wanted: &str| {
        let (path, text) = FILES.iter().find(|(p, _)| *p == wanted).expect("the file is in FILES");
        served(path, text)
    };
    format!(
        "This guide is the notebook skill followed by the Pluto reference (`endeavor-notebooks/reference/pluto.md`), so where the skill says to read that reference, it is below: don't ask for it again. \
         Other links to .md paths in this guide are further topics: call `notebook_guide` with `topic` set to the path when the guide says to read one.\n\n\
         {}\n---\n\n\
         {}\n---\n\n\
         If you have the tool `list_machines`, the notebooks can run on a server or cluster: call `notebook_guide` with `topic` set to `endeavor-machines` for how.\n",
        file(NOTEBOOKS),
        file(PLUTO)
    )
}

/// A skill file without its front matter, with its links rewritten as topics.
fn served(path: &str, text: &str) -> String {
    let body = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or(text, |(_, body)| body)
        .trim_start();
    let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find("](") {
        out.push_str(&rest[..at + 2]);
        rest = &rest[at + 2..];
        let Some(end) = rest.find(')') else { break };
        let target = &rest[..end];
        out.push_str(&if target.contains("://") || !target.ends_with(".md") { target.to_owned() } else { topic(dir, target) });
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// A link's target, relative to the folder of the file it's in, as a topic:
/// a path under `plugin/skills`, or just the skill's name for its SKILL.md.
fn topic(dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let path = parts.join("/");
    path.strip_suffix("/SKILL.md").map_or(path.clone(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_every_skill_file() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../plugin/skills");
        let mut on_disk = Vec::new();
        let mut folders = vec![std::path::PathBuf::from(root)];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(folder).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    folders.push(path);
                } else if path.extension().is_some_and(|e| e == "md") {
                    let parts: Vec<String> = path.strip_prefix(root).unwrap().components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                    // Only useful when this server isn't running, so it isn't served.
                    if parts[0] != "endeavor-setup" {
                        on_disk.push(parts.join("/"));
                    }
                }
            }
        }
        on_disk.sort();
        let mut served: Vec<String> = FILES.iter().map(|(p, _)| p.to_string()).collect();
        served.sort();
        assert_eq!(served, on_disk, "list each file in plugin/skills in FILES");
    }

    #[test]
    fn the_guide_is_the_notebook_skill_and_its_links_are_topics() {
        let guide = read(&json!({})).unwrap();
        assert!(guide.contains("\n# Working in a live notebook\n"), "{guide}");
        assert!(!guide.contains("\nname: endeavor-"), "front matter left in");
        for topic in ["pluto", "app", "errors"] {
            assert!(guide.contains(&format!("[reference/{topic}.md](endeavor-notebooks/reference/{topic}.md)")), "{topic}");
        }
        assert!(guide.contains("\n# Pluto notebooks (Julia)\n"), "the guide holds the cell rules");
        assert!(!guide.contains("# In the Endeavor app") && !guide.contains("# Error codes"), "other references are read when asked for");
        for link in guide.split("](").skip(1).filter_map(|l| l.split_once(')')).map(|(t, _)| t).filter(|t| !t.contains("://")) {
            assert!(read(&json!({ "topic": link })).is_ok(), "the guide links to {link}, which isn't a topic");
        }
        assert!(read(&json!({ "topic": "endeavor-machines" })).is_ok(), "the topic the guide's last line names");
    }

    #[test]
    fn only_an_agent_without_the_skills_is_pointed_at_the_machines_guide() {
        let pointer = "call `notebook_guide` with `topic` set to `endeavor-machines/SKILL.md` and follow it";
        let with_skills = instructions(true, true, true).unwrap();
        let without_skills = instructions(true, false, true).unwrap();
        assert!(!with_skills.contains(pointer), "{with_skills}");
        assert!(without_skills.contains(pointer), "{without_skills}");
        assert!(read(&json!({ "topic": "endeavor-machines/SKILL.md" })).is_ok(), "the topic it names");
        assert!(!instructions(true, false, false).unwrap().contains("endeavor-machines"), "no machine tools, no pointer");
    }

    #[test]
    fn only_an_agent_in_the_app_is_pointed_at_the_app_topic() {
        let approval = "The rules for runs the user must approve are in the topic `endeavor-notebooks/reference/app.md`.";
        assert!(instructions(false, false, false).unwrap().ends_with(approval));
        assert!(!instructions(true, false, true).unwrap().contains(approval));
    }

    #[test]
    fn reads_one_topic() {
        let errors = read(&json!({ "topic": "endeavor-notebooks/reference/errors.md" })).unwrap();
        let path = "endeavor-notebooks/reference/errors.md";
        assert_eq!(errors, served(path, FILES.iter().find(|(p, _)| *p == path).unwrap().1));
        assert!(errors.contains("[app.md](endeavor-notebooks/reference/app.md)"), "a reference's link to another is a topic too");
        assert!(read(&json!({ "topic": "endeavor-notebooks" })).unwrap().starts_with("# Working in a live notebook"));
        let missing = read(&json!({ "topic": "nope.md" })).unwrap_err();
        assert!(missing.starts_with("ArgumentError: not_found::No guide topic 'nope.md'. Topics: endeavor-notebooks/SKILL.md"), "{missing}");
    }
}
