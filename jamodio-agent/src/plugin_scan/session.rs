//! Machine à états d'UNE session worker — logique pure, sans I/O ni process.
//!
//! Le coordinateur (coordinator.rs) pompe les events NDJSON du worker et les
//! injecte ici ; à la mort du worker (exit, EOF, timeout) il appelle
//! [`Session::close`] pour obtenir le verdict : plugins collectés, item
//! condamné (le fameux « begin sans end »), items restants à rescanner.
//!
//! Séparée du process réel pour être testée unitairement à froid — le chemin
//! crash/hang réel est couvert par les tests d'intégration (Lot E, hooks
//! debug du worker).

use std::collections::VecDeque;

use jamodio_audio_core::plugin_host::PluginInfo;
use serde::{Deserialize, Serialize};

use super::protocol::WorkerEvent;

/// Raison de condamnation d'un item. Sérialisé (camelCase) : persisté dans le
/// cache disque (Lot C) et exposé au browser (Lot D).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockReason {
    /// Le worker est mort pendant le scan de l'item (crash natif ou panic).
    Crash,
    /// Aucun event pendant le délai imparti → worker tué par le coordinateur.
    Timeout,
}

impl BlockReason {
    /// Représentation wire (cohérente avec la sérialisation serde lowercase),
    /// pour construire les `BlockedPlugin` exposés au browser.
    pub fn as_wire(self) -> &'static str {
        match self {
            BlockReason::Crash => "crash",
            BlockReason::Timeout => "timeout",
        }
    }
}

/// Item condamné — l'unité de la blocklist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedItem {
    /// Item du protocole : path `.vst3` (Windows) ou `au:…` (macOS).
    pub item: String,
    pub reason: BlockReason,
}

/// Cause de fin de session, vue du coordinateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCause {
    /// Le worker est mort (exit propre, crash, ou stdout fermé).
    Exited,
    /// Silence > timeout → le coordinateur a tué le worker.
    TimedOut,
}

/// Verdict d'une session close.
#[derive(Debug)]
pub struct SessionEnd {
    /// Plugins collectés PENDANT cette session (le coordinateur accumule).
    pub plugins: Vec<PluginInfo>,
    /// Item condamné par cette session (au plus un — le courant).
    pub blocked: Option<BlockedItem>,
    /// Items restants à rescanner dans une nouvelle session (vide = fini).
    pub remaining: Vec<String>,
    /// Au moins un item a été terminé (`end` reçu) — sert de garde
    /// anti-boucle au respawn : une session qui ne progresse pas ET ne
    /// condamne personne ne doit pas être relancée indéfiniment.
    pub progressed: bool,
}

/// État d'une session worker en cours.
pub struct Session {
    /// Items envoyés au worker dont le `end` n'est pas encore reçu,
    /// dans l'ordre d'envoi (le worker traite séquentiellement).
    pending: VecDeque<String>,
    /// Item dont le `begin` est reçu mais pas le `end` — le condamné
    /// désigné si le worker meurt maintenant.
    current: Option<String>,
    plugins: Vec<PluginInfo>,
    progressed: bool,
}

impl Session {
    pub fn new(items: Vec<String>) -> Self {
        Self {
            pending: items.into(),
            current: None,
            plugins: Vec::new(),
            progressed: false,
        }
    }

