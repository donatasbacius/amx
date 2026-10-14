use crate::config::Config;
use crate::relay::{Action, AgentEvent, Ms, Role, State, now_ms};
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
    /// Who loopback callers are: the relay machine's owner.
    pub owner: Mutex<Identity>,
    pub listening: Mutex<Vec<String>>,
    pub shutdown: watch::Sender<bool>,
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
        Arc::new(Hub {
            state: Mutex::new(state),
            version: watch::Sender::new(0),
            cfg,
            ts,
            owner: Mutex::new(Identity {
                login: "local".into(),
                display_name: String::new(),
                node: "relay".into(),
            }),
            listening: Mutex::new(Vec::new()),
            shutdown: watch::Sender::new(false),
        })
    }

    pub fn read<R>(&self, f: impl FnOnce(&State, Ms) -> R) -> R {
        f(&self.state.lock().unwrap(), now_ms())
    }

    pub fn write<R>(&self, f: impl FnOnce(&mut State, Ms) -> R) -> R {
        let r = f(&mut self.state.lock().unwrap(), now_ms());
        self.bump();
        r
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
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

    /// Blocks until `name` has new events or something it must act on, or the timeout passes.
    pub async fn wait(&self, name: &str, timeout: Duration) -> (Vec<AgentEvent>, Vec<Action>) {
        let mut rx = self.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        self.state.lock().unwrap().set_waiting(name, 1, now_ms());
        let _guard = Waiting {
            hub: self,
            name: name.to_string(),
        };
        loop {
            rx.borrow_and_update();
            let (events, actions) = {
                let mut st = self.state.lock().unwrap();
                let now = now_ms();
                let ev = st.take_events(name);
                (ev, st.next_actions(name, now))
            };
            let urgent = actions.iter().any(|a| a.urgent);
            if !events.is_empty() || urgent {
                return (events, actions);
            }
            let mut down = self.shutdown.subscribe();
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return (events, actions),
                r = rx.changed() => if r.is_err() { return (events, actions) },
                _ = down.wait_for(|d| *d) => return (events, actions),
            }
        }
    }

    /// Events after `after` without consuming them; keeps the agent online while it blocks.
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
