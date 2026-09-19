//! Décider de masquer un trou AVANT qu'il s'entende — Lot 1.2 du chantier tampon.
//!
//! # Le problème
//!
//! Le masquage existant ne se déclenche que quand un paquet **arrive** en révélant
//! un saut de numérotation : il sait donc boucher le trou d'un paquet **perdu**,
//! jamais celui d'un paquet **en retard**. Dans ce second cas il n'y a aucune
//! arrivée, donc aucun code qui tourne : le tampon se vide et la sortie joue du
//! silence. Mesuré sur une session de 28 minutes (18/09/2026) : **2 trames
//! masquées contre 30 accrocs**. La machine à masquer existe et ne sert presque
//! jamais.
//!
//! # Ce que décide ce module
//!
//! À l'échéance de la trame attendue, et seulement là, trois issues :
//! attendre encore, fabriquer une trame de remplacement, ou fondre vers le
//! silence parce qu'on invente depuis trop longtemps.
//!
//! La décision vit ici, seule et sans état, pour être testée aux bords sans
//! réseau ni carte son. Le thread de décodage n'en garde que le compteur de
//! trames consécutives. **Rien de tout ceci ne tourne dans le callback audio.**
//!
//! # Pourquoi on n'invente pas longtemps
//!
//! Le masquage d'Opus prolonge la matière du son précédent. Sur deux ou trois
//! trames c'est inaudible ; au-delà, il produit une tenue artificielle, plus
//! gênante que le trou qu'elle remplace. D'où le plafond, puis le fondu.

/// Durée d'une trame Opus dans le flux Jamodio.
pub const FRAME_MS: f64 = 2.5;

/// Trames de masquage consécutives admises (7,5 ms) avant de fondre.
pub const MAX_CONSECUTIVE: u32 = 3;

/// Ce que le thread de décodage doit faire à l'échéance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conceal {
    /// Ne rien fabriquer : soit l'échéance n'est pas atteinte, soit le tampon a
    /// encore de quoi jouer jusqu'à la trame suivante.
    Wait,
    /// Pousser UNE trame de masquage (`FRAME_MS`).
    Frame,
    /// On invente depuis trop longtemps : fondre vers le silence.
    FadeToSilence,
}

/// Décision à l'échéance d'une trame.
///
/// - `late_by_ms` : retard sur l'échéance attendue. Négatif = pas encore l'heure.
/// - `fill_ms`    : ce qu'il reste à jouer dans le tampon de CE flux.
/// - `consecutive`: trames de masquage déjà poussées d'affilée pour ce flux.
///
/// Un tampon qui tient au moins une trame n'a besoin de rien : la sortie a de
/// quoi jouer pendant que le paquet finit d'arriver. C'est le cas le plus
/// fréquent, et c'est celui où il ne faut surtout pas inventer — le paquet en
/// retard arrivera, et sa trame inventée aurait pris sa place.
pub fn decide(late_by_ms: f64, fill_ms: f64, consecutive: u32) -> Conceal {
    // Une mesure non finie (NaN, infini) fait ATTENDRE : on n'invente pas du son
    // sur la foi d'une horloge folle. C'est aussi ce qui rend la comparaison
    // suivante sûre — elle ne voit plus que des nombres.
    if !late_by_ms.is_finite() || late_by_ms < 0.0 {
        return Conceal::Wait;
    }
    if fill_ms >= FRAME_MS {
        return Conceal::Wait;
    }
    if consecutive >= MAX_CONSECUTIVE {
        return Conceal::FadeToSilence;
    }
    Conceal::Frame
}

/// Combien de temps dormir avant la prochaine échéance, bornée pour que la
/// boucle se réveille même quand aucun flux n'attend rien.
///
/// Le réveil a été mesuré sur le PC de recette (Windows, thread promu comme
/// celui-ci) : dépassement médian 396 µs, p99 810 µs, pire cas 1,06 ms sur 800
/// mesures — soit une marge confortable sur une trame de 2,5 ms. C'est ce qui a
/// permis de garder l'attente simple et d'écarter une minuterie haute résolution
/// (sonde `wake_probe`, 19/09/2026).
pub fn sleep_until_deadline_ms(next_deadline_in_ms: f64) -> f64 {
    const MAX_SLEEP_MS: f64 = 5.0;
    if !next_deadline_in_ms.is_finite() || next_deadline_in_ms <= 0.0 {
        return 0.0;
    }
    next_deadline_in_ms.min(MAX_SLEEP_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avant_lecheance_on_ninvente_rien() {
        assert_eq!(decide(-1.0, 0.0, 0), Conceal::Wait);
        assert_eq!(decide(-0.001, 0.0, 2), Conceal::Wait);
    }

    #[test]
    fn un_tampon_qui_tient_une_trame_na_besoin_de_rien() {
        // Le paquet est en retard, mais la sortie a de quoi jouer : inventer
        // maintenant volerait sa place au paquet qui arrive.
        assert_eq!(decide(0.5, FRAME_MS, 0), Conceal::Wait);
        assert_eq!(decide(50.0, 12.0, 0), Conceal::Wait);
    }

    #[test]
    fn echeance_depassee_et_tampon_vide_on_masque() {
        assert_eq!(decide(0.0, 0.0, 0), Conceal::Frame);
        assert_eq!(decide(0.1, 2.49, 0), Conceal::Frame);
    }

    #[test]
    fn au_dela_de_trois_trames_on_fond_vers_le_silence() {
        for n in 0..MAX_CONSECUTIVE {
            assert_eq!(decide(1.0, 0.0, n), Conceal::Frame, "n={n}");
        }
        assert_eq!(decide(1.0, 0.0, MAX_CONSECUTIVE), Conceal::FadeToSilence);
        assert_eq!(decide(1.0, 0.0, 99), Conceal::FadeToSilence);
    }

    /// 7,5 ms : c'est la durée qu'on s'autorise à inventer, pas une de plus.
    #[test]
    fn le_plafond_vaut_bien_sept_millisecondes_et_demie() {
        assert_eq!(MAX_CONSECUTIVE as f64 * FRAME_MS, 7.5);
    }

    #[test]
    fn une_mesure_de_temps_absurde_fait_attendre_pas_masquer() {
        assert_eq!(decide(f64::NAN, 0.0, 0), Conceal::Wait);
        assert_eq!(decide(f64::NEG_INFINITY, 0.0, 0), Conceal::Wait);
        // Un retard « infini » n'est pas un retard mesuré : on attend aussi.
        assert_eq!(decide(f64::INFINITY, 0.0, 0), Conceal::Wait);
    }

    #[test]
    fn lattente_est_bornee_pour_que_la_boucle_respire() {
        assert_eq!(sleep_until_deadline_ms(1.2), 1.2);
        assert_eq!(sleep_until_deadline_ms(50.0), 5.0);
        // Échéance déjà passée, ou absurde → on ne dort pas.
        assert_eq!(sleep_until_deadline_ms(0.0), 0.0);
        assert_eq!(sleep_until_deadline_ms(-3.0), 0.0);
        assert_eq!(sleep_until_deadline_ms(f64::NAN), 0.0);
        assert_eq!(sleep_until_deadline_ms(f64::INFINITY), 0.0);
    }
}
