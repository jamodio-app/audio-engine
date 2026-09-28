//! Banc « N musiciens » de l'Audio Engine — cf. `PLAN-BANC-N-MUSICIENS-2026-09.md`
//! (dépôt du site).
//!
//! Un faux serveur et un pilote, contre le VRAI Audio Engine installé : de 2 à 9
//! musiciens (ou plus), des profils de lien déterministes, des mesures seconde par
//! seconde et un résumé qui dit si les critères tiennent. Pensé pour durer : un
//! scénario est un fichier JSON qu'une future interface pourra produire.

pub mod driver;
pub mod profile;
pub mod relay;
pub mod report;
pub mod rt;
pub mod run;
pub mod scenario;
pub mod server;
