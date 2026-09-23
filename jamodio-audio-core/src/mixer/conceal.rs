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

/// POURQUOI on renonce à masquer. Sans cette raison, un masquage qui ne part
/// JAMAIS est indiscernable d'un masquage qui n'a rien à faire : c'est ce qui a
/// bloqué le diagnostic du 20/09/2026, où le Mac accumulait 11 accrocs avec
/// zéro masquage sans qu'on puisse dire laquelle des conditions s'y opposait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Pas encore l'heure, ou mesure de temps inexploitable.
    NotDue,
    /// On ne connaît pas encore la régularité de ce lien : inventer serait
    /// deviner.
    LinkUnknown,
    /// Le paquet est en retard, mais pas plus que ce que ce lien produit
    /// d'habitude — il est sans doute encore en vol.
    WithinGrace,
    /// Le tampon tient jusqu'au prochain tirage de la sortie : la place du
    /// paquet est encore libre, on la lui laisse.
    BufferHolds,
    /// Le tampon se ré-amorce après un trou : la sortie n'y puise rien tant
    /// qu'il n'est pas remonté à sa cible. Une trame inventée n'y serait pas
    /// jouée plus tôt — elle prendrait seulement la place du vrai paquet, qui
    /// arrive ensuite et serait écarté. L'arrivée suivante réarme l'échéance.
    Repriming,
}

/// Ce que le thread de décodage doit faire à l'échéance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conceal {
    /// Ne rien fabriquer, pour la raison portée.
    Wait(Wait),
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
/// sortie. CONSTANTE DE CALIBRATION, pas encore établie par la mesure :
///
/// - la sonde du 19/09/2026 (396 µs médians, 1,06 ms au pire) mesurait
///   `thread::sleep`, minuterie haute résolution — PAS l'attente réelle de ce
///   thread (`recv_timeout`) ;
/// - `recv_timeout` sous Windows, minuterie fine posée (`timer_precision`),
///   mesuré hors agent le 22/09/2026 : ~0,85 ms de dépassement médian, 3,3 ms
///   au pire. 0,5 ms est donc sous le retard typique d'un réveil Windows.
///
/// On ne la relève pas à l'aveugle : une marge plus grande fait inventer plus
/// tôt, donc plus souvent pour rien. Les deux mesures qui la trancheront
/// existent depuis le Lot 1-C (23/09/2026) : le retard réel du réveil de ce
/// thread (`decode_wake_late_*` dans le journal perfstats) et les trous
/// survenus alors que le tampon avait été jugé suffisant
/// (`holesAfterBufferHolds`). Les masquages prématurés, eux, se jugent
/// désormais au tirage près (cf. [`premature_margin_ms`]). Réglage : après
/// lecture de ces chiffres, pas avant.
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
///    jouer pendant que le paquet finit d'arriver. Ce seuil vaut exactement la
///    taille du bloc de sortie plus la marge de réveil — il n'y a PAS de
///    plancher à une trame : en mettre un faisait inventer trop tôt partout où
///    le bloc est plus petit, c'est-à-dire sur nos deux plateformes (22
///    masquages prématurés sur 38, banc du 20/09/2026). `FRAME_MS` ne sert que
///    de repli quand la taille du bloc n'a pas été mesurée.
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
        return Conceal::Wait(Wait::NotDue);
    }
    let Some(tail) = jitter_tail_ms.filter(|t| t.is_finite()) else {
        return Conceal::Wait(Wait::LinkUnknown);
    };
    let grace = grace_ms(tail);
    if late_by_ms < grace {
        return Conceal::Wait(Wait::WithinGrace);
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
    // Remplissage non fini : même règle qu'en tête, on attend.
    if !fill_ms.is_finite() || fill_ms >= survival_ms(output_block_ms) {
        return Conceal::Wait(Wait::BufferHolds);
    }
    if consecutive >= MAX_CONSECUTIVE {
        return Conceal::FadeToSilence;
    }
    Conceal::Frame
}

