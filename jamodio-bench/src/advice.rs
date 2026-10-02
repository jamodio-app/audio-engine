//! `session-bench conseil` : faut-il une campagne pour cette pré-version ?
//!
//! Beaucoup de pré-versions ne servent qu'à des essais et ne touchent pas le
//! son (Ben, 01/10/2026). Le conseil liste ce qui a changé dans l'Audio Engine
//! depuis la version de référence de la machine, le range par domaine, et dit
//! « aucune », « rapide » ou « complète », fichiers à l'appui. La décision
//! reste à Ben. Dans le doute, on mesure : un fichier qu'aucune règle ne range
//! demande une campagne.

use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Area {
    /// Réception, décodage, tampon, mixage, sortie, priorités, réseau, plugins.
    AudioPath,
    /// Installeur et réglages de la machine (ex. le freinage réseau, W3-bis).
    Machine,
    /// Dépendances (bibliothèques tierces) : elles peuvent toucher le son.
    Dependencies,
    /// Aucune règle ne le range : on mesure.
    Unknown,
    /// Fenêtre, icônes, textes.
    Interface,
    /// Le banc lui-même.
    Bench,
    /// Intégration continue, scripts, documentation.
    Tooling,
    /// Tests et exemples.
    Tests,
    /// Seulement le numéro de version.
    VersionOnly,
}

impl Area {
    pub fn label(self) -> &'static str {
        match self {
            Area::AudioPath => "chemin audio",
            Area::Machine => "installeur et réglages de la machine",
            Area::Dependencies => "dépendances",
            Area::Unknown => "non rangé (on mesure dans le doute)",
            Area::Interface => "interface et textes",
            Area::Bench => "banc",
            Area::Tooling => "intégration continue, scripts, documentation",
            Area::Tests => "tests et exemples",
            Area::VersionOnly => "numéro de version seulement",
        }
    }

    /// Ce domaine demande-t-il une campagne ?
    pub fn needs_campaign(self) -> bool {
        matches!(self, Area::AudioPath | Area::Machine | Area::Dependencies | Area::Unknown)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advice {
    Aucune,
    Rapide,
    Complete,
}

impl Advice {
    pub fn label(self) -> &'static str {
        match self {
            Advice::Aucune => "○ AUCUNE campagne utile",
            Advice::Rapide => "▶ campagne RAPIDE (~30 min) : session-bench version",
            Advice::Complete => "▶▶ campagne COMPLÈTE (~1 h 30) : session-bench version complete",
        }
    }
}

/// Lignes modifiées d'un diff (sans les en-têtes « +++ » / « --- »).
fn changed_lines(diff: &str) -> impl Iterator<Item = &str> {
    diff.lines()
        .filter(|l| (l.starts_with('+') || l.starts_with('-')) && !l.starts_with("+++") && !l.starts_with("---"))
        .map(|l| l[1..].trim())
}

/// Le diff d'un manifeste ne change-t-il que des numéros de version de NOS
/// paquets ? (Cargo.lock : la ligne `version` suit le `name` du paquet.)
fn version_only(path: &str, diff: &str) -> bool {
    if path.ends_with("Cargo.lock") {
        let mut name = "";
        let mut any = false;
        for l in diff.lines() {
            if l.starts_with("+++") || l.starts_with("---") || l.starts_with("@@") {
                continue;
            }
            let body = l.get(1..).unwrap_or("").trim();
            if let Some(n) = body.strip_prefix("name = \"") {
                name = n.trim_end_matches('"');
            }
            if l.starts_with('+') || l.starts_with('-') {
                any = true;
                if !(body.starts_with("version = ") && name.starts_with("jamodio")) {
                    return false;
                }
            }
        }
        return any;
    }
    let mut lines = changed_lines(diff).peekable();
    lines.peek().is_some() && lines.all(|l| l.starts_with("version = ") || l.starts_with("\"version\":"))
}

/// Domaine d'un fichier modifié (`diff` : son diff, pour les manifestes).
pub fn classify(path: &str, diff: &str) -> Area {
    let p = path;
    if p.starts_with("jamodio-bench/") {
        return Area::Bench;
    }
    if p.ends_with("Cargo.lock") || p.ends_with("Cargo.toml") || p.ends_with("tauri.conf.json") {
        return if version_only(p, diff) {
            Area::VersionOnly
        } else if p.ends_with("tauri.conf.json") {
            Area::Machine
        } else {
            Area::Dependencies
        };
    }
    if p.starts_with(".github/") || p.starts_with("scripts/") || p.ends_with(".md") || p == "deny.toml" || p == "LICENSE" {
        return Area::Tooling;
    }
    if p.contains("/tests/") || p.contains("/examples/") || p.ends_with("scale_tests.rs") {
        return Area::Tests;
    }
    if p.starts_with("jamodio-agent/ui/") || p.starts_with("jamodio-agent/icons/") || p.starts_with("jamodio-agent/capabilities/") {
        return Area::Interface;
    }
    if p.starts_with("jamodio-agent/wix/") || p.ends_with("entitlements.plist") || p.ends_with("info.plist") || p == "jamodio-agent/build.rs" {
        return Area::Machine;
    }
    if p.starts_with("jamodio-agent/src/")
        || p.starts_with("jamodio-audio-core/src/")
        || p.starts_with("vendor/")
        || p.starts_with("jamodio-au-host/")
        || p.starts_with("jamodio-vst3-host/")
    {
        return Area::AudioPath;
    }
    Area::Unknown
}

