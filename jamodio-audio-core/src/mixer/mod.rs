pub mod ring_buffer;
// Le module `mixer.rs` porte le même nom que son parent — clippy le
// signale (`module_inception`), mais la structure est volontaire :
// `crate::mixer::mixer::AudioMixer` reste l'API publique stable
// référencée depuis le binaire agent + les hosts plugin. Renommer
// cascade dans 3+ fichiers sans gain réel.
#[allow(clippy::module_inception)]
pub mod mixer;
