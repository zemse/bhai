//! Dictation into the prompt box: ctrl+space starts a recorder command writing a WAV
//! file, ctrl+space again stops it with SIGINT, and a transcriber command turns the file
//! into the text that lands at the cursor. Both are the user's own programs, set in
//! `[dictation]` of the global config; the defaults are sox and whisper.cpp's
//! `whisper-cli`, so speech never leaves the machine and nothing is downloaded.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;

/// `[dictation]`. `{wav}` in either command is the recording's path, `{model}` is
/// `model` with a leading `~/` taken from `HOME`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub record: Vec<String>,
    pub transcribe: Vec<String>,
    pub model: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        let words = |s: &str| s.split(' ').map(String::from).collect();
        Self {
            // 16 kHz mono 16-bit is what whisper.cpp reads without resampling.
            record: words("sox -q -d -r 16000 -c 1 -b 16 {wav}"),
            transcribe: words("whisper-cli -m {model} -f {wav} -nt -np"),
            model: None,
        }
    }
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Named once for the process, from the global file.
pub fn set(settings: Option<Settings>) {
    if let Some(settings) = settings {
        let _ = SETTINGS.set(settings);
    }
}

/// A dictation on what the config set, or `None` when it has no `[dictation]`.
pub fn configured() -> Option<Dictation> {
    SETTINGS.get().cloned().map(Dictation::new)
}

/// In a recording's directory, which only the user can enter.
const WAV: &str = "speech.wav";
const LOG: &str = "recorder.log";

/// Every recording's directory until it is deleted, with the process group working on it
/// (the recorder, then the transcriber) while one runs unreaped. The group is its own, so
/// a signal that kills bhai never reaches it.
static RECORDINGS: Mutex<BTreeMap<PathBuf, Option<u32>>> = Mutex::new(BTreeMap::new());

fn recordings() -> std::sync::MutexGuard<'static, BTreeMap<PathBuf, Option<u32>>> {
    RECORDINGS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Kill whatever works on every recording and delete it, when bhai dies of a signal or a