/// Le conseil, pour des fichiers déjà rangés.
pub fn advise(files: &[(String, Area)], public: bool) -> Advice {
    if public {
        Advice::Complete
    } else if files.iter().any(|(_, a)| a.needs_campaign()) {
        Advice::Rapide
    } else {
        Advice::Aucune
    }
}

/// Le texte du conseil.
pub fn render(from: &str, to: &str, files: &[(String, Area)], public: bool) -> String {
    let advice = advise(files, public);
    let mut s = format!("Changements de l'Audio Engine de {from} à {to} : {} fichier(s).\n\n", files.len());
    let mut areas: Vec<Area> = files.iter().map(|(_, a)| *a).collect();
    areas.sort();
    areas.dedup();
    for area in areas {
        let mark = if area.needs_campaign() { "▶" } else { "○" };
        let _ = writeln!(s, "{mark} {} :", area.label());
        for (f, _) in files.iter().filter(|(_, a)| *a == area) {
            let _ = writeln!(s, "    {f}");
        }
    }
    let _ = writeln!(s, "\nConseil : {}", advice.label());
    if public {
        s.push_str("(version publique : toujours la campagne complète)\n");
    }
    s.push_str("La décision reste la tienne.\n");
    s
}

/// `git diff` entre deux révisions du dépôt de l'Audio Engine.
pub fn changed_files(from: &str, to: &str) -> Result<Vec<(String, Area)>, String> {
    let git = |args: &[&str]| -> Result<String, String> {
        let o = std::process::Command::new("git").args(args).output().map_err(|e| format!("git : {e} (lancer depuis le dépôt de l'Audio Engine)"))?;
        if !o.status.success() {
            return Err(format!("git {} : {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let names = git(&["diff", "--name-only", from, to])?;
    names
        .lines()
        .filter(|l| !l.is_empty())
        .map(|f| Ok((f.to_string(), classify(f, &git(&["diff", from, to, "--", f])?))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chaque_fichier_va_dans_son_domaine() {
        for (path, area) in [
            ("jamodio-agent/src/pipeline.rs", Area::AudioPath),
            ("jamodio-agent/src/audio/asio_host.rs", Area::AudioPath),
            ("jamodio-audio-core/src/mixer/ring_buffer.rs", Area::AudioPath),
            ("vendor/asio-sys/src/lib.rs", Area::AudioPath),
            ("jamodio-vst3-host/src/host.rs", Area::AudioPath),
            ("jamodio-agent/wix/network-throttling.wxs", Area::Machine),
            ("jamodio-agent/ui/index.html", Area::Interface),
            ("jamodio-bench/src/run.rs", Area::Bench),
            (".github/workflows/release.yml", Area::Tooling),
            ("CHANGELOG.md", Area::Tooling),
            ("jamodio-agent/tests/ws.rs", Area::Tests),
            ("jamodio-agent/examples/asio_latency_probe.rs", Area::Tests),
            ("jamodio-agent/src/pipeline/scale_tests.rs", Area::Tests),
            ("rien/de/connu.txt", Area::Unknown),
        ] {
            assert_eq!(classify(path, ""), area, "{path}");
        }
    }

    /// Un manifeste qui ne change que NOTRE numéro de version ne demande rien ;
    /// une dépendance tierce, si.
    #[test]
    fn un_numero_de_version_seul_ne_demande_pas_de_campagne() {
        let bump = "--- a/Cargo.toml\n+++ b/Cargo.toml\n@@ -3 +3 @@\n-version = \"0.6.6-14\"\n+version = \"0.6.6-15\"\n";
        assert_eq!(classify("jamodio-agent/Cargo.toml", bump), Area::VersionOnly);
        let dep = "@@\n-tokio = \"1.40\"\n+tokio = \"1.41\"\n";
        assert_eq!(classify("jamodio-agent/Cargo.toml", dep), Area::Dependencies);
        let lock_ours = " [[package]]\n name = \"jamodio-agent\"\n-version = \"0.6.6-14\"\n+version = \"0.6.6-15\"\n";
        assert_eq!(classify("Cargo.lock", lock_ours), Area::VersionOnly);
        let lock_theirs = " [[package]]\n name = \"opus\"\n-version = \"0.3.0\"\n+version = \"0.3.1\"\n";
        assert_eq!(classify("Cargo.lock", lock_theirs), Area::Dependencies);
        let tauri = "-  \"version\": \"0.6.6-14\",\n+  \"version\": \"0.6.6-15\",\n";
        assert_eq!(classify("jamodio-agent/tauri.conf.json", tauri), Area::VersionOnly);
    }

    #[test]
    fn le_conseil_suit_le_domaine_le_plus_sensible() {
        let f = |v: &[(&str, Area)]| v.iter().map(|(p, a)| (p.to_string(), *a)).collect::<Vec<_>>();
        let ui = f(&[("jamodio-agent/ui/x.js", Area::Interface), ("Cargo.lock", Area::VersionOnly)]);
        assert_eq!(advise(&ui, false), Advice::Aucune);
        assert_eq!(advise(&ui, true), Advice::Complete, "version publique : toujours complète");
        let audio = f(&[("jamodio-agent/ui/x.js", Area::Interface), ("jamodio-agent/src/pipeline.rs", Area::AudioPath)]);
        assert_eq!(advise(&audio, false), Advice::Rapide);
        let text = render("v0.6.6-14", "HEAD", &audio, false);
        assert!(text.contains("▶ chemin audio :\n    jamodio-agent/src/pipeline.rs"), "{text}");
        assert!(text.contains("Conseil : ▶ campagne RAPIDE") && text.contains("La décision reste la tienne"), "{text}");
    }
}