/// Délai de grâce accordé à un paquet en retard sur CE lien.
fn grace_ms(jitter_tail_ms: f64) -> f64 {
    jitter_tail_ms.clamp(GRACE_MIN_MS, GRACE_MAX_MS)
}

/// Ce qu'un tirage de la sortie consomme d'un coup ; repli sur une trame tant
/// que la taille du bloc n'a pas été mesurée.
fn output_draw_ms(output_block_ms: f64) -> f64 {
    if output_block_ms.is_finite() && output_block_ms > 0.0 {
        output_block_ms
    } else {
        FRAME_MS
    }
}

/// Seuil de survie du tampon (cf. `decide`, point 3).
fn survival_ms(output_block_ms: f64) -> f64 {
    if output_block_ms.is_finite() && output_block_ms > 0.0 {
        output_block_ms + WAKE_SLACK_MS
    } else {
        FRAME_MS
    }
}

/// Dans combien de temps la décision `why` peut-elle CHANGER, faute de paquet ?
///
/// Un paquet qui arrive réveille le thread de toute façon ; cette fonction ne
/// répond que pour le cas où rien n'arrive. Avant elle, le thread revenait
/// examiner le flux toutes les 0,5 ms (le plancher de sommeil) : ~1 600 réveils
/// par seconde sur deux flux, dont plus de 99,9 % pour reconduire la même
/// attente (session du 21/09/2026). Or la réponse se calcule :
///
/// - `WithinGrace` : la grâce finit à une heure connue ;
/// - `BufferHolds` : le tampon ne peut pas passer sous le seuil de survie
///   avant `fill − seuil − un tirage`. La sortie consomme au rythme du temps,
///   mais PAR BLOCS : un tirage peut tomber tout de suite, d'où le bloc retiré
///   — c'est ce qui garantit qu'on ne se réveille jamais APRÈS le premier
///   instant où masquer devient possible ;
/// - `LinkUnknown` : seule l'arrivée de paquets peut le lever ; on repasse à la
///   trame suivante, sans plus ;
/// - `NotDue` : l'échéance elle-même dit quand revenir — `None`.
///
/// Le résultat n'est jamais négatif ; le plancher de sommeil s'applique ensuite
/// (`sleep_until_deadline_ms`). Une mesure non finie rend `0` : on revient au
/// plus tôt plutôt que de dormir sur une horloge folle.
pub fn recheck_in_ms(
    why: Wait,
    late_by_ms: f64,
    fill_ms: f64,
    output_block_ms: f64,
    jitter_tail_ms: Option<f64>,
) -> Option<f64> {
    let ms = match why {
        Wait::NotDue | Wait::Repriming => return None,
        Wait::LinkUnknown => FRAME_MS,
        Wait::WithinGrace => match jitter_tail_ms {
            Some(tail) if tail.is_finite() => grace_ms(tail) - late_by_ms,
            _ => 0.0,
        },
        Wait::BufferHolds => {
            fill_ms - survival_ms(output_block_ms) - output_draw_ms(output_block_ms)
        }
    };
    Some(if ms.is_finite() { ms.max(0.0) } else { 0.0 })
}

/// Combien de temps dormir avant la prochaine échéance, bornée pour que la
/// boucle se réveille même quand aucun flux n'attend rien.
///
/// Précision du réveil : sous Windows, elle dépend de la minuterie du processus
/// (15,6 ms par défaut, ~1 ms pendant une session — cf. `timer_precision`) ; sur
/// macOS elle est sub-milliseconde. Voir `WAKE_SLACK_MS` pour ce qui reste à
/// mesurer.
pub fn sleep_until_deadline_ms(next_deadline_in_ms: f64) -> f64 {
    const MAX_SLEEP_MS: f64 = 5.0;
    /// Plancher de sommeil. Une échéance déjà dépassée rendait `0`, ce qui
    /// n'était sans danger que tant que la boucle réarmait l'échéance à chaque
    /// examen — le défaut même qu'on vient de corriger. Sans plancher, un
    /// retard qu'on laisse courir ferait tourner le thread de décodage sans
    /// pause, à priorité audio (MMCSS « Pro Audio » / QoS USER_INTERACTIVE) :
    /// il volerait le CPU au callback et transformerait une excursion réseau
    /// en accroc local.
    ///
    /// 0,5 ms n'ajoute AUCUNE latence au chemin nominal : `recv_timeout` rend
    /// la main dès qu'un paquet arrive, il ne dort pas jusqu'au bout.
    const MIN_SLEEP_MS: f64 = 0.5;
    if !next_deadline_in_ms.is_finite() {
        return MIN_SLEEP_MS;
    }
    next_deadline_in_ms.clamp(MIN_SLEEP_MS, MAX_SLEEP_MS)
}

