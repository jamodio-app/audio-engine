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

/// Bornes du délai de grâce accordé à un paquet en retard. Le plancher évite
/// qu'un lien exceptionnellement régulier nous fasse inventer au moindre
/// frémissement ; le plafond évite qu'une estimation de gigue emballée désarme
/// le masquage pour de bon.
const GRACE_MIN_MS: f64 = 1.0;
const GRACE_MAX_MS: f64 = 10.0;

/// Imprécision du réveil du thread de décodage, à couvrir en plus du bloc de
/// sortie. Mesurée par la sonde 1.2a sur le PC (thread promu MMCSS) : 396 µs
/// médians, 810 µs au p99, 1,06 ms au pire. On retient 0,5 ms — au-dessus du
/// cas courant, sous le p99 : les rares réveils plus tardifs sont rattrapés au
/// suivant, alors qu'un seuil calé sur le pire cas ferait inventer trop tôt en
/// permanence.
const WAKE_SLACK_MS: f64 = 0.5;

/// Décision à l'échéance d'une trame.
///
/// - `late_by_ms` : retard sur l'échéance attendue. Négatif = pas encore l'heure.
/// - `fill_ms`    : ce qu'il reste à jouer dans le tampon de CE flux.
/// - `output_block_ms` : durée que le callback de SORTIE consomme d'un seul
///   tirage. `0` (ou non finie) tant qu'on ne la connaît pas : on retombe alors
///   sur la durée d'une trame.
/// - `jitter_tail_ms` : pire retard récemment mesuré SUR CE LIEN, ou `None` tant
///   que la mesure n'est pas chaude.
/// - `consecutive`: trames de masquage déjà poussées d'affilée pour ce flux.
///
/// Trois raisons d'attendre plutôt que d'inventer, dans cet ordre :
///
/// 1. **On ne connaît pas encore le lien** (`jitter_tail_ms == None`). Inventer
///    sans savoir ce que ce réseau fait d'habitude, c'est deviner.
/// 2. **Le paquet est en retard, mais pas plus que d'habitude.** C'est la leçon
///    du banc du 19/09/2026 : avec la seule condition « le tampon contient moins
///    d'une trame », le masquage a tiré **59 fois sur 74 alors qu'aucun trou
///    n'existait** — le paquet arrivait juste après et se faisait écarter, sa
///    place ayant été prise. On n'invente donc qu'au-delà du pire retard que ce
///    lien produit déjà.
/// 3. **Le tampon survit au prochain tirage de la sortie.** La sortie a de quoi
///    jouer pendant que le paquet finit d'arriver. Ce seuil vaut la taille du
///    bloc de sortie, jamais moins d'une trame : le mesurer en trames a coûté
///    13 accrocs et zéro masquage au banc Mac du 20/09/2026.
pub fn decide(
    late_by_ms: f64,
    fill_ms: f64,
    output_block_ms: f64,
    jitter_tail_ms: Option<f64>,
    consecutive: u32,
) -> Conceal {
    // Une mesure non finie (NaN, infini) fait ATTENDRE : on n'invente pas du son
    // sur la foi d'une horloge folle. C'est aussi ce qui rend les comparaisons
    // suivantes sûres — elles ne voient plus que des nombres.
    if !late_by_ms.is_finite() || late_by_ms < 0.0 {
        return Conceal::Wait;
    }
    let Some(tail) = jitter_tail_ms.filter(|t| t.is_finite()) else {
        return Conceal::Wait;
    };
    let grace = tail.clamp(GRACE_MIN_MS, GRACE_MAX_MS);
    if late_by_ms < grace {
        return Conceal::Wait;
    }
    // Combien le tampon doit contenir pour survivre jusqu'au prochain examen :
    // ce que le callback de SORTIE consomme d'un coup, plus l'imprécision du
    // réveil. Rien de plus — chaque dixième de milliseconde au-dessus fait
    // inventer alors que le vrai paquet était en route.
    //
    // Deux erreurs successives ont mené ici, toutes deux mesurées :
    // - « une trame Opus » (2,5 ms). Choisi sans mesure. Au banc du 20/09/2026,
    //   **22 masquages sur 38 étaient prématurés** — le paquet arrivait avant
    //   que le tampon ne se vide, avec 1,41 ms de rab en moyenne et jusqu'à
    //   2,40 ms, soit presque le seuil entier.
    // - `max(bloc, FRAME_MS)`. Le plancher d'une trame gardait le défaut
    //   partout où le bloc est plus petit — c'est-à-dire sur nos deux
    //   plateformes, qui livrent 64 frames (1,33 ms).
    //
    // `FRAME_MS` ne reste que comme repli quand la taille du bloc n'a pas été
    // mesurée : on ne devine pas un seuil plus court que ce qu'on sait.
    let survival_ms = if output_block_ms.is_finite() && output_block_ms > 0.0 {
        output_block_ms + WAKE_SLACK_MS
    } else {
        FRAME_MS
    };
    if fill_ms >= survival_ms {
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

/// Le masquage qu'on vient de faire était-il PRÉMATURÉ ?
///
/// La question posée au banc du 20/09/2026 : sur les trames inventées, combien
/// l'ont été alors que le vrai paquet allait arriver à temps ? Le compteur
/// `underruns` ne pouvait pas y répondre — il ne compte que les trous
/// RÉELLEMENT rendus, donc un masquage réussi et un masquage inutile ont la
/// même signature (une trame inventée, aucun accroc).
///
/// On compare ce que le tampon pouvait encore tenir À L'INSTANT du masquage
/// (`fill_ms_at_conceal`) au temps qu'a réellement mis le paquet à arriver
/// ensuite (`arrival_delay_ms`) :
/// - le paquet arrive AVANT que le tampon ne se vide → il aurait été joué à sa
///   place, on a inventé pour rien **et** on lui a volé sa place ;
/// - il arrive après → le trou aurait été réel, le masquage a fait son travail.
///
/// Une mesure non finie ne prouve rien : on ne compte pas un prématuré qu'on
/// n'a pas établi.
pub fn was_premature(fill_ms_at_conceal: f64, arrival_delay_ms: f64) -> bool {
    premature_margin_ms(fill_ms_at_conceal, arrival_delay_ms).is_some()
}

/// DE COMBIEN le masquage était-il prématuré ? `None` s'il ne l'était pas.
///
/// C'est la marge qu'on n'a pas su attendre : le tampon tenait encore
/// `fill_ms_at_conceal`, le paquet est arrivé au bout de `arrival_delay_ms`, il
/// restait donc cette différence de rab. Savoir COMBIEN décide du réglage :
/// quelques dizaines de microsecondes se rattrapent en avançant le seuil de
/// survie ; plusieurs millisecondes veulent dire que le délai de grâce est trop
/// court. Sans ce chiffre, on ne saurait pas lequel des deux toucher — et on
/// réglerait au jugé.
pub fn premature_margin_ms(fill_ms_at_conceal: f64, arrival_delay_ms: f64) -> Option<f64> {
    if !fill_ms_at_conceal.is_finite() || !arrival_delay_ms.is_finite() {
        return None;
    }
    if arrival_delay_ms < 0.0 || arrival_delay_ms >= fill_ms_at_conceal {
        return None;
    }
    Some(fill_ms_at_conceal - arrival_delay_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gigue typique du banc : ~2,5 ms de queue.
    const TAIL: Option<f64> = Some(2.5);

    /// Le masquage prématuré, celui qu'on cherche à compter.
    #[test]
    fn un_paquet_qui_arrive_avant_que_le_tampon_se_vide_prouve_un_masquage_de_trop() {
        // Le tampon tenait encore 4 ms ; le paquet est arrivé 1,5 ms après le
        // masquage. Il aurait été joué à sa place.
        assert!(was_premature(4.0, 1.5));
        // Il arrive après que le tampon se soit vidé : le trou était réel.
        assert!(!was_premature(4.0, 4.0));
        assert!(!was_premature(4.0, 9.0));
    }

    #[test]
    fn la_marge_dit_de_combien_on_a_tire_trop_tot() {
        // Le tampon tenait 4 ms, le paquet est arrivé au bout de 1,5 ms :
        // il restait 2,5 ms de rab.
        assert_eq!(premature_margin_ms(4.0, 1.5), Some(2.5));
        // Pile à la limite : pas prématuré, donc pas de marge.
        assert_eq!(premature_margin_ms(4.0, 4.0), None);
        assert_eq!(premature_margin_ms(4.0, 9.0), None);
        // Mesure inexploitable : on ne fabrique pas une marge.
        assert_eq!(premature_margin_ms(f64::NAN, 1.0), None);
        assert_eq!(premature_margin_ms(4.0, -1.0), None);
    }

    #[test]
    fn un_tampon_vide_ne_produit_jamais_de_premature() {
        // Rien à tenir : aucun délai d'arrivée ne peut être « à temps ».
        assert!(!was_premature(0.0, 0.0));
        assert!(!was_premature(0.0, 0.5));
    }

    #[test]
    fn une_mesure_absurde_ne_compte_pas_un_premature() {
        // On ne compte pas ce qu'on n'a pas établi.
        assert!(!was_premature(f64::NAN, 1.0));
        assert!(!was_premature(4.0, f64::NAN));
        assert!(!was_premature(f64::INFINITY, 1.0));
        // Horloge à l'envers (paquet horodaté avant le masquage) : on s'abstient.
        assert!(!was_premature(4.0, -1.0));
    }

    #[test]
    fn avant_lecheance_on_ninvente_rien() {
        assert_eq!(decide(-1.0, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait);
        assert_eq!(decide(-0.001, 0.0, FRAME_MS, TAIL, 2), Conceal::Wait);
    }

    /// Le correctif du 20/09 : un paquet à peine en retard arrive encore.
    #[test]
    fn un_retard_ordinaire_ne_declenche_rien() {
        // 1 ms de retard sur un lien dont la queue de gigue vaut 2,5 ms : le
        // paquet est encore en vol, et inventer lui volerait sa place.
        assert_eq!(decide(1.0, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait);
        assert_eq!(decide(2.49, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait);
        // Au-delà du pire retard connu du lien, on n'attend plus.
        assert_eq!(decide(2.5, 0.0, FRAME_MS, TAIL, 0), Conceal::Frame);
    }

    /// Le seuil de survie est la taille du bloc de SORTIE, plus l'imprécision
    /// du réveil — et rien de plus.
    #[test]
    fn un_gros_bloc_de_sortie_exige_un_tampon_plus_garni() {
        // 512 frames = 10,7 ms consommés d'un coup : 5 ms dans le tampon ne
        // survivront pas au prochain tirage, même si c'est « plus d'une trame ».
        assert_eq!(decide(5.0, 5.0, 10.7, TAIL, 0), Conceal::Frame);
        // Au-delà du bloc PLUS la marge de réveil, on laisse le paquet arriver.
        assert_eq!(decide(5.0, 10.7 + WAKE_SLACK_MS + 0.1, 10.7, TAIL, 0), Conceal::Wait);
        // Juste en dessous, le prochain tirage n'est pas garanti : on masque.
        assert_eq!(decide(5.0, 10.7 + WAKE_SLACK_MS - 0.1, 10.7, TAIL, 0), Conceal::Frame);
    }

    /// Correctif du 20/09 — un petit bloc descend BIEN sous la durée d'une
    /// trame. Le plancher `FRAME_MS` qui existait ici faisait inventer alors
    /// que le paquet arrivait : 22 masquages prématurés sur 38 au banc.
    #[test]
    fn un_petit_bloc_de_sortie_descend_sous_une_trame() {
        // ASIO/CoreAudio, 64 frames = 1,33 ms. Seuil = 1,83 ms.
        let bloc = 64.0 * 1000.0 / 48_000.0;
        // 2 ms de tampon : l'ancienne règle masquait (2 < 2,5), la nouvelle
        // attend — et c'est ce qu'il fallait faire.
        assert_eq!(decide(5.0, 2.0, bloc, TAIL, 0), Conceal::Wait);
        // En dessous du bloc + marge, en revanche, le trou est certain.
        assert_eq!(decide(5.0, 1.5, bloc, TAIL, 0), Conceal::Frame);
    }

    #[test]
    fn une_taille_de_bloc_inconnue_retombe_sur_la_trame() {
        // Sortie pas encore démarrée (0) ou mesure absurde : on ne devine pas un
        // seuil plus court que ce qu'on sait, on garde la prudence d'une trame.
        assert_eq!(decide(5.0, 2.0, 0.0, TAIL, 0), Conceal::Frame);
        assert_eq!(decide(5.0, 2.6, 0.0, TAIL, 0), Conceal::Wait);
        assert_eq!(decide(5.0, 2.6, f64::NAN, TAIL, 0), Conceal::Wait);
        assert_eq!(decide(5.0, 2.0, -1.0, TAIL, 0), Conceal::Frame);
    }

    #[test]
    fn tant_quon_ne_connait_pas_le_lien_on_ninvente_pas() {
        assert_eq!(decide(50.0, 0.0, FRAME_MS, None, 0), Conceal::Wait);
        assert_eq!(decide(50.0, 0.0, FRAME_MS, Some(f64::NAN), 0), Conceal::Wait);
    }

    #[test]
    fn la_grace_reste_dans_des_bornes_raisonnables() {
        // Lien exceptionnellement régulier : on accorde quand même 1 ms.
        assert_eq!(decide(0.5, 0.0, FRAME_MS, Some(0.01), 0), Conceal::Wait);
        assert_eq!(decide(1.0, 0.0, FRAME_MS, Some(0.01), 0), Conceal::Frame);
        // Estimation emballée : la grâce est plafonnée, le masquage reste possible.
        assert_eq!(decide(9.9, 0.0, FRAME_MS, Some(500.0), 0), Conceal::Wait);
        assert_eq!(decide(10.0, 0.0, FRAME_MS, Some(500.0), 0), Conceal::Frame);
    }

    #[test]
    fn un_tampon_qui_tient_jusquau_prochain_tirage_na_besoin_de_rien() {
        // Le paquet est en retard, mais la sortie a de quoi jouer : inventer
        // maintenant volerait sa place au paquet qui arrive.
        assert_eq!(decide(5.0, FRAME_MS + WAKE_SLACK_MS, FRAME_MS, TAIL, 0), Conceal::Wait);
        assert_eq!(decide(50.0, 12.0, FRAME_MS, TAIL, 0), Conceal::Wait);
    }

    #[test]
    fn echeance_depassee_et_tampon_vide_on_masque() {
        assert_eq!(decide(2.5, 0.0, FRAME_MS, TAIL, 0), Conceal::Frame);
        assert_eq!(decide(3.0, 2.49, FRAME_MS, TAIL, 0), Conceal::Frame);
    }

    #[test]
    fn au_dela_de_trois_trames_on_fond_vers_le_silence() {
        for n in 0..MAX_CONSECUTIVE {
            assert_eq!(decide(5.0, 0.0, FRAME_MS, TAIL, n), Conceal::Frame, "n={n}");
        }
        assert_eq!(decide(5.0, 0.0, FRAME_MS, TAIL, MAX_CONSECUTIVE), Conceal::FadeToSilence);
        assert_eq!(decide(5.0, 0.0, FRAME_MS, TAIL, 99), Conceal::FadeToSilence);
    }

    /// 7,5 ms : c'est la durée qu'on s'autorise à inventer, pas une de plus.
    #[test]
    fn le_plafond_vaut_bien_sept_millisecondes_et_demie() {
        assert_eq!(MAX_CONSECUTIVE as f64 * FRAME_MS, 7.5);
    }

    #[test]
    fn une_mesure_de_temps_absurde_fait_attendre_pas_masquer() {
        assert_eq!(decide(f64::NAN, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait);
        assert_eq!(decide(f64::NEG_INFINITY, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait);
        // Un retard « infini » n'est pas un retard mesuré : on attend aussi.
        assert_eq!(decide(f64::INFINITY, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait);
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
