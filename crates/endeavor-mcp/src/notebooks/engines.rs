//! The notebook engines a runtime drives, one adapter each, behind one
//! `POST /adapter` for the rest of the core (docs/runtime-core.md). Each
//! engine joins when it starts, Pluto's too (Julia starts when it's first
//! needed). A call goes to the engine of the notebook it names, of the file it
//! opens, or, with neither, to Pluto's; `snapshot` and `status` without a notebook ask every
//! engine that runs and put their notebooks together.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use wire::backend::Backend;

use super::Upstream;

pub struct Engines {
    /// In the order they joined.
    parts: Mutex<Vec<(Backend, Arc<dyn Upstream>)>>,
    /// The engine each notebook it has seen is open in.
    ids: Mutex<HashMap<String, Backend>>,
}

impl Engines {
    /// The engines, starting with Pluto's if it runs.
    pub fn new(pluto: Option<Arc<dyn Upstream>>) -> Engines {
        Engines { parts: Mutex::new(pluto.map(|pluto| (Backend::Pluto, pluto)).into_iter().collect()), ids: Mutex::default() }
    }

    /// An engine, once it runs, in the place of one it replaces. False if it's there already: the
    /// caller follows the notifications of one that's new.
    pub fn add(&self, backend: Backend, upstream: Arc<dyn Upstream>) -> bool {
        let mut parts = self.parts.lock().unwrap();
        if parts.iter().any(|(b, u)| *b == backend && Arc::ptr_eq(u, &upstream)) {
            return false;
        }
        parts.retain(|(b, _)| *b != backend);
        parts.push((backend, upstream));
        true
    }

    pub fn parts(&self) -> Vec<(Backend, Arc<dyn Upstream>)> {
        self.parts.lock().unwrap().clone()
    }

    /// The engine notebook `id` is open in, as far as we know: Pluto's for one we haven't seen.
    pub fn backend_of(&self, id: &str) -> Backend {
        self.ids.lock().unwrap().get(id).copied().unwrap_or(Backend::Pluto)
    }

    pub fn has(&self, backend: Backend) -> bool {
        self.part(backend).is_some()
    }

    fn part(&self, backend: Backend) -> Option<Arc<dyn Upstream>> {
        self.parts.lock().unwrap().iter().find(|(b, _)| *b == backend).map(|(_, u)| u.clone())
    }

    /// Notebook `id` is open in `backend`'s engine.
    pub fn learn(&self, id: &str, backend: Backend) {
        if !id.is_empty() {
            self.ids.lock().unwrap().insert(id.to_owned(), backend);
        }
    }

    pub fn forget(&self, id: &str) {
        self.ids.lock().unwrap().remove(id);
    }

    /// Whether `upstream` is still `backend`'s engine: not dropped or replaced.
    pub fn is_current(&self, backend: Backend, upstream: &Arc<dyn Upstream>) -> bool {
        self.part(backend).is_some_and(|now| Arc::ptr_eq(&now, upstream))
    }

    /// `upstream`, another engine than Pluto's, stopped answering: its
    /// notebooks are gone with it, and the next notebook opened in it starts
    /// it again. Nothing if it was already replaced.
    pub fn drop_engine(&self, backend: Backend, upstream: &Arc<dyn Upstream>) {
        if backend == Backend::Pluto {
            return;
        }
        let mut parts = self.parts.lock().unwrap();
        let before = parts.len();
        parts.retain(|(b, u)| !(*b == backend && Arc::ptr_eq(u, upstream)));
        if parts.len() != before {
            self.ids.lock().unwrap().retain(|_, b| *b != backend);
        }
    }

    /// The reply to one `POST /adapter` call, from the engine it belongs to.
    pub fn adapter(&self, raw: &[u8]) -> io::Result<String> {
        let message: Value = serde_json::from_slice(raw).map_err(|_| io::ErrorKind::InvalidData)?;
        let params = &message["params"];
        let method = message["method"].as_str().unwrap_or_default();
        let id = params["notebook_id"].as_str();
        match (method, id) {
            ("snapshot" | "status", None) => self.every(method, raw),
            ("open" | "new", _) => {
                let backend = params["path"].as_str().map_or(Backend::Pluto, of_path);
                let reply = self.on(backend, raw)?;
                if let Some(id) = result(&reply).and_then(|r| r["notebook_id"].as_str().map(str::to_owned)) {
                    self.learn(&id, backend);
                }
                Ok(reply)
            }
            (_, Some(id)) => {
                // An id no running engine reported: not open, rather than "Julia isn't running".
                if !self.ids.lock().unwrap().contains_key(id) && !self.has(Backend::Pluto) {
                    let error = format!("notebook_not_found::No notebook with id '{id}' is open. Run list_notebooks to see what's open.");
                    return Ok(json!({ "error": error }).to_string());
                }
                let backend = self.backend_of(id);
                let reply = self.on(backend, raw)?;
                if method == "shutdown" && result(&reply).is_some() {
                    self.forget(id);
                }
                Ok(reply)
            }
            _ => self.on(Backend::Pluto, raw),
        }
    }

