//! What the main screen says of a newer version, and when the tray says it. Decisions only, with no window in them, so that a test can ask them: `ui.rs` draws what they answer.

/// How far the update to a newer release has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage<'a> {
    /// Nothing of its setup is here or on its way.
    Nothing,
    /// Its setup is being fetched, `percent` of the way.
    Fetching { version: &'a str, percent: u64 },
    /// Its setup is here and checked.
    Ready { version: &'a str },
}

/// What is known when the notice is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Situation<'a> {
    /// The newer release that the app's own look found and the person has not sent away with Later.
    pub offered: Option<&'a str>,
    pub stage: Stage<'a>,
    /// The setup that is here can be run: this copy is one the installer put where it is.
    pub setup_runs: bool,
    /// The version whose notice was sent away with Later.
    pub dismissed: Option<&'a str>,
    /// Later was pressed on the setup that is here: it is offered again at the next start.
    pub ready_closed: bool,
    /// A new build has come in over this one, which has no version to name.
    pub new_build: bool,
    /// The person said yes while work was going on: it restarts when the work is done.
    pub restart_when_done: bool,
    /// The setup that waits is being read again before it is run.
    pub checking: bool,
}

/// What the notice says and offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A newer release is out and nothing of it is fetched (a copy that is no install, a connection that is metered): its button opens the release's page.
    Page { version: String },
    /// Its setup is on its way: the button says how far, and cannot be pressed.
    Fetching { version: String, percent: u64 },
    /// Its setup is here (`version`), or a new build is (none): one press updates, and Dogmind restarts. With work under way the press waits for the work (`waiting`), and is pressed
    /// again to take that back; while the setup is read before it is run, the button says so (`checking`).
    UpdateNow { version: Option<String>, waiting: bool, checking: bool },
}

/// The notice of a newer version, if there is one to give. Later, on a version, keeps it from being said again unprompted.
pub fn notice(now: &Situation) -> Option<Notice> {
    let ready = match now.stage {
        Stage::Ready { version } => Some(version),
        _ => None,
    };
    let sent_away = |version: &str| now.dismissed.is_some_and(|dismissed| !crate::updates::is_to_be_told(version, Some(dismissed)));
    // (work is waiting for the press: said whatever else was)
    if now.restart_when_done {
        return Some(Notice::UpdateNow { version: ready.map(str::to_string), waiting: true, checking: now.checking });
    }
    if let Some(version) = ready.filter(|version| now.setup_runs && !now.ready_closed && !sent_away(version)) {
        return Some(Notice::UpdateNow { version: Some(version.to_string()), waiting: false, checking: now.checking });
    }
    if now.new_build {
        return Some(Notice::UpdateNow { version: None, waiting: false, checking: now.checking });
    }
    let offered = now.offered?;
    match now.stage {
        Stage::Fetching { version, percent } if version == offered => Some(Notice::Fetching { version: offered.to_string(), percent }),
        // (the setup of this release is here, and sent away: it is not told of twice)
        Stage::Ready { version } if version == offered => None,
        _ => Some(Notice::Page { version: offered.to_string() }),
    }
}

/// Whether the main screen has the notice on a page: on the conversation and on the changes, which are what the window shows when no dialog is over it; not on the first screens,
/// where a new install has just fetched the newest there is. A dialog stands over the page that has it.
pub fn is_on_page(chat: bool, changes: bool, first_screens: bool) -> bool {
    (chat || changes) && !first_screens
}