    /// Injecte un event NDJSON du worker.
    pub fn on_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Begin { item } => {
                // Sanity : le worker traite dans l'ordre d'envoi. Un écart =
                // bug interne (même binaire des deux côtés) — on log et on
                // fait confiance au flux, `item` devient le courant.
                if self.pending.front() != Some(&item) {
                    tracing::warn!(
                        target: "jamodio::plugin",
                        item,
                        expected = ?self.pending.front(),
                        "worker begin hors séquence — flux adopté tel quel"
                    );
                }
                self.current = Some(item);
            }
            WorkerEvent::Plugin { info } => self.plugins.push(info),
            WorkerEvent::End { item } => {
                self.current = None;
                self.progressed = true;
                // Retire l'item terminé (normalement en tête).
                if let Some(pos) = self.pending.iter().position(|i| *i == item) {
                    self.pending.remove(pos);
                }
            }
        }
    }

    /// Clôt la session (worker mort ou tué) et rend le verdict.
    pub fn close(mut self, cause: CloseCause) -> SessionEnd {
        let blocked = self.current.take().map(|item| {
            // Le condamné sort des restants — c'est LE mécanisme de
            // progression : chaque crash retire exactement un item.
            if let Some(pos) = self.pending.iter().position(|i| *i == item) {
                self.pending.remove(pos);
            }
            BlockedItem {
                item,
                reason: match cause {
                    CloseCause::Exited => BlockReason::Crash,
                    CloseCause::TimedOut => BlockReason::Timeout,
                },
            }
        });
        SessionEnd {
            plugins: self.plugins,
            blocked,
            remaining: self.pending.into(),
            progressed: self.progressed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamodio_audio_core::plugin_host::{PluginInfo, PluginRef};

    fn info(name: &str) -> PluginInfo {
        PluginInfo {
            name: name.into(),
            manufacturer: "T".into(),
            plugin_ref: PluginRef::Vst3 { path: format!("/x/{name}.vst3"), uid: "00".into() },
            latency_samples: 0,
            has_editor: false,
            incompatible: false,
            has_input_bus: true,
            is_instrument: false,
        }
    }

    fn begin(i: &str) -> WorkerEvent {
        WorkerEvent::Begin { item: i.into() }
    }
    fn end(i: &str) -> WorkerEvent {
        WorkerEvent::End { item: i.into() }
    }

    #[test]
    fn clean_run_collects_everything() {
        let mut s = Session::new(vec!["a".into(), "b".into()]);
        s.on_event(begin("a"));
        s.on_event(WorkerEvent::Plugin { info: info("A1") });
        s.on_event(end("a"));
        s.on_event(begin("b"));
        s.on_event(end("b"));
        let out = s.close(CloseCause::Exited);
        assert_eq!(out.plugins.len(), 1);
        assert_eq!(out.blocked, None);
        assert!(out.remaining.is_empty());
        assert!(out.progressed);
    }

    #[test]
    fn crash_mid_item_condemns_it_and_keeps_the_rest() {
        // Le cas terrain : mort pendant « b » → b condamné, c à rescanner,
        // les plugins déjà collectés (a) sont conservés.
        let mut s = Session::new(vec!["a".into(), "b".into(), "c".into()]);
        s.on_event(begin("a"));
        s.on_event(WorkerEvent::Plugin { info: info("A1") });
        s.on_event(end("a"));
        s.on_event(begin("b"));
        // … crash natif ici, pas de end.
        let out = s.close(CloseCause::Exited);
        assert_eq!(out.plugins.len(), 1);
        assert_eq!(
            out.blocked,
            Some(BlockedItem { item: "b".into(), reason: BlockReason::Crash })
        );
        assert_eq!(out.remaining, vec!["c".to_string()]);
        assert!(out.progressed);
    }

    #[test]
    fn timeout_condemns_with_timeout_reason() {
        let mut s = Session::new(vec!["a".into(), "b".into()]);
        s.on_event(begin("a"));
        let out = s.close(CloseCause::TimedOut);
        assert_eq!(
            out.blocked,
            Some(BlockedItem { item: "a".into(), reason: BlockReason::Timeout })
        );
        assert_eq!(out.remaining, vec!["b".to_string()]);
        assert!(!out.progressed);
    }

    #[test]
    fn death_between_items_condemns_nobody() {
        // Mort sans begin orphelin (crash de l'infra worker, pas d'un
        // plugin) : personne n'est condamné, tout reste à faire —
        // c'est la garde no-progress du coordinateur qui borne la relance.
        let mut s = Session::new(vec!["a".into(), "b".into()]);
        s.on_event(begin("a"));
        s.on_event(end("a"));
        let out = s.close(CloseCause::Exited);
        assert_eq!(out.blocked, None);
        assert_eq!(out.remaining, vec!["b".to_string()]);
        assert!(out.progressed);
    }

    #[test]
    fn out_of_order_end_still_removes_item() {
        // Robustesse : end d'un item qui n'est pas en tête (ne devrait pas
        // arriver — même binaire — mais le flux fait foi).
        let mut s = Session::new(vec!["a".into(), "b".into()]);
        s.on_event(begin("b"));
        s.on_event(end("b"));
        let out = s.close(CloseCause::Exited);
        assert_eq!(out.blocked, None);
        assert_eq!(out.remaining, vec!["a".to_string()]);
    }
}