    fn on(&self, backend: Backend, raw: &[u8]) -> io::Result<String> {
        match self.part(backend) {
            Some(upstream) => upstream.adapter(raw),
            None => Ok(json!({ "error": format!("unsupported::{} notebooks aren't running in this runtime", language(backend)) }).to_string()),
        }
    }

    /// `snapshot` or `status` of every notebook: the first engine's reply
    /// (Pluto's, when Julia runs), with the other engines' notebooks added to its own. Each notebook in a snapshot carries
    /// its engine's `seq`, which only means something within that engine.
    /// Pluto failing fails the call. Another engine failing is left out, so a
    /// broken R never hides the Julia notebooks; one that doesn't answer at all
    /// is dropped with its notebooks.
    fn every(&self, method: &str, raw: &[u8]) -> io::Result<String> {
        let mut whole: Option<Value> = None;
        for (backend, upstream) in self.parts() {
            let reply = match upstream.adapter(raw) {
                Ok(reply) => reply,
                Err(e) if backend != Backend::Pluto => {
                    eprintln!("{} notebooks' engine didn't answer {method} ({e}); leaving it out", language(backend));
                    self.drop_engine(backend, &upstream);
                    continue;
                }
                Err(e) => return Err(e),
            };
            let mut reply: Value = match serde_json::from_str(&reply) {
                Ok(reply) => reply,
                Err(_) if backend != Backend::Pluto => {
                    eprintln!("{} notebooks' engine answered {method} with something that isn't JSON; leaving it out", language(backend));
                    continue;
                }
                Err(_) => return Err(io::ErrorKind::InvalidData.into()),
            };
            if reply.get("error").is_some() {
                if backend != Backend::Pluto {
                    eprintln!("{} notebooks' engine failed {method}: {}; leaving it out", language(backend), reply["error"]);
                    continue;
                }
                return Ok(reply.to_string());
            }
            let seq = reply["result"]["seq"].clone();
            let mut notebooks = match reply["result"]["notebooks"].take() {
                Value::Array(notebooks) => notebooks,
                _ => Vec::new(),
            };
            for nb in &mut notebooks {
                self.learn(nb["notebook_id"].as_str().unwrap_or_default(), backend);
                if method == "snapshot" && nb.get("seq").is_none() {
                    nb["seq"] = seq.clone();
                }
            }
            match &mut whole {
                None => {
                    reply["result"]["notebooks"] = notebooks.into();
                    whole = Some(reply);
                }
                Some(whole) => {
                    if let Value::Array(all) = &mut whole["result"]["notebooks"] {
                        all.extend(notebooks);
                    }
                }
            }
        }
        Ok(whole.unwrap_or_else(|| json!({ "result": { "notebooks": [] } })).to_string())
    }
}

/// The engine a notebook at `path` belongs to, by its name alone: the file
/// may not exist yet. Pluto's for any name no other engine claims.
pub fn of_path(path: &str) -> Backend {
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("R" | "r") => Backend::Ember,
        _ => Backend::Pluto,
    }
}

pub fn language(backend: Backend) -> &'static str {
    match backend {
        Backend::Pluto => "Julia",
        Backend::Ember => "R",
    }
}