/// panic.
pub fn kill_all() {
    for (dir, group) in std::mem::take(&mut *recordings()) {
        if let Some(group) = group {
            interrupt(group, libc::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Kill whatever works on `dir` and delete it.
fn discard(dir: &Path) {
    if let Some(Some(group)) = recordings().remove(dir) {
        interrupt(group, libc::SIGKILL);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Delete `dir` once nothing works on it.
fn release(dir: &Path) {
    recordings().remove(dir);
    let _ = std::fs::remove_dir_all(dir);
}

/// Name the group now working on `dir`; false when it was discarded meanwhile.
fn claim(dir: &Path, group: Option<u32>) -> bool {
    match recordings().get_mut(dir) {
        Some(slot) => {
            *slot = group;
            true
        }
        None => false,
    }
}

/// Where a recording or its transcript goes once the transcriber is done.
type Heard = Arc<Mutex<Option<Result<String, String>>>>;

enum State {
    Idle,
    Recording { child: Child, dir: PathBuf },
    Transcribing { heard: Heard, dir: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Recording,
    Transcribing,
}

pub struct Dictation {
    settings: Settings,
    state: State,
}

impl Dictation {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            state: State::Idle,
        }
    }

    pub fn status(&self) -> Status {
        match self.state {
            State::Idle => Status::Idle,
            State::Recording { .. } => Status::Recording,
            State::Transcribing { .. } => Status::Transcribing,
        }
    }

    /// Start recording when idle, or stop and start transcribing when recording.
    /// Transcribing already, it does nothing.
    pub fn toggle(&mut self) -> Result<(), String> {
        match self.state {
            State::Idle => self.start(),
            State::Recording { .. } => {
                self.stop(true);
                Ok(())
            }
            State::Transcribing { .. } => Ok(()),
        }
    }

    fn start(&mut self) -> Result<(), String> {
        let dir = std::env::temp_dir().join(format!("bhai-dictation-{}", uuid::Uuid::new_v4()));
        let wav = dir.join(WAV);
        let model = self.settings.model.as_deref();
        // The transcriber's placeholders are checked now, not after the user has spoken.
        fill(&self.settings.transcribe, &wav, model)?;
        let argv = fill(&self.settings.record, &wav, model)?;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        recordings().insert(dir.clone(), None);
        let spawned = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(dir.join(LOG))
            .map_err(|e| format!("{}: {e}", dir.join(LOG).display()))
            .and_then(|stderr| {
                command(&argv)
                    .stdout(Stdio::null())
                    .stderr(stderr)
                    .process_group(0)
                    .spawn()
                    .map_err(|e| format!("could not start {}: {e}", argv[0]))
            });
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                release(&dir);
                return Err(e);
            }
        };
        if !claim(&dir, Some(child.id())) {
            // A signal took the directory while the recorder started.
            interrupt(child.id(), libc::SIGKILL);
            let _ = child.wait();
            return Err("dictation was stopped".to_string());
        }
        self.state = State::Recording { child, dir };
        Ok(())
    }

    /// End the recording; with `keep`, transcribe what it caught.
    fn stop(&mut self, keep: bool) {
        // Transcribing, the state holds the only way to the transcript.
        if !matches!(self.state, State::Recording { .. }) {
            return;
        }
        let State::Recording { mut child, dir } = std::mem::replace(&mut self.state, State::Idle)
        else {
            unreachable!()
        };
        let running = matches!(child.try_wait(), Ok(None));
        if running {
            // The group, so a recorder wrapped in a shell stops too. sox writes the WAV
            // header's lengths on SIGINT.
            interrupt(child.id(), if keep { libc::SIGINT } else { libc::SIGKILL });
        }
        if !keep {
            let _ = child.wait();
            release(&dir);
            return;
        }
        let heard: Heard = Arc::default();
        self.state = State::Transcribing {
            heard: Arc::clone(&heard),
            dir: dir.clone(),
        };
        let settings = self.settings.clone();
        std::thread::spawn(move || {
            let result = finish(child, running, &dir, &settings);
            release(&dir);
            *heard.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        });
    }

    /// Drop a recording without transcribing it. Returns whether there was one.
    pub fn cancel(&mut self) -> bool {
        let recording = self.status() == Status::Recording;
        self.stop(false);
        recording
    }

    /// The transcript once there is one. A recorder that ends by itself, as one that
    /// stops on silence does, is transcribed as if it had been stopped.
    pub fn poll(&mut self) -> Option<Result<String, String>> {
        match &mut self.state {
            State::Recording { child, .. } => {
                let ended = !matches!(child.try_wait(), Ok(None));
                if ended {
                    self.stop(true);
                }
                None
            }
            State::Transcribing { heard, .. } => {
                let done = heard.lock().unwrap_or_else(|e| e.into_inner()).take();
                if done.is_some() {
                    self.state = State::Idle;
                }
                done
            }
            State::Idle => None,
        }
    }
}

impl Drop for Dictation {
    /// The transcriber's thread would outlive a quitting bhai no more than it does, so the
    /// transcriber is killed and the recording deleted here.
    fn drop(&mut self) {
        match &self.state {
            State::Recording { .. } => self.stop(false),
            State::Transcribing { dir, .. } => discard(dir),
            State::Idle => {}
        }
    }
}

/// Wait out the recorder, then run the transcriber on what it wrote.
fn finish(
    mut child: Child,
    stopped: bool,
    dir: &Path,
    settings: &Settings,
) -> Result<String, String> {
    let (wav, log) = (dir.join(WAV), dir.join(LOG));
    let status = child.wait();
    // Reaped, the group's id may be reused, so it is no longer killed.
    let kept = claim(dir, None);
    let status = status.map_err(|e| e.to_string())?;
    if !kept {
        return Err("dictation was stopped".to_string());
    }
    // A recorder that was stopped may well report the signal; one that quit by itself
    // with a failure has its reason in the log.
    if !stopped && !status.success() {
        let said = std::fs::read_to_string(&log).unwrap_or_default();
        return Err(last_line(&said).unwrap_or(&status.to_string()).to_string());
    }
    // A WAV header alone is 44 bytes.
    if std::fs::metadata(&wav).map_or(0, |m| m.len()) <= 44 {
        return Err("the recorder wrote no audio".to_string());
    }
    let argv = fill(&settings.transcribe, &wav, settings.model.as_deref())?;
    let transcriber = command(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", argv[0]))?;
    if !claim(dir, Some(transcriber.id())) {
        interrupt(transcriber.id(), libc::SIGKILL);
    }
    let out = transcriber.wait_with_output();
    let kept = claim(dir, None);
    let out = out.map_err(|e| e.to_string())?;
    if !kept {
        return Err("dictation was stopped".to_string());
    }
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        return Err(last_line(&said)
            .unwrap_or(&out.status.to_string())
            .to_string());
    }
    let text = clean(&String::from_utf8_lossy(&out.stdout));
    match text.is_empty() {
        true => Err("heard nothing".to_string()),
        false => Ok(text),
    }
}

fn command(argv: &[String]) -> Command {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    for name in crate::childenv::withheld() {
        command.env_remove(name);
    }
    command
}

/// `argv` with its placeholders filled in.
fn fill(argv: &[String], wav: &Path, model: Option<&str>) -> Result<Vec<String>, String> {
    if argv.is_empty() {
        return Err("[dictation] has an empty command".to_string());
    }
    let wants_model = argv.iter().any(|a| a.contains("{model}"));
    let model = match model {
        Some(model) => expand(model),
        None if wants_model => {
            return Err("[dictation] needs `model`, the path of a whisper model".to_string());
        }
        None => String::new(),
    };
    let wav = wav.to_string_lossy();
    Ok(argv
        .iter()
        .map(|a| a.replace("{wav}", &wav).replace("{model}", &model))
        .collect())
}

fn expand(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

/// The transcript as one line: whisper prints a line per segment, indented, and marks
/// silence and noise with bracketed tags like `[BLANK_AUDIO]` that are not speech.
fn clean(out: &str) -> String {
    out.split_whitespace()
        .filter(|word| {
            !((word.starts_with('[') && word.ends_with(']'))
                || (word.starts_with('(') && word.ends_with(')')))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().rev().map(str::trim).find(|l| !l.is_empty())
}

#[cfg(test)]
impl Dictation {
    /// Block until the recorder has written `bytes`, and return where.
    pub fn wait_for_audio(&self, bytes: u64) -> PathBuf {
        let State::Recording { dir, .. } = &self.state else {
            panic!("not recording");
        };
        let wav = &dir.join(WAV);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::fs::metadata(wav).map_or(0, |m| m.len()) < bytes {
            assert!(
                std::time::Instant::now() < deadline,
                "the recorder wrote nothing"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        wav.clone()
    }

    /// The recorder's process group.
    pub fn recorder(&self) -> u32 {
        let State::Recording { child, .. } = &self.state else {
            panic!("not recording");
        };
        child.id()
    }
}

#[allow(unsafe_code)]
fn interrupt(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: `killpg` only sends a signal; the group is the recorder's or the
        // transcriber's own.
        unsafe {
            libc::killpg(pid, signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Vec<String> {
        ["sh", "-c", script, "sh", "{wav}"]
            .map(String::from)
            .to_vec()
    }

    /// A recorder that writes a fake WAV and then waits to be stopped.
    const RECORD: &str = "head -c 100 /dev/zero > \"$1\"; exec sleep 30";

    fn settle(dictation: &mut Dictation) -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(done) = dictation.poll() {
                return done;
            }
            assert!(Instant::now() < deadline, "no transcript");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_stopped_recording_is_transcribed_into_one_line_and_its_files_go() {
        let mut dictation = Dictation::new(Settings {
            record: sh(RECORD),
            // The model placeholder arrives filled, and the tags whisper adds are dropped.
            transcribe: [
                sh("printf ' fix the [BLANK_AUDIO] lexer\\n using %s\\n' \"$2\""),
                vec!["{model}".to_string()],
            ]
            .concat(),
            model: Some("tiny.bin".to_string()),
        });
        dictation.toggle().unwrap();
        assert_eq!(dictation.status(), Status::Recording);
        let wav = dictation.wait_for_audio(100);
        dictation.toggle().unwrap();
        assert_eq!(dictation.status(), Status::Transcribing);
        assert_eq!(
            settle(&mut dictation).as_deref(),
            Ok("fix the lexer using tiny.bin")
        );
        assert_eq!(dictation.status(), Status::Idle);
        assert!(!wav.parent().unwrap().exists());
    }

    #[test]
    fn only_the_user_can_read_a_recording() {
        use std::os::unix::fs::PermissionsExt as _;
        let mut dictation = Dictation::new(Settings {
            record: sh(RECORD),
            transcribe: sh("true"),
            model: None,
        });
        dictation.toggle().unwrap();
        let wav = dictation.wait_for_audio(100);
        let dir = wav.parent().unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir), 0o700);
        assert_eq!(mode(&dir.join(LOG)), 0o600);
        assert!(dictation.cancel());
        assert!(!dir.exists());
    }

    /// A transcriber that says its process group in the recording's directory, then
    /// takes its time.
    const SLOW: &str = "echo $$ > \"$(dirname \"$1\")/started\"; exec sleep 30";

    /// Block until `SLOW` has started for the recording at `wav`, and return its group.
    fn transcriber(wav: &Path) -> i32 {
        let started = wav.parent().unwrap().join("started");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let said = std::fs::read_to_string(&started).unwrap_or_default();
            if let Ok(group) = said.trim().parse() {
                return group;
            }
            assert!(Instant::now() < deadline, "no transcriber");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn alive(group: i32) -> bool {
        #[allow(unsafe_code)]
        // SAFETY: signal 0 only asks whether the group exists.
        let sent = unsafe { libc::killpg(group, 0) };
        sent == 0
    }

    /// Wait out a killed group's reaping, which whoever holds it does in a moment.
    fn gone(group: i32) -> bool {
        let killed = Instant::now();
        while alive(group) && killed.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        !alive(group)
    }

    #[test]
    fn quitting_while_transcribing_kills_the_transcriber_and_deletes_the_recording() {
        let mut dictation = Dictation::new(Settings {
            record: sh(RECORD),
            transcribe: sh(SLOW),
            model: None,
        });
        dictation.toggle().unwrap();
        let wav = dictation.wait_for_audio(100);
        dictation.toggle().unwrap();
        let group = transcriber(&wav);
        assert!(wav.exists());
        drop(dictation);
        assert!(
            !wav.parent().unwrap().exists(),
            "the recording outlived bhai"
        );
        assert!(gone(group), "transcriber {group} outlived bhai");
    }

    #[test]
    fn a_recorder_that_stops_by_itself_is_transcribed() {
        let mut dictation = Dictation::new(Settings {
            record: sh("head -c 100 /dev/zero > \"$1\""),
            transcribe: sh("echo said it"),
            model: None,
        });
        dictation.toggle().unwrap();
        assert_eq!(settle(&mut dictation).as_deref(), Ok("said it"));
    }

    #[test]
    fn failures_say_why() {
        let missing = |record: Vec<String>, transcribe: Vec<String>, model: Option<&str>| {
            let mut dictation = Dictation::new(Settings {
                record,
                transcribe,
                model: model.map(String::from),
            });
            match dictation.toggle() {
                Err(e) => e,
                Ok(()) => settle(&mut dictation).unwrap_err(),
            }
        };
        // The default transcriber needs a model, and says so before recording.
        let defaults = Settings::default();
        let e = missing(sh(RECORD), defaults.transcribe, None);
        assert!(e.contains("needs `model`"), "{e}");
        let e = missing(vec!["/nonexistent/rec".to_string()], sh("true"), None);
        assert!(e.contains("could not start /nonexistent/rec"), "{e}");
        let e = missing(sh("echo no mic >&2; exit 1"), sh("true"), None);
        assert_eq!(e, "no mic");
        let e = missing(sh(": > \"$1\""), sh("true"), None);
        assert_eq!(e, "the recorder wrote no audio");
        let record = sh("head -c 100 /dev/zero > \"$1\"");
        let e = missing(record.clone(), sh("echo bad model >&2; exit 2"), None);
        assert_eq!(e, "bad model");
        let e = missing(record, sh("echo '[BLANK_AUDIO]'"), None);
        assert_eq!(e, "heard nothing");
    }

    #[test]
    fn a_cancelled_recording_is_not_transcribed() {
        let mut dictation = Dictation::new(Settings {
            record: sh(RECORD),
            transcribe: sh("echo should not run"),
            model: None,
        });
        assert!(!dictation.cancel());
        dictation.toggle().unwrap();
        let wav = dictation.wait_for_audio(100);
        assert!(dictation.cancel());
        assert_eq!(dictation.status(), Status::Idle);
        assert_eq!(dictation.poll(), None);
        assert!(!wav.parent().unwrap().exists());
    }

    #[test]
    fn esc_while_transcribing_keeps_the_transcript() {
        let mut dictation = Dictation::new(Settings {
            record: sh(RECORD),
            transcribe: sh("echo kept"),
            model: None,
        });
        dictation.toggle().unwrap();
        dictation.wait_for_audio(100);
        dictation.toggle().unwrap();
        assert!(!dictation.cancel());
        assert_eq!(dictation.status(), Status::Transcribing);
        assert_eq!(settle(&mut dictation).as_deref(), Ok("kept"));
    }

    /// Set in the copy of the test binary that `a_signal_ends_the_recorder_with_bhai`
    /// starts, to the file it writes the process group working on its recording to.
    const SIGNALLED: &str = "BHAI_TEST_DICTATION_SIGNALLED";
    /// Set alongside it when the signal is to come while transcribing.
    const TRANSCRIBING: &str = "BHAI_TEST_DICTATION_TRANSCRIBING";

    /// The test runs itself again as the process to signal, since the signal kills it.
    #[test]
    fn a_signal_ends_the_recorder_with_bhai() {
        if let Some(file) = std::env::var_os(SIGNALLED) {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                crate::tools::bash::kill_all_on_signal().unwrap();
                let mut dictation = Dictation::new(Settings {
                    record: sh("head -c 100 /dev/zero > \"$1\"; exec sleep 600"),
                    transcribe: sh(SLOW),
                    model: None,
                });
                dictation.toggle().unwrap();
                let wav = dictation.wait_for_audio(100);
                let group = match std::env::var_os(TRANSCRIBING) {
                    Some(_) => {
                        dictation.toggle().unwrap();
                        transcriber(&wav)
                    }
                    None => i32::try_from(dictation.recorder()).unwrap(),
                };
                std::fs::write(&file, format!("{group} {}", wav.display())).unwrap();
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
            return;
        }
        signalled(false);
        signalled(true);
    }

    fn signalled(transcribing: bool) {
        let file = std::env::temp_dir().join(format!("bhai-signal-{}", uuid::Uuid::new_v4()));
        let mut command = Command::new(std::env::current_exe().unwrap());
        if transcribing {
            command.env(TRANSCRIBING, "1");
        }
        let mut child = command
            .args(["a_signal_ends_the_recorder_with_bhai", "--nocapture"])
            .env(SIGNALLED, &file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let begun = Instant::now();
        let (group, wav) = loop {
            let said = std::fs::read_to_string(&file).unwrap_or_default();
            if let Some((group, wav)) = said.split_once(' ')
                && let Ok(group) = group.parse::<i32>()
            {
                break (group, PathBuf::from(wav));
            }
            assert!(begun.elapsed() < Duration::from_secs(20), "no recorder");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(alive(group));
        assert!(wav.exists());
        let pid = i32::try_from(child.id()).unwrap();
        #[allow(unsafe_code)]
        // SAFETY: only sends a signal, to the child started above.
        unsafe {
            libc::kill(pid, libc::SIGHUP);
        }
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGHUP));
        let _ = std::fs::remove_file(&file);
        assert!(
            gone(group),
            "{group} outlived bhai, transcribing: {transcribing}"
        );
        assert!(
            !wav.parent().unwrap().exists(),
            "the recording outlived bhai"
        );
    }

    #[test]
    fn the_model_path_takes_home_and_whisper_tags_are_not_words() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand("~/m.bin"), format!("{home}/m.bin"));
        assert_eq!(expand("/m.bin"), "/m.bin");
        assert_eq!(clean("  hi (wind)\n there [MUSIC]\n"), "hi there");
    }
}