/// Le masquage qu'on vient de faire était-il PRÉMATURÉ ?
///
/// La question posée au banc du 20/09/2026 : sur les trames inventées, combien
/// l'ont été alors que le vrai paquet allait arriver à temps ? Le compteur
/// `underruns` ne pouvait pas y répondre — il ne compte que les trous
/// RÉELLEMENT rendus, donc un masquage réussi et un masquage inutile ont la
/// même signature (une trame inventée, aucun accroc).
///
/// # Le critère exact (Lot 1-B, 23/09/2026)
///
/// La trame inventée est ajoutée APRÈS la vraie matière que le tampon tenait
/// (`fill_ms_at_conceal`). Tant que la sortie n'a pas fini cette vraie matière,
/// elle n'a pas touché à l'invention. Donc, quand le vrai paquet arrive :
/// - la sortie a consommé **au plus** `fill_ms_at_conceal` depuis le masquage →
///   aucun tirage n'a eu besoin de l'invention ; sans elle, le paquet aurait été
///   poussé à temps pour le tirage suivant. **Prématuré**, et on lui a volé sa
///   place ;
/// - elle a consommé **plus** → un tirage a puisé dans l'invention : sans elle,
///   ce tirage aurait rendu un trou. Le masquage a fait son travail.
///
/// `consumed_since_ms` est la consommation RÉELLE de la sortie entre le
/// masquage et l'arrivée, lue à la position de lecture du tampon
/// (`ring_buffer::consumed_ms_between`) — pas le temps écoulé. C'est tout le
/// correctif : l'ancien critère comparait le délai d'arrivée à « le tampon est
/// vide » en temps continu, alors que la sortie tire par blocs et par à-coups
/// (un pilote ASIO rappelle à ~945 / ~2 300 µs). Il se trompait donc jusqu'à un
/// bloc — 1,33 ms, du même ordre que la marge moyenne mesurée (1,1 ms).
///
/// Une mesure non finie ou négative ne prouve rien : on ne compte pas un
/// prématuré qu'on n'a pas établi.
///
/// Rendu : DE COMBIEN le masquage était prématuré — la vraie matière que la
/// sortie n'avait pas encore jouée quand le paquet est arrivé — `None` s'il ne
/// l'était pas. Quelques dizaines de microsecondes se rattrapent sur le seuil
/// de survie ; plusieurs millisecondes veulent dire que le délai de grâce est
/// trop court. Sans ce chiffre, on réglerait au jugé.
pub fn premature_margin_ms(fill_ms_at_conceal: f64, consumed_since_ms: f64) -> Option<f64> {
    if !fill_ms_at_conceal.is_finite() || !consumed_since_ms.is_finite() {
        return None;
    }
    if fill_ms_at_conceal < 0.0 || consumed_since_ms < 0.0 || consumed_since_ms > fill_ms_at_conceal {
        return None;
    }
    Some(fill_ms_at_conceal - consumed_since_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gigue typique du banc : ~2,5 ms de queue.
    const TAIL: Option<f64> = Some(2.5);

    /// Le masquage prématuré, celui qu'on cherche à compter : la sortie n'avait
    /// pas fini la vraie matière quand le paquet est arrivé.
    #[test]
    fn un_paquet_arrive_avant_que_la_sortie_entame_l_invention_prouve_un_masquage_de_trop() {
        // Le tampon tenait 4 ms ; la sortie en a joué 1,5 avant l'arrivée.
        assert_eq!(premature_margin_ms(4.0, 1.5), Some(2.5));
        // Elle a joué exactement la vraie matière : aucun tirage n'a encore
        // puisé dans l'invention, le paquet serait passé au suivant.
        assert_eq!(premature_margin_ms(4.0, 4.0), Some(0.0));
    }

    /// Un tirage a puisé dans l'invention : sans elle, il rendait un trou.
    #[test]
    fn une_sortie_qui_a_entame_l_invention_prouve_un_masquage_utile() {
        assert_eq!(premature_margin_ms(4.0, 4.0 + 1.0 / 96.0), None);
        assert_eq!(premature_margin_ms(4.0, 9.0), None);
    }

    /// Tampon vide au masquage, et aucun tirage avant l'arrivée du paquet :
    /// il aurait été joué à temps. L'ancien critère (en temps) ne pouvait pas
    /// le voir.
    #[test]
    fn un_tampon_vide_sans_tirage_avant_l_arrivee_etait_un_masquage_de_trop() {
        assert_eq!(premature_margin_ms(0.0, 0.0), Some(0.0));
        // Un tirage a eu lieu : il a joué l'invention.
        assert_eq!(premature_margin_ms(0.0, 1.33), None);
    }

    /// Ce qui juge, c'est la consommation par BLOCS, pas le temps : 3 ms
    /// écoulées sur un tampon de 3,5 ms, mais la sortie a pris deux blocs de
    /// 1,33 ms seulement — le paquet arrive à temps.
    #[test]
    fn la_consommation_par_blocs_juge_et_pas_le_temps_ecoule() {
        let deux_blocs = 2.0 * 64.0 * 1000.0 / 48_000.0;
        assert!(premature_margin_ms(3.5, deux_blocs).is_some());
        // Trois blocs d'un coup (rafale ASIO) dépassent le tampon : utile.
        assert!(premature_margin_ms(3.5, 1.5 * deux_blocs).is_none());
    }

    #[test]
    fn une_mesure_absurde_ne_compte_pas_un_premature() {
        // On ne compte pas ce qu'on n'a pas établi.
        assert!(premature_margin_ms(f64::NAN, 1.0).is_none());
        assert!(premature_margin_ms(4.0, f64::NAN).is_none());
        assert!(premature_margin_ms(f64::INFINITY, 1.0).is_none());
        assert!(premature_margin_ms(4.0, -1.0).is_none());
        assert!(premature_margin_ms(-1.0, 0.0).is_none());
    }

    #[test]
    fn avant_lecheance_on_ninvente_rien() {
        assert_eq!(decide(-1.0, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait(Wait::NotDue));
        assert_eq!(decide(-0.001, 0.0, FRAME_MS, TAIL, 2), Conceal::Wait(Wait::NotDue));
    }

    /// Le correctif du 20/09 : un paquet à peine en retard arrive encore.
    #[test]
    fn un_retard_ordinaire_ne_declenche_rien() {
        // 1 ms de retard sur un lien dont la queue de gigue vaut 2,5 ms : le
        // paquet est encore en vol, et inventer lui volerait sa place.
        assert_eq!(decide(1.0, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait(Wait::WithinGrace));
        assert_eq!(decide(2.49, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait(Wait::WithinGrace));
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
        assert!(matches!(decide(5.0, 10.7 + WAKE_SLACK_MS + 0.1, 10.7, TAIL, 0), Conceal::Wait(_)));
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
        assert_eq!(decide(5.0, 2.0, bloc, TAIL, 0), Conceal::Wait(Wait::BufferHolds));
        // En dessous du bloc + marge, en revanche, le trou est certain.
        assert_eq!(decide(5.0, 1.5, bloc, TAIL, 0), Conceal::Frame);
    }

    #[test]
    fn une_taille_de_bloc_inconnue_retombe_sur_la_trame() {
        // Sortie pas encore démarrée (0) ou mesure absurde : on ne devine pas un
        // seuil plus court que ce qu'on sait, on garde la prudence d'une trame.
        assert_eq!(decide(5.0, 2.0, 0.0, TAIL, 0), Conceal::Frame);
        assert!(matches!(decide(5.0, 2.6, 0.0, TAIL, 0), Conceal::Wait(_)));
        assert!(matches!(decide(5.0, 2.6, f64::NAN, TAIL, 0), Conceal::Wait(_)));
        assert_eq!(decide(5.0, 2.0, -1.0, TAIL, 0), Conceal::Frame);
    }

    #[test]
    fn tant_quon_ne_connait_pas_le_lien_on_ninvente_pas() {
        assert_eq!(decide(50.0, 0.0, FRAME_MS, None, 0), Conceal::Wait(Wait::LinkUnknown));
        assert_eq!(
            decide(50.0, 0.0, FRAME_MS, Some(f64::NAN), 0),
            Conceal::Wait(Wait::LinkUnknown)
        );
    }

    #[test]
    fn la_grace_reste_dans_des_bornes_raisonnables() {
        // Lien exceptionnellement régulier : on accorde quand même 1 ms.
        assert!(matches!(decide(0.5, 0.0, FRAME_MS, Some(0.01), 0), Conceal::Wait(_)));
        assert_eq!(decide(1.0, 0.0, FRAME_MS, Some(0.01), 0), Conceal::Frame);
        // Estimation emballée : la grâce est plafonnée, le masquage reste possible.
        assert!(matches!(decide(9.9, 0.0, FRAME_MS, Some(500.0), 0), Conceal::Wait(_)));
        assert_eq!(decide(10.0, 0.0, FRAME_MS, Some(500.0), 0), Conceal::Frame);
    }

    #[test]
    fn un_tampon_qui_tient_jusquau_prochain_tirage_na_besoin_de_rien() {
        // Le paquet est en retard, mais la sortie a de quoi jouer : inventer
        // maintenant volerait sa place au paquet qui arrive.
        assert_eq!(
            decide(5.0, FRAME_MS + WAKE_SLACK_MS, FRAME_MS, TAIL, 0),
            Conceal::Wait(Wait::BufferHolds)
        );
        assert_eq!(decide(50.0, 12.0, FRAME_MS, TAIL, 0), Conceal::Wait(Wait::BufferHolds));
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
        assert_eq!(decide(f64::NAN, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait(Wait::NotDue));
        assert_eq!(
            decide(f64::NEG_INFINITY, 0.0, FRAME_MS, TAIL, 0),
            Conceal::Wait(Wait::NotDue)
        );
        // Un retard « infini » n'est pas un retard mesuré : on attend aussi.
        assert!(matches!(decide(f64::INFINITY, 0.0, FRAME_MS, TAIL, 0), Conceal::Wait(_)));
    }

    #[test]
    fn lattente_ne_descend_jamais_a_zero() {
        // Échéance dépassée : on dort quand même un peu. Sans ce plancher, la
        // boucle tournerait sans pause à priorité audio tant que le retard
        // court (c'est-à-dire pendant toute une excursion réseau).
        assert_eq!(sleep_until_deadline_ms(0.0), 0.5);
        assert_eq!(sleep_until_deadline_ms(-12.0), 0.5);
        assert_eq!(sleep_until_deadline_ms(0.1), 0.5);
        assert_eq!(sleep_until_deadline_ms(f64::NAN), 0.5);
        // Une échéance à venir reste respectée.
        assert_eq!(sleep_until_deadline_ms(2.5), 2.5);
    }

    #[test]
    fn lattente_est_bornee_pour_que_la_boucle_respire() {
        assert_eq!(sleep_until_deadline_ms(1.2), 1.2);
        assert_eq!(sleep_until_deadline_ms(50.0), 5.0);
        // Une mesure absurde ne fait pas tourner la boucle sans pause.
        assert_eq!(sleep_until_deadline_ms(f64::INFINITY), 0.5);
    }

    // ─── Quand revenir examiner un flux (M2, 21/09/2026) ────────────────

    const BLOC: f64 = 64.0 * 1000.0 / 48_000.0; // 1,33 ms, nos deux plateformes

    #[test]
    fn pendant_la_grace_on_revient_pile_a_sa_fin() {
        // Grâce = 2,5 ms (TAIL), 0,5 ms déjà écoulée : rien ne peut changer avant 2 ms.
        let r = recheck_in_ms(Wait::WithinGrace, 0.5, 0.0, BLOC, TAIL).unwrap();
        assert!((r - 2.0).abs() < 1e-9, "{r}");
        // Et la décision prise à cet instant n'est plus « dans la grâce ».
        assert_ne!(decide(0.5 + r, 0.0, BLOC, TAIL, 0), Conceal::Wait(Wait::WithinGrace));
    }

    #[test]
    fn un_tampon_garni_laisse_dormir_ce_quil_tient_moins_un_tirage() {
        // 6 ms en stock, seuil 1,83 ms, un tirage de 1,33 ms peut tomber tout de suite.
        let r = recheck_in_ms(Wait::BufferHolds, 5.0, 6.0, BLOC, TAIL).unwrap();
        assert!((r - (6.0 - (BLOC + 0.5) - BLOC)).abs() < 1e-9, "{r}");
    }

    /// LA garantie : quel que soit l'instant où tombent les tirages de la
    /// sortie, le tampon ne passe JAMAIS sous le seuil de survie avant l'heure
    /// du prochain examen. Sinon, dormir plus longtemps ferait masquer plus tard
    /// qu'avant — c'est-à-dire laisser passer un trou.
    #[test]
    fn on_ne_se_reveille_jamais_apres_le_moment_ou_masquer_devient_possible() {
        let seuil = BLOC + 0.5;
        for fill0 in [1.9, 2.5, 3.2, 4.0, 6.0, 9.7, 15.0] {
            let r = recheck_in_ms(Wait::BufferHolds, 3.0, fill0, BLOC, TAIL).unwrap();
            for phase_pct in 0..100 {
                // Premier tirage à `phase` ms, puis un tirage par bloc.
                let phase = BLOC * phase_pct as f64 / 100.0;
                let mut t = 0.0;
                while t < r {
                    let tirages = if t < phase { 0.0 } else { ((t - phase) / BLOC).floor() + 1.0 };
                    let fill = fill0 - tirages * BLOC;
                    assert!(
                        fill >= seuil - 1e-9,
                        "fill0={fill0} phase={phase:.2} t={t:.2} : tampon {fill:.2} sous le seuil avant le réveil ({r:.2} ms)"
                    );
                    t += 0.01;
                }
            }
        }
    }

    #[test]
    fn un_tampon_deja_au_seuil_fait_revenir_au_plus_tot() {
        assert_eq!(recheck_in_ms(Wait::BufferHolds, 3.0, BLOC + 0.6, BLOC, TAIL), Some(0.0));
    }

    #[test]
    fn un_lien_inconnu_se_reexamine_a_la_trame_suivante() {
        assert_eq!(recheck_in_ms(Wait::LinkUnknown, 1.0, 0.0, BLOC, None), Some(FRAME_MS));
    }

    #[test]
    fn avant_lecheance_cest_lecheance_qui_dit_quand_revenir() {
        assert_eq!(recheck_in_ms(Wait::NotDue, -1.0, 0.0, BLOC, TAIL), None);
    }

    #[test]
    fn une_mesure_absurde_fait_revenir_au_plus_tot() {
        assert_eq!(recheck_in_ms(Wait::WithinGrace, f64::NAN, 0.0, BLOC, TAIL), Some(0.0));
        assert_eq!(recheck_in_ms(Wait::BufferHolds, 3.0, f64::INFINITY, BLOC, TAIL), Some(0.0));
        assert_eq!(recheck_in_ms(Wait::WithinGrace, 0.5, 0.0, BLOC, Some(f64::NAN)), Some(0.0));
    }
}
