//! Coordinateur de scan out-of-process — pilote le(s) process worker jetables.
//!
//! Boucle de résilience (PLAN-PLUGIN-SCAN-OOP §3.3) :
//! 1. spawn un worker sur la liste d'items restants ;
//! 2. pompe ses events NDJSON, avec un watchdog par item (silence trop long
//!    → worker figé sur un plugin → on le tue) ;
//! 3. à la mort du worker (exit, crash, EOF, ou kill sur timeout), la
//!    [`Session`] rend son verdict : plugins collectés, item condamné
//!    (le `begin` sans `end`), items restants ;
//! 4. si des items restent ET qu'on a progressé/condamné, on respawn sur le
//!    reste. Une session qui ne condamne personne ET ne progresse pas
//!    (mort de l'infra worker, pas d'un plugin) n'est PAS relancée → pas de
//!    boucle infinie.
//!
//! Isolation process : le worker ne doit jamais survivre à l'agent. Windows →
//! Job Object `KILL_ON_JOB_CLOSE` ([`job`]) ; Unix → `kill()` best-effort au
//! Drop du [`Child`] (le worker sort aussi de lui-même quand son stdin se
//! ferme, cf. worker::run_loop).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use jamodio_audio_core::plugin_host::PluginInfo;

use super::session::{BlockedItem, CloseCause, Session};

/// Silence maximal toléré entre deux events d'un worker. Au-delà, on considère
/// le worker figé sur le plugin courant (hang natif) et on le tue. Large
/// devant le coût d'instanciation d'un plugin lourd (Kontakt, BFD ~4 s) pour
/// ne jamais condamner un plugin lent à tort.
const ITEM_TIMEOUT: Duration = Duration::from_secs(30);

/// Garde-fou absolu : nombre maximum de sessions worker pour un seul scan.
/// En régime normal, 1 session (aucun crash) à N sessions (N-1 plugins
/// fautifs). Cette borne protège d'une pathologie imprévue (worker qui
/// condamnerait sans fin) — dépassement = on arrête et on log.
const MAX_SESSIONS: usize = 256;

/// Résultat complet d'un scan out-of-process.
#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub plugins: Vec<PluginInfo>,
    /// Items condamnés pendant CE scan (crash ou timeout).
    pub blocked: Vec<BlockedItem>,
}

/// Scanne `items` hors-process et rend plugins + blocklist. `worker_cmd`
/// construit une commande fraîche vers `agent --plugin-scan-worker` (injecté
/// pour les tests, qui pointent vers un binaire factice).
pub fn scan_items(items: Vec<String>, worker_cmd: &dyn Fn() -> Command) -> ScanOutcome {
    scan_items_with(items, worker_cmd, ITEM_TIMEOUT)
}

/// Variante avec timeout explicite — les tests l'appellent avec un délai court
/// pour exercer le chemin hang sans attendre 30 s.
fn scan_items_with(
    items: Vec<String>,
    worker_cmd: &dyn Fn() -> Command,
    item_timeout: Duration,
) -> ScanOutcome {
    let mut outcome = ScanOutcome::default();
    let mut remaining = items;
    let mut sessions = 0;

    while !remaining.is_empty() {
        if sessions >= MAX_SESSIONS {
            tracing::error!(
                target: "jamodio::plugin",
                remaining = remaining.len(),
                "scan: {MAX_SESSIONS} sessions worker atteintes — abandon du reste"
            );
            break;
        }
        sessions += 1;

        let end = match run_session(remaining.clone(), worker_cmd, item_timeout) {
            Ok(end) => end,
            Err(e) => {
                tracing::error!(
                    target: "jamodio::plugin",
                    error = %e,
                    "scan: impossible de lancer le worker — scan interrompu"
                );
                break;
            }
        };

        outcome.plugins.extend(end.plugins);
        let condemned = end.blocked.clone();
        if let Some(b) = end.blocked {
            tracing::warn!(
                target: "jamodio::plugin",
                item = %b.item,
                reason = ?b.reason,
                "scan: plugin blocklisté (worker mort en le scannant)"
            );
            outcome.blocked.push(b);
        }

        // Anti-boucle : rien condamné ET aucun item terminé = l'infra worker
        // a échoué (pas un plugin) → relancer ne changerait rien.
        if condemned.is_none() && !end.progressed {
            tracing::error!(
                target: "jamodio::plugin",
                remaining = end.remaining.len(),
                "scan: worker mort sans progrès ni coupable — reste non scanné"
            );
            break;
        }

        remaining = end.remaining;
    }

    outcome
}

