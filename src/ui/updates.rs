//! Telling the operator a newer version is out: a line at the right of the
//! status bar, which never takes the focus or covers anything, and the
//! Updates part of Settings, About.

use super::*;
use crate::update::{CURRENT, Release, Updates};

/// The check at start waits for the desk to settle first.
pub(super) const CHECK_AFTER: Duration = Duration::from_secs(5);

/// The check running now, or how the last one this run ended.
#[derive(Clone, Debug, PartialEq, Default)]
pub(super) enum Checking {
    #[default]
    Idle,
    Running,
    /// Shown in About only, never on the desk.
    Failed(String),
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl App {
    /// Asks GitHub away from the window's thread; the answer is taken by
    /// `follow_update_check`.
    pub(super) fn check_for_update(&mut self) {
        if self.update_checking == Checking::Running {
            return;
        }
        self.update_checking = Checking::Running;
        self.saved.updates.checked_at = Some(now());
        self.saved.updates.checked_by = CURRENT.into();
        let answer = self.update_answer.clone();
        let fetch = self.update_fetch;
        std::thread::spawn(move || {
            // A panic must still answer, or About would say "Checking" forever.
            let got = std::panic::catch_unwind(|| fetch(CURRENT))
                .unwrap_or_else(|_| Err("the check stopped unexpectedly".into()));
            *answer.lock().unwrap() = Some(got);
        });
    }

    /// Starts the check at start once its time has come, if it is on and a day
    /// has passed, and takes what a check answered.
    pub(super) fn follow_update_check(&mut self) {
        if self.update_at.is_some_and(|at| Instant::now() >= at) {
            self.update_at = None;
            if self.saved.updates.due(now(), CURRENT) {
                self.check_for_update();
            }
        }
        let answer = self.update_answer.lock().unwrap().take();
        match answer {
            Some(Ok(newest)) => {
                self.saved.updates.answered(newest.as_ref());
                self.update_checking = Checking::Idle;
            }
            Some(Err(e)) => self.update_checking = Checking::Failed(e),
            None => {}
        }
    }

    /// At the right of the status line: the version, which opens its page,
    /// and a cross that puts this version's notice away.
    pub(super) fn update_notice(&mut self, ui: &mut egui::Ui) {
        let Some(release) = self.saved.updates.notice(CURRENT) else { return };
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let cross = egui::Button::new(RichText::new("×").size(11.0).color(DIM)).frame(false);
            let cross = named(ui.add(cross), t!("update.dismiss")).on_hover_text(t!("update.dismiss"));
            if cross.clicked() {
                self.saved.updates.dismissed = release.version.clone();
            }
            // The window's own text colour would hide that it is a link.
            let blue = ui.visuals().hyperlink_color;
            let text =
                RichText::new(t!("update.available", version = release.version)).size(11.0).color(blue);
            if ui.link(text).on_hover_text(t!("update.open_page")).clicked() {
                (self.open_in_browser)(&release.page);
            }
        });
    }

    /// Settings, About: the switch, what the last check said, and a way to ask now.
    pub(super) fn about_updates(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(t!("update.title")).strong());
        ui.checkbox(&mut self.saved.updates.check, t!("update.at_start"));
        let u = &self.saved.updates;
        let newer = u.newer(CURRENT);
        let line = match &self.update_checking {
            Checking::Running => t!("update.checking").to_string(),
            Checking::Failed(e) => t!("update.failed", error = e),
            Checking::Idle => last_check(u, newer.as_ref()),
        };
        ui.label(line);
        if let Some(release) = newer
            && ui.link(t!("update.open_page")).clicked()
        {
            (self.open_in_browser)(&release.page);
        }
        let running = self.update_checking == Checking::Running;
        if ui.add_enabled(!running, egui::Button::new(t!("update.check_now"))).clicked() {
            self.check_for_update();
        }
        ui.label(RichText::new(t!("update.what_is_sent")).small().color(DIM));
    }
}

fn last_check(u: &Updates, newer: Option<&Release>) -> String {
    let when = u.checked_at.and_then(|at| chrono::DateTime::from_timestamp(at, 0));
    let Some(when) = when else { return t!("update.never").into() };
    let when = when.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M");
    match newer {
        Some(r) => t!("update.found", when = when, version = r.version),
        None => t!("update.up_to_date", when = when),
    }
}

/// For the check picture of About: as if a check had just found the next version.
pub(super) fn as_if_found(u: &mut Updates) {
    let mut next = semver::Version::parse(CURRENT).expect("Cargo gives a version");
    next.minor += 1;
    next.patch = 0;
    next.pre = semver::Prerelease::EMPTY;
    u.found = Some(next.to_string());
    u.checked_at = Some(now());
    u.checked_by = CURRENT.into();
}