fn result(reply: &str) -> Option<Value> {
    let mut reply: Value = serde_json::from_str(reply).ok()?;
    if reply.get("error").is_some() {
        return None;
    }
    Some(reply["result"].take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    /// An engine that answers from a table and records what it was asked.
    #[derive(Default)]
    struct Fake {
        notebooks: Vec<Value>,
        seq: u64,
        asked: Mutex<Vec<String>>,
        down: bool,
    }

    impl Upstream for Fake {
        fn adapter(&self, raw: &[u8]) -> io::Result<String> {
            if self.down {
                return Err(io::ErrorKind::NotConnected.into());
            }
            let message: Value = serde_json::from_slice(raw).unwrap();
            let method = message["method"].as_str().unwrap().to_owned();
            self.asked.lock().unwrap().push(method.clone());
            let result = match method.as_str() {
                "snapshot" if message["params"]["notebook_id"].is_null() => json!({ "notebooks": self.notebooks, "seq": self.seq }),
                "status" => json!({ "pluto": "running", "notebooks": self.notebooks }),
                "open" | "new" => json!({ "notebook_id": format!("{}-new", self.seq), "path": message["params"]["path"] }),
                _ => json!({ "ok": true }),
            };
            Ok(json!({ "result": result }).to_string())
        }

        fn notifications(&self) -> io::Result<Box<dyn BufRead + Send>> {
            Err(io::ErrorKind::NotConnected.into())
        }
    }

    fn call(engines: &Engines, method: &str, params: Value) -> Value {
        serde_json::from_str(&engines.adapter(json!({ "method": method, "params": params }).to_string().as_bytes()).unwrap()).unwrap()
    }

    fn asked(fake: &Fake) -> Vec<String> {
        std::mem::take(&mut *fake.asked.lock().unwrap())
    }

    fn two() -> (Engines, Arc<Fake>, Arc<Fake>) {
        let pluto = Arc::new(Fake { notebooks: vec![json!({ "notebook_id": "p1" })], seq: 7, ..Default::default() });
        let ember = Arc::new(Fake { notebooks: vec![json!({ "notebook_id": "e1" }), json!({ "notebook_id": "e2" })], seq: 3, ..Default::default() });
        let engines = Engines::new(Some(pluto.clone()));
        engines.add(Backend::Ember, ember.clone());
        (engines, pluto, ember)
    }

    #[test]
    fn with_pluto_alone_every_call_is_plutos() {
        let pluto = Arc::new(Fake { notebooks: vec![json!({ "notebook_id": "p1" })], seq: 7, ..Default::default() });
        let engines = Engines::new(Some(pluto.clone()));
        assert_eq!(call(&engines, "snapshot", json!({})), json!({ "result": { "notebooks": [{ "notebook_id": "p1", "seq": 7 }], "seq": 7 } }));
        call(&engines, "run", json!({ "notebook_id": "unknown" }));
        call(&engines, "open", json!({ "path": "/a/b.jl", "run": false }));
        call(&engines, "new", json!({}));
        assert_eq!(asked(&pluto), ["snapshot", "run", "open", "new"]);
    }

    #[test]
    fn a_snapshot_of_all_puts_every_engines_notebooks_together_each_with_its_seq() {
        let (engines, _, _) = two();
        let all = call(&engines, "snapshot", json!({}));
        let seqs: Vec<(&str, u64)> = all["result"]["notebooks"].as_array().unwrap().iter().map(|nb| (nb["notebook_id"].as_str().unwrap(), nb["seq"].as_u64().unwrap())).collect();
        assert_eq!(seqs, [("p1", 7), ("e1", 3), ("e2", 3)]);
        let status = call(&engines, "status", json!({}));
        assert_eq!(status["result"]["pluto"], "running", "Pluto's own fields stay");
        assert_eq!(status["result"]["notebooks"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_call_goes_to_the_engine_its_notebook_is_open_in() {
        let (engines, pluto, ember) = two();
        call(&engines, "snapshot", json!({}));
        asked(&pluto);
        asked(&ember);
        call(&engines, "run", json!({ "notebook_id": "e2" }));
        call(&engines, "graph", json!({ "notebook_id": "p1" }));
        assert_eq!((asked(&pluto), asked(&ember)), (vec!["graph".to_owned()], vec!["run".to_owned()]));
        call(&engines, "shutdown", json!({ "notebook_id": "e2" }));
        asked(&ember);
        call(&engines, "run", json!({ "notebook_id": "e2" }));
        assert_eq!((asked(&pluto), asked(&ember)), (vec!["run".to_owned()], vec![]), "a notebook shut down is forgotten");
    }

    #[test]
    fn opening_or_making_a_notebook_goes_by_its_name() {
        let (engines, pluto, ember) = two();
        let opened = call(&engines, "open", json!({ "path": "/a/b.R", "run": false }));
        call(&engines, "new", json!({ "path": "/a/c.r" }));
        call(&engines, "new", json!({ "path": "/a/d.jl" }));
        call(&engines, "new", json!({ "folder": "/a" }));
        assert_eq!((asked(&pluto), asked(&ember)), (vec!["new".to_owned(), "new".to_owned()], vec!["open".to_owned(), "new".to_owned()]));
        call(&engines, "run", json!({ "notebook_id": opened["result"]["notebook_id"] }));
        assert_eq!(asked(&ember), ["run"], "and later calls follow it");
    }

    #[test]
    fn an_engine_not_running_has_no_notebooks_and_takes_no_calls() {
        let pluto = Arc::new(Fake { notebooks: vec![json!({ "notebook_id": "p1" })], seq: 7, ..Default::default() });
        let engines = Engines::new(Some(pluto.clone()));
        let opened = call(&engines, "open", json!({ "path": "/a/b.R", "run": false }));
        assert_eq!(opened["error"], "unsupported::R notebooks aren't running in this runtime");
        assert!(asked(&pluto).is_empty());
        engines.add(Backend::Ember, Arc::new(Fake { down: true, ..Default::default() }));
        assert_eq!(call(&engines, "snapshot", json!({}))["result"]["notebooks"].as_array().unwrap().len(), 1, "one not answering is skipped");
    }

    #[test]
    fn pluto_failing_fails_the_whole_snapshot() {
        let pluto = Arc::new(Fake { down: true, ..Default::default() });
        let engines = Engines::new(Some(pluto));
        assert_eq!(engines.adapter(json!({ "method": "snapshot", "params": {} }).to_string().as_bytes()).unwrap_err().kind(), io::ErrorKind::NotConnected, "Pluto not answering yet is an error, as before");
        let engines = Engines::new(Some(Arc::new(Failing)));
        engines.add(Backend::Ember, Arc::new(Fake { notebooks: vec![json!({ "notebook_id": "e1" })], ..Default::default() }));
        assert_eq!(call(&engines, "snapshot", json!({}))["error"], "ArgumentError: broken", "and so is Pluto's error");
    }

    #[test]
    fn another_engine_failing_leaves_out_only_its_own_notebooks() {
        let (engines, _, _) = two();
        engines.add(Backend::Ember, Arc::new(Failing));
        assert_eq!(call(&engines, "snapshot", json!({}))["result"]["notebooks"], json!([{ "notebook_id": "p1", "seq": 7 }]));
        assert!(engines.has(Backend::Ember), "it answered, so it stays");
    }

    #[test]
    fn another_engine_that_stopped_answering_is_dropped_with_its_notebooks() {
        let (engines, pluto, _) = two();
        call(&engines, "snapshot", json!({}));
        assert_eq!(engines.backend_of("e1"), Backend::Ember);
        let dead: Arc<dyn Upstream> = Arc::new(Fake { down: true, ..Default::default() });
        engines.add(Backend::Ember, dead.clone());
        assert_eq!(call(&engines, "status", json!({}))["result"]["notebooks"], json!([{ "notebook_id": "p1" }]));
        assert!(!engines.has(Backend::Ember) && !engines.is_current(Backend::Ember, &dead));
        assert_eq!(engines.backend_of("e1"), Backend::Pluto, "its notebooks are forgotten");
        let opened = call(&engines, "open", json!({ "path": "/a/b.R", "run": false }));
        assert_eq!(opened["error"], "unsupported::R notebooks aren't running in this runtime", "until it starts again");
        let again: Arc<dyn Upstream> = Arc::new(Fake::default());
        engines.add(Backend::Ember, again.clone());
        engines.drop_engine(Backend::Ember, &dead);
        assert!(engines.is_current(Backend::Ember, &again), "dropping the old one again leaves the new one");
        engines.drop_engine(Backend::Pluto, &(pluto as Arc<dyn Upstream>));
        assert!(engines.has(Backend::Pluto), "Pluto's is never dropped");
    }

    struct Failing;

    impl Upstream for Failing {
        fn adapter(&self, _: &[u8]) -> io::Result<String> {
            Ok(json!({ "error": "ArgumentError: broken" }).to_string())
        }

        fn notifications(&self) -> io::Result<Box<dyn BufRead + Send>> {
            Err(io::ErrorKind::NotConnected.into())
        }
    }
}
