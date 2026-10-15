use crate::config::Config;
use crate::relay::{AgentEvent, Ms, Role, State, now_ms};
use crate::tailscale::{Identity, Tailscale};
use anyhow::Result;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// The relay's shared state plus change notification for long-polls and the web page.
pub struct Hub {
    state: Mutex<State>,
    version: watch::Sender<u64>,
    pub cfg: Config,
    pub ts: Tailscale,
    /// Who loopback callers are: the relay machine's owner, unknown until Tailscale names it.
    pub owner: Mutex<Option<Identity>>,
    pub listening: Mutex<Vec<String>>,
    pub shutdown: watch::Sender<bool>,
    saving: Mutex<()>,
    pub conns: Arc<crate::server::Conns>,
}

struct Waiting<'a> {
    hub: &'a Hub,
    name: String,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.hub.state.lock().unwrap().set_waiting(&self.name, -1, now_ms());
    }
}

impl Hub {
    pub fn new(cfg: Config, ts: Tailscale) -> Arc<Hub> {
        let state = State::new(cfg.timing, cfg.consensus, cfg.history);
        let local = Identity {
            login: "local".into(),
            display_name: String::new(),
            node: "relay".into(),
            user_id: 0,
        };
        Arc::new(Hub {
            state: Mutex::new(state),
            version: watch::Sender::new(0),
            owner: Mutex::new((!cfg.tailnet || cfg.tailnet_listen.is_some()).then_some(local)),
            cfg,
            ts,
            listening: Mutex::new(Vec::new()),
            shutdown: watch::Sender::new(false),
            saving: Mutex::new(()),
            conns: Arc::default(),
        })
    }

    pub fn read<R>(&self, f: impl FnOnce(&State, Ms) -> R) -> R {
        f(&self.state.lock().unwrap(), now_ms())
    }

    pub fn write<R>(&self, f: impl FnOnce(&mut State, Ms) -> R) -> R {
        let r = self.quiet(f);
        self.bump();
        r
    }

    /// A change nobody else needs to see or the state file to keep.
    pub fn quiet<R>(&self, f: impl FnOnce(&mut State, Ms) -> R) -> R {
        f(&mut self.state.lock().unwrap(), now_ms())
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    /// Writes the `--state` file, if there is one.
    pub fn save(&self) -> Result<()> {
        match &self.cfg.state {
            Some(path) => {
                let _one = self.saving.lock().unwrap();
                crate::persist::save(path, &self.read(|st, now| st.snapshot(now)))
            }
            None => Ok(()),
        }
    }

    pub fn tick(&self) {
        let changed = self.state.lock().unwrap().tick(now_ms());
        if changed {
            self.bump();
        }
    }

    pub fn set_web_url(&self, url: &str) {
        self.state.lock().unwrap().web_url = url.trim_end_matches('/').to_string();
    }

    pub fn web_url(&self) -> String {
        self.state.lock().unwrap().web_url.clone()
    }

    /// Marks the agent alive; only wakes watchers when it comes online.
    pub fn touch(&self, name: &str, who: &Identity, role: Option<Role>) -> Result<()> {
        let now = now_ms();
        let mut st = self.state.lock().unwrap();
        let before = (st.agents.online(name, now), st.agents.get(name).map(|a| a.role));
        st.touch(name, who, role, now)?;
        let after = (true, st.agents.get(name).map(|a| a.role));
        let changed = st.tick(now) || before != after;
        drop(st);
        if changed {
            self.bump();
        }
        Ok(())
    }

    /// The name check of `touch` for read-only calls, which leave presence alone.
    pub fn check_name(&self, name: &str, who: &Identity) -> Result<()> {
        self.state.lock().unwrap().check_name(name, who, now_ms())
    }

    /// Blocks until `name` has new events or something it must act on, or the timeout passes.
    /// Progress events are dropped unless `progress` is set. See `State::take_events` for `ack`.
    pub async fn wait(&self, name: &str, timeout: Duration, progress: bool, ack: Option<u64>) -> (Vec<AgentEvent>, u64) {
        let mut rx = self.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        self.state.lock().unwrap().set_waiting(name, 1, now_ms());
        let _guard = Waiting {
            hub: self,
            name: name.to_string(),
        };
        loop {
            rx.borrow_and_update();
            let (taken, urgent) = {
                let mut st = self.state.lock().unwrap();
                let now = now_ms();
                (st.take_events(name, progress, ack, now), st.next_actions(name, now).iter().any(|a| a.urgent))
            };
            if !taken.0.is_empty() || urgent {
                return taken;
            }
            let mut down = self.shutdown.subscribe();
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return taken,
                r = rx.changed() => if r.is_err() { return taken },
                _ = down.wait_for(|d| *d) => return taken,
            }
        }
    }

    /// Events after `after` without consuming them, progress left out; keeps the agent online while it blocks.
    pub async fn watch_events(&self, name: &str, after: u64, timeout: Duration) -> (Vec<AgentEvent>, u64) {
        let mut rx = self.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        self.state.lock().unwrap().set_waiting(name, 1, now_ms());
        let _guard = Waiting {
            hub: self,
            name: name.to_string(),
        };
        loop {
            rx.borrow_and_update();
            let (events, last) = self.state.lock().unwrap().peek_events(name, after);
            let now = now_ms();
            let events: Vec<AgentEvent> = events.into_iter().filter(|e| !e.progress).map(|e| e.rendered(now)).collect();
            if !events.is_empty() || last < after {
                return (events, last);
            }
            let mut down = self.shutdown.subscribe();
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return (events, last),
                r = rx.changed() => if r.is_err() { return (events, last) },
                _ = down.wait_for(|d| *d) => return (events, last),
            }
        }
    }
}
