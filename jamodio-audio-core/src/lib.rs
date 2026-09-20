// 19/09/2026 — la seule chose qu'on ne mesurait pas : la CONTINUITÉ du signal
// capté au bord des blocs. C'est elle qui distinguait une prise saine d'une prise
// « horrible » que rien d'autre ne différenciait.
pub mod edge_continuity;
pub mod codec;
pub mod gain;
pub mod net;
pub mod mixer;
pub mod perfstats;
pub mod plugin_host;
pub mod protocol;
pub mod record;
pub mod sync;
pub mod voice_isolation;