/// Une session = un process worker de sa naissance à sa mort. Rend le verdict
/// de la [`Session`]. `Err` uniquement si le worker ne démarre pas (spawn KO).
fn run_session(
    items: Vec<String>,
    worker_cmd: &dyn Fn() -> Command,
    item_timeout: Duration,
) -> std::io::Result<super::session::SessionEnd> {
    let mut cmd = worker_cmd();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Guard::spawn(cmd)?;

    // Isolation : ne jamais laisser le worker survivre à l'agent.
    #[cfg(target_os = "windows")]
    job::assign_current(child.inner());

    // stderr → log agent (thread dédié, se termine à l'EOF du pipe).
    if let Some(stderr) = child.inner().stderr.take() {
        std::thread::Builder::new()
            .name("scan-worker-stderr".into())
            .spawn(move || relay_stderr(stderr))
            .ok();
    }

    // Alimente stdin depuis un thread : écrire toute la liste d'un coup
    // pourrait bloquer si le worker ne lit pas assez vite (pipe plein) et on
    // veut pomper stdout en parallèle. Le thread se termine en fermant stdin
    // (EOF → le worker sort proprement une fois la liste épuisée).
    let mut stdin = child.inner().stdin.take().expect("stdin piped");
    let items_for_writer = items.clone();
    std::thread::Builder::new()
        .name("scan-worker-stdin".into())
        .spawn(move || {
            for item in items_for_writer {
                if writeln!(stdin, "{item}").is_err() {
                    break; // worker mort — rien de plus à envoyer
                }
            }
            // Drop de stdin = EOF côté worker.
        })
        .ok();

    // Pompe stdout ligne par ligne sur un thread → channel, pour appliquer un
    // timeout par event (recv_timeout). Lire directement bloquerait sans
    // borne si le worker fige sur un plugin.
    let stdout = child.inner().stdout.take().expect("stdout piped");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("scan-worker-stdout".into())
        .spawn(move || pump_stdout(stdout, tx))
        .ok();

    let mut session = Session::new(items);
    let cause = loop {
        match rx.recv_timeout(item_timeout) {
            Ok(line) => match serde_json::from_str(&line) {
                Ok(event) => session.on_event(event),
                Err(e) => tracing::warn!(
                    target: "jamodio::plugin",
                    error = %e,
                    line = %line,
                    "scan: ligne worker illisible — ignorée"
                ),
            },
            // Pipe fermé : le worker est mort (exit propre ou crash). On
            // distingue via le code de sortie plus bas.
            Err(RecvTimeoutError::Disconnected) => break CloseCause::Exited,
            // Silence trop long : worker figé sur le plugin courant.
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    target: "jamodio::plugin",
                    timeout_s = item_timeout.as_secs(),
                    "scan: worker silencieux — kill (plugin figé)"
                );
                child.kill();
                break CloseCause::TimedOut;
            }
        }
    };

    // Attend la sortie effective pour logger le code (diagnostic crash vs exit
    // propre) et reaper le process. Le Drop du Guard tue de toute façon.
    let status = child.wait();
    tracing::info!(
        target: "jamodio::plugin",
        ?cause,
        exit = ?status.ok().and_then(|s| s.code()),
        "scan: session worker terminée"
    );

    Ok(session.close(cause))
}

/// Lit stdout ligne par ligne et pousse chaque ligne dans le channel. Se
/// termine à l'EOF (worker mort) ou si le receiver est lâché.
fn pump_stdout(stdout: ChildStdout, tx: mpsc::Sender<String>) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        match line {
            Ok(l) => {
                if tx.send(l).is_err() {
                    break; // coordinateur a lâché (kill) — inutile de continuer
                }
            }
            Err(_) => break, // pipe cassé
        }
    }
}

/// Relaie le stderr du worker dans le log agent, sous un target dédié pour
/// distinguer les lignes du worker de celles de l'agent.
fn relay_stderr(stderr: std::process::ChildStderr) {
    let reader = BufReader::new(stderr);
    for line in reader.lines().map_while(Result::ok) {
        if !line.trim().is_empty() {
            tracing::debug!(target: "jamodio::scan-worker", "{line}");
        }
    }
}

/// Enveloppe RAII autour du `Child` : garantit que le process est tué si le
/// coordinateur sort par un chemin inattendu (panic, `?`). Le worker jetable
/// ne doit jamais fuiter.
struct Guard {
    child: Option<Child>,
}

impl Guard {
    fn spawn(mut cmd: Command) -> std::io::Result<Self> {
        let child = cmd.spawn()?;
        Ok(Self { child: Some(child) })
    }
    fn inner(&mut self) -> &mut Child {
        self.child.as_mut().expect("child vivant")
    }
    fn kill(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
        }
    }
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.inner().wait()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            // try_wait pour ne pas bloquer si déjà mort ; sinon kill best-effort.
            if matches!(c.try_wait(), Ok(None)) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// Job Object Windows : lie le worker à un job `KILL_ON_JOB_CLOSE`. Si l'agent
/// meurt (crash, kill), le handle du job se ferme → l'OS tue le worker. Filet
/// dur en plus du Drop du Guard (qui ne couvre pas un kill -9 de l'agent).
#[cfg(target_os = "windows")]
mod job {
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;

