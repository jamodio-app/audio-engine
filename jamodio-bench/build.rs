//! Grave dans le binaire le commit du banc d'où il est compilé : un binaire
//! copié sur une autre machine (l'émetteur distant) dit de quelle version il
//! est, et chaque campagne note avec quel banc elle a été mesurée.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let o = Command::new("git").args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn main() {
    // Le dernier commit qui touche le BANC (ce dossier) : un commit ailleurs
    // dans le dépôt (Audio Engine, CI) ne change pas la version du banc, et ne
    // doit pas faire croire qu'un émetteur à jour est périmé.
    let commit = git(&["log", "-1", "--format=%h", "--", "."])
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "inconnu".into());
    // Des changements non commités dans le banc : le commit seul mentirait.
    let dirty = git(&["status", "--porcelain", "--", "."]).is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=BENCH_COMMIT={commit}{}", if dirty { "+modifié" } else { "" });
    // Recompiler quand le commit change (HEAD ou la branche) ou quand le banc change.
    // Dans un worktree, HEAD est dans son dossier propre, les branches dans le
    // dossier commun.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
    }
    if let Some(common) = git(&["rev-parse", "--git-common-dir"]) {
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={common}/{r}");
        }
        println!("cargo:rerun-if-changed={common}/packed-refs");
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
}
