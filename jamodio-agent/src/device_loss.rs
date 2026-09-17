//! Périphériques perdus en session, et ce qu'on en dit au navigateur.
//!
//! Règle (device strict) : si le musicien a choisi X, il a X, ou une erreur
//! explicite — jamais une bascule silencieuse. Recette du 17/09/2026 :
//! - débrancher le casque jack emportait son micro (l'ENTRÉE) : l'Audio Engine
//!   n'arrivait plus à rien rouvrir, tout le son s'arrêtait et la page n'en savait
//!   rien (D5b) ;
//! - une SORTIE choisie débranchée en session retombait sur la sortie du système
//!   sans le dire (D5c).
//!
//! Cet état ne décide rien : il retient ce qui est perdu (pour savoir quoi
//! surveiller et quand on est revenu) et accumule les événements que le
//! superviseur relaie au navigateur. Un événement n'est émis qu'aux transitions :
//! jamais de répétition tant que rien ne change.

/// Événement à relayer au navigateur (ids `{idx}:{name}` des périphériques).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// L'entrée choisie a disparu : plus de capture. `keeps_output` : la réception
    /// continue (hors ASIO) ; faux quand l'interface entière manque (ASIO : entrée
    /// et sortie sont la même interface).
    InputLost { device: String, keeps_output: bool },
    /// La même entrée est revenue et la capture est repartie.
    InputRestored { device: String },
    /// La sortie choisie a disparu : le son passe par `fallback` (sortie du système).
    OutputLost { device: String, fallback: String },
    /// La sortie choisie est revenue et le son y repasse.
    OutputRestored { device: String },
}

#[derive(Debug, Default)]
pub struct DeviceLoss {
    input: Option<String>,
    /// Sortie choisie perdue, et la sortie de repli par laquelle passe le son.
    output: Option<(String, String)>,
    pending: Vec<DeviceEvent>,
}

impl DeviceLoss {
    /// L'entrée `device` est introuvable (`keeps_output` : la réception continue).
    pub fn input_lost(&mut self, device: &str, keeps_output: bool) {
        if self.input.as_deref() == Some(device) {
            return;
        }
        self.input = Some(device.to_string());
        self.pending.push(DeviceEvent::InputLost {
            device: device.to_string(),
            keeps_output,
        });
    }

    /// La capture est repartie : si une entrée était perdue, elle est revenue.
    pub fn input_back(&mut self) {
        if let Some(device) = self.input.take() {
            self.pending.push(DeviceEvent::InputRestored { device });
        }
    }

    /// La sortie choisie `device` est introuvable ; le son passe par `fallback`.
    /// Réémis si le repli change (la sortie de repli a disparu à son tour) : le
    /// message « le son passe par X » doit rester vrai.
    pub fn output_fell_back(&mut self, device: &str, fallback: &str) {
        if self.output.as_ref().is_some_and(|(d, f)| d == device && f == fallback) {
            return;
        }
        self.output = Some((device.to_string(), fallback.to_string()));
        self.pending.push(DeviceEvent::OutputLost {
            device: device.to_string(),
            fallback: fallback.to_string(),
        });
    }

    /// La sortie choisie est rouverte : si elle était perdue, elle est revenue.
    pub fn output_back(&mut self) {
        if let Some((device, _)) = self.output.take() {
            self.pending.push(DeviceEvent::OutputRestored { device });
        }
    }

    /// Nouvelle capture ou fin de session : les pertes passées ne concernent plus
    /// rien (le navigateur repart de `capture-started`). Aucun événement.
    pub fn forget(&mut self) {
        self.input = None;
        self.output = None;
        self.pending.clear();
    }

    /// Le musicien a choisi une autre sortie : la sortie perdue ne compte plus. Aucun
    /// événement (le navigateur connaît son propre choix).
    pub fn forget_output(&mut self) {
        self.output = None;
    }

    pub fn lost_input(&self) -> Option<&str> {
        self.input.as_deref()
    }

    pub fn lost_output(&self) -> Option<&str> {
        self.output.as_ref().map(|(d, _)| d.as_str())
    }

    /// Événements à relayer, dans l'ordre (vidés).
    pub fn take_events(&mut self) -> Vec<DeviceEvent> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_lost_then_back_emits_each_transition_once() {
        let mut loss = DeviceLoss::default();
        loss.input_lost("0:Microphone externe", true);
        loss.input_lost("0:Microphone externe", true); // relances en boucle : rien de plus
        assert_eq!(loss.lost_input(), Some("0:Microphone externe"));
        loss.input_back();
        loss.input_back(); // capture saine : rien de plus
        assert_eq!(loss.lost_input(), None);
        assert_eq!(
            loss.take_events(),
            vec![
                DeviceEvent::InputLost {
                    device: "0:Microphone externe".into(),
                    keeps_output: true
                },
                DeviceEvent::InputRestored {
                    device: "0:Microphone externe".into()
                },
            ]
        );
        assert!(loss.take_events().is_empty());
    }

    #[test]
    fn a_healthy_rebuild_emits_nothing() {
        let mut loss = DeviceLoss::default();
        loss.input_back();
        loss.output_back();
        assert!(loss.take_events().is_empty());
    }

    #[test]
    fn output_fell_back_then_back() {
        let mut loss = DeviceLoss::default();
        loss.output_fell_back("2:Écouteurs externes", "Haut-parleurs MacBook Pro");
        loss.output_fell_back("2:Écouteurs externes", "Haut-parleurs MacBook Pro");
        assert_eq!(loss.lost_output(), Some("2:Écouteurs externes"));
        loss.output_back();
        assert_eq!(
            loss.take_events(),
            vec![
                DeviceEvent::OutputLost {
                    device: "2:Écouteurs externes".into(),
                    fallback: "Haut-parleurs MacBook Pro".into(),
                },
                DeviceEvent::OutputRestored {
                    device: "2:Écouteurs externes".into()
                },
            ]
        );
    }

    #[test]
    fn forget_drops_state_and_pending_events_silently() {
        let mut loss = DeviceLoss::default();
        loss.input_lost("0:Microphone externe", false);
        loss.output_fell_back("2:Écouteurs externes", "Haut-parleurs MacBook Pro");
        loss.forget();
        assert_eq!(loss.lost_input(), None);
        assert_eq!(loss.lost_output(), None);
        assert!(loss.take_events().is_empty());
        loss.input_back();
        assert!(
            loss.take_events().is_empty(),
            "rien à dire d'une perte oubliée"
        );
    }

    #[test]
    fn a_new_fallback_is_announced_again() {
        let mut loss = DeviceLoss::default();
        loss.output_fell_back("2:Écouteurs externes", "Haut-parleurs MacBook Pro");
        loss.output_fell_back("2:Écouteurs externes", "BlackHole 2ch");
        assert_eq!(loss.lost_output(), Some("2:Écouteurs externes"));
        assert_eq!(
            loss.take_events(),
            vec![
                DeviceEvent::OutputLost {
                    device: "2:Écouteurs externes".into(),
                    fallback: "Haut-parleurs MacBook Pro".into(),
                },
                DeviceEvent::OutputLost {
                    device: "2:Écouteurs externes".into(),
                    fallback: "BlackHole 2ch".into(),
                },
            ]
        );
    }
}