/// Whether the tray tells of a version: once, and only while the window is hidden. A version that was seen on the main screen, or sent away, has been told.
pub fn balloon_due(version: &str, window_hidden: bool, told: Option<&str>, dismissed: Option<&str>) -> bool {
    window_hidden && crate::updates::is_to_be_told(version, told) && crate::updates::is_to_be_told(version, dismissed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nothing<'a>() -> Situation<'a> {
        Situation { offered: None, stage: Stage::Nothing, setup_runs: true, dismissed: None, ready_closed: false, new_build: false, restart_when_done: false, checking: false }
    }

    #[test]
    fn nothing_is_said_until_a_release_is_found() {
        assert_eq!(notice(&nothing()), None);
        // (a look the person turned off since, or sent away, leaves no offer)
        assert_eq!(notice(&Situation { dismissed: Some("0.2.0"), ..nothing() }), None);
    }

    #[test]
    fn a_release_that_is_found_is_told_with_its_page_until_its_setup_is_on_its_way() {
        let found = Situation { offered: Some("0.2.0"), ..nothing() };
        assert_eq!(notice(&found), Some(Notice::Page { version: "0.2.0".into() }));
        let fetching = Situation { stage: Stage::Fetching { version: "0.2.0", percent: 37 }, ..found };
        assert_eq!(notice(&fetching), Some(Notice::Fetching { version: "0.2.0".into(), percent: 37 }));
        // (the percent is all that the notice follows)
        let later = Situation { stage: Stage::Fetching { version: "0.2.0", percent: 38 }, ..found };
        assert_ne!(notice(&fetching), notice(&later));
        // (a fetch of another release is not this one's)
        let other = Situation { stage: Stage::Fetching { version: "0.1.9", percent: 5 }, ..found };
        assert_eq!(notice(&other), Some(Notice::Page { version: "0.2.0".into() }));
    }

    #[test]
    fn with_the_setup_here_one_press_updates() {
        let ready = Situation { offered: Some("0.2.0"), stage: Stage::Ready { version: "0.2.0" }, ..nothing() };
        assert_eq!(notice(&ready), Some(Notice::UpdateNow { version: Some("0.2.0".into()), waiting: false, checking: false }));
        // (found again at a start: nothing was looked for, and the setup that was kept is told of)
        let kept = Situation { offered: None, ..ready };
        assert_eq!(notice(&kept), Some(Notice::UpdateNow { version: Some("0.2.0".into()), waiting: false, checking: false }));
        assert_eq!(notice(&Situation { checking: true, ..ready }), Some(Notice::UpdateNow { version: Some("0.2.0".into()), waiting: false, checking: true }));
        // (a setup that cannot be run from here is not offered to be)
        assert_eq!(notice(&Situation { setup_runs: false, ..kept }), None);
        assert_eq!(notice(&Situation { setup_runs: false, ..ready }), None, "and its release is not told of as new either");
    }

    #[test]
    fn work_under_way_is_waited_for_and_the_notice_says_so() {
        let ready = Situation { offered: Some("0.2.0"), stage: Stage::Ready { version: "0.2.0" }, restart_when_done: true, ..nothing() };
        assert_eq!(notice(&ready), Some(Notice::UpdateNow { version: Some("0.2.0".into()), waiting: true, checking: false }));
        // (said whatever Later did to the notice before: the person has asked for it since)
        assert!(notice(&Situation { ready_closed: true, dismissed: Some("0.2.0"), ..ready }).is_some());
    }

    #[test]
    fn later_sends_a_version_away_and_a_newer_one_is_told_again() {
        let ready = Situation { offered: Some("0.2.0"), stage: Stage::Ready { version: "0.2.0" }, ..nothing() };
        assert_eq!(notice(&Situation { ready_closed: true, ..ready }), None, "Later on the setup that is here");
        assert_eq!(notice(&Situation { dismissed: Some("0.2.0"), offered: None, ..ready }), None, "and at the next start it is the same version");
        assert_eq!(notice(&Situation { dismissed: Some("v0.2.0"), offered: None, ..ready }), None, "however the tag is written");
        let newer = Situation { dismissed: Some("0.2.0"), offered: Some("0.3.0"), stage: Stage::Nothing, ..nothing() };
        assert_eq!(notice(&newer), Some(Notice::Page { version: "0.3.0".into() }));
    }

    #[test]
    fn a_new_build_over_this_one_is_told_without_a_version() {
        assert_eq!(notice(&Situation { new_build: true, ..nothing() }), Some(Notice::UpdateNow { version: None, waiting: false, checking: false }));
    }

    #[test]
    fn the_notice_is_on_the_conversation_and_the_changes_and_not_on_the_first_screens() {
        assert!(is_on_page(true, false, false));
        assert!(is_on_page(false, true, false));
        assert!(!is_on_page(false, false, false), "a page that is a dialog's, or a setting's");
        assert!(!is_on_page(true, false, true));
        assert!(!is_on_page(false, true, true));
    }

    #[test]
    fn the_tray_tells_of_a_version_once_and_only_while_the_window_is_hidden() {
        assert!(balloon_due("0.2.0", true, None, None));
        assert!(!balloon_due("0.2.0", false, None, None), "the window is open: the main screen says it");
        assert!(!balloon_due("0.2.0", true, Some("0.2.0"), None), "once");
        assert!(!balloon_due("v0.2.0", true, Some("0.2.0"), None), "however the tag is written");
        assert!(balloon_due("0.3.0", true, Some("0.2.0"), None), "a newer version is told again");
        assert!(!balloon_due("0.2.0", true, None, Some("0.2.0")), "not one that was sent away");
    }
}
