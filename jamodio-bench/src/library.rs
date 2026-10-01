//! Scénarios nommés : ce que le banc sait lancer sans fichier ni option.
//!
//! Lot R1 (PLAN-BANC-REALISTE-2026-10) : les réseaux réalistes, chaque effet
//! d'abord SEUL (pour que chacun ait sa cause), puis mêlés. La campagne de
//! version (R5) s'appuiera sur cette bibliothèque. Les liens non calibrés le
//! disent (`origin`) : leurs valeurs viendront de vraies sessions en R2.

use crate::profile::{Absence, Change, Link, PeerProfile};
use crate::scenario::Scenario;

/// Nom et description de chaque scénario, dans l'ordre où les lancer.
pub const NAMED: [(&str, &str); 8] = [
    ("regulier-9", "9 musiciens, flux parfaitement réguliers — la référence du jour (5 min 30)"),
    ("derive-100ppm", "8 flux réguliers, horloges à −100 / +100 ppm en alternance (critère 7, 30 min 30)"),
    ("pics-seuls", "8 flux sans gigue, avec seulement les pics du Wi-Fi chargé (5 min 30)"),
    ("desordre-seul", "8 flux sans gigue, avec seulement le désordre du Wi-Fi chargé (5 min 30)"),
    ("rafales-seules", "8 flux sans gigue, avec seulement les pertes en rafales du Wi-Fi chargé (5 min 30)"),
    ("evenements", "saut de route 12 → 30 ms puis retour (m3), départ et retour d'un musicien (m5) (8 min)"),
    ("un-wifi-charge-parmi-8", "7 Ethernet ; m9 passe d'Ethernet à Wi-Fi chargé à 5 min (critère 6, 10 min)"),
    ("9-reseaux-mixtes", "3 Ethernet, 2 fibre, 1 ADSL, 1 Wi-Fi chargé, 1 4G, dérives de −80 à +80 ppm (critère 5, 10 min)"),
];

/// Le scénario nommé, ou `None` s'il n'existe pas.
pub fn named(name: &str) -> Option<Scenario> {
    let wifi_charge = Link::preset("wifi-charge").expect("préréglage");
    let regular = || PeerProfile::preset("regular").expect("préréglage");
    // Un seul effet du Wi-Fi chargé, sur des flux sans gigue.
    let only = |label: &str, set: &dyn Fn(&mut PeerProfile)| {
        let mut p = PeerProfile { name: label.into(), origin: wifi_charge.origin.clone(), ..regular() };
        set(&mut p);
        vec![p]
    };
    let nine = |name: &str, step_secs: u64, peers: Vec<PeerProfile>| Scenario {
        name: name.into(),
        from_musicians: 9,
        to_musicians: 9,
        step_secs,
        peers,
        ..Scenario::default()
    };
    let with = |link: &str, drift_ppm: f64| PeerProfile { drift_ppm, ..PeerProfile::preset(link).expect("préréglage") };
    Some(match name {
        "regulier-9" => nine(name, 330, vec![regular()]),
        "derive-100ppm" => nine(name, 1830, vec![with("regular", -100.0), with("regular", 100.0)]),
        "pics-seuls" => nine(name, 330, only("pics-wifi-charge", &|p| p.spikes = wifi_charge.spikes)),
        "desordre-seul" => nine(name, 330, only("desordre-wifi-charge", &|p| p.reorder = wifi_charge.reorder)),
        "rafales-seules" => nine(name, 330, only("rafales-wifi-charge", &|p| p.burst_loss = wifi_charge.burst_loss)),
        "evenements" => {
            let route = |ms: f64| Link { name: format!("route-{ms}ms"), base_delay_ms: ms, ..Link::preset("regular").expect("préréglage") };
            let m3 = PeerProfile {
                changes: vec![Change { at_s: 120.0, link: route(30.0) }, Change { at_s: 240.0, link: route(12.0) }],
                ..PeerProfile::from_link(route(12.0))
            };
            let m5 = PeerProfile { absences: vec![Absence { at_s: 330.0, for_s: 20.0 }], ..regular() };
            nine(name, 480, vec![regular(), m3, regular(), m5, regular(), regular(), regular(), regular()])
        }
        "un-wifi-charge-parmi-8" => {
            let mut peers = vec![PeerProfile::preset("ethernet").expect("préréglage"); 8];
            peers[7].changes = vec![Change { at_s: 300.0, link: wifi_charge.clone() }];
            nine(name, 600, peers)
        }
        "9-reseaux-mixtes" => nine(
            name,
            600,
            vec![
                with("ethernet", -80.0),
                with("fibre", 60.0),
                with("ethernet", -40.0),
                with("adsl", 20.0),
                with("fibre", 0.0),
                with("wifi-charge", -20.0),
                with("ethernet", 40.0),
                with("4g", 80.0),
            ],
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chaque_scenario_nomme_existe_se_valide_et_porte_son_nom() {
        for (name, _) in NAMED {
            let s = named(name).unwrap_or_else(|| panic!("{name}"));
            s.validate().unwrap_or_else(|e| panic!("{name} : {e}"));
            assert_eq!(s.name, name);
            assert_eq!((s.from_musicians, s.to_musicians), (9, 9), "{name} : 9 musiciens d'emblée");
        }
        assert!(named("inconnu").is_none());
    }

    #[test]
    fn neuf_reseaux_mixtes_a_la_composition_annoncee() {
        let s = named("9-reseaux-mixtes").unwrap();
        let links: Vec<&str> = (2..=9).map(|m| s.peer(m).name.as_str()).collect();
        let count = |l: &str| links.iter().filter(|&&x| x == l).count();
        assert_eq!((count("ethernet"), count("fibre"), count("adsl"), count("wifi-charge"), count("4g")), (3, 2, 1, 1, 1));
        let ppm: Vec<f64> = (2..=9).map(|m| s.peer(m).drift_ppm).collect();
        assert!(ppm.iter().all(|p| p.abs() <= 80.0));
        assert!(ppm.contains(&-80.0) && ppm.contains(&80.0), "de −80 à +80");
    }

    /// Un effet seul n'apporte que lui : le reste du lien est régulier.
    #[test]
    fn un_effet_seul_n_apporte_que_lui() {
        for (name, check) in [
            ("pics-seuls", (true, false, false)),
            ("desordre-seul", (false, true, false)),
            ("rafales-seules", (false, false, true)),
        ] {
            let p = named(name).unwrap().peer(2).clone();
            assert_eq!((p.spikes.is_some(), p.reorder.is_some(), p.burst_loss.is_some()), check, "{name}");
            assert_eq!((p.jitter, p.loss_pct, p.drift_ppm), (crate::profile::Jitter::None, 0.0, 0.0), "{name}");
        }
        let d = named("derive-100ppm").unwrap();
        assert!(d.is_drift_only());
        assert_eq!((d.peer(2).drift_ppm, d.peer(3).drift_ppm), (-100.0, 100.0));
        assert!(d.step_secs >= 1800 + d.warmup_secs, "30 min mesurées");
    }

    #[test]
    fn le_wifi_charge_arrive_a_mi_parcours_chez_un_seul() {
        let s = named("un-wifi-charge-parmi-8").unwrap();
        let changed: Vec<u32> = (2..=9).filter(|&m| !s.peer(m).changes.is_empty()).collect();
        assert_eq!(changed, vec![9]);
        assert_eq!(s.peer(9).link_name_at(299.0), "ethernet");
        assert_eq!(s.peer(9).link_name_at(300.0), "wifi-charge");
    }
}