    /// Un seul job pour tout le process agent : tous les workers y sont
    /// assignés. Fermé implicitement à la mort du process → workers tués.
    static JOB: OnceLock<usize> = OnceLock::new();

    fn job_handle() -> HANDLE {
        *JOB.get_or_init(|| {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    tracing::error!(target: "jamodio::plugin", "CreateJobObject a échoué — worker non isolé");
                    return 0usize;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    tracing::error!(target: "jamodio::plugin", "SetInformationJobObject a échoué — worker non isolé");
                    CloseHandle(job);
                    return 0usize;
                }
                // Assigner l'agent lui-même au job serait fatal (il se tuerait
                // en fermant le job) — on n'assigne QUE les workers.
                let _ = GetCurrentProcess;
                job as usize
            }
        }) as HANDLE
    }

    pub fn assign_current(child: &Child) {
        let job = job_handle();
        if job as usize == 0 {
            return; // création du job KO — déjà loggé, on continue sans isolation
        }
        let handle = child.as_raw_handle() as HANDLE;
        let ok = unsafe { AssignProcessToJobObject(job, handle) };
        if ok == 0 {
            tracing::warn!(target: "jamodio::plugin", "AssignProcessToJobObject a échoué — worker non isolé (Drop reste le filet)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::session::BlockReason;

    /// Timeout court pour le chemin hang — assez large devant le coût de spawn
    /// d'un process pour ne pas condamner à tort un item lent à démarrer.
    const TEST_TIMEOUT: Duration = Duration::from_millis(800);

    /// Chemin du binaire mock-worker (bin dédié `mock_scan_worker`, cf.
    /// src/bin/mock_scan_worker.rs). `CARGO_BIN_EXE_*` n'est PAS défini pour
    /// les tests unitaires d'un crate bin (seulement pour tests/), donc on le
    /// résout depuis l'exécutable de test : `target/<profil>/deps/test-bin`
    /// → le sibling `target/<profil>/mock_scan_worker`.
    fn mock_worker_path() -> std::path::PathBuf {
        let test_exe = std::env::current_exe().expect("current_exe");
        let profile_dir = test_exe
            .parent()
            .and_then(|deps| deps.parent()) // deps/ → <profil>/
            .expect("target/<profil>");
        let name = if cfg!(windows) { "mock_scan_worker.exe" } else { "mock_scan_worker" };
        profile_dir.join(name)
    }

    fn mock_cmd(scenario: &'static str) -> impl Fn() -> Command {
        let path = mock_worker_path();
        move || {
            let mut c = Command::new(&path);
            c.env("JMO_MOCK_SCENARIO", scenario);
            c
        }
    }

    #[test]
    fn all_clean_collects_every_plugin() {
        let out = scan_items_with(
            vec!["a".into(), "b".into(), "c".into()],
            &mock_cmd("clean"),
            TEST_TIMEOUT,
        );
        // clean : 1 plugin par item, aucun blocage.
        assert_eq!(out.plugins.len(), 3);
        assert!(out.blocked.is_empty());
    }

    #[test]
    fn crash_on_item_blocklists_and_rescans_rest() {
        // Le mock crashe (exit ≠ 0) en scannant "b" ; a et c survivent, b
        // finit blocklisté reason=crash. C'est le scénario du rapport terrain.
        let out = scan_items_with(
            vec!["a".into(), "b".into(), "c".into()],
            &mock_cmd("crash-on-b"),
            TEST_TIMEOUT,
        );
        let names: Vec<&str> = out.plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"plug-a"), "a manquant: {names:?}");
        assert!(names.contains(&"plug-c"), "c manquant (rescan post-crash): {names:?}");
        assert!(!names.contains(&"plug-b"));
        assert_eq!(out.blocked.len(), 1);
        assert_eq!(out.blocked[0].item, "b");
        assert_eq!(out.blocked[0].reason, BlockReason::Crash);
    }

    #[test]
    fn hang_on_item_times_out_and_blocklists() {
        // Le mock se fige sur "b" (sleep long) → timeout → kill → blocklist.
        let out = scan_items_with(
            vec!["a".into(), "b".into()],
            &mock_cmd("hang-on-b"),
            TEST_TIMEOUT,
        );
        let names: Vec<&str> = out.plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"plug-a"), "a doit survivre au hang de b");
        assert_eq!(out.blocked.len(), 1);
        assert_eq!(out.blocked[0].item, "b");
        assert_eq!(out.blocked[0].reason, BlockReason::Timeout);
    }

    #[test]
    fn spawn_failure_is_handled_gracefully() {
        // Binaire inexistant → aucun panic, résultat vide.
        let out = scan_items_with(
            vec!["a".into()],
            &|| Command::new("/nonexistent/jmo-worker-xyz"),
            TEST_TIMEOUT,
        );
        assert!(out.plugins.is_empty());
        assert!(out.blocked.is_empty());
    }
}
