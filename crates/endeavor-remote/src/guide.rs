//! Endeavor's Pluto skills (`plugin/skills`) for agents that don't load the
//! Claude Code plugin: the `notebook_guide` tool serves them, and the server's
//! MCP `instructions` say to call it first. A session whose agent loads the
//! plugin says so with `X-Endeavor-Skills: plugin` and gets neither.

use serde_json::{Value, json};

pub const TOOL: &str = "notebook_guide";

/// Each skill file by its path under `plugin/skills`.
const FILES: [(&str, &str); 14] = [
    ("pluto-session/SKILL.md", include_str!("../../../plugin/skills/pluto-session/SKILL.md")),
    ("pluto-workflow/SKILL.md", include_str!("../../../plugin/skills/pluto-workflow/SKILL.md")),
    ("pluto-semantics/SKILL.md", include_str!("../../../plugin/skills/pluto-semantics/SKILL.md")),
    ("pluto-session/reference/lifecycle-tools.md", include_str!("../../../plugin/skills/pluto-session/reference/lifecycle-tools.md")),
    ("pluto-workflow/reference/annotations.md", include_str!("../../../plugin/skills/pluto-workflow/reference/annotations.md")),
    ("pluto-workflow/reference/edit-loop.md", include_str!("../../../plugin/skills/pluto-workflow/reference/edit-loop.md")),
    ("pluto-workflow/reference/errors.md", include_str!("../../../plugin/skills/pluto-workflow/reference/errors.md")),
    ("pluto-workflow/reference/pluto-mental-model.md", include_str!("../../../plugin/skills/pluto-workflow/reference/pluto-mental-model.md")),
    ("pluto-workflow/reference/safe-preview.md", include_str!("../../../plugin/skills/pluto-workflow/reference/safe-preview.md")),
    ("pluto-semantics/reference/agent-examples.md", include_str!("../../../plugin/skills/pluto-semantics/reference/agent-examples.md")),
    ("pluto-semantics/reference/cell-structure.md", include_str!("../../../plugin/skills/pluto-semantics/reference/cell-structure.md")),
    ("pluto-semantics/reference/error-kinds.md", include_str!("../../../plugin/skills/pluto-semantics/reference/error-kinds.md")),
    ("pluto-semantics/reference/grammar.md", include_str!("../../../plugin/skills/pluto-semantics/reference/grammar.md")),
    ("pluto-semantics/reference/reactivity.md", include_str!("../../../plugin/skills/pluto-semantics/reference/reactivity.md")),
];

/// The server's MCP `instructions`.
pub const INSTRUCTIONS: &str = "These tools edit and run a live Pluto (Julia) notebook that the user sees in Endeavor, next to this chat. \
Before your first notebook tool call in a session, call `notebook_guide` once with no arguments and follow what it says: \
how to find this session's notebook, the read-edit-run loop, when the user must approve a run, and how to lay out cells.";

/// `tools/list`'s entry for the guide.
pub fn schema() -> Value {
    json!({
        "name": TOOL,
        "description": "How to work in Endeavor's Pluto notebooks with these tools. Call it once, with no arguments, before your first other notebook tool call in a session, and follow it. \
It names further topics (paths ending in .md); call it again with `topic` set to one of them when you need that detail.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "A topic the guide names, such as \"pluto-workflow/reference/errors.md\". Leave it out for the guide itself." }
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

/// The three skills in the order a session needs them.
fn whole() -> String {
    let mut out = String::from(
        "# Working in Endeavor's notebooks\n\n\
         This guide has three parts: pluto-session (finding or creating this session's notebook), \
         pluto-workflow (reading, editing and running cells) and pluto-semantics (how to lay out cells). \
         A part names another in bold, such as **pluto-workflow**. Links to .md paths are further topics: \
         call `notebook_guide` with `topic` set to the path when you need one.\n\
         Tool names such as `Read`, `Bash` or `Write` refer to your own file and shell tools, whatever they are called.\n",
    );
    for (path, text) in FILES.iter().filter(|(p, _)| p.ends_with("/SKILL.md")) {
        out.push_str("\n---\n\n");
        out.push_str(&served(path, text));
    }
    out
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
                    on_disk.push(path.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        on_disk.sort();
        let mut served: Vec<String> = FILES.iter().map(|(p, _)| p.to_string()).collect();
        served.sort();
        assert_eq!(served, on_disk, "list each file in plugin/skills in FILES");
    }

    #[test]
    fn the_guide_has_each_skill_and_its_links_are_topics() {
        let guide = read(&json!({})).unwrap();
        for heading in ["# Pluto session orientation", "# Pluto workflow (cell editing)", "# Pluto cell semantics"] {
            assert!(guide.contains(heading), "{heading}");
        }
        assert!(!guide.contains("\nname: pluto-"), "front matter left in");
        assert!(guide.contains("[annotations.md](pluto-workflow/reference/annotations.md)"));
        assert!(guide.contains("[pluto-workflow](pluto-workflow)"));
        assert!(guide.contains("(pluto-semantics/reference/cell-structure.md)"));
        for link in guide.split("](").skip(1).filter_map(|l| l.split_once(')')).map(|(t, _)| t).filter(|t| !t.contains("://")) {
            assert!(read(&json!({ "topic": link })).is_ok(), "the guide links to {link}, which isn't a topic");
        }
    }

    #[test]
    fn reads_one_topic() {
        let errors = read(&json!({ "topic": "pluto-workflow/reference/errors.md" })).unwrap();
        assert_eq!(errors, served("pluto-workflow/reference/errors.md", FILES[6].1));
        assert!(read(&json!({ "topic": "pluto-semantics" })).unwrap().starts_with("# Pluto cell semantics"));
        let missing = read(&json!({ "topic": "nope.md" })).unwrap_err();
        assert!(missing.starts_with("ArgumentError: not_found::No guide topic 'nope.md'. Topics: pluto-session/SKILL.md"), "{missing}");
    }
}
